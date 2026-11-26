use anyhow::Result;
use custody_core::Network::{BscTestnet, MangoTestnet};
use custody_signer::{create_unlock_file, Signer};
use custody_store::{
    initialize,
    wallets::{request, Purpose},
    Cursor, PgPool,
};
use std::collections::HashSet;

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn bsc_sweep_rules_wait_for_the_scanners_first_allowlist(db: PgPool) -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let secret = tmp.path().join("secrets/unlock");
    create_unlock_file(&secret)?;
    let signer = Signer::open(db.clone(), &tmp.path().join("keys"), &secret).await?;
    let contract = alloy_primitives::address!("edb22908ca8b42f0bbe0ec337f5f21948c83f046");
    let config = include_str!("../../../config/sweep.example.toml");
    initialize(
        &db,
        BscTestnet,
        Cursor {
            height: 10,
            digest: [10; 32],
        },
    )
    .await?;

    // A visible cursor (and even a native-only sweep rule) must not signal readiness.
    let native_only = "[[sweep]]\nnetwork='bsc-testnet'\nasset='native'\nmin_sweep_amount='1'";
    for rules in [native_only, config] {
        let error = signer
            .configure_sweeps(BscTestnet, rules)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("has not activated"), "{error}");
    }
    let counts: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM assets WHERE network='bsc-testnet' AND asset<>''), (SELECT count(*) FROM sweep_rules)")
        .fetch_one(&db).await?;
    assert_eq!(counts, (0, 0));

    // A fresh installation may choose metadata different from the example.
    let tokens = vec![(contract.into_array(), "CUSTOM".to_owned(), 6)];
    let rules = custody_store::configure_tokens(&db, tokens.clone()).await?;
    signer.configure_sweeps(BscTestnet, config).await?;
    assert_eq!(custody_store::configure_tokens(&db, tokens).await?, rules);
    signer.configure_sweeps(BscTestnet, config).await?;
    let row: (String, i16, String) = sqlx::query_as("SELECT a.symbol,a.decimals,s.minimum::text FROM assets a JOIN sweep_rules s USING(network,asset) WHERE a.network='bsc-testnet' AND a.asset=$1")
        .bind(contract.as_slice()).fetch_one(&db).await?;
    assert_eq!(row, ("CUSTOM".into(), 6, "1000000".into()));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn wallet_lifecycle(db: PgPool) -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let vault = tmp.path().join("keystore");
    let secret = tmp.path().join("secrets/unlock");
    create_unlock_file(&secret)?;
    let signer = Signer::open(db.clone(), &vault, &secret).await?;
    let anchor = Cursor {
        height: 30,
        digest: [5; 32],
    };
    assert_eq!(signer.pending().await?, 0);
    initialize(&db, BscTestnet, anchor).await?;
    assert!(signer
        .wallet(MangoTestnet, Purpose::Deposit("alice"))
        .await
        .is_err());
    assert_eq!(signer.pending().await?, 2);
    let first_platform = signer.platform().await?;
    assert_eq!(first_platform.len(), 2);
    assert!(first_platform.iter().all(|w| w.network == BscTestnet));
    let early_alice = signer.wallet(BscTestnet, Purpose::Deposit("alice")).await?;
    assert_eq!(signer.pending().await?, 0);
    initialize(&db, MangoTestnet, anchor).await?;
    assert_eq!(signer.pending().await?, 2);
    let platform = signer.platform().await?;
    assert!(first_platform.iter().all(|w| platform.contains(w)));
    let (first, second) = tokio::join!(
        signer.wallet(BscTestnet, Purpose::Deposit("alice")),
        signer.wallet(BscTestnet, Purpose::Deposit("alice"))
    );
    let alice = first?;
    assert_eq!(alice, early_alice);
    assert_eq!(alice, second?);
    let mut addresses: HashSet<_> = platform.iter().map(|w| w.formatted()).collect();
    assert!(addresses.insert(alice.formatted()));
    for net in [BscTestnet, MangoTestnet] {
        for user in ["alice", "bob"] {
            request(&db, net, Purpose::Deposit(user)).await?;
        }
    }
    assert_eq!(signer.pending().await?, 3);
    for (net, user) in [
        (BscTestnet, "bob"),
        (MangoTestnet, "alice"),
        (MangoTestnet, "bob"),
    ] {
        assert!(addresses.insert(
            signer
                .wallet(net, Purpose::Deposit(user))
                .await?
                .formatted()
        ));
    }
    assert_eq!(addresses.len(), 8);
    drop(signer);
    let restarted = Signer::open(db.clone(), &vault, &secret).await?;
    assert_eq!(platform, restarted.platform().await?);
    assert_eq!(
        alice,
        restarted
            .wallet(BscTestnet, Purpose::Deposit("alice"))
            .await?
    );
    let id = request(&db, BscTestnet, Purpose::Deposit("alice")).await?;
    let file = vault.join(format!("{id}.keybox"));
    std::fs::remove_file(&file)?;
    assert!(Signer::open(db, &vault, &secret).await.is_err());
    assert!(!file.exists());
    Ok(())
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn rollback_after_key_write_recovers_same_wallet(db: PgPool) -> Result<()> {
    for net in [BscTestnet, MangoTestnet] {
        initialize(
            &db,
            net,
            Cursor {
                height: 10,
                digest: [2; 32],
            },
        )
        .await?;
    }
    let tmp = tempfile::tempdir()?;
    let vault = tmp.path().join("vault");
    let secret = tmp.path().join("credentials/unlock");
    create_unlock_file(&secret)?;
    let signer = Signer::open(db.clone(), &vault, &secret).await?;
    let id = request(&db, MangoTestnet, Purpose::Deposit("alice")).await?;
    sqlx::query("ALTER TABLE wallets ADD CONSTRAINT simulate_failure CHECK (false)")
        .execute(&db)
        .await?;
    assert!(signer
        .wallet(MangoTestnet, Purpose::Deposit("alice"))
        .await
        .is_err());
    let file = vault.join(format!("{id}.keybox"));
    let encrypted = std::fs::read(&file)?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM wallets")
            .fetch_one(&db)
            .await?,
        0
    );
    drop(signer);
    sqlx::query("ALTER TABLE wallets DROP CONSTRAINT simulate_failure")
        .execute(&db)
        .await?;
    let signer = Signer::open(db.clone(), &vault, &secret).await?;
    signer
        .wallet(MangoTestnet, Purpose::Deposit("alice"))
        .await?;
    assert_eq!(encrypted, std::fs::read(file)?);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM wallets")
            .fetch_one(&db)
            .await?,
        1
    );
    Ok(())
}
