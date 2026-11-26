//! PostgreSQL storage for testnet custody.
pub mod funding;
pub mod mango;
pub mod mango_coins;
pub mod payouts;
pub mod reconcile;
pub mod registry;
pub mod sweeps;
pub mod wallets;
pub mod withdrawals;
use anyhow::{ensure, Context, Result};
use custody_core::Network;
use serde::Serialize;
use sha2::{Digest, Sha256};
pub use sqlx::PgPool;
use std::collections::HashMap;

pub static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub fn network_key(network: Network) -> &'static str {
    match network {
        Network::MangoTestnet => "mango-testnet",
        Network::BscTestnet => "bsc-testnet",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub height: i64,
    pub digest: [u8; 32],
}

#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    pub tx: [u8; 32],
    pub recipient: Vec<u8>,
    pub sender: Option<Vec<u8>>,
    /// Empty for native, otherwise a raw ERC-20 address or canonical Mango type.
    pub asset: Vec<u8>,
    pub amount: String,
    /// (block-wide index, transaction-local index)
    pub log: Option<(i64, i64)>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OpeningDelta {
    pub address: Vec<u8>,
    pub asset: Vec<u8>,
    pub delta: String,
}

pub struct FinalizedBlock {
    pub openings: Vec<OpeningDelta>,
    pub network: Network,
    pub height: i64,
    pub digest: [u8; 32],
    pub parent: [u8; 32],
    pub receipts: Vec<Receipt>,
}

#[derive(Debug, PartialEq)]
pub enum Applied {
    Committed { deposits: usize },
    AlreadyCommitted,
}

/// Telemetry failures must not alter or advance the authoritative cursor.
pub async fn note_scan_head(db: &PgPool, net: Network, head: i64, committed: i64) {
    if let Err(e)=sqlx::query("INSERT INTO scan_head_samples AS old(network,head,committed) VALUES($1,$2,$3)
        ON CONFLICT(network) DO UPDATE SET head=excluded.head,committed=excluded.committed,sampled_at=clock_timestamp()
        WHERE (old.head,old.committed)<=(excluded.head,excluded.committed)")
        .bind(network_key(net)).bind(head).bind(committed).execute(db).await {
        eprintln!("{} head sample not saved: {e}",network_key(net));
    }
}

pub enum ScanStage {
    Finality,
    Range(i64),
}

/// Persist only the failure stage, never an RPC URL or the provider's response.
/// A telemetry write failure must not replace the scanner's original error.
pub async fn observed<T>(
    db: &PgPool,
    net: Network,
    stage: ScanStage,
    result: Result<T>,
) -> Result<T> {
    if result.is_err() {
        let (stage, height) = match stage {
            ScanStage::Finality => ("finality", None),
            ScanStage::Range(height) => ("range", Some(height)),
        };
        if let Err(e) = sqlx::query("INSERT INTO scan_issues(network,stage,next_height)
            VALUES($1,$2,$3)
            ON CONFLICT(network) DO UPDATE SET stage=excluded.stage,next_height=excluded.next_height,observed_at=excluded.observed_at")
            .bind(network_key(net)).bind(stage).bind(height).execute(db).await {
            eprintln!("{} scanner issue not recorded: {e}",network_key(net));
        }
    }
    result
}

pub async fn cursor(db: &PgPool, net: Network) -> Result<Option<Cursor>> {
    let row: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT height, digest::bytea FROM scan_cursors WHERE network=$1")
            .bind(network_key(net))
            .fetch_optional(db)
            .await?;
    row.map(|(height, bytes)| {
        Ok(Cursor {
            height,
            digest: bytes
                .try_into()
                .ok()
                .context("bad persisted digest length")?,
        })
    })
    .transpose()
}

pub async fn initialize(db: &PgPool, net: Network, anchor: Cursor) -> Result<Cursor> {
    ensure!(
        anchor.height >= -1 && (anchor.height != -1 || anchor.digest == [0; 32]),
        "invalid scan anchor"
    );
    sqlx::query("INSERT INTO scan_cursors(network,height,digest) VALUES($1,$2,$3) ON CONFLICT(network) DO NOTHING")
        .bind(network_key(net)).bind(anchor.height).bind(anchor.digest.as_slice()).execute(db).await?;
    cursor(db, net).await?.context("missing scan cursor")
}

/// Inputs must be in canonical transaction/log order and include all incoming
/// candidates under the active asset rules, regardless of wallet registration.
pub async fn apply(db: &PgPool, b: &FinalizedBlock) -> Result<Applied> {
    apply_with_rules(db, b, None).await
}

/// Keep the original BSC evidence fingerprint for replay across upgrades.
pub async fn configure_tokens(
    db: &PgPool,
    mut tokens: Vec<([u8; 20], String, u8)>,
) -> Result<[u8; 32]> {
    tokens.sort_by_key(|t| t.0);
    ensure!(
        tokens.windows(2).all(|w| w[0].0 != w[1].0),
        "duplicate token"
    );
    let rules: [u8; 32] = Sha256::digest(serde_json::to_vec(&tokens)?).into();
    registry::activate(
        db,
        Network::BscTestnet,
        tokens
            .into_iter()
            .map(|(id, s, d)| (id.to_vec(), s, d.into()))
            .collect(),
        |_| Ok(Some(rules)),
    )
    .await?;
    Ok(rules)
}

pub async fn apply_with_rules(
    db: &PgPool,
    b: &FinalizedBlock,
    rules: Option<[u8; 32]>,
) -> Result<Applied> {
    apply_evidence(db, b, rules, None).await
}

pub async fn apply_evidence(
    db: &PgPool,
    b: &FinalizedBlock,
    rules: Option<[u8; 32]>,
    executions: Option<&[payouts::Execution]>,
) -> Result<Applied> {
    apply_all(db, b, rules, executions, None, None).await
}
pub async fn apply_checkpoint(
    db: &PgPool,
    b: &FinalizedBlock,
    executions: &[mango::Execution],
) -> Result<Applied> {
    ensure!(
        b.network == Network::MangoTestnet,
        "expected Mango checkpoint"
    );
    apply_all(db, b, None, None, Some(executions), None).await
}
pub async fn apply_mango_full(
    db: &PgPool,
    b: &FinalizedBlock,
    executions: &[mango::Execution],
    coins: &mango_coins::Changes,
) -> Result<Applied> {
    apply_mango_with_rules(db, b, executions, coins, None).await
}
pub async fn apply_mango_with_rules(
    db: &PgPool,
    b: &FinalizedBlock,
    executions: &[mango::Execution],
    coins: &mango_coins::Changes,
    rules: Option<[u8; 32]>,
) -> Result<Applied> {
    ensure!(
        b.network == Network::MangoTestnet,
        "expected Mango checkpoint"
    );
    apply_all(db, b, rules, None, Some(executions), Some(coins)).await
}
async fn apply_all(
    db: &PgPool,
    b: &FinalizedBlock,
    rules: Option<[u8; 32]>,
    executions: Option<&[payouts::Execution]>,
    mango: Option<&[mango::Execution]>,
    coins: Option<&mango_coins::Changes>,
) -> Result<Applied> {
    let net = network_key(b.network);
    let width = if b.network == Network::MangoTestnet {
        32
    } else {
        20
    };
    let mut addresses = Vec::new();
    let mut evidence = Sha256::new();
    for r in &b.receipts {
        ensure!(
            r.recipient.len() == width && r.sender.as_ref().is_none_or(|a| a.len() == width),
            "invalid receipt address"
        );
        ensure!(
            !r.amount.is_empty()
                && !r.amount.starts_with('0')
                && r.amount.bytes().all(|b| b.is_ascii_digit()),
            "invalid raw amount"
        );
        ensure!(
            (r.asset.is_empty() && r.log.is_none())
                || (b.network == Network::MangoTestnet
                    && !r.asset.is_empty()
                    && r.asset.len() <= 512
                    && rules.is_some()
                    && r.log.is_none())
                || (b.network == Network::BscTestnet
                    && r.asset.len() == 20
                    && r.log.is_some_and(|(block, tx)| block >= 0 && tx >= 0)),
            "invalid asset or log locator"
        );
        ensure!(
            b.network != Network::BscTestnet || r.sender.is_some(),
            "missing EVM sender"
        );
        addresses.push(&r.recipient);
        addresses.extend(r.sender.iter());
        serde_json::to_writer(&mut evidence, r)?;
    }
    if let Some(executions) = executions {
        ensure!(
            b.network == Network::BscTestnet,
            "unexpected EVM execution evidence"
        );
        use sha2::Digest;
        evidence.update(b"evm-executions-v1:");
        serde_json::to_writer(&mut evidence, executions)?;
    }
    addresses.sort_unstable();
    let mango_hash = if let Some(items) = mango {
        let mut h = Sha256::new();
        serde_json::to_writer(&mut h, items)?;
        Some(h.finalize().to_vec())
    } else {
        None
    };
    addresses.dedup();
    let evidence: [u8; 32] = evidence.finalize().into();
    let mut opening_hash = Sha256::new();
    serde_json::to_writer(&mut opening_hash, &b.openings)?;
    let opening_hash: [u8; 32] = opening_hash.finalize().into();
    let mut tx = db.begin().await?;
    let (last, digest, current_rules): (i64, Vec<u8>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT height,digest::bytea,rules::bytea FROM scan_cursors WHERE network=$1 FOR UPDATE",
    )
    .bind(net)
    .fetch_one(&mut *tx)
    .await
    .context("reading initialized cursor")?;
    ensure!(
        current_rules.as_deref() == rules.as_ref().map(|r| r.as_slice()),
        "asset rules changed: discard prefetched results and reload configuration"
    );
    if b.network == Network::MangoTestnet {
        let types: std::collections::BTreeSet<_> = b
            .receipts
            .iter()
            .filter(|r| !r.asset.is_empty())
            .map(|r| r.asset.as_slice())
            .collect();
        if !types.is_empty() {
            let active:i64 = sqlx::query_scalar("SELECT count(*) FROM assets WHERE network='mango-testnet' AND enabled_from<=$1 AND asset=ANY($2::bytea[])")
                .bind(b.height).bind(types.iter().copied().collect::<Vec<_>>()).fetch_one(&mut *tx).await?;
            ensure!(
                active as usize == types.len(),
                "inactive Mango deposit asset"
            );
        }
    }
    if b.height <= last {
        let same: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM scanned_blocks WHERE network=$1 AND height=$2 AND digest=$3 AND parent=$4 AND evidence_hash=$5 AND (mango_execution_hash IS NULL OR mango_execution_hash=$6) AND opening_evidence=$7)")
            .bind(net).bind(b.height).bind(b.digest.as_slice()).bind(b.parent.as_slice()).bind(evidence.as_slice())
            .bind(&mango_hash).bind(opening_hash.as_slice())
            .fetch_one(&mut *tx).await?;
        ensure!(
            same,
            "conflicting replay or height at/before initial anchor"
        );
        tx.commit().await?;
        return Ok(Applied::AlreadyCommitted);
    }
    ensure!(
        last.checked_add(1) == Some(b.height) && digest == b.parent,
        "non-contiguous height or parent digest"
    );
    sqlx::query("INSERT INTO scanned_blocks(network,height,digest,parent,evidence_hash,mango_execution_hash,opening_evidence) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(net).bind(b.height).bind(b.digest.as_slice()).bind(b.parent.as_slice()).bind(evidence.as_slice()).bind(mango_hash).bind(opening_hash.as_slice()).execute(&mut *tx).await?;
    if !b.openings.is_empty() {
        let owners: Vec<_> = b.openings.iter().map(|d| d.address.as_slice()).collect();
        let assets: Vec<_> = b.openings.iter().map(|d| d.asset.as_slice()).collect();
        let amounts: Vec<_> = b.openings.iter().map(|d| d.delta.as_str()).collect();
        sqlx::query("INSERT INTO opening_balances(network,address,asset,amount)
            SELECT $1,x.address,x.asset,sum(x.delta::numeric)
            FROM unnest($2::bytea[],$3::bytea[],$4::text[]) x(address,asset,delta)
            JOIN wallets w ON w.network=$1 AND w.address=x.address
            WHERE NOT EXISTS(SELECT 1 FROM assets a WHERE a.network=$1 AND a.asset=x.asset)
            GROUP BY x.address,x.asset HAVING sum(x.delta::numeric)<>0
            ON CONFLICT(network,asset,address) DO UPDATE SET amount=opening_balances.amount+excluded.amount")
            .bind(net).bind(owners).bind(assets).bind(amounts).execute(&mut *tx).await?;
    }
    let wallets: HashMap<Vec<u8>, Option<String>> = sqlx::query_as::<_, (Vec<u8>, Option<String>)>(
        "SELECT address,owner_id FROM wallets WHERE network=$1 AND address=ANY($2::bytea[])",
    )
    .bind(net)
    .bind(&addresses)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let mut deposits = 0;
    let mut dirty = std::collections::BTreeSet::new();
    for r in &b.receipts {
        let Some(owner) = wallets.get(&r.recipient) else {
            continue;
        };
        if owner.is_some() {
            dirty.insert(r.recipient.as_slice());
        }
        let planned: Option<(Vec<u8>, String)> = if b.network == Network::MangoTestnet {
            sqlx::query_as("SELECT sender,amount::text FROM mango_internal_outputs WHERE network=$1 AND tx=$2 AND recipient=$3 AND asset=$4")
                .bind(net).bind(r.tx.as_slice()).bind(&r.recipient).bind(&r.asset).fetch_optional(&mut *tx).await?
        } else {
            None
        };
        if let Some((_, amount)) = &planned {
            ensure!(
                amount == &r.amount,
                "Mango internal receipt differs from signed order"
            );
        }
        if let Some(sender) = planned
            .as_ref()
            .map(|p| &p.0)
            .or_else(|| r.sender.as_ref().filter(|s| wallets.contains_key(*s)))
        {
            sqlx::query("INSERT INTO internal_receipts(network,height,tx,sender,recipient,asset,amount,log_index) VALUES($1,$2,$3,$4,$5,$6,$7::text::positive_raw,$8)")
                .bind(net).bind(b.height).bind(r.tx.as_slice()).bind(sender).bind(&r.recipient).bind(&r.asset).bind(&r.amount)
                .bind(r.log.map(|l|l.0)).execute(&mut *tx).await?;
            continue;
        }
        let Some(user) = owner else {
            let origin = mango.and_then(|xs| xs.iter().find(|e| e.hash == r.tx));
            let managed = if let Some(e) = origin {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM wallets WHERE network=$1 AND address=$2)",
                )
                .bind(net)
                .bind(e.sender.as_slice())
                .fetch_one(&mut *tx)
                .await?
            } else {
                false
            };
            if !managed {
                funding::record(&mut tx, b, r).await?;
            }
            continue;
        };
        let id: i64 = sqlx::query_scalar("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES($1,$2,'custody',$3,$4::text::positive_raw) RETURNING id")
            .bind(net).bind(&r.asset).bind(format!("user:{user}:available")).bind(&r.amount).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO deposits(network,height,tx,recipient,sender,asset,log_index,tx_log_index,journal_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(net).bind(b.height).bind(r.tx.as_slice()).bind(&r.recipient).bind(&r.sender).bind(&r.asset)
            .bind(r.log.map(|l|l.0)).bind(r.log.map(|l|l.1)).bind(id).execute(&mut *tx).await?;
        deposits += 1;
    }
    if let Some(executions) = executions {
        payouts::settle(&mut tx, b, executions).await?;
    }
    if !dirty.is_empty() {
        sqlx::query("INSERT INTO sweep_checks(network,address) SELECT $1,x FROM unnest($2::bytea[]) x
          ON CONFLICT(network,address) DO UPDATE SET revision=sweep_checks.revision+1,
          next_check=CASE WHEN sweep_checks.finalized_failures>0 THEN greatest(sweep_checks.next_check,now()) ELSE now() END
          WHERE sweep_checks.finalized_failures<4")
          .bind(net).bind(dirty.into_iter().collect::<Vec<_>>()).execute(&mut *tx).await?;
    }
    if let Some(items) = mango {
        mango::settle(&mut tx, b, items).await?;
    }
    if let Some(changes) = coins {
        mango_coins::apply(&mut tx, b.height, changes).await?;
    }
    sqlx::query("UPDATE scan_cursors SET height=$2,digest=$3,last_advanced_at=clock_timestamp() WHERE network=$1")
        .bind(net)
        .bind(b.height)
        .bind(b.digest.as_slice())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied::Committed { deposits })
}
