//! Isolated test-wallet provisioning. No arbitrary-message signing interface.
mod bsc;
mod external;
mod mango;
mod mango_sweep;
mod sweep;
mod sweep_config;
mod vault;
pub use external::external_wallets;
pub use vault::create_unlock_file;

use anyhow::{ensure, Context, Result};
use custody_core::Network;
use custody_store::{
    wallets::{self, Purpose},
    PgPool,
};
use std::{path::Path, sync::Arc};
use uuid::Uuid;
use vault::Vault;

pub struct Signer {
    db: PgPool,
    vault: Arc<Vault>,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Wallet {
    pub network: Network,
    pub slot: String,
    pub address: Vec<u8>,
}

impl Wallet {
    pub fn formatted(&self) -> String {
        match self.network {
            Network::BscTestnet => {
                alloy_primitives::Address::from_slice(&self.address).to_checksum(None)
            }
            Network::MangoTestnet => format!("0x{}", hex::encode(&self.address)),
        }
    }
}

pub fn parse_network(net: &str) -> Result<Network> {
    match net {
        "bsc-testnet" => Ok(Network::BscTestnet),
        "mango-testnet" => Ok(Network::MangoTestnet),
        _ => anyhow::bail!("unknown test network"),
    }
}

impl Signer {
    pub async fn open(db: PgPool, root: &Path, unlock: &Path) -> Result<Self> {
        let (id,registered):(String,bool)=sqlx::query_as("SELECT id::text,EXISTS(SELECT 1 FROM wallets) FROM custody_installation WHERE singleton")
            .fetch_one(&db).await?;
        let vault = Arc::new(Vault::open(
            root,
            unlock,
            Uuid::parse_str(&id)?,
            registered,
        )?);
        let actor = Self { db, vault };
        let existing:Vec<(String,Option<String>)>=sqlx::query_as(
            "SELECT w.key_ref,a.id::text FROM wallets w LEFT JOIN wallet_allocations a ON a.id::text=w.key_ref AND a.network=w.network AND a.role=w.role AND a.owner_id IS NOT DISTINCT FROM w.owner_id")
            .fetch_all(&actor.db).await?;
        for (_, id) in existing {
            actor
                .resolve(&id.context(
                    "registered wallet lacks its original allocation; restore the database",
                )?)
                .await?;
        }
        Ok(actor)
    }

    pub async fn wallet(&self, net: Network, purpose: Purpose<'_>) -> Result<Wallet> {
        self.resolve(&wallets::request(&self.db, net, purpose).await?)
            .await
    }

    pub async fn platform(&self) -> Result<Vec<Wallet>> {
        let mut result = Vec::new();
        for net in [Network::MangoTestnet, Network::BscTestnet] {
            if !wallets::network_ready(&self.db, net).await? {
                continue;
            }
            for purpose in [Purpose::Hot, Purpose::Gas] {
                result.push(self.wallet(net, purpose).await?);
            }
        }
        Ok(result)
    }

    pub async fn pending(&self) -> Result<usize> {
        // A second scanner can initialize long after the signer starts.
        sqlx::query(
            "INSERT INTO wallet_allocations(network,role)
             SELECT c.network,p.role FROM scan_cursors c CROSS JOIN (VALUES ('hot'),('gas')) p(role)
             WHERE NOT EXISTS (SELECT 1 FROM wallet_allocations a WHERE a.network=c.network AND a.slot=p.role)
             ON CONFLICT(network,slot) DO NOTHING",
        )
        .execute(&self.db)
        .await?;
        let ids:Vec<String>=sqlx::query_scalar("SELECT a.id::text FROM wallet_allocations a LEFT JOIN wallets w ON a.id::text=w.key_ref WHERE w.key_ref IS NULL ORDER BY a.id LIMIT 32")
            .fetch_all(&self.db).await?;
        for id in &ids {
            self.resolve(id).await?;
        }
        Ok(ids.len())
    }

    async fn resolve(&self, id: &str) -> Result<Wallet> {
        let uuid = Uuid::parse_str(id)?;
        let mut tx = self.db.begin().await?;
        let (net,role,owner,slot):(String,String,Option<String>,String)=sqlx::query_as(
            "SELECT network::text,role,owner_id,slot FROM wallet_allocations WHERE id=$1::text::uuid FOR UPDATE")
            .bind(id).fetch_one(&mut *tx).await?;
        let network = parse_network(&net)?;
        let registered:Option<(String,Vec<u8>)>=sqlx::query_as(
            "SELECT key_ref,address FROM wallets WHERE network=$1 AND role=$2 AND owner_id IS NOT DISTINCT FROM $3::text")
            .bind(&net).bind(&role).bind(&owner).fetch_optional(&mut *tx).await?;
        ensure!(
            registered.as_ref().is_none_or(|(key, _)| key == id),
            "wallet allocation mismatch, refusing to replace wallet"
        );
        let new = registered.is_none();
        let v = self.vault.clone();
        let label = slot.clone();
        let address = tokio::task::spawn_blocking(move || {
            v.address(
                uuid,
                network,
                &label,
                registered.as_ref().map(|r| r.1.as_slice()),
            )
        })
        .await??;
        if new {
            // Coordinate publication with the scanner's committing transaction.
            sqlx::query("SELECT height FROM scan_cursors WHERE network=$1 FOR UPDATE")
                .bind(&net)
                .fetch_one(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO wallets(network,address,role,owner_id,key_ref) VALUES($1,$2,$3,$4,$5)",
            )
            .bind(&net)
            .bind(&address)
            .bind(&role)
            .bind(owner)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Wallet {
            network,
            slot,
            address,
        })
    }
}
