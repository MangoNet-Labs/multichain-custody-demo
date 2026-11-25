use super::{network_key, PgPool};
use anyhow::{ensure, Result};
use custody_core::Network;

#[derive(Clone, Copy)]
pub enum Purpose<'a> {
    Deposit(&'a str),
    Hot,
    Gas,
}

pub async fn network_ready(db: &PgPool, net: Network) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM scan_cursors WHERE network=$1)")
            .bind(network_key(net))
            .fetch_one(db)
            .await?,
    )
}

/// Persist the allocation before the signer touches its volume. No key
/// material is returned here; a wallet is usable only after registration.
pub async fn request(db: &PgPool, net: Network, purpose: Purpose<'_>) -> Result<String> {
    ensure!(
        network_ready(db, net).await?,
        "initialize the {} scan cursor before issuing wallets",
        network_key(net)
    );
    let (role, owner) = match purpose {
        Purpose::Hot => ("hot", None),
        Purpose::Gas => ("gas", None),
        Purpose::Deposit(id) => ("deposit", Some(id)),
    };
    let net = network_key(net);
    sqlx::query("INSERT INTO wallet_allocations(network,role,owner_id) VALUES($1,$2,$3) ON CONFLICT(network,slot) DO NOTHING")
        .bind(net).bind(role).bind(owner).execute(db).await?;
    Ok(sqlx::query_scalar("SELECT id::text FROM wallet_allocations WHERE network=$1 AND role=$2 AND owner_id IS NOT DISTINCT FROM $3::text")
        .bind(net).bind(role).bind(owner).fetch_one(db).await?)
}
