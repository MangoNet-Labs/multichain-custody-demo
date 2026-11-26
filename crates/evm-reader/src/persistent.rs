use super::{Header, Observed, Reader};
use anyhow::{ensure, Context, Result};
use custody_core::{
    pipeline::{log_progress, ordered_range_observed, FetchLimit, HeadPace},
    AssetId,
    Network::BscTestnet,
};
use custody_store::{
    apply_evidence, configure_tokens, cursor, initialize, observed, Cursor, FinalizedBlock, PgPool,
    Receipt, ScanStage, MIGRATIONS,
};
use serde_json::json;
use std::{env, num::NonZeroUsize, sync::Arc, time::Duration};

#[cfg(test)]
mod tests;

async fn header(source: Arc<Reader>, tag: String) -> Result<Header> {
    tokio::task::spawn_blocking(move || {
        source
            .node
            .call("eth_getBlockByNumber", json!([tag, false]))
    })
    .await?
}

async fn finalized(
    source: &Arc<Reader>,
    db: &PgPool,
    last: Option<&Cursor>,
) -> Result<(Header, i64)> {
    let read = async {
        let head = header(source.clone(), "finalized".into()).await?;
        let height = i64::try_from(head.number.to::<u64>())?;
        if let Some(c) = last {
            ensure!(
                height >= c.height,
                "RPC finalized head behind stored cursor"
            );
            ensure!(
                height != c.height || head.hash.0 == c.digest,
                "finalized head conflicts with stored cursor"
            );
        }
        Ok((head, height))
    }
    .await;
    observed(db, BscTestnet, ScanStage::Finality, read).await
}

impl Observed {
    pub(super) fn into_finalized(self, height: u64) -> Result<FinalizedBlock> {
        let mut receipts = Vec::with_capacity(self.incoming.len());
        for incoming in self.incoming {
            let asset = match incoming.asset {
                AssetId::BscTestnetBnb => vec![],
                AssetId::BscTestnetErc20(addr) => addr.to_vec(),
                _ => anyhow::bail!("unexpected non-EVM asset"),
            };
            let log = match (incoming.log_index, incoming.tx_log_index) {
                (None, None) => None,
                (Some(global), Some(local)) => Some((
                    i64::try_from(global.to::<u64>())?,
                    i64::try_from(local.to::<u64>())?,
                )),
                _ => anyhow::bail!("incomplete log locator"),
            };
            receipts.push(Receipt {
                tx: incoming.tx.0,
                recipient: incoming.to.to_vec(),
                sender: Some(incoming.from.to_vec()),
                asset,
                amount: incoming.amount.to_string(),
                log,
            });
        }
        Ok(FinalizedBlock {
            openings: self.openings,
            network: BscTestnet,
            height: height.try_into()?,
            digest: self.hash.0,
            parent: self.parent.0,
            receipts,
        })
    }
}

async fn sync(
    source: &Arc<Reader>,
    db: &PgPool,
    rules: [u8; 32],
    concurrency: NonZeroUsize,
    ready: NonZeroUsize,
    pace: &mut HeadPace,
) -> Result<bool> {
    let last = cursor(db, BscTestnet)
        .await?
        .context("missing BSC cursor")?;
    let (head, target) = finalized(source, db, Some(&last))
        .await
        .inspect_err(|_| *pace = HeadPace::default())?;
    let end = head.number.to::<u64>();
    if pace.sample("BSC", target, last.height) {
        custody_store::note_scan_head(db, BscTestnet, target, last.height).await;
    }
    if target == last.height {
        return Ok(false);
    }
    let start = u64::try_from(last.height.checked_add(1).context("height overflow")?)?;
    let mut committed = last.height;
    let mut log = log_progress("BSC");
    let ranged = ordered_range_observed(
        start..=end,
        concurrency,
        ready,
        |h| source.clone().fetch(h),
        |h, mut b| async move {
            ensure!(
                h != end || b.hash == head.hash,
                "finalized head changed during scan"
            );
            let executions = std::mem::take(&mut b.executions);
            apply_evidence(db, &b.into_finalized(h)?, Some(rules), Some(&executions)).await?;
            Ok(())
        },
        |p| {
            committed = p.height as i64;
            log(p);
        },
    )
    .await;
    observed(
        db,
        BscTestnet,
        ScanStage::Range(committed.saturating_add(1)),
        ranged,
    )
    .await?;
    Ok(true)
}

pub async fn run(
    source: Arc<Reader>,
    concurrency: NonZeroUsize,
    ready: NonZeroUsize,
    follow: bool,
) -> Result<()> {
    let db =
        PgPool::connect(&env::var("DATABASE_URL").context("database mode requires DATABASE_URL")?)
            .await?;
    MIGRATIONS.run(&db).await?;
    if cursor(&db, BscTestnet).await?.is_none() {
        let (head, _) = finalized(&source, &db, None).await?;
        let anchor = match custody_core::start_height("BSC_START_HEIGHT")? {
            None => Cursor {
                height: i64::try_from(head.number.to::<u64>())?,
                digest: head.hash.0,
            },
            Some(start) => {
                ensure!(
                    start <= head.number.to::<u64>(),
                    "start height is not finalized"
                );
                if start == 0 {
                    Cursor {
                        height: -1,
                        digest: [0; 32],
                    }
                } else {
                    let prior = header(source.clone(), format!("0x{:x}", start - 1)).await?;
                    ensure!(
                        prior.number.to::<u64>() == start - 1,
                        "unexpected anchor height"
                    );
                    Cursor {
                        height: i64::try_from(start - 1)?,
                        digest: prior.hash.0,
                    }
                }
            }
        };
        initialize(&db, BscTestnet, anchor).await?;
    }
    let rules = configure_tokens(
        &db,
        source
            .tokens
            .values()
            .map(|t| (t.contract.into_array(), t.symbol.clone(), t.decimals))
            .collect(),
    )
    .await?;
    let mut delay = 1;
    let mut pace = HeadPace::default();
    let mut limit = FetchLimit::new(concurrency);
    loop {
        match sync(&source, &db, rules, limit.current(), ready, &mut pace).await {
            Ok(_) if !follow => return Ok(()),
            Ok(true) => {
                delay = 1;
                limit.drained();
                continue;
            }
            Ok(false) => delay = 1,
            Err(e)
                if follow
                    && matches!(
                        e.downcast_ref::<ureq::Error>(),
                        Some(
                            ureq::Error::Io(_)
                                | ureq::Error::Timeout(_)
                                | ureq::Error::StatusCode(429 | 500..=599)
                        )
                    ) =>
            {
                if matches!(
                    e.downcast_ref::<ureq::Error>(),
                    Some(ureq::Error::StatusCode(429))
                ) {
                    limit.throttled();
                    eprintln!("BSC rate limited; fetch concurrency={}", limit.current());
                }
                delay = (delay * 2).min(30);
                eprintln!("BSC RPC unavailable; retrying from committed cursor in {delay}s");
            }
            Err(e) => return Err(e),
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}
