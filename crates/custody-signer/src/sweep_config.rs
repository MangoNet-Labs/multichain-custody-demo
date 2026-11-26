use super::{parse_network, Signer};
use anyhow::{ensure, Context, Result};
use custody_core::Network;
use serde::Deserialize;
use std::collections::BTreeMap;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    sweep: Vec<Rule>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    network: String,
    asset: String,
    min_sweep_amount: String,
}

impl Signer {
    pub async fn configure_sweeps(&self, network: Network, contents: &str) -> Result<()> {
        let config: Config = toml::from_str(contents)?;
        let network_name = custody_store::network_key(network);
        let mut tx = self.db.begin().await?;
        let ready: bool =
            sqlx::query_scalar("SELECT initialized FROM asset_registries WHERE network=$1")
                .bind(network_name)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            ready,
            "{network_name} scanner has not activated its asset list yet"
        );
        sqlx::query("LOCK TABLE sweep_rules IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        let mut rules = BTreeMap::new();
        for r in config.sweep {
            let net = parse_network(&r.network)?;
            if net != network {
                continue;
            }
            let asset = match (net, r.asset.as_str()) {
                (Network::MangoTestnet, "0x2::mgo::MGO") | (Network::BscTestnet, "native") => {
                    vec![]
                }
                (Network::BscTestnet, addr) => addr.parse::<alloy_primitives::Address>()?.to_vec(),
                (Network::MangoTestnet, ty) => {
                    ensure!(!ty.is_empty(), "missing Mango sweep coin type");
                    ty.as_bytes().to_vec()
                }
            };
            let decimals: i16 =
                sqlx::query_scalar("SELECT decimals FROM assets WHERE network=$1 AND asset=$2")
                    .bind(&r.network)
                    .bind(&asset)
                    .fetch_optional(&mut *tx)
                    .await?
                    .context("sweep asset is not indexed")?;
            let raw = custody_store::withdrawals::raw_amount(
                &r.min_sweep_amount,
                decimals.try_into()?,
                net,
            )?;
            ensure!(
                rules.insert((r.network, asset), raw).is_none(),
                "duplicate sweep rule"
            );
        }
        let old: Vec<(String, Vec<u8>, String)> = sqlx::query_as(
            "SELECT network::text,asset,minimum::text FROM sweep_rules WHERE network=$1",
        )
        .bind(network_name)
        .fetch_all(&mut *tx)
        .await?;
        let old: BTreeMap<_, _> = old.into_iter().map(|(n, a, m)| ((n, a), m)).collect();
        if rules != old {
            let changed_existing = old.iter().any(|(key, value)| rules.get(key) != Some(value));
            let added: Vec<_> = rules
                .keys()
                .filter(|k| !old.contains_key(*k))
                .map(|(_, a)| a.clone())
                .collect();
            sqlx::query("DELETE FROM sweep_rules WHERE network=$1")
                .bind(network_name)
                .execute(&mut *tx)
                .await?;
            for ((net, asset), raw) in rules {
                sqlx::query("INSERT INTO sweep_rules VALUES($1,$2,$3::text::positive_raw)")
                    .bind(net)
                    .bind(asset)
                    .bind(raw)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(
                "UPDATE sweep_checks SET revision=revision+1,
                 next_check=CASE WHEN finalized_failures>0 THEN greatest(next_check,now()) ELSE now() END
                 WHERE network=$1 AND finalized_failures<4 AND ($2 OR EXISTS (
                   SELECT 1 FROM deposits d WHERE d.network=sweep_checks.network
                   AND d.recipient=sweep_checks.address AND d.asset=ANY($3::bytea[])))",
            )
            .bind(network_name)
            .bind(changed_existing)
            .bind(added)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
