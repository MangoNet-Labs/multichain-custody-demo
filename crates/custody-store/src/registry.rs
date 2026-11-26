use crate::{network_key, PgPool};
use anyhow::{ensure, Result};
use custody_core::Network;

/// Canonical asset identifier, display symbol and decimal precision. The native
/// asset belongs to the network and is not part of the configurable list.
pub type Asset = (Vec<u8>, String, i16);

type Row = (Vec<u8>, String, i16, i64);

/// Workers may validate prospective additions, but only a scanner holding its
/// cursor can activate them. An older configuration cannot remove a listed coin.
pub async fn validate_config(db: &PgPool, network: Network, wanted: Vec<Asset>) -> Result<()> {
    let net = network_key(network);
    let stored: Vec<Asset> = sqlx::query_as(
        "SELECT asset,symbol,decimals FROM assets
        WHERE network=$1 AND asset<>''",
    )
    .bind(net)
    .fetch_all(db)
    .await?;
    validate(&wanted, &stored)
}

fn validate(wanted: &[Asset], stored: &[Asset]) -> Result<()> {
    let ids: std::collections::BTreeSet<_> = wanted.iter().map(|a| &a.0).collect();
    ensure!(
        ids.len() == wanted.len() && ids.iter().all(|id| !id.is_empty()),
        "invalid asset list"
    );
    ensure!(
        stored.iter().all(|a| wanted.contains(a)),
        "cannot remove or redefine a listed asset; restore the original configuration"
    );
    Ok(())
}

pub async fn activate(
    db: &PgPool,
    network: Network,
    wanted: Vec<Asset>,
    fingerprint: impl FnOnce(&[Row]) -> Result<Option<[u8; 32]>>,
) -> Result<Vec<Row>> {
    let net = network_key(network);
    let mut tx = db.begin().await?;
    let (height, digest): (i64, Vec<u8>) =
        sqlx::query_as("SELECT height,digest::bytea FROM scan_cursors WHERE network=$1 FOR UPDATE")
            .bind(net)
            .fetch_one(&mut *tx)
            .await?;
    let initialized: bool =
        sqlx::query_scalar("SELECT initialized FROM asset_registries WHERE network=$1 FOR UPDATE")
            .bind(net)
            .fetch_one(&mut *tx)
            .await?;
    let stored: Vec<Asset> =
        sqlx::query_as("SELECT asset,symbol,decimals FROM assets WHERE network=$1 AND asset<>''")
            .bind(net)
            .fetch_all(&mut *tx)
            .await?;
    validate(&wanted, &stored)?;
    for (id, symbol, decimals) in wanted {
        if initialized && !stored.iter().any(|a| a.0 == id) {
            let negative:bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM opening_balances WHERE network=$1 AND asset=$2 AND amount<0)")
                .bind(net).bind(&id).fetch_one(&mut *tx).await?;
            ensure!(!negative,"cannot list asset with negative observed pre-listing holdings; verify token transfer semantics");
        }
        sqlx::query(
            "INSERT INTO assets(network,asset,symbol,decimals,enabled_from,opening_digest)
            VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(network,asset) DO NOTHING",
        )
        .bind(net)
        .bind(id)
        .bind(symbol)
        .bind(decimals)
        .bind(if initialized {
            height
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("height overflow"))?
        } else {
            0
        })
        .bind(initialized.then_some(&digest))
        .execute(&mut *tx)
        .await?;
    }
    let rows:Vec<Row>=sqlx::query_as("SELECT asset,symbol,decimals,enabled_from FROM assets WHERE network=$1 AND asset<>'' ORDER BY asset")
        .bind(net).fetch_all(&mut *tx).await?;
    sqlx::query("UPDATE scan_cursors SET rules=$2 WHERE network=$1")
        .bind(net)
        .bind(fingerprint(&rows)?.map(|h| h.to_vec()))
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE asset_registries SET initialized=true WHERE network=$1")
        .bind(net)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}
