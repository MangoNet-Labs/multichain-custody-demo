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
