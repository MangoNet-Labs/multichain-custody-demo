use anyhow::{ensure, Context, Result};
use axum::{extract::State, routing::post, Json, Router};
use base64::{engine::general_purpose::STANDARD, Engine};
use custody_core::{
    AssetId,
    Network::{BscTestnet, MangoTestnet},
};
use custody_store::{
    initialize, mango::Execution, withdrawals, Applied, Cursor, FinalizedBlock, PgPool, Receipt,
};
use mgo_types::{
    base_types::{MgoAddress, ObjectID, SequenceNumber},
    crypto::{default_hash, MgoSignature, Signature, SignatureScheme, ToFromBytes},
    digests::{ObjectDigest, TransactionDigest},
    transaction::{TransactionData, TransactionDataAPI},
};
use serde_json::{json, Value};
use shared_crypto::intent::{Intent, IntentMessage};
use std::{
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Fixture {
    token: bool,
    wrong: bool,
    poor: bool,
    bad_dry: bool,
    lost: bool,
    sends: Vec<Value>,
    sender: Option<MgoAddress>,
    sweep: Option<(MgoAddress, MgoAddress, MgoAddress)>,
}
const NATIVE: &str = "0x2::mgo::MGO";
fn token() -> String {
    format!("0x{:064x}::other::MGO", 42)
}
async fn handle(State(f): State<Arc<Mutex<Fixture>>>, Json(v): Json<Value>) -> Json<Value> {
    let mut f = f.lock().unwrap();
    let arg = &v["params"];
    let result = match v["method"].as_str().unwrap() {
        "mgo_getCheckpoint" => {
            json!({"sequenceNumber":"0","digest":if f.wrong{"other"}else{mango_worker::TESTNET_GENESIS}})
        }
        "mgox_getCoins" => {
            if f.poor {
                json!({"data":[],"hasNextPage":false,"nextCursor":null})
            } else {
                let i = if arg[2].is_null() { 1 } else { 2 };
                let is_token = arg[1] == token();
                let n = if is_token { i + 3 } else { i };
                let id = ObjectID::new([n; 32]);
                let balance = if i == 1 { "700000000" } else { "400000000" };
                let coin = json!({"coinType":if is_token{token()}else{NATIVE.into()},"version":"4","balance":balance,"coinObjectId":id,"digest":ObjectDigest::new([n+10;32])});
                json!({"data":[coin],"nextCursor":id,"hasNextPage":i==1})
            }
        }
        "mgox_getReferenceGasPrice" => json!("1000"),
        "mgo_dryRunTransactionBlock" => {
            json!({"effects":{"status":{"status":if f.bad_dry{"failure"}else{"success"}}},
                "balanceChanges":[{"owner":{"AddressOwner":f.sweep.map(|(_,h,_)|h).unwrap_or_else(||MgoAddress::from_bytes([88;32]).unwrap())},"amount":"1000000000","coinType":if f.token {token()}else{NATIVE.into()}}]})
        }
        "mgo_executeTransactionBlock" => {
            let tx: TransactionData =
                bcs::from_bytes(&STANDARD.decode(arg[0].as_str().unwrap()).unwrap()).unwrap();
            let sender = f.sender.unwrap();
            let recipient = MgoAddress::from_bytes([88; 32]).unwrap();
            let reference = |i: u8| {
                (
                    ObjectID::new([i; 32]),
                    SequenceNumber::from_u64(4),
                    ObjectDigest::new([i + 10; 32]),
                )
            };
            if let Some((deposit, hot, gas)) = f.sweep {
                use mgo_types::{
                    programmable_transaction_builder::ProgrammableTransactionBuilder,
                    transaction::ObjectArg,
                };
                let mut pt = ProgrammableTransactionBuilder::new();
                let objs =
                    [1, 2].map(|i| pt.obj(ObjectArg::ImmOrOwnedObject(reference(i))).unwrap());
                if f.token {
                    use mgo_types::{transaction::Command, Identifier};
                    let vector = pt.command(Command::MakeMoveVec(None, objs.to_vec()));
                    let dest = pt.pure(hot).unwrap();
                    pt.programmable_move_call(
                        mgo_types::MGO_FRAMEWORK_PACKAGE_ID,
                        Identifier::new("pay").unwrap(),
                        Identifier::new("join_vec_and_transfer").unwrap(),
                        vec![mgo_types::parse_mgo_type_tag(&token()).unwrap()],
                        vec![vector, dest],
                    );
                } else {
                    pt.transfer_args(hot, objs.to_vec());
                }
                let expected = TransactionData::new_programmable_allow_sponsor(
                    deposit,
                    vec![reference(3)],
                    pt.finish(),
                    10_000_000,
                    1000,
                    gas,
                );
                assert_eq!(expected, tx);
                assert_eq!(arg[1].as_array().unwrap().len(), 2);
                for (idx, owner) in [deposit, gas].iter().enumerate() {
                    Signature::from_bytes(&STANDARD.decode(arg[1][idx].as_str().unwrap()).unwrap())
                        .unwrap()
                        .verify_secure(
                            &IntentMessage::new(Intent::mgo_transaction(), &tx),
                            *owner,
                            SignatureScheme::ED25519,
                        )
                        .unwrap();
                }
            } else {
                let expected = if f.token {
                    use mgo_types::{
                        programmable_transaction_builder::ProgrammableTransactionBuilder,
                        transaction::{Command, ObjectArg},
                        Identifier,
                    };
                    let mut pt = ProgrammableTransactionBuilder::new();
                    let [a, b] =
                        [4, 5].map(|n| pt.obj(ObjectArg::ImmOrOwnedObject(reference(n))).unwrap());
                    pt.command(Command::MergeCoins(a, vec![b]));
                    let amount = pt.pure(1_000_000_000u64).unwrap();
                    let dest = pt.pure(recipient).unwrap();
                    pt.programmable_move_call(
                        mgo_types::MGO_FRAMEWORK_PACKAGE_ID,
                        Identifier::new("pay").unwrap(),
                        Identifier::new("split_and_transfer").unwrap(),
                        vec![mgo_types::parse_mgo_type_tag(&token()).unwrap()],
                        vec![a, amount, dest],
                    );
                    TransactionData::new_programmable(
                        sender,
                        vec![reference(1)],
                        pt.finish(),
                        10_000_000,
                        1000,
                    )
                } else {
                    TransactionData::new_pay_mgo(
                        sender,
                        vec![reference(2)],
                        vec![recipient],
                        vec![1_000_000_000],
                        reference(1),
                        10_000_000,
                        1000,
                    )
                    .unwrap()
                };
                assert_eq!(expected, tx);
                assert_eq!(tx.gas_owner(), sender);
                let sig =
                    Signature::from_bytes(&STANDARD.decode(arg[1][0].as_str().unwrap()).unwrap())
                        .unwrap();
                sig.verify_secure(
                    &IntentMessage::new(Intent::mgo_transaction(), &tx),
                    sender,
                    SignatureScheme::ED25519,
                )
                .unwrap();
            }
            f.sends.push(arg.clone());
            if f.lost {
                f.lost = false;
                return Json(
                    json!({"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"lost reply"}}),
                );
            }
            json!({"digest":TransactionDigest::new(default_hash(&tx))})
        }
        other => panic!("unexpected RPC {other}"),
    };
    Json(json!({"jsonrpc":"2.0","id":1,"result":result}))
}
#[derive(Clone, Copy, PartialEq)]
enum End {
    Paid,
    Failed,
    Conflict,
    NoReceipt,
    WrongAsset,
    Sweep,
}
async fn scenario(db: PgPool, end: End, is_token: bool) -> Result<()> {
    let f = Arc::new(Mutex::new(Fixture {
        token: is_token,
        ..Fixture::default()
    }));
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let rpc = format!("http://{}", listener.local_addr()?);
    let server = axum::Server::from_tcp(listener)?.serve(
        Router::new()
            .route("/", post(handle))
            .with_state(f.clone())
            .into_make_service(),
    );
    let task = tokio::spawn(server);
    let mut url = url::Url::parse(&std::env::var("DATABASE_URL")?)?;
    url.set_path(
        &sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(&db)
            .await?,
    );
    let tmp = tempfile::tempdir()?;
    let cfg = tmp.path().join("sweeps.toml");
    let t = token();
    let key = if is_token { t.as_bytes() } else { &[] };
    std::fs::write(
        &cfg,
        format!(
            "[[sweep]]\nnetwork='mango-testnet'\nasset='{}'\nmin_sweep_amount='1'\n",
            if is_token { &t } else { NATIVE }
        ),
    )?;
    let signer =
        std::env::var("CUSTODY_TEST_SIGNER_BIN").context("use scripts/test-postgres.sh --mango")?;
    let worker = env!("CARGO_BIN_EXE_mango-worker");
    let run = |bin: &str, args: &[&str]| {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .env("DATABASE_URL", url.as_str())
            .env("MANGO_WORKER_BIN", worker)
            .env("MANGO_RPC_URL", &rpc)
            .env("MANGO_PAYOUT_GAS_BUDGET", "10000000")
            .env("CUSTODY_SWEEP_CONFIG", &cfg)
            .env("CUSTODY_KEYSTORE", tmp.path().join("vault"))
            .env("CUSTODY_UNLOCK_FILE", tmp.path().join("credentials/key"));
        tokio::task::spawn_blocking(move || cmd.output())
    };
    for n in [MangoTestnet, BscTestnet] {
        initialize(
            &db,
            n,
            Cursor {
                height: 10,
                digest: [10; 32],
            },
        )
        .await?;
    }
    for args in [
        vec!["init-unlock"],
        vec!["platform"],
        vec!["deposit", "alice", "mango-testnet"],
    ] {
        let out = run(&signer, &args).await??;
        ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stamp = is_token.then_some([42u8; 32]);
    if is_token {
        sqlx::query(
            "INSERT INTO assets(network,asset,symbol,decimals) VALUES('mango-testnet',$1,'MGO',9)",
        )
        .bind(key)
        .execute(&db)
        .await?;
        sqlx::query("UPDATE scan_cursors SET rules=$1 WHERE network='mango-testnet'")
            .bind(stamp.unwrap().as_slice())
            .execute(&db)
            .await?;
    }
    let hot: Vec<u8> = sqlx::query_scalar(
        "SELECT address FROM wallets WHERE network='mango-testnet' AND role='hot'",
    )
    .fetch_one(&db)
    .await?;
    f.lock().unwrap().sender = Some(MgoAddress::from_bytes(&hot)?);
    let alice: Vec<u8> = sqlx::query_scalar("SELECT address FROM wallets WHERE owner_id='alice'")
        .fetch_one(&db)
        .await?;
    let receipt = |tx, recipient| Receipt {
        tx,
        recipient,
        sender: None,
        asset: key.to_vec(),
        amount: "1000000000".into(),
        log: None,
    };
    let block = |h: u8, receipts| FinalizedBlock {
        network: MangoTestnet,
        height: h.into(),
        digest: [h; 32],
        parent: [h - 1; 32],
        receipts,
    };
    custody_store::apply_with_rules(
        &db,
        &block(11, vec![receipt([11; 32], alice.clone())]),
        stamp,
    )
    .await?;
    if end == End::Sweep {
        let gas: Vec<u8> = sqlx::query_scalar(
            "SELECT address FROM wallets WHERE network='mango-testnet' AND role='gas'",
        )
        .fetch_one(&db)
        .await?;
        f.lock().unwrap().sweep = Some((
            MgoAddress::from_bytes(&alice)?,
            MgoAddress::from_bytes(&hot)?,
            MgoAddress::from_bytes(&gas)?,
        ));
        for (i, owner, balance) in [
            (1u8, &alice, 700_000_000),
            (2, &alice, 300_000_000),
            (3, &gas, 10_000_000),
        ] {
            sqlx::query("INSERT INTO mango_wallet_coins(object_id,owner,version,digest,balance,changed_height,asset) VALUES($1,$2,4,$3,$4::bigint,11,$5)")
                .bind([i;32].as_slice()).bind(owner).bind([i+10;32].as_slice()).bind(i64::from(balance)).bind(if i==3{&[][..]}else{key}).execute(&db).await?;
            if i == 2 {
                let o = run(&signer, &["mango-sweep-once"]).await??;
                ensure!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
                assert!(String::from_utf8(o.stdout)?.contains("prepared=false"));
                let detail: String = sqlx::query_scalar("SELECT detail FROM sweep_checks")
                    .fetch_one(&db)
                    .await?;
                assert!(detail.contains("platform gas"), "{detail}");
                sqlx::query("UPDATE sweep_checks SET next_check=now()")
                    .execute(&db)
                    .await?;
            }
        }
        let o = run(&signer, &["mango-sweep-once"]).await??;
        ensure!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(String::from_utf8(o.stdout)?.contains("prepared=true"));
        assert!(
            String::from_utf8(run(&signer, &["mango-sweep-once"]).await??.stdout)?
                .contains("prepared=false")
        );
        let signature: Vec<u8> = sqlx::query_scalar("SELECT sponsor_signature FROM mango_payouts")
            .fetch_one(&db)
            .await?;
        sqlx::query("UPDATE mango_payouts SET sponsor_signature=signature")
            .execute(&db)
            .await?;
        assert!(!run(worker, &["--broadcast-once"]).await??.status.success());
        assert!(f.lock().unwrap().sends.is_empty());
        sqlx::query("UPDATE mango_payouts SET sponsor_signature=$1")
            .bind(signature)
            .execute(&db)
            .await?;
        f.lock().unwrap().lost = true;
        assert!(!run(worker, &["--broadcast-once"]).await??.status.success());
        sqlx::query("UPDATE mango_payouts SET next_send=now()")
            .execute(&db)
            .await?;
        let o = run(worker, &["--broadcast-once"]).await??;
        ensure!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        {
            let f = f.lock().unwrap();
            assert_eq!(f.sends.len(), 2);
            assert_eq!(f.sends[0], f.sends[1]);
        }
        let hash: Vec<u8> = sqlx::query_scalar("SELECT digest::bytea FROM mango_payouts")
            .fetch_one(&db)
            .await?;
        let hash = hash.try_into().unwrap();
        let e = Execution {
            hash,
            sender: alice.as_slice().try_into()?,
            payer: gas.as_slice().try_into()?,
            success: true,
            net_gas: 750,
            consumed: vec![([1; 32], 4), ([2; 32], 4), ([3; 32], 4)],
        };
        let b = block(12, vec![receipt(hash, hot)]);
        let changes = Default::default();
        custody_store::apply_mango_with_rules(&db, &b, std::slice::from_ref(&e), &changes, stamp)
            .await?;
        assert_eq!(
            custody_store::apply_mango_with_rules(&db, &b, &[e], &changes, stamp).await?,
            Applied::AlreadyCommitted
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM sweep_orders")
                .fetch_one(&db)
                .await?,
            "completed"
        );
        let (user,fee,n):(String,String,i64)=sqlx::query_as("SELECT sum(delta) FILTER(WHERE account='user:alice:available')::text,sum(delta) FILTER(WHERE account='platform:gas')::text,(SELECT count(*) FROM internal_receipts) FROM journal_entries").fetch_one(&db).await?;
        assert_eq!((user.as_str(), fee.as_str(), n), ("1000000000", "-750", 1));
        assert_eq!(
            sqlx::query_scalar::<_, Vec<u8>>("SELECT asset FROM internal_receipts")
                .fetch_one(&db)
                .await?,
            key
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM deposits")
                .fetch_one(&db)
                .await?,
            1
        );
        task.abort();
        return Ok(());
    }
    withdrawals::submit(
        &db,
        withdrawals::Request {
            user: "alice",
            request_id: "test",
            asset: if is_token {
                AssetId::MangoTestnetCoin(t.clone())
            } else {
                AssetId::MangoTestnetMgo
            },
            recipient: &[88; 32],
            amount: "1",
        },
    )
    .await?;
    if end == End::Paid {
        for defect in 0..3 {
            {
                let mut s = f.lock().unwrap();
                s.wrong = defect == 0;
                s.poor = defect == 1;
                s.bad_dry = defect == 2;
            }
            let out = run(&signer, &["mango-prepare-once"]).await??;
            assert_eq!(out.status.success(), defect != 0);
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mango_payouts")
                    .fetch_one(&db)
                    .await?,
                0
            );
            sqlx::query("UPDATE withdrawals SET next_prepare=now()")
                .execute(&db)
                .await?;
        }
        f.lock().unwrap().bad_dry = false;
    }
    let signed = run(&signer, &["mango-prepare-once"]).await??;
    ensure!(
        signed.status.success(),
        "{}",
        String::from_utf8_lossy(&signed.stderr)
    );
    assert!(String::from_utf8(signed.stdout)?.contains("prepared=true"));
    let hash: Vec<u8> = sqlx::query_scalar("SELECT digest::bytea FROM mango_payouts")
        .fetch_one(&db)
        .await?;
    if end == End::Paid {
        sqlx::query("UPDATE payment_control SET paused=true")
            .execute(&db)
            .await?;
        assert!(run(worker, &["--broadcast-once"]).await??.status.success());
        assert!(f.lock().unwrap().sends.is_empty());
        sqlx::query("UPDATE payment_control SET paused=false")
            .execute(&db)
            .await?;
        f.lock().unwrap().lost = true;
        assert!(!run(worker, &["--broadcast-once"]).await??.status.success());
        sqlx::query("UPDATE mango_payouts SET next_send=now()")
            .execute(&db)
            .await?;
        let sent = run(worker, &["--broadcast-once"]).await??;
        ensure!(
            sent.status.success(),
            "{}",
            String::from_utf8_lossy(&sent.stderr)
        );
        let s = f.lock().unwrap();
        assert_eq!(s.sends.len(), 2);
        assert_eq!(s.sends[0], s.sends[1]);
    }
    let hash = if end == End::Conflict {
        [51; 32]
    } else {
        hash.try_into().unwrap()
    };
    let e = Execution {
        hash,
        sender: hot.clone().try_into().unwrap(),
        payer: hot.try_into().unwrap(),
        success: end != End::Failed,
        net_gas: if end == End::Paid { -200 } else { 900 },
        consumed: if end == End::Failed {
            vec![([1; 32], 4)]
        } else if is_token {
            vec![([1; 32], 4), ([4; 32], 4), ([5; 32], 4)]
        } else {
            vec![([1; 32], 4), ([2; 32], 4)]
        },
    };
    let b = block(
        12,
        if end == End::Paid {
            vec![receipt(hash, vec![88; 32])]
        } else if end == End::WrongAsset {
            vec![Receipt {
                asset: vec![],
                ..receipt(hash, vec![88; 32])
            }]
        } else {
            vec![]
        },
    );
    let changes = Default::default();
    custody_store::apply_mango_with_rules(&db, &b, std::slice::from_ref(&e), &changes, stamp)
        .await?;
    assert_eq!(
        custody_store::apply_mango_with_rules(&db, &b, &[e], &changes, stamp).await?,
        Applied::AlreadyCommitted
    );
    let balances:Vec<String>=sqlx::query_scalar("SELECT coalesce(sum(delta) FILTER(WHERE account='user:alice:available'),0)::text FROM journal_entries UNION ALL SELECT coalesce(sum(delta) FILTER(WHERE account='user:alice:frozen'),0)::text FROM journal_entries UNION ALL SELECT coalesce(sum(delta) FILTER(WHERE account='platform:gas'),0)::text FROM journal_entries").fetch_all(&db).await?;
    let status: String = sqlx::query_scalar("SELECT status FROM withdrawals")
        .fetch_one(&db)
        .await?;
    let (expected, amounts) = match end {
        End::Paid => ("completed", ["0", "0", "200"]),
        End::Failed => ("failed", ["1000000000", "0", "-900"]),
        End::Conflict => ("review", ["0", "1000000000", "0"]),
        End::NoReceipt | End::WrongAsset => ("review", ["0", "1000000000", "-900"]),
        End::Sweep => unreachable!(),
    };
    assert_eq!(status, expected);
    assert_eq!(balances, amounts);
    assert!(!sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM journal_entries WHERE (account='platform:gas' AND asset<>'') OR (account LIKE 'user:%' AND asset<>$1))")
        .bind(key).fetch_one(&db).await?);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mango_payout_inputs WHERE NOT reserved")
            .fetch_one(&db)
            .await?,
        if end == End::Failed && is_token {
            2
        } else {
            i64::from(end == End::Failed)
        }
    );
    assert!(Path::new(worker).is_file());
    task.abort();
    Ok(())
}
macro_rules! case {
    ($n:ident,$e:ident) => {
        #[sqlx::test(migrations = "../migrations")]
        #[ignore = "run scripts/test-postgres.sh --mango"]
        async fn $n(db: PgPool) -> Result<()> {
            scenario(db, End::$e, false).await
        }
    };
}
case!(multi_coin_verified_payment_and_rebroadcast, Paid);
case!(confirmed_failure_releases_principal, Failed);
case!(conflicting_object_version_keeps_freeze, Conflict);
case!(missing_finalized_gain_keeps_freeze, NoReceipt);
case!(
    sponsored_sweep_reservations_signatures_rebroadcast_and_settlement,
    Sweep
);
#[sqlx::test(migrations = "../migrations")]
#[ignore = "run scripts/test-postgres.sh --mango"]
async fn allowlisted_token_sponsored_sweep(db: PgPool) -> Result<()> {
    scenario(db, End::Sweep, true).await
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "run scripts/test-postgres.sh --mango"]
async fn allowlisted_token_withdrawal_rebroadcast(db: PgPool) -> Result<()> {
    scenario(db, End::Paid, true).await
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "run scripts/test-postgres.sh --mango"]
async fn token_withdrawal_wrong_asset_is_not_settled(db: PgPool) -> Result<()> {
    scenario(db, End::WrongAsset, true).await
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "run scripts/test-postgres.sh --mango"]
async fn failed_token_execution_refunds_token_not_gas(db: PgPool) -> Result<()> {
    scenario(db, End::Failed, true).await
}
