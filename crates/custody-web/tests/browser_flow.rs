use anyhow::Result;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    Router,
};
use custody_core::Network::{BscTestnet, MangoTestnet};
use custody_store::{apply, initialize, Cursor, FinalizedBlock, PgPool, Receipt};
use tower::ServiceExt;

async fn request(
    router: &Router,
    uri: &str,
    fields: Option<&str>,
    host: &str,
    origin: Option<&str>,
) -> Result<(StatusCode, String)> {
    let mut r = Request::builder().uri(uri).header("host", host);
    if let Some(origin) = origin {
        r = r.header("origin", origin);
    }
    if fields.is_some() {
        r = r
            .method(Method::POST)
            .header("content-type", "application/x-www-form-urlencoded");
    }
    let response = router
        .clone()
        .oneshot(r.body(Body::from(fields.unwrap_or("").to_owned()))?)
        .await?;
    let status = response.status();
    Ok((
        status,
        String::from_utf8(to_bytes(response.into_body(), 1 << 20).await?.to_vec())?,
    ))
}
const HOST: &str = "127.0.0.1:4310";
const ORIGIN: &str = "http://127.0.0.1:4310";

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn mango_token_withdrawal_uses_its_own_precision_and_ledger(db: PgPool) -> Result<()> {
    let t = format!("0x{:064x}::usd::USD", 10);
    sqlx::query(
        "INSERT INTO assets(network,asset,symbol,decimals) VALUES('mango-testnet',$1,'MGO',6)",
    )
    .bind(t.as_bytes())
    .execute(&db)
    .await?;
    sqlx::query("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES('mango-testnet',$1,'custody','user:alice:available',1234567)")
        .bind(t.as_bytes()).execute(&db).await?;
    let router = custody_web::app(db.clone());
    let page = request(&router, "/wallet/alice", None, HOST, None).await?.1;
    assert!(
        page.contains(&t)
            && page.contains(">1.234567<")
            && page.contains(&format!("value=\"{t}\""))
    );
    assert_eq!(page.matches("value=\"mango-native\"").count(), 1);
    let form = format!(
        "asset={t}&recipient=0x{}&amount=1&request_id={}",
        "01".repeat(32),
        uuid::Uuid::new_v4()
    );
    let (status, _) = request(
        &router,
        "/wallet/alice/withdraw",
        Some(&form),
        HOST,
        Some(ORIGIN),
    )
    .await?;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM withdrawals WHERE asset=$1 AND amount=1000000"
        )
        .bind(t.as_bytes())
        .fetch_one(&db)
        .await?,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn scanner_health_distinguishes_finality_backlog_stale_samples_and_recovery(
    db: PgPool,
) -> Result<()> {
    use custody_store::{note_scan_head, observed, ScanStage};
    let router = custody_web::app(db.clone());
    let page = || async {
        request(&router, "/operations/alice", None, HOST, None)
            .await
            .map(|r| r.1)
    };
    assert!(page().await?.contains("Initializing"));
    observed::<()>(
        &db,
        BscTestnet,
        ScanStage::Finality,
        Err(anyhow::anyhow!("private provider detail")),
    )
    .await
    .unwrap_err();
    let waiting = page().await?;
    assert!(waiting.contains("Waiting for BSC finality"));
    assert!(!waiting.contains("private provider detail"));
    initialize(
        &db,
        BscTestnet,
        Cursor {
            height: 10,
            digest: [10; 32],
        },
    )
    .await?;
    note_scan_head(&db, BscTestnet, 13, 10).await;
    let block = |h: u8| FinalizedBlock {
        openings: vec![],
        network: BscTestnet,
        height: h.into(),
        parent: [h - 1; 32],
        digest: [h; 32],
        receipts: vec![],
    };
    apply(&db, &block(11)).await?;
    let active = page().await?;
    assert!(active.contains("Catching up") && active.contains("Remaining to sample: 2"));
    assert!(!active.contains("Waiting for BSC finality"));
    sqlx::query("UPDATE scan_head_samples SET sampled_at=now()-interval '2 minutes'")
        .execute(&db)
        .await?;
    // The range is still being written even though its initial head sample is old.
    // Move the old failure further into the past to preserve the order of events.
    sqlx::query("UPDATE scan_issues SET observed_at=now()-interval '3 minutes'")
        .execute(&db)
        .await?;
    assert!(page().await?.contains("Catching up"));
    observed::<()>(
        &db,
        BscTestnet,
        ScanStage::Range(12),
        Err(anyhow::anyhow!("bad receipt")),
    )
    .await
    .unwrap_err();
    assert!(page().await?.contains("Scan interrupted at height 12"));
    apply(&db, &block(12)).await?;
    apply(&db, &block(13)).await?;
    let stale = page().await?;
    assert!(stale.contains("Awaiting fresh head sample") && !stale.contains("Scan interrupted"));
    note_scan_head(&db, BscTestnet, 13, 13).await;
    assert!(page().await?.contains("At sampled head"));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn mango_can_issue_addresses_while_bsc_has_no_cursor(db: PgPool) -> Result<()> {
    let router = custody_web::app(db.clone());
    initialize(
        &db,
        MangoTestnet,
        Cursor {
            height: 9,
            digest: [9; 32],
        },
    )
    .await?;
    let page = request(&router, "/wallet/bob", None, HOST, None).await?.1;
    assert!(page.contains("name=\"network\" value=\"mango-testnet\""));
    assert!(page.contains("Waiting for the BSC Testnet scan cursor."));
    for (network, expected) in [
        ("mango-testnet", StatusCode::SEE_OTHER),
        ("bsc-testnet", StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let form = format!("network={network}");
        assert_eq!(
            request(
                &router,
                "/wallet/bob/deposit",
                Some(&form),
                HOST,
                Some(ORIGIN)
            )
            .await?
            .0,
            expected
        );
    }
    let allocations: Vec<String> =
        sqlx::query_scalar("SELECT network::text FROM wallet_allocations")
            .fetch_all(&db)
            .await?;
    assert_eq!(allocations, ["mango-testnet"]);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn accounts_addresses_evidence_and_idempotent_withdrawal(db: PgPool) -> Result<()> {
    let router = custody_web::app(db.clone());
    let page = request(&router, "/wallet/alice", None, HOST, None).await?;
    assert_eq!(page.0, StatusCode::OK);
    assert!(page.1.contains("Waiting for the BSC Testnet scan cursor."));
    assert!(page
        .1
        .contains("Waiting for the Mango Testnet scan cursor."));
    let path = "/wallet/alice/deposit";
    let form = Some("network=bsc-testnet");
    assert_eq!(
        request(&router, path, form, HOST, Some(ORIGIN)).await?.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    initialize(
        &db,
        BscTestnet,
        Cursor {
            height: 10,
            digest: [10; 32],
        },
    )
    .await?;
    let page = request(&router, "/wallet/alice", None, HOST, None).await?.1;
    assert!(page.contains("name=\"network\" value=\"bsc-testnet\""));
    assert!(!page.contains("name=\"network\" value=\"mango-testnet\""));
    assert!(page.contains("Waiting for the Mango Testnet scan cursor."));
    assert_eq!(
        request(
            &router,
            path,
            Some("network=mango-testnet"),
            HOST,
            Some(ORIGIN)
        )
        .await?
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for _ in 0..2 {
        assert_eq!(
            request(&router, path, form, HOST, Some(ORIGIN)).await?.0,
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM wallet_allocations")
            .fetch_one(&db)
            .await?,
        1
    );
    assert!(request(&router, "/wallet/alice", None, HOST, None)
        .await?
        .1
        .contains("Awaiting signer"));
    sqlx::query("INSERT INTO wallets(network,address,role,owner_id,key_ref) VALUES('bsc-testnet',$1,'deposit','alice','fixture')").bind([1u8;20].as_slice()).execute(&db).await?;
    apply(
        &db,
        &FinalizedBlock {
            openings: vec![],
            network: BscTestnet,
            height: 11,
            parent: [10; 32],
            digest: [11; 32],
            receipts: vec![Receipt {
                tx: [19; 32],
                recipient: vec![1; 20],
                sender: Some(vec![2; 20]),
                asset: vec![],
                amount: "1100000000000000000".into(),
                log: None,
            }],
        },
    )
    .await?;
    let page = request(&router, "/wallet/alice", None, HOST, None).await?.1;
    assert!(page.contains(">1.1<"));
    let tx = format!("0x{}", hex::encode([19; 32]));
    assert!(request(&router, "/activity/alice", None, HOST, None)
        .await?
        .1
        .contains(&tx));
    assert!(!request(&router, "/activity/bob", None, HOST, None)
        .await?
        .1
        .contains(&tx));
    let id = page
        .split_once("name=\"request_id\" value=\"")
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap();
    let fields = format!(
        "asset=native&recipient=0x{}&amount=1&request_id={id}",
        hex::encode([3; 20])
    );
    for _ in 0..2 {
        assert_eq!(
            request(
                &router,
                "/wallet/alice/withdraw",
                Some(&fields),
                HOST,
                Some(ORIGIN)
            )
            .await?
            .0,
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM withdrawals")
            .fetch_one(&db)
            .await?,
        1
    );
    let page = request(&router, "/wallet/alice", None, HOST, None).await?.1;
    assert!(page.contains(">0.1<") && page.contains("Frozen: 1"));
    let list = request(
        &router,
        "/activity/alice?kind=withdrawals",
        None,
        HOST,
        None,
    )
    .await?;
    assert_eq!(list.0, StatusCode::OK);
    assert!(list.1.contains(">queued<"));
    assert!(
        !request(&router, "/activity/bob?kind=withdrawals", None, HOST, None)
            .await?
            .1
            .contains(">queued<")
    );
    assert_eq!(
        request(
            &router,
            "/wallet/bob/withdraw",
            Some(&fields),
            HOST,
            Some(ORIGIN)
        )
        .await?
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(
            &router,
            "/activity/alice?kind=deposits&page=0",
            None,
            HOST,
            None
        )
        .await?
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn withdraw_native_mango_uses_separate_ledger_and_full_address(db: PgPool) -> Result<()> {
    let router = custody_web::app(db.clone());
    sqlx::query("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES('mango-testnet','','custody','user:alice:available',1500000000)")
        .execute(&db).await?;
    let id = uuid::Uuid::new_v4();
    for (addr, status) in [
        (hex::encode([4; 20]), StatusCode::UNPROCESSABLE_ENTITY),
        (hex::encode([4; 32]), StatusCode::SEE_OTHER),
    ] {
        let fields = format!("asset=mango-native&amount=1.2&recipient=0x{addr}&request_id={id}");
        assert_eq!(
            request(
                &router,
                "/wallet/alice/withdraw",
                Some(&fields),
                HOST,
                Some(ORIGIN)
            )
            .await?
            .0,
            status
        );
    }
    let (n, a): (String, String) =
        sqlx::query_as("SELECT network::text,amount::text FROM withdrawals")
            .fetch_one(&db)
            .await?;
    assert_eq!((n.as_str(), a.as_str()), ("mango-testnet", "1200000000"));
    let (_, page) = request(
        &router,
        "/activity/alice?kind=withdrawals",
        None,
        HOST,
        None,
    )
    .await?;
    assert!(page.contains("Mango Testnet") && page.contains("1.2 MGO"));
    sqlx::query("INSERT INTO wallets(network,address,role,key_ref) VALUES('mango-testnet',$1,'hot','fixture-hot')")
        .bind([3u8;32].as_slice()).execute(&db).await?;
    sqlx::query("INSERT INTO mango_payouts(merge_for,sender,digest,tx_bytes,signature,budget) SELECT id,$1,$2,$3,$4,1000 FROM withdrawals")
        .bind([3u8;32].as_slice()).bind([5u8;32].as_slice()).bind([1u8].as_slice()).bind([0u8;97].as_slice()).execute(&db).await?;
    sqlx::query("UPDATE withdrawals SET status='consolidating'")
        .execute(&db)
        .await?;
    let (status, page) = request(
        &router,
        "/activity/alice?kind=withdrawals",
        None,
        HOST,
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("consolidating") && page.contains("Latest hot-wallet consolidation"));
    assert!(!page.contains("Signed transaction") && !page.contains("Finalized transaction"));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn local_only_pause_is_explicit_and_mutations_need_same_origin(db: PgPool) -> Result<()> {
    let router = custody_web::app(db.clone());
    assert_eq!(
        request(&router, "/wallet/alice", None, "example.invalid", None)
            .await?
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&router, "/wallet/nobody", None, HOST, None)
            .await?
            .0,
        StatusCode::NOT_FOUND
    );
    for origin in [None, Some("null"), Some("http://127.0.0.1:4311")] {
        assert_eq!(
            request(
                &router,
                "/operations/alice/payments",
                Some("paused=true"),
                HOST,
                origin
            )
            .await?
            .0,
            StatusCode::FORBIDDEN
        );
    }
    for paused in [true, true, false] {
        assert_eq!(
            request(
                &router,
                "/operations/alice/payments",
                Some(&format!("paused={paused}")),
                HOST,
                Some(ORIGIN)
            )
            .await?
            .0,
            StatusCode::SEE_OTHER
        );
        let v: bool = sqlx::query_scalar("SELECT paused FROM payment_control")
            .fetch_one(&db)
            .await?;
        assert_eq!(v, paused);
        let page = request(&router, "/operations/alice", None, HOST, None).await?;
        assert_eq!(page.0, StatusCode::OK);
        assert!(page.1.contains(if paused {
            "Resume payments"
        } else {
            "Pause payments"
        }));
    }
    Ok(())
}
