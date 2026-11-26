use crate::{payouts::Execution, FinalizedBlock};
use anyhow::{ensure, Result};
use sqlx::{FromRow, Postgres, Transaction};
use std::collections::HashMap;

#[derive(FromRow)]
struct Attempt {
    id: String,
    kind: String,
    deposit_address: Vec<u8>,
    sender: Vec<u8>,
    recipient: Vec<u8>,
    asset: Vec<u8>,
    amount: String,
    nonce: i64,
    digest: Vec<u8>,
    max_fee: String,
}

/// Called only while settling new finalized evidence. A top-up is not a
/// successful sweep: resetting its budget could otherwise burn gas forever.
pub(crate) async fn schedule_after_settlement(
    tx: &mut Transaction<'_, Postgres>,
    network: &str,
    address: &[u8],
    status: &str,
    detail: &str,
    topup: bool,
) -> Result<()> {
    ensure!(
        matches!(status, "completed" | "failed" | "review"),
        "invalid settlement status"
    );
    sqlx::query("UPDATE sweep_checks SET revision=revision+1,lease=NULL,lease_until=NULL,
        finalized_failures=CASE WHEN $3='failed' THEN least(finalized_failures+1,4)
          WHEN $3='completed' AND NOT $5 THEN 0 ELSE finalized_failures END,
        next_check=CASE WHEN $3='completed' THEN now()
          WHEN $3='failed' AND finalized_failures<3 THEN now()+interval '30 seconds'*(1<<finalized_failures::int)
          ELSE NULL END,
        detail=CASE WHEN $3='failed' AND finalized_failures>=3 THEN $4 || '; retry limit reached' ELSE $4 END
        WHERE network=$1 AND address=$2")
        .bind(network).bind(address).bind(status).bind(detail).bind(topup).execute(&mut **tx).await?;
    Ok(())
}
pub(crate) async fn settle_bsc(
    tx: &mut Transaction<'_, Postgres>,
    b: &FinalizedBlock,
    by_nonce: &HashMap<(&[u8], u64), &Execution>,
    senders: &[&[u8]],
) -> Result<()> {
    let pending:Vec<Attempt>=sqlx::query_as("SELECT s.id::text,s.kind,s.deposit_address,s.sender,s.recipient,s.asset,s.amount::text,p.nonce,p.digest::bytea,p.max_fee::text
        FROM sweep_orders s JOIN evm_payouts p ON p.sweep_id=s.id WHERE p.confirmed_height IS NULL AND p.sender=ANY($1::bytea[]) AND s.status IN ('signed','submitted') FOR UPDATE OF s,p")
        .bind(senders).fetch_all(&mut **tx).await?;
    for s in pending {
        let Some(e) = by_nonce.get(&(s.sender.as_slice(), s.nonce as u64)) else {
            continue;
        };
        let mut gas = None;
        let (status, detail) = if s.digest != e.hash {
            ("review", "sweep nonce consumed by another transaction")
        } else {
            let valid: bool =
                sqlx::query_scalar("SELECT $1::text::numeric BETWEEN 0 AND $2::text::numeric")
                    .bind(&e.fee)
                    .bind(&s.max_fee)
                    .fetch_one(&mut **tx)
                    .await?;
            ensure!(valid, "actual sweep gas exceeds signed maximum");
            if e.fee != "0" {
                gas=Some(sqlx::query_scalar::<_,i64>("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES('bsc-testnet','','platform:gas','custody',$1::text::positive_raw) RETURNING id")
                    .bind(&e.fee).fetch_one(&mut **tx).await?);
            }
            let mut outgoing = b
                .receipts
                .iter()
                .filter(|r| r.tx == e.hash && r.sender.as_deref() == Some(&s.sender));
            let paid = outgoing.next().is_some_and(|r| {
                r.recipient == s.recipient && r.asset == s.asset && r.amount == s.amount
            }) && outgoing.next().is_none();
            match (e.success, paid) {
                (true, true) => ("completed", "confirmed internal transfer"),
                (false, _) => ("failed", "sweep transaction reverted"),
                _ => (
                    "review",
                    "receipt does not prove the planned internal transfer",
                ),
            }
        };
        sqlx::query("UPDATE sweep_orders SET status=$2,detail=$3 WHERE id=$1::text::uuid")
            .bind(&s.id)
            .bind(status)
            .bind(detail)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE evm_payouts SET confirmed_height=$2,observed_tx=$3,gas_journal=$4 WHERE sweep_id=$1::text::uuid")
            .bind(&s.id).bind(b.height).bind(e.hash.as_slice()).bind(gas).execute(&mut **tx).await?;
        schedule_after_settlement(
            tx,
            "bsc-testnet",
            &s.deposit_address,
            status,
            detail,
            s.kind == "gas_topup",
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{apply, initialize, network_key, Cursor, PgPool, Receipt};
    use custody_core::Network::{BscTestnet, MangoTestnet};

    #[sqlx::test(migrations = "../../migrations")]
    #[ignore = "run sh scripts/test-postgres.sh"]
    async fn finalized_retry_contract_for_both_networks(db: PgPool) -> Result<()> {
        for (net, len) in [(BscTestnet, 20), (MangoTestnet, 32)] {
            let k = network_key(net);
            let addr = vec![1u8; len];
            initialize(
                &db,
                net,
                Cursor {
                    height: 0,
                    digest: [0; 32],
                },
            )
            .await?;
            sqlx::query("INSERT INTO wallets VALUES($1,$2,'deposit','alice',$3)")
                .bind(k)
                .bind(&addr)
                .bind(k)
                .execute(&db)
                .await?;
            let deposit = |h: u8| FinalizedBlock {
                openings: vec![],
                network: net,
                height: h.into(),
                digest: [h; 32],
                parent: [h - 1; 32],
                receipts: vec![Receipt {
                    tx: [h; 32],
                    recipient: addr.clone(),
                    sender: Some(vec![2; len]),
                    asset: vec![],
                    amount: "1".into(),
                    log: None,
                }],
            };
            apply(&db, &deposit(1)).await?;
            for n in 1..=4i16 {
                let mut tx = db.begin().await?;
                schedule_after_settlement(&mut tx, k, &addr, "failed", "execution failed", false)
                    .await?;
                let (failures,wait):(i16,Option<i32>)=sqlx::query_as(
                    "SELECT finalized_failures,extract(epoch FROM next_check-now())::int FROM sweep_checks WHERE network=$1")
                    .bind(k).fetch_one(&mut *tx).await?;
                assert_eq!(failures, n);
                assert_eq!(wait, (n < 4).then(|| 30 << (n - 1)));
                tx.commit().await?;
                apply(&db, &deposit(n as u8 + 1)).await?;
                let preserved:bool=sqlx::query_scalar("SELECT CASE WHEN finalized_failures=4 THEN next_check IS NULL ELSE next_check>now() END FROM sweep_checks WHERE network=$1")
                    .bind(k).fetch_one(&db).await?;
                assert!(preserved);
                if n == 1 && net == BscTestnet {
                    let mut tx = db.begin().await?;
                    schedule_after_settlement(&mut tx, k, &addr, "completed", "gas funded", true)
                        .await?;
                    tx.commit().await?;
                }
            }
        }
        Ok(())
    }
}
