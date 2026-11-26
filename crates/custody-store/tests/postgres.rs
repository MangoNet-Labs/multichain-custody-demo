use custody_core::Network::{BscTestnet, MangoTestnet};
use custody_store::{apply, cursor, initialize, Applied, Cursor, FinalizedBlock, Receipt};
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn withdrawal_reservations_are_exact_idempotent_and_atomic(db: PgPool) -> anyhow::Result<()> {
    use custody_store::withdrawals::{submit, Request};
    setup(&db).await?;
    apply(
        &db,
        &block(11, vec![candidate(1, 1, "10000000000000000000")]),
    )
    .await?;
    let external = [19u8; 20];
    let make = |id, amount| Request {
        user: "alice",
        request_id: id,
        asset: custody_core::AssetId::BscTestnetBnb,
        recipient: &external,
        amount,
    };
    assert!(submit(
        &db,
        Request {
            recipient: &[1; 20],
            ..make("bad", "1")
        }
    )
    .await
    .is_err());
    assert!(submit(
        &db,
        Request {
            user: "bob",
            ..make("bad", "1")
        }
    )
    .await
    .is_err());
    let (a, b) = tokio::join!(
        submit(&db, make("one", "2")),
        submit(&db, make("one", "2.0"))
    );
    assert_eq!(a?, b?);
    assert!(submit(&db, make("one", "3")).await.is_err());
    let (a, b) = tokio::join!(
        submit(&db, make("two", "6")),
        submit(&db, make("three", "6"))
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:available'"
        )
        .fetch_one(&db)
        .await?,
        "2000000000000000000"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:frozen'"
        )
        .fetch_one(&db)
        .await?,
        "8000000000000000000"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM withdrawals WHERE status='queued'")
            .fetch_one(&db)
            .await?,
        2
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn registry_additions_start_at_next_height_and_invalidate_prefetch(
    db: PgPool,
) -> anyhow::Result<()> {
    use custody_store::{apply_with_rules, configure_tokens};
    setup(&db).await?;
    let first = ([7; 20], "MGO".to_owned(), 9);
    let v1 = configure_tokens(&db, vec![first.clone()]).await?;
    apply_with_rules(&db, &block(11, vec![]), Some(v1)).await?;
    let added = ([5; 20], "TEST".to_owned(), 18);
    let v2 = configure_tokens(&db, vec![first.clone(), added.clone()]).await?;
    assert_ne!(v1, v2);
    assert!(apply_with_rules(&db, &block(12, vec![]), Some(v1))
        .await
        .is_err());
    assert!(configure_tokens(&db, vec![]).await.is_err());
    assert!(configure_tokens(&db, vec![(first.0, "OTHER".into(), 9)])
        .await
        .is_err());
    assert!(apply_with_rules(&db, &block(12, vec![]), Some([0; 32]))
        .await
        .is_err());
    assert_eq!(cursor(&db, BscTestnet).await?.unwrap().height, 11);
    let restored = configure_tokens(&db, vec![first, added]).await?;
    assert_eq!(v2, restored);
    apply_with_rules(&db, &block(12, vec![]), Some(restored)).await?;
    Ok(())
}

fn candidate(id: u8, recipient: u8, amount: &str) -> Receipt {
    Receipt {
        tx: [id; 32],
        recipient: vec![recipient; 20],
        sender: Some(vec![9; 20]),
        asset: vec![],
        amount: amount.into(),
        log: None,
    }
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn opening_is_frozen_once_and_a_full_sweep_moves_but_does_not_credit_it(
    db: PgPool,
) -> anyhow::Result<()> {
    use custody_store::{apply_with_rules, configure_tokens, reconcile, OpeningDelta};
    setup(&db).await?;
    let original = ([8; 20], "OLD".to_owned(), 6);
    let new = ([5; 20], "NEW".to_owned(), 6);
    let r1 = configure_tokens(&db, vec![original.clone()]).await?;
    let mut before = block(11, vec![]);
    for (owner, n) in [(1, "5"), (1, "-1"), (33, "12")] {
        before.openings.push(OpeningDelta {
            address: vec![owner; 20],
            asset: new.0.to_vec(),
            delta: n.into(),
        });
    }
    apply_with_rules(&db, &before, Some(r1)).await?;
    assert_eq!(
        apply_with_rules(&db, &before, Some(r1)).await?,
        Applied::AlreadyCommitted
    );
    before.openings[0].delta = "8".into();
    assert!(apply_with_rules(&db, &before, Some(r1)).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sweep_checks")
            .fetch_one(&db)
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM opening_balances")
            .fetch_one(&db)
            .await?,
        1
    );
    let r2 = configure_tokens(&db, vec![new.clone(), original.clone()]).await?;
    let checkpoint: (i64, Vec<u8>) =
        sqlx::query_as("SELECT enabled_from,opening_digest::bytea FROM assets WHERE symbol='NEW'")
            .fetch_one(&db)
            .await?;
    assert_eq!(checkpoint, (12, vec![11; 32]));
    let transfer = |tx: u8, from: u8, to: u8, amount: &str| Receipt {
        tx: [tx; 32],
        recipient: vec![to; 20],
        sender: Some(vec![from; 20]),
        asset: new.0.to_vec(),
        amount: amount.into(),
        log: Some((0, 0)),
    };
    let incoming = block(12, vec![transfer(12, 9, 1, "2")]);
    assert!(apply_with_rules(&db, &incoming, Some(r1)).await.is_err());
    apply_with_rules(&db, &incoming, Some(r2)).await?;
    apply_with_rules(&db, &block(13, vec![transfer(13, 1, 3, "6")]), Some(r2)).await?;
    assert_eq!(
        configure_tokens(&db, vec![original, new.clone()]).await?,
        r2
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT amount::text FROM opening_balances")
            .fetch_one(&db)
            .await?,
        "4"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:available'"
        )
        .fetch_one(&db)
        .await?,
        "2"
    );
    let snap = reconcile::snapshot(&db, BscTestnet).await?;
    let observations = snap
        .wallets
        .iter()
        .map(|w| {
            (
                (w.address.clone(), w.asset.clone()),
                if w.address == vec![3; 20] && w.asset == new.0 {
                    "6"
                } else {
                    "0"
                }
                .into(),
            )
        })
        .collect();
    assert!(reconcile::finish(&db, snap, observations).await?.1);
    Ok(())
}

fn block(height: i64, receipts: Vec<Receipt>) -> FinalizedBlock {
    FinalizedBlock {
        openings: vec![],
        network: BscTestnet,
        height,
        digest: [height as u8; 32],
        parent: [(height - 1) as u8; 32],
        receipts,
    }
}
async fn setup(db: &PgPool) -> anyhow::Result<()> {
    initialize(
        db,
        BscTestnet,
        Cursor {
            height: 10,
            digest: [10; 32],
        },
    )
    .await?;
    for (id, owner, role) in [
        (1, Some("alice"), "deposit"),
        (2, Some("bob"), "deposit"),
        (3, None, "gas"),
    ] {
        sqlx::query("INSERT INTO wallets(network,address,owner_id,role,key_ref) VALUES('bsc-testnet',$1,$2,$3,$4)")
            .bind(vec![id as u8;20]).bind(owner).bind(role).bind(format!("test:{id}")).execute(db).await?;
    }
    Ok(())
}
#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn platform_capital_and_fixed_height_audit(db: PgPool) -> anyhow::Result<()> {
    use custody_store::{
        apply_evidence, funding,
        reconcile::{self, Balances},
    };
    setup(&db).await?;
    let mut b = block(11, vec![candidate(1, 1, "500"), candidate(2, 3, "70")]);
    apply_evidence(&db, &b, None, Some(&[])).await?;
    assert_eq!(funding::backfill(&db, &b, Some(&[]), None).await?, 0);
    b.receipts[1].amount = "90".into();
    assert!(funding::backfill(&db, &b, Some(&[]), None).await.is_err());
    let at_11 = reconcile::snapshot(&db, BscTestnet).await?;
    let balances = |s: &reconcile::Snapshot| {
        s.wallets
            .iter()
            .map(|w| {
                let raw = if w.asset.is_empty() {
                    match w.address[0] {
                        1 => "500",
                        3 => "70",
                        _ => "0",
                    }
                } else {
                    "0"
                };
                ((w.address.clone(), w.asset.clone()), raw.to_owned())
            })
            .collect::<Balances>()
    };
    let obs = balances(&at_11);
    apply(&db, &block(12, vec![candidate(3, 2, "200")])).await?;
    assert!(reconcile::finish(&db, at_11, obs).await?.1);
    let at_12 = reconcile::snapshot(&db, BscTestnet).await?;
    let mut missing = balances(&at_12);
    missing.pop_first();
    assert!(reconcile::finish(&db, at_12, missing).await.is_err());
    let s = reconcile::snapshot(&db, BscTestnet).await?;
    let bad = balances(&s);
    assert!(!reconcile::finish(&db, s, bad).await?.1);
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT paused AND EXISTS(SELECT 1 FROM reconciliation_holds) FROM payment_control"
        )
        .fetch_one(&db)
        .await?
    );
    let s = reconcile::snapshot(&db, BscTestnet).await?;
    let mut good = balances(&s);
    good.insert((vec![2; 20], vec![]), "200".into());
    assert!(reconcile::finish(&db, s, good).await?.1);
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT paused AND NOT EXISTS(SELECT 1 FROM reconciliation_holds) FROM payment_control"
        )
        .fetch_one(&db)
        .await?
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reconciliation_runs")
            .fetch_one(&db)
            .await?,
        3
    );
    Ok(())
}
#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn concurrent_replay_and_separate_internal_funding(db: PgPool) -> anyhow::Result<()> {
    setup(&db).await?;
    let mut topup = candidate(2, 2, "300");
    topup.sender = Some(vec![3; 20]);
    let mut token = candidate(1, 1, "1000000000");
    let contract = [7; 20];
    let rules =
        Some(custody_store::configure_tokens(&db, vec![(contract, "MGO".into(), 9)]).await?);
    token.asset = contract.to_vec();
    token.log = Some((0, 0));
    let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let b = block(
        11,
        vec![candidate(1, 1, max), token, topup, candidate(3, 200, "99")],
    );
    let apply_block = |block| custody_store::apply_with_rules(&db, block, rules);
    let (a, z) = tokio::join!(apply_block(&b), apply_block(&b));
    let outcomes = [a?, z?];
    assert!(
        outcomes.contains(&Applied::Committed { deposits: 2 })
            && outcomes.contains(&Applied::AlreadyCommitted)
    );
    assert_eq!(sqlx::query_scalar::<_,String>("SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:available' AND asset=''").fetch_one(&db).await?,max);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT sum(delta)::text FROM journal_entries")
            .fetch_one(&db)
            .await?,
        "0"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM internal_receipts")
            .fetch_one(&db)
            .await?,
        1
    );
    let mut conflicting = b;
    conflicting.receipts[1].amount = "2".into();
    assert!(custody_store::apply_with_rules(&db, &conflicting, rules)
        .await
        .is_err());
    assert_eq!(
        initialize(
            &db,
            BscTestnet,
            Cursor {
                height: 300,
                digest: [4; 32]
            }
        )
        .await?
        .height,
        11
    );
    Ok(())
}
#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn mid_block_failure_rolls_back_and_restart_replays(db: PgPool) -> anyhow::Result<()> {
    setup(&db).await?;
    let good = candidate(1, 1, "1");
    let duplicate = candidate(1, 2, "2");
    assert!(apply(&db, &block(11, vec![good.clone(), duplicate]))
        .await
        .is_err());
    for table in ["deposits", "journal", "scanned_blocks"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&db)
                .await?,
            0
        );
    }
    assert_eq!(cursor(&db, BscTestnet).await?.unwrap().height, 10);
    assert!(apply(&db, &block(12, vec![])).await.is_err());
    let mut wrong_parent = block(11, vec![]);
    wrong_parent.parent = [42; 32];
    assert!(apply(&db, &wrong_parent).await.is_err());
    assert_eq!(
        apply(&db, &block(11, vec![good])).await?,
        Applied::Committed { deposits: 1 }
    );
    assert_eq!(
        apply(&db, &block(12, vec![])).await?,
        Applied::Committed { deposits: 0 }
    );
    assert_eq!(cursor(&db, BscTestnet).await?.unwrap().height, 12);
    assert!(cursor(&db, MangoTestnet).await?.is_none());
    Ok(())
}
#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn mango_net_changes_in_one_tx_are_per_owner(db: PgPool) -> anyhow::Result<()> {
    initialize(
        &db,
        MangoTestnet,
        Cursor {
            height: 4,
            digest: [4; 32],
        },
    )
    .await?;
    let mut receipts = Vec::new();
    for (id, user) in [(1u8, "alice"), (2, "bob")] {
        let address = vec![id; 32];
        sqlx::query("INSERT INTO wallets VALUES('mango-testnet',$1,'deposit',$2,$2)")
            .bind(&address)
            .bind(user)
            .execute(&db)
            .await?;
        receipts.push(Receipt {
            tx: [42; 32],
            recipient: address,
            sender: None,
            asset: vec![],
            amount: id.to_string(),
            log: None,
        });
    }
    let b = FinalizedBlock {
        openings: vec![],
        network: MangoTestnet,
        height: 5,
        digest: [5; 32],
        parent: [4; 32],
        receipts,
    };
    assert_eq!(apply(&db, &b).await?, Applied::Committed { deposits: 2 });
    assert_eq!(apply(&db, &b).await?, Applied::AlreadyCommitted);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT amount::text FROM journal ORDER BY amount")
            .fetch_all(&db)
            .await?,
        ["1", "2"]
    );
    let mut mixed = b;
    mixed.height = 6;
    mixed.parent = [5; 32];
    mixed.digest = [6; 32];
    for r in &mut mixed.receipts {
        r.tx = [45; 32];
    }
    sqlx::query("INSERT INTO mango_internal_outputs(tx,sender,recipient,amount,order_ref) VALUES($1,$2,$3,2,'fixture-order')")
        .bind([45u8;32].as_slice()).bind(&mixed.receipts[0].recipient).bind(&mixed.receipts[1].recipient).execute(&db).await?;
    mixed.receipts[1].amount = "3".into();
    assert!(apply(&db, &mixed).await.is_err());
    assert_eq!(cursor(&db, MangoTestnet).await?.unwrap().height, 5);
    mixed.receipts[1].amount = "2".into();
    assert_eq!(
        apply(&db, &mixed).await?,
        Applied::Committed { deposits: 1 }
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM internal_receipts")
            .fetch_one(&db)
            .await?,
        1
    );
    Ok(())
}
