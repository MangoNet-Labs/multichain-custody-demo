use custody_core::Network::MangoTestnet;
use custody_store::{apply_mango_full, cursor, initialize, Applied, Cursor, PgPool};
use mango_worker::decode_full;
use mgo_types::digests::{CheckpointDigest, TransactionDigest};

// SQLx creates a disposable database. These are fixture addresses, not wallet
// keys, and are never registered by the live worker.
#[sqlx::test(migrations = "../migrations")]
#[ignore = "run sh scripts/test-postgres.sh --live-mango"]
async fn recorded_checkpoint_to_postgres(db: PgPool) -> anyhow::Result<()> {
    let (b, exec, coins) = decode_full(include_bytes!("fixtures/testnet-106658.bcs"), 106658)?;
    initialize(
        &db,
        MangoTestnet,
        Cursor {
            height: b.height - 1,
            digest: b.parent,
        },
    )
    .await?;
    sqlx::query("INSERT INTO wallets VALUES('mango-testnet',$1,'deposit','alice','fixture-only')")
        .bind(&b.receipts[0].recipient)
        .execute(&db)
        .await?;
    assert_eq!(
        apply_mango_full(&db, &b, &exec, &coins).await?,
        Applied::Committed { deposits: 1 }
    );
    assert_eq!(
        apply_mango_full(&db, &b, &exec, &coins).await?,
        Applied::AlreadyCommitted
    );
    let (owner, qty, height): (Vec<u8>, String, i64) =
        sqlx::query_as("SELECT owner,balance::text,changed_height FROM mango_wallet_coins")
            .fetch_one(&db)
            .await?;
    assert_eq!(owner, b.receipts[0].recipient);
    assert_eq!((qty.as_str(), height), ("100000000000", 106658));
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:available'"
        )
        .fetch_one(&db)
        .await?,
        "100000000000"
    );
    assert_eq!(cursor(&db, MangoTestnet).await?.unwrap().digest, b.digest);
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "run sh scripts/test-postgres.sh --mango"]
async fn batch_payment_from_full_checkpoint_credits_both_users_once(
    db: PgPool,
) -> anyhow::Result<()> {
    let (b, executions, coins) =
        decode_full(include_bytes!("fixtures/localnet-two-recipients.bcs"), 27)?;
    assert_eq!(
        CheckpointDigest::new(b.digest).base58_encode(),
        "7EBxg7kWa812XgUAhyXmN3WNaLxJxQfQLBcYEaGZ2C7n"
    );
    let tx: TransactionDigest = "25vMTxHbb3x28znkosMtdTu7bZ4MpnwfbwprPBmtwgCR".parse()?;
    assert_eq!(b.receipts.len(), 2);
    assert!(b
        .receipts
        .iter()
        .all(|r| r.tx == tx.into_inner() && r.asset.is_empty()));
    initialize(
        &db,
        MangoTestnet,
        Cursor {
            height: 26,
            digest: b.parent,
        },
    )
    .await?;
    for (byte, user) in [(0xaau8, "alice"), (0xbbu8, "bob")] {
        sqlx::query("INSERT INTO wallets VALUES('mango-testnet',$1,'deposit',$2,$2)")
            .bind([byte; 32].as_slice())
            .bind(user)
            .execute(&db)
            .await?;
    }
    assert_eq!(
        apply_mango_full(&db, &b, &executions, &coins).await?,
        Applied::Committed { deposits: 2 }
    );
    assert_eq!(
        apply_mango_full(&db, &b, &executions, &coins).await?,
        Applied::AlreadyCommitted
    );
    assert_eq!(
        cursor(&db, MangoTestnet).await?.unwrap(),
        Cursor {
            height: 27,
            digest: b.digest
        }
    );
    let amounts: Vec<(String,String)> = sqlx::query_as(
        "SELECT account,sum(delta)::text FROM journal_entries WHERE account LIKE 'user:%' GROUP BY account ORDER BY account")
        .fetch_all(&db).await?;
    assert_eq!(
        amounts,
        [
            ("user:alice:available".into(), "1000000000".into()),
            ("user:bob:available".into(), "2000000000".into())
        ]
    );
    let evidence: Vec<(Vec<u8>,String)> = sqlx::query_as(
        "SELECT d.recipient,j.amount::text FROM deposits d JOIN journal j ON j.id=d.journal_id WHERE d.tx=$1 AND d.height=27 ORDER BY d.recipient")
        .bind(tx.into_inner().as_slice()).fetch_all(&db).await?;
    assert_eq!(
        evidence,
        [
            (vec![0xaa; 32], "1000000000".into()),
            (vec![0xbb; 32], "2000000000".into())
        ]
    );
    Ok(())
}
