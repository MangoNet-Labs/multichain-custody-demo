use crate::{network_key, Cursor, PgPool};
use anyhow::{ensure, Context, Result};
use custody_core::Network;
use sqlx::FromRow;
use std::collections::BTreeMap;

#[derive(FromRow)]
pub struct Wallet {
    pub address: Vec<u8>,
    pub asset: Vec<u8>,
    pub expected: String,
}
#[derive(FromRow)]
struct Asset {
    asset: Vec<u8>,
    available: String,
    frozen: String,
    capital: String,
    net_fees: String,
    book_assets: String,
}
pub struct Snapshot {
    net: Network,
    pub cursor: Cursor,
    pub wallets: Vec<Wallet>,
    assets: Vec<Asset>,
}
pub type Balances = BTreeMap<(Vec<u8>, Vec<u8>), String>;

// Orders, journals, object inventory, and cursor must all come from one snapshot;
// chain reads take place after releasing it and are pinned to its height.
pub async fn snapshot(db: &PgPool, net: Network) -> Result<Snapshot> {
    let key = network_key(net);
    let mut tx = db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let (height,d):(i64,Vec<u8>)=sqlx::query_as("SELECT c.height,c.digest::bytea FROM scan_cursors c JOIN scanned_blocks b USING(network,height,digest) WHERE network=$1")
        .bind(key).fetch_optional(&mut *tx).await?.context("no committed blocks for audit")?;
    let wallets:Vec<Wallet>=sqlx::query_as("WITH balances AS(SELECT address,asset,sum(delta) raw FROM custody_wallet_movements WHERE network=$1 AND height<=$2 GROUP BY address,asset)
        SELECT w.address,a.asset,coalesce(b.raw,0)::text AS expected FROM wallets w JOIN assets a USING(network) LEFT JOIN balances b ON b.address=w.address AND b.asset=a.asset WHERE w.network=$1 ORDER BY w.address,a.asset")
        .bind(key).bind(height).fetch_all(&mut *tx).await?;
    ensure!(!wallets.is_empty(), "no registered custody wallets");
    let assets=sqlx::query_as("SELECT a.asset,
        coalesce(sum(delta) FILTER(WHERE account LIKE 'user:%:available'),0)::text AS available,
        coalesce(sum(delta) FILTER(WHERE account LIKE 'user:%:frozen'),0)::text AS frozen,
        coalesce(sum(delta) FILTER(WHERE account='platform:capital'),0)::text AS capital,
        (-coalesce(sum(delta) FILTER(WHERE account='platform:gas'),0))::text AS net_fees,
        (-coalesce(sum(delta) FILTER(WHERE account='custody'),0))::text AS book_assets
        FROM assets a LEFT JOIN journal_entries j USING(network,asset) WHERE a.network=$1 GROUP BY a.asset ORDER BY a.asset")
        .bind(key).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Snapshot {
        net,
        cursor: Cursor {
            height,
            digest: d.as_slice().try_into()?,
        },
        wallets,
        assets,
    })
}
pub type MangoInventory = BTreeMap<[u8; 32], ([u8; 32], Vec<u8>, u64)>;
pub async fn finish(db: &PgPool, s: Snapshot, observed: Balances) -> Result<(i64, bool)> {
    ensure!(
        s.net == Network::BscTestnet,
        "Mango requires a full object inventory"
    );
    persist(db, s, observed, None).await
}
pub async fn finish_mango(db: &PgPool, s: Snapshot, coins: &MangoInventory) -> Result<(i64, bool)> {
    ensure!(s.net == Network::MangoTestnet, "Mango inventory only");
    let mut sums: BTreeMap<_, u128> = s
        .wallets
        .iter()
        .map(|w| ((w.address.clone(), w.asset.clone()), 0))
        .collect();
    for (addr, asset, balance) in coins.values() {
        *sums
            .get_mut(&(addr.to_vec(), asset.clone()))
            .context("unexpected observed Mango owner")? += u128::from(*balance);
    }
    persist(
        db,
        s,
        sums.into_iter().map(|(a, v)| (a, v.to_string())).collect(),
        Some(coins),
    )
    .await
}
async fn persist(
    db: &PgPool,
    s: Snapshot,
    mut observed: Balances,
    inventory: Option<&MangoInventory>,
) -> Result<(i64, bool)> {
    let mut tx = db.begin().await?;
    let net = network_key(s.net);
    ensure!(
        observed.len() == s.wallets.len(),
        "incomplete observed wallet set"
    );
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO reconciliation_runs(network,height,digest) VALUES($1,$2,$3) RETURNING id",
    )
    .bind(net)
    .bind(s.cursor.height)
    .bind(s.cursor.digest.as_slice())
    .fetch_one(&mut *tx)
    .await?;
    if let Some(coins) = inventory {
        for chunk in coins.iter().collect::<Vec<_>>().chunks(512) {
            let ids = chunk
                .iter()
                .map(|(id, _)| id.as_slice())
                .collect::<Vec<_>>();
            let owners = chunk
                .iter()
                .map(|(_, v)| v.0.as_slice())
                .collect::<Vec<_>>();
            let balances = chunk
                .iter()
                .map(|(_, v)| v.2.to_string())
                .collect::<Vec<_>>();
            let assets = chunk
                .iter()
                .map(|(_, v)| v.1.as_slice())
                .collect::<Vec<_>>();
            sqlx::query("INSERT INTO reconciliation_mango_coins(run_id,object_id,owner,balance,asset) SELECT $1,x.id,x.owner,x.balance::numeric,x.asset FROM unnest($2::bytea[],$3::bytea[],$4::text[],$5::bytea[]) x(id,owner,balance,asset)")
                .bind(id).bind(ids).bind(owners).bind(balances).bind(assets).execute(&mut *tx).await?;
        }
    }
    for w in s.wallets {
        let raw = observed
            .remove(&(w.address.clone(), w.asset.clone()))
            .context("missing observed balance")?;
        ensure!(
            !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()),
            "invalid observed balance"
        );
        sqlx::query("INSERT INTO reconciliation_wallets VALUES($1,$2,$3,$4::text::numeric,$5::text::numeric)")
            .bind(id).bind(w.address).bind(w.asset).bind(w.expected).bind(raw).execute(&mut *tx).await?;
    }
    for a in s.assets {
        sqlx::query("INSERT INTO reconciliation_assets SELECT $1,$2,$3::text::numeric,$4::text::numeric,$5::text::numeric,$6::text::numeric,$7::text::numeric,coalesce(sum(observed),0)
            FROM reconciliation_wallets WHERE run_id=$1 AND asset=$2")
            .bind(id).bind(a.asset).bind(a.available).bind(a.frozen).bind(a.capital).bind(a.net_fees).bind(a.book_assets).execute(&mut *tx).await?;
    }
    let matched:bool=sqlx::query_scalar("UPDATE reconciliation_runs SET matched=NOT EXISTS(SELECT 1 FROM reconciliation_wallets WHERE run_id=$1 AND expected<>observed)
        AND NOT EXISTS(SELECT 1 FROM reconciliation_assets WHERE run_id=$1 AND (book_assets<>observed OR book_assets<>available+frozen+capital-net_fees)) WHERE id=$1 RETURNING matched")
        .bind(id).fetch_one(&mut *tx).await?;
    sqlx::query("SELECT paused FROM payment_control WHERE singleton FOR UPDATE")
        .fetch_one(&mut *tx)
        .await?;
    let latest:bool=sqlx::query_scalar("SELECT id=$2 FROM reconciliation_runs WHERE network=$1 ORDER BY height DESC,id DESC LIMIT 1").bind(net).bind(id).fetch_one(&mut *tx).await?;
    if latest && !matched {
        sqlx::query("INSERT INTO reconciliation_holds VALUES($1,$2) ON CONFLICT(network) DO UPDATE SET run_id=excluded.run_id").bind(net).bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE payment_control SET paused=true WHERE singleton")
            .execute(&mut *tx)
            .await?;
    } else if latest {
        // A corrected audit removes the hold, but never resumes the operator's pause.
        sqlx::query("DELETE FROM reconciliation_holds WHERE network=$1")
            .bind(net)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((id, matched))
}
