use alloy_primitives::{Address, B256, U64};
use anyhow::{ensure, Context, Result};
use custody_core::pipeline::ordered_range;
use evm_reader::rpc::BscRpc;
use evm_reader::{configured_tokens, scan_observed, Block, Incoming, Receipt, Token};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    env,
    num::NonZeroUsize,
    sync::Arc,
};

mod persistent;
mod reconcile;

#[derive(Deserialize)]
struct Header {
    number: U64,
    hash: B256,
}

struct Reader {
    node: BscRpc,
    tokens: HashMap<Address, Token>,
    watched: Option<HashSet<Address>>,
}

async fn historical_tokens(
    db: &custody_store::PgPool,
    height: u64,
) -> Result<HashMap<Address, Token>> {
    let rows: Vec<custody_store::registry::Asset> = sqlx::query_as(
        "SELECT asset,symbol,decimals FROM assets WHERE network='bsc-testnet' AND asset<>'' AND enabled_from<=$1")
        .bind(i64::try_from(height)?).fetch_all(db).await?;
    rows.into_iter()
        .map(|(id, symbol, decimals)| {
            let contract = Address::try_from(id.as_slice())?;
            Ok((
                contract,
                Token {
                    contract,
                    symbol,
                    decimals: decimals.try_into()?,
                },
            ))
        })
        .collect()
}

struct Observed {
    openings: Vec<custody_store::OpeningDelta>,
    hash: B256,
    parent: B256,
    transactions: usize,
    incoming: Vec<Incoming>,
    executions: Vec<custody_store::payouts::Execution>,
}

impl Reader {
    async fn fetch(self: Arc<Self>, height: u64) -> Result<Observed> {
        let n = U64::from(height);
        let headers = Arc::clone(&self);
        let bodies = Arc::clone(&self);
        let (block, receipts) = tokio::join!(
            tokio::task::spawn_blocking(move || headers
                .node
                .call::<Block>("eth_getBlockByNumber", json!([n, true]))),
            tokio::task::spawn_blocking(move || bodies
                .node
                .call::<Vec<Receipt>>("eth_getBlockReceipts", json!([n]))),
        );
        let block =
            block?.with_context(|| format!("BSC block {height}: full transactions required"))?;
        let receipts =
            receipts?.with_context(|| format!("BSC block {height}: complete receipts required"))?;
        tokio::task::spawn_blocking(move || self.decode(n, block, receipts))
            .await?
            .with_context(|| format!("BSC finalized block {height}"))
    }

    fn decode(&self, n: U64, block: Block, receipts: Vec<Receipt>) -> Result<Observed> {
        ensure!(block.number == n, "unexpected block number");
        let mut openings = Vec::new();
        let (incoming, executions) = scan_observed(
            &block,
            &receipts,
            &self.tokens,
            |to| self.watched.as_ref().is_none_or(|set| set.contains(to)),
            &mut openings,
        )?;
        Ok(Observed {
            openings,
            hash: block.hash,
            parent: block.parent_hash,
            transactions: block.transactions.len(),
            incoming,
            executions,
        })
    }
}

fn setting(key: &str, default: usize, max: usize) -> Result<NonZeroUsize> {
    let n = match env::var(key) {
        Ok(value) => value.parse().with_context(|| format!("invalid {key}"))?,
        Err(env::VarError::NotPresent) => default,
        Err(err) => return Err(err.into()),
    };
    ensure!((1..=max).contains(&n), "{key} must be between 1 and {max}");
    Ok(NonZeroUsize::new(n).unwrap())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let blocks = setting("BSC_RECENT_BLOCKS", 1, 1024)?;
    let concurrency = setting("BSC_FETCH_CONCURRENCY", 4, 16)?;
    let ready = setting("BSC_READY_CAPACITY", 8, 128)?;
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--init-assets"] {
        let db = custody_store::PgPool::connect(&env::var("DATABASE_URL")?).await?;
        custody_store::MIGRATIONS.run(&db).await?;
        return evm_reader::check_assets(&db).await;
    }
    if args == ["--reconcile"] {
        let db = custody_store::PgPool::connect(&env::var("DATABASE_URL")?).await?;
        return reconcile::run(&db, BscRpc::from_env()?).await;
    }
    if args == ["--broadcast"] || args == ["--broadcast-once"] {
        let db = custody_store::PgPool::connect(
            &env::var("DATABASE_URL").context("database mode requires DATABASE_URL")?,
        )
        .await?;
        custody_store::MIGRATIONS.run(&db).await?;
        evm_reader::check_assets(&db).await?;
        let node = BscRpc::from_env()?;
        loop {
            let sent = evm_reader::broadcast::send_ready(&db, &node).await;
            if args == ["--broadcast-once"] {
                sent?;
                return Ok(());
            }
            if let Err(e) = sent {
                eprintln!("BSC broadcast deferred: {e:#}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
    let persist = args
        .first()
        .is_some_and(|v| v == "--sync" || v == "--follow");
    let backfill = matches!(args.as_slice(),[cmd,_] if cmd=="--backfill-funding");
    ensure!(
        !persist || args.len() == 1,
        "database mode uses registered wallets, not command-line addresses"
    );
    let mut watched = HashSet::new();
    for arg in args.iter().filter(|_| !persist && !backfill) {
        let addr: Address = arg.parse().context("expected a BSC deposit address")?;
        ensure!(!addr.is_zero(), "zero address is not a deposit address");
        watched.insert(addr);
    }
    let replay = if backfill {
        let db = custody_store::PgPool::connect(&env::var("DATABASE_URL")?).await?;
        Some((db, args[1].parse::<u64>()?))
    } else {
        None
    };
    let tokens = match &replay {
        Some((db, h)) => historical_tokens(db, *h).await?,
        None => configured_tokens()?,
    };
    let node = BscRpc::from_env()?;
    node.verify()?;
    let source = Arc::new(Reader {
        node,
        tokens,
        watched: (!persist && !backfill).then_some(watched),
    });
    if persist {
        return persistent::run(source, concurrency, ready, args[0] == "--follow").await;
    }
    if let Some((db, h)) = replay {
        let mut observed = source.fetch(h).await?;
        let executions = std::mem::take(&mut observed.executions);
        let b = observed.into_finalized(h)?;
        println!(
            "platform funding recorded={}",
            custody_store::funding::backfill(&db, &b, Some(&executions), None).await?
        );
        return Ok(());
    }
    let head: Header = source
        .node
        .call("eth_getBlockByNumber", json!(["finalized", false]))?;
    let end = head.number.to::<u64>();
    let start = end.saturating_sub(blocks.get() as u64 - 1);
    let mut previous = None;
    ordered_range(start..=end, concurrency, ready, |height| Arc::clone(&source).fetch(height), |height, block| {
        let result = (|| -> Result<()> {
            ensure!(previous.is_none_or(|hash| hash == block.parent), "parent hash mismatch at {height}");
            ensure!(height != end || block.hash == head.hash, "finalized head hash mismatch at {height}");
            previous = Some(block.hash);
            println!("BSC Testnet finalized block {height} {}: {} transactions with matching receipts, {} incoming candidates",
                block.hash, block.transactions, block.incoming.len());
            for item in block.incoming {
                println!("{:?} {} -> {} raw={} tx={} log={:?}", item.asset, item.from, item.to, item.amount, item.tx, item.log_index);
            }
            Ok(())
        })();
        std::future::ready(result)
    }).await
}
