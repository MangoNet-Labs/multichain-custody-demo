use super::{reconcile, Node};
use anyhow::{ensure, Context, Result};
use custody_store::{reconcile::MangoInventory, Cursor, PgPool};
use mango_worker::rules::{Rules, Token};
use sqlx::{PgConnection, Row};
use std::{
    collections::{BTreeMap, HashSet},
    env,
    num::NonZeroUsize,
    path::PathBuf,
};
#[cfg(test)]
mod tests;

pub fn configured() -> Result<Rules> {
    let path = match env::var_os("MANGO_COIN_ALLOWLIST") {
        Some(path) => PathBuf::from(path),
        None => {
            let name = "config/mango-testnet-coins.json";
            if std::path::Path::new(name).is_file() {
                name.into()
            } else {
                PathBuf::from("..").join(name)
            }
        }
    };
    let tokens: Vec<Token> = serde_json::from_reader(
        std::fs::File::open(&path)
            .with_context(|| format!("opening Mango coin allowlist {}", path.display()))?,
    )?;
    Rules::new(tokens.into_iter().map(|t| (t, 0)))
}

async fn rows(conn: &mut PgConnection) -> Result<Rules> {
    let rows=sqlx::query("SELECT convert_from(asset,'UTF8') AS coin_type,symbol,decimals,enabled_from FROM assets WHERE network='mango-testnet' AND asset<>''")
        .fetch_all(conn).await?;
    Rules::new(
        rows.into_iter()
            .map(|r| -> Result<_> {
                Ok((
                    Token {
                        coin_type: r.try_get("coin_type")?,
                        symbol: r.try_get("symbol")?,
                        decimals: r.try_get::<i16, _>("decimals")?.try_into()?,
                    },
                    r.try_get("enabled_from")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?,
    )
}

pub async fn stored(db: &PgPool) -> Result<Rules> {
    rows(&mut *db.acquire().await?).await
}

async fn locked_anchor(conn: &mut PgConnection) -> Result<(Cursor, Option<Vec<u8>>)> {
    let (height,digest,stamp):(i64,Vec<u8>,Option<Vec<u8>>)=sqlx::query_as(
        "SELECT height,digest::bytea,rules::bytea FROM scan_cursors WHERE network='mango-testnet' FOR UPDATE")
        .fetch_one(conn).await?;
    Ok((
        Cursor {
            height,
            digest: digest.as_slice().try_into()?,
        },
        stamp,
    ))
}
async fn managed_owners(conn: &mut PgConnection) -> Result<HashSet<[u8; 32]>> {
    Ok(sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT address FROM wallets WHERE network='mango-testnet'",
    )
    .fetch_all(conn)
    .await?
    .iter()
    .map(|v| v.as_slice().try_into())
    .collect::<Result<_, _>>()?)
}

pub async fn activate(
    db: &PgPool,
    node: &Node,
    limits: (NonZeroUsize, NonZeroUsize),
    wanted: &Rules,
) -> Result<Rules> {
    let mut tx = db.begin().await?;
    let (anchor, stamp) = locked_anchor(&mut tx).await?;
    let known = rows(&mut tx).await?;
    ensure!(
        stamp == known.fingerprint().map(|h| h.to_vec()),
        "Mango asset registry fingerprint mismatch"
    );
    let mut added: BTreeMap<_, _> = wanted
        .entries()
        .map(|(t, _)| (t.coin_type.as_str(), t))
        .collect();
    for (t, _) in known.entries() {
        ensure!(
            added.remove(t.coin_type.as_str()) == Some(t),
            "cannot remove or redefine an enabled Mango coin"
        );
    }
    if added.is_empty() {
        tx.commit().await?;
        return Ok(known);
    }
    let from = anchor
        .height
        .checked_add(1)
        .context("checkpoint overflow")?;
    let owners = managed_owners(&mut tx).await?;
    if !owners.is_empty() {
        let first:Option<(i64,Vec<u8>)>=sqlx::query_as("SELECT height,parent::bytea FROM scanned_blocks WHERE network='mango-testnet' ORDER BY height LIMIT 1").fetch_optional(&mut *tx).await?;
        if let Some((first, parent)) = first {
            // Never hold a SQL transaction across a historical REST replay.
            // Both address issuance and block commits take the cursor lock.
            tx.commit().await?;
            let check = node.with_rules(Rules::new(added.values().map(|t| ((*t).clone(), 0)))?);
            let mut inventory = MangoInventory::new();
            reconcile::replay(
                &check,
                limits,
                Cursor {
                    height: first - 1,
                    digest: parent.as_slice().try_into()?,
                },
                anchor,
                &owners,
                &mut inventory,
            )
            .await
            .context("checking opening Mango token inventory")?;
            ensure!(inventory.values().all(|(_,asset,raw)|asset.is_empty() || *raw==0),
                "cannot enable Mango token with a nonzero opening balance in a managed wallet; no historical deposits were credited");
            tx = db.begin().await?;
            ensure!(locked_anchor(&mut tx).await?==(anchor,stamp) && managed_owners(&mut tx).await?==owners,
                "Mango cursor, rules or managed wallets changed during opening check; restart activation");
        }
    }
    for t in added.values() {
        sqlx::query("INSERT INTO assets(network,asset,symbol,decimals,enabled_from) VALUES('mango-testnet',$1,$2,$3,$4)")
            .bind(t.coin_type.as_bytes()).bind(&t.symbol).bind(i16::from(t.decimals)).bind(from).execute(&mut *tx).await?;
    }
    let active = rows(&mut tx).await?;
    sqlx::query("UPDATE scan_cursors SET rules=$1 WHERE network='mango-testnet'")
        .bind(active.fingerprint().map(|h| h.to_vec()))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(active)
}
