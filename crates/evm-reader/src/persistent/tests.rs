use super::*;
use alloy_primitives::{Address, B256};
use axum::{extract::State, routing::post, Json, Router};
use custody_core::AssetId::BscTestnetBnb;
use custody_store::withdrawals::{submit, Request};
use evm_reader::{allowlist, rpc::BscRpc};
use serde_json::Value;
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq)]
enum StateOfChain {
    Unfinalized,
    NoFinality,
    NoReceipts,
    MixedFork,
    Restored,
}

const ALICE: Address = Address::new([1; 20]);
const HOT: Address = Address::new([2; 20]);
const DEST: Address = Address::new([3; 20]);
const DEPOSIT: B256 = B256::new([21; 32]);
const PAYOUT: B256 = B256::new([22; 32]);
const CANON: B256 = B256::new([11; 32]);
const FORK: B256 = B256::new([99; 32]);

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn replay_uses_registry_at_target_height(db: PgPool) -> Result<()> {
    initialize(
        &db,
        BscTestnet,
        Cursor {
            height: 10,
            digest: [10; 32],
        },
    )
    .await?;
    configure_tokens(&db, vec![]).await?;
    configure_tokens(&db, vec![([7; 20], "TEST".into(), 9)]).await?;
    assert!(crate::historical_tokens(&db, 10).await?.is_empty());
    assert_eq!(crate::historical_tokens(&db, 11).await?.len(), 1);
    Ok(())
}

fn block(hash: B256) -> Value {
    let tx = |i, id, from, to, value| {
        json!({"transactionIndex":i, "hash":id, "from":from, "to":to,
        "value":value,"nonce":"0x0","blockNumber":"0xb","blockHash":hash})
    };
    json!({"number":"0xb","hash":hash,"parentHash":B256::new([10;32]),"transactions":[
        tx("0x0",DEPOSIT,DEST,ALICE,if hash==FORK {"0xff"} else {"0x2"}),
        tx("0x1",PAYOUT,HOT,DEST,"0x1")
    ]})
}
fn receipts(hash: B256) -> Value {
    let entry = |i, id| {
        json!({"blockNumber":"0xb","blockHash":hash,"transactionIndex":i,"transactionHash":id,
        "status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x1","logs":[]})
    };
    json!([entry("0x0", DEPOSIT), entry("0x1", PAYOUT)])
}
struct Provider {
    phase: StateOfChain,
    fallbacks: usize,
}
async fn rpc(State(s): State<Arc<Mutex<Provider>>>, Json(r): Json<Value>) -> Json<Value> {
    use StateOfChain::*;
    let mut s = s.lock().unwrap();
    let result = match (r["method"].as_str(), r["params"][0].as_str()) {
        (Some("eth_getBlockByNumber"), Some("finalized")) => match s.phase {
            Unfinalized => Some(json!({"number":"0xa","hash":B256::new([10;32])})),
            NoFinality => None,
            _ => Some(json!({"number":"0xb","hash":CANON})),
        },
        (Some("eth_getBlockByNumber"), Some("0xb")) => Some(block(CANON)),
        (Some("eth_getBlockReceipts"), Some("0xb")) => match s.phase {
            NoReceipts => None,
            MixedFork => Some(receipts(FORK)),
            _ => Some(receipts(CANON)),
        },
        // Available but not a valid fallback for finalized data.
        (Some("eth_getBlockByNumber"), Some("latest")) => {
            s.fallbacks += 1;
            Some(block(FORK))
        }
        (Some("eth_getTransactionReceipt"), _) => {
            s.fallbacks += 1;
            Some(receipts(FORK)[0].clone())
        }
        _ => {
            s.fallbacks += 1;
            None
        }
    };
    Json(match result {
        Some(v) => json!({"id":r["id"],"jsonrpc":"2.0","result":v}),
        None => {
            json!({"id":r["id"],"jsonrpc":"2.0","error":{"code":-32601,"message":"unsupported"}})
        }
    })
}
async fn ledger(db: &PgPool) -> Result<(i64, i64, String, String, String)> {
    Ok(sqlx::query_as(
        "SELECT (SELECT height FROM scan_cursors WHERE network='bsc-testnet'),
        (SELECT count(*) FROM deposits),(SELECT status FROM withdrawals),
        (SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:available'),
        (SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:frozen')",
    )
    .fetch_one(db)
    .await?)
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn outage_and_unfinalized_fork_cannot_credit_or_settle(db: PgPool) -> Result<()> {
    use StateOfChain::*;
    let node = Arc::new(Mutex::new(Provider {
        phase: Unfinalized,
        fallbacks: 0,
    }));
    let app = Router::new().route("/", post(rpc)).with_state(node.clone());
    let sock = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", sock.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(sock, app).await.unwrap() });
    initialize(
        &db,
        BscTestnet,
        Cursor {
            height: 9,
            digest: [9; 32],
        },
    )
    .await?;
    sqlx::query("INSERT INTO wallets VALUES ('bsc-testnet',$1,'deposit','alice','fixture-alice'),('bsc-testnet',$2,'hot',NULL,'fixture-hot')")
        .bind(ALICE.as_slice()).bind(HOT.as_slice()).execute(&db).await?;
    custody_store::apply(
        &db,
        &FinalizedBlock {
            openings: vec![],
            network: BscTestnet,
            height: 10,
            digest: [10; 32],
            parent: [9; 32],
            receipts: vec![Receipt {
                tx: [10; 32],
                recipient: ALICE.to_vec(),
                sender: Some(DEST.to_vec()),
                asset: vec![],
                amount: "1".into(),
                log: None,
            }],
        },
    )
    .await?;
    let order = submit(
        &db,
        Request {
            user: "alice",
            request_id: "fixture",
            asset: BscTestnetBnb,
            recipient: DEST.as_slice(),
            amount: "0.000000000000000001",
        },
    )
    .await?;
    // This test starts after broadcast. Signing and lost-ack recovery have separate tests.
    sqlx::query(
        "INSERT INTO evm_payouts(withdrawal_id,sender,nonce,digest,signed_tx,max_fee)
        VALUES($1::text::uuid,$2,0,$3,$4,21000)",
    )
    .bind(&order.id)
    .bind(HOT.as_slice())
    .bind(PAYOUT.as_slice())
    .bind([1u8].as_slice())
    .execute(&db)
    .await?;
    sqlx::query("UPDATE withdrawals SET status='submitted' WHERE id=$1::text::uuid")
        .bind(&order.id)
        .execute(&db)
        .await?;
    let tokens = allowlist(include_bytes!("../../../../config/bsc-testnet-tokens.json"))?;
    let rules = configure_tokens(
        &db,
        tokens
            .values()
            .map(|t| (t.contract.into_array(), t.symbol.clone(), t.decimals))
            .collect(),
    )
    .await?;
    let reader = Arc::new(Reader {
        node: BscRpc::new(url),
        tokens,
        watched: None,
    });
    let slots = NonZeroUsize::new(2).unwrap();
    let mut pace = HeadPace::default();
    let unchanged = (10, 1, "submitted".into(), "0".into(), "1".into());
    assert!(!sync(&reader, &db, rules, slots, slots, &mut pace).await?);
    assert_eq!(ledger(&db).await?, unchanged);
    for (phase, reason) in [
        (NoFinality, "unsupported"),
        (NoReceipts, "complete receipts required"),
        (MixedFork, "position mismatch"),
    ] {
        node.lock().unwrap().phase = phase;
        let error = sync(&reader, &db, rules, slots, slots, &mut pace)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
        assert_eq!(ledger(&db).await?, unchanged);
        let issue: (String, Option<i64>) =
            sqlx::query_as("SELECT stage,next_height FROM scan_issues WHERE network='bsc-testnet'")
                .fetch_one(&db)
                .await?;
        assert_eq!(
            issue,
            if phase == NoFinality {
                ("finality".into(), None)
            } else {
                ("range".into(), Some(11))
            }
        );
    }
    node.lock().unwrap().phase = Restored;
    assert!(sync(&reader, &db, rules, slots, slots, &mut pace).await?);
    assert!(!sync(&reader, &db, rules, slots, slots, &mut pace).await?);
    assert_eq!(
        ledger(&db).await?,
        (11, 2, "completed".into(), "2".into(), "0".into())
    );
    assert_eq!(cursor(&db, BscTestnet).await?.unwrap().digest, CANON.0);
    assert_eq!(node.lock().unwrap().fallbacks, 0);
    task.abort();
    Ok(())
}
