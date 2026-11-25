use super::rpc::BscRpc;
use alloy_primitives::{keccak256, Bytes, B256};
use anyhow::{ensure, Result};
use custody_store::PgPool;
use serde_json::json;

/// Claim one committed outbox row before network I/O. Losing the RPC response
/// is indistinguishable from success until the finalized block is scanned.
pub async fn send_ready(db: &PgPool, node: &BscRpc) -> Result<bool> {
    let mut tx = db.begin().await?;
    let paused: bool =
        sqlx::query_scalar("SELECT paused FROM payment_control WHERE singleton FOR SHARE")
            .fetch_one(&mut *tx)
            .await?;
    if paused {
        return Ok(false);
    }
    #[derive(sqlx::FromRow)]
    struct Outbox {
        id: String,
        sweep: bool,
        digest: Vec<u8>,
        signed_tx: Vec<u8>,
    }
    let row:Option<Outbox>=sqlx::query_as("SELECT coalesce(p.withdrawal_id,p.sweep_id)::text AS id,p.sweep_id IS NOT NULL AS sweep,p.digest::bytea,p.signed_tx
        FROM evm_payouts p LEFT JOIN withdrawals w ON w.id=p.withdrawal_id LEFT JOIN sweep_orders s ON s.id=p.sweep_id
        WHERE coalesce(w.status,s.status) IN ('signed','submitted') AND p.confirmed_height IS NULL AND p.next_send<=now()
        ORDER BY p.next_send LIMIT 1 FOR UPDATE OF p SKIP LOCKED").fetch_optional(&mut *tx).await?;
    let Some(Outbox {
        id,
        sweep,
        digest,
        signed_tx: raw,
    }) = row
    else {
        return Ok(false);
    };
    let expected = keccak256(&raw);
    ensure!(
        expected.as_slice() == digest,
        "stored signed transaction/hash mismatch"
    );
    sqlx::query("UPDATE evm_payouts SET send_count=send_count+1,next_send=now()+interval '30 seconds' WHERE digest=$1")
        .bind(&digest).execute(&mut *tx).await?;
    tx.commit().await?;
    let rpc = node.clone();
    let response = tokio::task::spawn_blocking(move || -> Result<()> {
        rpc.verify()?;
        let reported: B256 = rpc.call("eth_sendRawTransaction", json!([Bytes::from(raw)]))?;
        ensure!(
            reported == expected,
            "node returned another transaction hash"
        );
        Ok(())
    })
    .await?;
    let table = if sweep { "sweep_orders" } else { "withdrawals" };
    let detail = if response.is_ok() {
        "waiting for finalized execution"
    } else {
        "broadcast unconfirmed; retrying the same signed bytes"
    };
    sqlx::query(&format!("UPDATE {table} SET status=CASE WHEN $2 THEN 'submitted' ELSE status END,detail=$3 WHERE id=$1::text::uuid AND status IN ('signed','submitted')"))
        .bind(id).bind(response.is_ok()).bind(detail).execute(db).await?;
    response?;
    Ok(true)
}
