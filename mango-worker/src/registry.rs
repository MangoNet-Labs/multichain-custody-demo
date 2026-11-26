use anyhow::{Context, Result};
use custody_core::Network::MangoTestnet;
use custody_store::{registry, PgPool};
use mango_worker::rules::{Rules, Token};
use sqlx::{PgConnection, Row};
use std::{env, path::PathBuf};
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

pub async fn validate_config(db: &PgPool, wanted: &Rules) -> Result<()> {
    registry::validate_config(
        db,
        MangoTestnet,
        wanted
            .entries()
            .map(|(t, _)| {
                (
                    t.coin_type.as_bytes().to_vec(),
                    t.symbol.clone(),
                    t.decimals.into(),
                )
            })
            .collect(),
    )
    .await
}

pub async fn activate(db: &PgPool, wanted: &Rules) -> Result<Rules> {
    let to_rules = |rows: &[(Vec<u8>, String, i16, i64)]| {
        Rules::new(
            rows.iter()
                .map(|(id, s, d, h)| {
                    Ok((
                        Token {
                            coin_type: String::from_utf8(id.clone())?,
                            symbol: s.clone(),
                            decimals: (*d).try_into()?,
                        },
                        *h,
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        )
    };
    let rows = registry::activate(
        db,
        MangoTestnet,
        wanted
            .entries()
            .map(|(t, _)| {
                (
                    t.coin_type.as_bytes().to_vec(),
                    t.symbol.clone(),
                    t.decimals.into(),
                )
            })
            .collect(),
        |r| Ok(to_rules(r)?.fingerprint()),
    )
    .await?;
    to_rules(&rows)
}
