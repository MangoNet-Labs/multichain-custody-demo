use anyhow::{bail, ensure, Context, Result};
use custody_core::{
    pipeline::{log_progress, ordered_range_observed, FetchLimit, HeadPace},
    Network::MangoTestnet,
};
use custody_store::{
    apply_mango_with_rules, cursor, initialize, observed, Cursor, PgPool, ScanStage, MIGRATIONS,
};
use mango_worker::{decode_with_rules, rules::Rules, FullObservation, TESTNET_GENESIS};
use mgo_types::{
    base_types::MgoAddress, digests::TransactionDigest,
    messages_checkpoint::CertifiedCheckpointSummary,
};
use std::{env, num::NonZeroUsize, time::Duration};

const MIB: usize = 1024 * 1024;
mod payments;
mod reconcile;
mod registry;

#[derive(Clone)]
struct Node {
    rules: std::sync::Arc<Rules>,
    base: String,
    client: reqwest::Client,
    body_limit: usize,
}
impl Node {
    fn with_rules(&self, rules: Rules) -> Self {
        Self {
            rules: rules.into(),
            ..self.clone()
        }
    }
    async fn get(&self, path: &str, format: &str, limit: usize) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .get(format!("{}/checkpoints{path}", self.base))
            .header("Accept", format)
            .send()
            .await?
            .error_for_status()?;
        let len = response.content_length().unwrap_or(0);
        ensure!(
            len <= limit as u64,
            "Mango response above configured byte limit"
        );
        let mut body = Vec::with_capacity(len as usize);
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                chunk.len() <= limit - body.len(),
                "Mango response above configured byte limit"
            );
            // Do not round buffer capacity above the per-request reservation.
            if body.len() + chunk.len() > body.capacity() {
                let cap = (body.len() + chunk.len())
                    .max(body.capacity().saturating_mul(2))
                    .min(limit);
                body.reserve_exact(cap - body.len());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
    async fn header(&self, height: Option<u64>) -> Result<Cursor> {
        let path = height.map(|h| format!("/{h}")).unwrap_or_default();
        let bytes = self.get(&path, "application/json", MIB).await?;
        let summary: CertifiedCheckpointSummary = serde_json::from_slice(&bytes)?;
        ensure!(
            height.is_none_or(|h| h == summary.sequence_number),
            "incorrect summary height"
        );
        Ok(Cursor {
            height: summary.sequence_number.try_into()?,
            digest: summary.digest().into_inner(),
        })
    }
    async fn fetch(&self, height: u64) -> Result<FullObservation> {
        let bytes = self
            .get(
                &format!("/{height}/full"),
                "application/bcs",
                self.body_limit,
            )
            .await?;
        let rules = self.rules.clone();
        tokio::task::spawn_blocking(move || decode_with_rules(&bytes, height, &rules))
            .await?
            .with_context(|| format!("decoding finalized Mango checkpoint {height}"))
    }
}

fn setting(key: &str, default: usize, max: usize) -> Result<usize> {
    let value = match env::var(key) {
        Ok(raw) => raw.parse().with_context(|| format!("invalid {key}"))?,
        Err(env::VarError::NotPresent) => default,
        Err(e) => return Err(e.into()),
    };
    ensure!((1..=max).contains(&value), "{key} must be 1..={max}");
    Ok(value)
}

async fn catch_up(
    node: &Node,
    db: &PgPool,
    parallel: NonZeroUsize,
    ready: NonZeroUsize,
    pace: &mut HeadPace,
) -> Result<bool> {
    let last = cursor(db, MangoTestnet)
        .await?
        .context("missing Mango cursor")?;
    let read = async {
        let tip = node.header(None).await?;
        ensure!(tip.height >= last.height, "node behind persisted cursor");
        ensure!(
            last.height != tip.height || last == tip,
            "finalized digest conflicts with stored cursor"
        );
        Ok(tip)
    }
    .await;
    let tip = observed(db, MangoTestnet, ScanStage::Finality, read)
        .await
        .inspect_err(|_| *pace = HeadPace::default())?;
    if pace.sample("Mango", tip.height, last.height) {
        custody_store::note_scan_head(db, MangoTestnet, tip.height, last.height).await;
    }
    if tip.height == last.height {
        return Ok(false);
    }
    let mut committed = last.height;
    let fingerprint = node.rules.fingerprint();
    let mut log = log_progress("Mango");
    let ranged = ordered_range_observed(
        (last.height + 1) as u64..=tip.height as u64,
        parallel,
        ready,
        |h| node.fetch(h),
        |_, (b, executions, coins)| async move {
            ensure!(
                b.height != tip.height || b.digest == tip.digest,
                "finalized head changed"
            );
            apply_mango_with_rules(db, &b, &executions, &coins, fingerprint).await?;
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
        MangoTestnet,
        ScanStage::Range(committed.saturating_add(1)),
        ranged,
    )
    .await?;
    Ok(true)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--init-assets"] || args == ["--broadcast"] || args == ["--broadcast-once"] {
        let db = PgPool::connect(&env::var("DATABASE_URL")?).await?;
        MIGRATIONS.run(&db).await?;
        registry::validate_config(&db, &registry::configured()?).await?;
        if args == ["--init-assets"] {
            return Ok(());
        }
    }
    if args == ["--quote-sweep"] {
        return payments::sweep_quote_command().await;
    }
    if args.first().is_some_and(|s| s == "--quote-withdrawal") {
        return payments::quote_command(&args).await;
    }
    if args == ["--broadcast"] || args == ["--broadcast-once"] {
        return payments::relay(args == ["--broadcast-once"]).await;
    }
    let limit = setting("MANGO_MAX_CHECKPOINT_MIB", 32, 256)?;
    let budget = setting("MANGO_INFLIGHT_MIB", 64, 1024)?;
    let parallel =
        NonZeroUsize::new(setting("MANGO_FETCH_CONCURRENCY", 2, 16)?.min(budget / limit))
            .context("in-flight budget must fit at least one maximum-size checkpoint")?;
    let ready = NonZeroUsize::new(setting("MANGO_READY_CAPACITY", 8, 128)?).unwrap();
    let node = Node {
        rules: Default::default(),
        base: env::var("MANGO_REST_URL")
            .unwrap_or_else(|_| "https://fullnode.testnet2.mangonetwork.io/rest".into())
            .trim_end_matches('/')
            .into(),
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(25))
            .build()?,
        body_limit: limit * MIB,
    };
    let genesis = node.header(Some(0)).await?;
    ensure!(
        mgo_types::digests::CheckpointDigest::new(genesis.digest).base58_encode()
            == TESTNET_GENESIS,
        "not Mango Testnet"
    );
    if args == ["--reconcile"] {
        let db = PgPool::connect(&env::var("DATABASE_URL")?).await?;
        return reconcile::run(&db, &node, parallel, ready).await;
    }
    if let [cmd, h] = args.as_slice() {
        if cmd == "--backfill-funding" {
            let db = PgPool::connect(&env::var("DATABASE_URL")?).await?;
            let (b, exec, _) = node
                .with_rules(registry::stored(&db).await?)
                .fetch(h.parse()?)
                .await?;
            println!(
                "platform funding recorded={}",
                custody_store::funding::backfill(&db, &b, None, Some(&exec)).await?
            );
            return Ok(());
        }
    }
    if args.is_empty() || (args.len() <= 2 && args[0] == "inspect") {
        let h = match args.get(1) {
            Some(s) => s.parse()?,
            None => node.header(None).await?.height as u64,
        };
        let expected = node.header(Some(h)).await?;
        let (b, _, _) = node.with_rules(registry::configured()?).fetch(h).await?;
        ensure!(
            b.digest == expected.digest,
            "checkpoint does not match summary"
        );
        println!(
            "Mango Testnet checkpoint {h}: {} candidate receipts",
            b.receipts.len()
        );
        for r in b.receipts {
            println!(
                "{} {} asset={} raw={}",
                TransactionDigest::new(r.tx),
                MgoAddress::from_bytes(r.recipient)?,
                if r.asset.is_empty() {
                    "native MGO"
                } else {
                    std::str::from_utf8(&r.asset)?
                },
                r.amount
            );
        }
        return Ok(());
    }
    ensure!(
        args.len() == 1 && matches!(args[0].as_str(), "--sync" | "--follow"),
        "usage: mango-worker [inspect [height] | --sync | --follow]"
    );
    let db =
        PgPool::connect(&env::var("DATABASE_URL").context("database mode requires DATABASE_URL")?)
            .await?;
    MIGRATIONS.run(&db).await?;
    if cursor(&db, MangoTestnet).await?.is_none() {
        let tip = observed(
            &db,
            MangoTestnet,
            ScanStage::Finality,
            node.header(None).await,
        )
        .await?;
        let anchor = match custody_core::start_height("MANGO_START_HEIGHT")? {
            None => tip,
            Some(start) => {
                ensure!(
                    start <= tip.height as u64,
                    "start checkpoint is not finalized"
                );
                if start == 0 {
                    Cursor {
                        height: -1,
                        digest: [0; 32],
                    }
                } else {
                    node.header(Some(start - 1)).await?
                }
            }
        };
        initialize(&db, MangoTestnet, anchor).await?;
    }
    let node = node.with_rules(registry::activate(&db, &registry::configured()?).await?);
    let mut delay = 1;
    let mut pace = HeadPace::default();
    let mut limit = FetchLimit::new(parallel);
    loop {
        match catch_up(&node, &db, limit.current(), ready, &mut pace).await {
            Ok(_) if args[0] == "--sync" => return Ok(()),
            Ok(true) => {
                delay = 1;
                limit.drained();
                continue;
            }
            Ok(false) => delay = 1,
            Err(e)
                if args[0] == "--follow"
                    && e.downcast_ref::<reqwest::Error>().is_some_and(|r| {
                        r.is_connect()
                            || r.is_timeout()
                            || r.is_body()
                            || r.status()
                                .is_some_and(|s| s.is_server_error() || s.as_u16() == 429)
                    }) =>
            {
                if e.downcast_ref::<reqwest::Error>()
                    .and_then(|r| r.status())
                    .is_some_and(|s| s.as_u16() == 429)
                {
                    limit.throttled();
                    eprintln!("Mango rate limited; fetch concurrency={}", limit.current());
                }
                delay = (delay * 2).min(30);
                eprintln!("Mango node unavailable; retrying from committed cursor in {delay}s");
            }
            Err(e) => bail!(e),
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}
