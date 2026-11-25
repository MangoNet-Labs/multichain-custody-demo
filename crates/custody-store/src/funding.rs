use crate::{mango, network_key, payouts, FinalizedBlock, PgPool, Receipt};
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

// Caller holds the chain cursor lock; never overwrite an earlier observation.
pub(crate) async fn record(
    tx: &mut Transaction<'_, Postgres>,
    b: &FinalizedBlock,
    r: &Receipt,
) -> Result<bool> {
    let net = network_key(b.network);
    let existing:Option<(i64,String)>=sqlx::query_as("SELECT height,amount::text FROM platform_funding f JOIN journal j ON j.id=f.journal_id
        WHERE f.network=$1 AND f.tx=$2 AND f.recipient=$3 AND f.asset=$4 AND log_index IS NOT DISTINCT FROM $5")
        .bind(net).bind(r.tx.as_slice()).bind(&r.recipient).bind(&r.asset).bind(r.log.map(|l|l.0)).fetch_optional(&mut **tx).await?;
    if let Some((h, amount)) = existing {
        ensure!(
            h == b.height && amount == r.amount,
            "platform funding evidence differs"
        );
        return Ok(false);
    }
    let id:i64=sqlx::query_scalar("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES($1,$2,'custody','platform:capital',$3::text::positive_raw) RETURNING id")
        .bind(net).bind(&r.asset).bind(&r.amount).fetch_one(&mut **tx).await?;
    sqlx::query("INSERT INTO platform_funding(network,height,tx,recipient,asset,log_index,sender,journal_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(net).bind(b.height).bind(r.tx.as_slice()).bind(&r.recipient).bind(&r.asset).bind(r.log.map(|l|l.0)).bind(&r.sender).bind(id).execute(&mut **tx).await?;
    Ok(true)
}
/// Upgrade old installations without inventing an opening balance. A replay may
/// only book external platform receipts from a previously committed identical block.
pub async fn backfill(
    db: &PgPool,
    b: &FinalizedBlock,
    evm: Option<&[payouts::Execution]>,
    mgo: Option<&[mango::Execution]>,
) -> Result<usize> {
    let net = network_key(b.network);
    ensure!(
        if net == "bsc-testnet" {
            evm.is_some() && mgo.is_none()
        } else {
            mgo.is_some() && evm.is_none()
        },
        "missing chain evidence"
    );
    let mut h = Sha256::new();
    for r in &b.receipts {
        serde_json::to_writer(&mut h, r)?;
    }
    if let Some(items) = evm {
        h.update(b"evm-executions-v1:");
        serde_json::to_writer(&mut h, items)?;
    }
    let mango_hash = if let Some(items) = mgo {
        let mut m = Sha256::new();
        serde_json::to_writer(&mut m, items)?;
        Some(m.finalize().to_vec())
    } else {
        None
    };
    let mut tx = db.begin().await?;
    sqlx::query("SELECT height FROM scan_cursors WHERE network=$1 FOR UPDATE")
        .bind(net)
        .fetch_one(&mut *tx)
        .await?;
    let ok:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM scanned_blocks WHERE network=$1 AND height=$2 AND digest=$3 AND parent=$4 AND evidence_hash=$5 AND mango_execution_hash IS NOT DISTINCT FROM $6)")
        .bind(net).bind(b.height).bind(b.digest.as_slice()).bind(b.parent.as_slice()).bind(h.finalize().to_vec()).bind(mango_hash).fetch_one(&mut *tx).await?;
    ensure!(
        ok,
        "funding replay does not match saved checkpoint/block evidence"
    );
    let mut count = 0;
    for r in &b.receipts {
        let sender = r.sender.as_deref().or_else(|| {
            mgo.and_then(|xs| xs.iter().find(|e| e.hash == r.tx))
                .map(|e| e.sender.as_slice())
        });
        let external:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM wallets WHERE network=$1 AND address=$2 AND role IN ('hot','gas'))
            AND NOT EXISTS(SELECT 1 FROM wallets WHERE network=$1 AND address=$3)
            AND NOT EXISTS(SELECT 1 FROM internal_receipts WHERE network=$1 AND tx=$4 AND recipient=$2 AND asset=$5 AND log_index IS NOT DISTINCT FROM $6)")
            .bind(net).bind(&r.recipient).bind(sender).bind(r.tx.as_slice()).bind(&r.asset).bind(r.log.map(|l|l.0)).fetch_one(&mut *tx).await?;
        if external && record(&mut tx, b, r).await? {
            count += 1;
        }
    }
    tx.commit().await?;
    Ok(count)
}
