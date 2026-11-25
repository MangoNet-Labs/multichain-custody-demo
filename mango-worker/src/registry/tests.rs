use super::*;
use axum::{routing::get, Router};
use custody_core::Network::MangoTestnet;
use custody_store::{apply_mango_full, apply_mango_with_rules, initialize, Applied};
use mango_worker::{decode_full, decode_with_rules};
use mgo_types::{full_checkpoint_content::CheckpointData, gas_coin::GAS};

const MIXED: &[u8] = include_bytes!("../../tests/fixtures/localnet-mixed-assets.bcs");
fn token() -> Token {
    let cp: CheckpointData = bcs::from_bytes(MIXED).unwrap();
    let tag = cp
        .transactions
        .iter()
        .flat_map(|t| &t.output_objects)
        .filter_map(|obj| obj.coin_type_maybe())
        .find(|tag| !GAS::is_gas_type(tag))
        .unwrap();
    Token {
        coin_type: tag.to_canonical_string(true),
        symbol: "MGO".into(),
        decimals: 9,
    }
}
fn node() -> Node {
    let cp: CheckpointData = bcs::from_bytes(MIXED).unwrap();
    let header = serde_json::to_vec(&cp.checkpoint_summary).unwrap();
    let router = Router::new()
        .route("/checkpoints/142", get(move || async move { header }))
        .route("/checkpoints/142/full", get(|| async { MIXED }));
    let server =
        axum::Server::bind(&"127.0.0.1:0".parse().unwrap()).serve(router.into_make_service());
    let base = format!("http://{}", server.local_addr());
    tokio::spawn(server);
    Node {
        base,
        client: reqwest::Client::new(),
        rules: Default::default(),
        body_limit: 1 << 20,
    }
}
fn limits() -> (NonZeroUsize, NonZeroUsize) {
    let one = NonZeroUsize::new(1).unwrap();
    (one, one)
}
async fn setup(db: &PgPool) -> Result<()> {
    let (b, _, _) = decode_full(MIXED, 142)?;
    initialize(
        db,
        MangoTestnet,
        Cursor {
            height: 141,
            digest: b.parent,
        },
    )
    .await?;
    sqlx::query("INSERT INTO wallets VALUES('mango-testnet',$1,'deposit','alice','fixture')")
        .bind([0xaau8; 32].as_slice())
        .execute(db)
        .await?;
    Ok(())
}
#[test]
fn type_aliases_share_identity_and_activation_is_inclusive() {
    let t = token();
    let before = Rules::new([(t.clone(), 143)]).unwrap();
    let (b, _, _) = decode_with_rules(MIXED, 142, &before).unwrap();
    assert_eq!(b.receipts.len(), 1);
    let r = Rules::new([(t.clone(), 142)]).unwrap();
    let (b, _, _) = decode_with_rules(MIXED, 142, &r).unwrap();
    assert_eq!(b.receipts.len(), 2);
    assert!(b.receipts.iter().any(|r| r.asset == t.coin_type.as_bytes()
        && r.amount == "50000000000"
        && r.log.is_none()));
    let alias = |s: &str| Token {
        coin_type: s.into(),
        ..t.clone()
    };
    assert!(Rules::new([(alias("0xa::usd::USD"), 0), (alias("0x000a::usd::USD"), 0)]).is_err());
    for invalid in [
        "u8",
        "0x2::mgo::MGO",
        "0x2::coin::Coin<0xa::usd::USD>",
        "invalid",
    ] {
        assert!(Rules::new([(alias(invalid), 0)]).is_err());
    }
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "run sh scripts/test-postgres.sh --mango"]
async fn mixed_deposit_replay_and_independent_inventory(db: PgPool) -> Result<()> {
    setup(&db).await?;
    let node = node();
    let t = token();
    let wanted = Rules::new([(t.clone(), 0)])?;
    let rules = activate(&db, &node, limits(), &wanted).await?;
    assert_eq!(rules.entries().next().unwrap().1, 142);
    let (b, exec, coins) = decode_with_rules(MIXED, 142, &rules)?;
    let (stale, _, _) = decode_full(MIXED, 142)?;
    assert!(apply_mango_full(&db, &stale, &exec, &coins).await.is_err());
    for result in [
        Applied::Committed { deposits: 2 },
        Applied::AlreadyCommitted,
    ] {
        assert_eq!(
            apply_mango_with_rules(&db, &b, &exec, &coins, rules.fingerprint()).await?,
            result
        );
    }
    let rows:Vec<(Vec<u8>,String)>=sqlx::query_as("SELECT asset,delta::text FROM journal_entries WHERE account='user:alice:available' ORDER BY asset").fetch_all(&db).await?;
    assert_eq!(
        rows,
        [
            (vec![], "3000000000".into()),
            (t.coin_type.as_bytes().into(), "50000000000".into())
        ]
    );
    let spendable: Vec<String> =
        sqlx::query_scalar("SELECT balance::text FROM mango_wallet_coins ORDER BY asset")
            .fetch_all(&db)
            .await?;
    assert_eq!(spendable, ["3000000000", "50000000000"]);
    for _ in 0..2 {
        reconcile::run(&db, &node, limits().0, limits().1).await?;
    }
    assert!(activate(&db, &node, limits(), &Rules::default())
        .await
        .is_err());
    assert_eq!(
        activate(&db, &node, limits(), &wanted).await?.fingerprint(),
        rules.fingerprint()
    );
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "run sh scripts/test-postgres.sh --mango"]
async fn existing_token_holdings_abort_activation_without_retrocredit(db: PgPool) -> Result<()> {
    setup(&db).await?;
    let (b, exec, coins) = decode_full(MIXED, 142)?;
    apply_mango_full(&db, &b, &exec, &coins).await?;
    let n = node();
    let wanted = Rules::new([(token(), 0)])?;
    let err = activate(&db, &n, limits(), &wanted)
        .await
        .err()
        .context("expected rejection")?;
    ensure!(
        err.to_string().contains("nonzero opening balance"),
        "{err:#}"
    );
    assert!(stored(&db).await?.fingerprint().is_none());
    let absent = Token {
        coin_type: "0xbeef::test::TEST".into(),
        ..token()
    };
    let rules = activate(&db, &n, limits(), &Rules::new([(absent, 0)])?).await?;
    assert_eq!(rules.entries().next().unwrap().1, 143);
    reconcile::run(&db, &n, limits().0, limits().1).await?;
    Ok(())
}
