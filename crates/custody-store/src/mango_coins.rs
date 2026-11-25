use anyhow::Result;
use sqlx::{Postgres, Transaction};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct Owned {
    pub owner: [u8; 32],
    pub version: u64,
    pub digest: [u8; 32],
    pub balance: u64,
    pub asset: Vec<u8>,
}
/// Last tracked coin state per touched object in an already verified checkpoint.
/// A missing output removes an object from the wallet inventory.
pub type Changes = BTreeMap<[u8; 32], Option<Owned>>;

pub(crate) async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    height: i64,
    changes: &Changes,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let ids: Vec<_> = changes.keys().map(|id| id.as_slice()).collect();
    sqlx::query("DELETE FROM mango_wallet_coins WHERE object_id=ANY($1::bytea[])")
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    let mut ids = vec![];
    let mut owners = vec![];
    let mut versions = vec![];
    let mut digests = vec![];
    let mut balances = vec![];
    let mut assets = vec![];
    for (id, c) in changes {
        if let Some(c) = c {
            assets.push(c.asset.as_slice());
            ids.push(id.as_slice());
            owners.push(c.owner.as_slice());
            versions.push(i64::try_from(c.version)?);
            digests.push(c.digest.as_slice());
            balances.push(c.balance.to_string());
        }
    }
    if !ids.is_empty() {
        sqlx::query("INSERT INTO mango_wallet_coins(object_id,owner,version,digest,balance,changed_height,asset)
            SELECT c.id,c.owner,c.v,c.d,c.b::numeric,$6,c.asset FROM unnest($1::bytea[],$2::bytea[],$3::bigint[],$4::bytea[],$5::text[],$7::bytea[]) AS c(id,owner,v,d,b,asset)
            JOIN wallets w ON w.network='mango-testnet' AND w.address=c.owner")
            .bind(ids).bind(owners).bind(versions).bind(digests).bind(balances).bind(height).bind(assets).execute(&mut **tx).await?;
    }
    Ok(())
}
