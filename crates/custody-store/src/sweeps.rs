use crate::{payouts::Execution, FinalizedBlock};
use anyhow::{ensure, Result};
use sqlx::{FromRow, Postgres, Transaction};
use std::collections::HashMap;

#[derive(FromRow)]
struct Attempt {
    id: String,
    deposit_address: Vec<u8>,
    sender: Vec<u8>,
    recipient: Vec<u8>,
    asset: Vec<u8>,
    amount: String,
    nonce: i64,
    digest: Vec<u8>,
    max_fee: String,
}
pub(crate) async fn settle_bsc(
    tx: &mut Transaction<'_, Postgres>,
    b: &FinalizedBlock,
    by_nonce: &HashMap<(&[u8], u64), &Execution>,
    senders: &[&[u8]],
) -> Result<()> {
    let pending:Vec<Attempt>=sqlx::query_as("SELECT s.id::text,s.deposit_address,s.sender,s.recipient,s.asset,s.amount::text,p.nonce,p.digest::bytea,p.max_fee::text
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
        sqlx::query("UPDATE sweep_checks SET revision=revision+1,next_check=CASE WHEN $2='completed' THEN now() ELSE NULL END,detail=$3 WHERE network='bsc-testnet' AND address=$1")
            .bind(&s.deposit_address).bind(status).bind(detail).execute(&mut **tx).await?;
    }
    Ok(())
}
