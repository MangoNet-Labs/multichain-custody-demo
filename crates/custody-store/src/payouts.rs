use crate::FinalizedBlock;
use anyhow::{ensure, Result};
use serde::Serialize;
use sqlx::{FromRow, Postgres, Transaction};
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct Execution {
    pub hash: [u8; 32],
    pub sender: [u8; 20],
    pub nonce: u64,
    pub success: bool,
    pub fee: String,
}

#[derive(FromRow)]
struct Attempt {
    id: String,
    user_id: String,
    asset: Vec<u8>,
    recipient: Vec<u8>,
    amount: String,
    sender: Vec<u8>,
    nonce: i64,
    digest: Vec<u8>,
    max_fee: String,
}

pub(crate) async fn settle(
    tx: &mut Transaction<'_, Postgres>,
    b: &FinalizedBlock,
    executions: &[Execution],
) -> Result<()> {
    let mut by_nonce = HashMap::new();
    for e in executions {
        ensure!(
            by_nonce.insert((e.sender.as_slice(), e.nonce), e).is_none(),
            "duplicate sender/nonce in block"
        );
    }
    let senders: Vec<&[u8]> = executions.iter().map(|e| e.sender.as_slice()).collect();
    let rows:Vec<Attempt>=sqlx::query_as("SELECT w.id::text,w.user_id,w.asset,w.recipient,w.amount::text,p.sender,p.nonce,p.digest::bytea,p.max_fee::text FROM evm_payouts p JOIN withdrawals w ON w.id=p.withdrawal_id WHERE p.confirmed_height IS NULL AND p.sender=ANY($1::bytea[]) AND w.status IN ('signed','submitted') FOR UPDATE OF w,p")
        .bind(&senders).fetch_all(&mut **tx).await?;
    for p in rows {
        let Some(e) = by_nonce.get(&(p.sender.as_slice(), p.nonce as u64)) else {
            continue;
        };
        let exact = e.hash.as_slice() == p.digest;
        let mut gas = None;
        let mut journal = None;
        let (status, detail) = if !exact {
            (
                "review",
                "finalized nonce was consumed by another transaction",
            )
        } else {
            let fee_ok: bool =
                sqlx::query_scalar("SELECT $1::text::numeric BETWEEN 0 AND $2::text::numeric")
                    .bind(&e.fee)
                    .bind(&p.max_fee)
                    .fetch_one(&mut **tx)
                    .await?;
            ensure!(fee_ok, "actual fee exceeds the signed maximum");
            if e.fee != "0" {
                gas=Some(sqlx::query_scalar::<_,i64>("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES('bsc-testnet','','platform:gas','custody',$1::text::positive_raw) RETURNING id")
                    .bind(&e.fee).fetch_one(&mut **tx).await?);
            }
            let mut outgoing = b
                .receipts
                .iter()
                .filter(|r| r.tx == e.hash && r.sender.as_ref() == Some(&p.sender));
            let paid = outgoing.next().is_some_and(|r| {
                r.asset == p.asset && r.recipient == p.recipient && r.amount == p.amount
            }) && outgoing.next().is_none();
            if e.success && !paid {
                (
                    "review",
                    "successful receipt does not prove the ordered payment",
                )
            } else {
                let credit = if e.success {
                    "custody".into()
                } else {
                    format!("user:{}:available", p.user_id)
                };
                journal=Some(sqlx::query_scalar::<_,i64>("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES('bsc-testnet',$1,$2,$3,$4::text::positive_raw) RETURNING id")
                    .bind(&p.asset).bind(format!("user:{}:frozen",p.user_id)).bind(credit).bind(&p.amount).fetch_one(&mut **tx).await?);
                if e.success {
                    ("completed", "finalized payment matched")
                } else {
                    ("failed", "finalized transaction reverted")
                }
            }
        };
        sqlx::query("UPDATE withdrawals SET status=$2,detail=$3 WHERE id=$1::text::uuid")
            .bind(&p.id)
            .bind(status)
            .bind(detail)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE evm_payouts SET confirmed_height=$2,observed_tx=$3,settlement_journal=$4,gas_journal=$5 WHERE withdrawal_id=$1::text::uuid")
            .bind(&p.id).bind(b.height).bind(e.hash.as_slice()).bind(journal).bind(gas).execute(&mut **tx).await?;
    }
    crate::sweeps::settle_bsc(tx, b, &by_nonce, &senders).await
}
