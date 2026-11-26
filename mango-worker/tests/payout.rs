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
#[path = "payout/fragmentation.rs"]
mod fragmentation;

#[derive(Default)]
struct Fixture {
    inventory: Option<fragmentation::Inventory>,
    hot_page_reads: usize,
    balance_reads: usize,
    token: bool,
    wrong: bool,
    poor: bool,
    bad_dry: bool,
    lost: bool,
    sends: Vec<Value>,
    sender: Option<MgoAddress>,
    sweep_round: u8,
    sweep_reads: usize,
    sweep_ahead: bool,
    sweep: Option<(MgoAddress, MgoAddress, MgoAddress)>,
}
const NATIVE: &str = "0x2::mgo::MGO";
fn token() -> String {
    format!("0x{:064x}::other::MGO", 42)
}
fn sweep_coins(token: bool, round: u8) -> Vec<(u8, u64)> {
    match (token, round) {
        (false, 0) => vec![(1, 700_000_000), (2, 300_000_000)],
        (true, 0) => (121..171).map(|id| (id, 0)).collect(),
        (true, 1) => (11..61).map(|id| (id, 10_000_000)).collect(),
        (true, 2) => (61..111).map(|id| (id, 20_000_000)).collect(),
        _ => panic!("unexpected sweep page"),
    }
}
async fn handle(State(f): State<Arc<Mutex<Fixture>>>, Json(v): Json<Value>) -> Json<Value> {
    let mut f = f.lock().unwrap();
    let arg = &v["params"];
    let result = match v["method"].as_str().unwrap() {
        "mgox_getCoins" if f.sweep.is_some() => {
            assert_eq!(arg[0], json!(f.sweep.unwrap().0));
            assert_eq!(arg[1], if f.token { token() } else { NATIVE.into() });
            assert!(arg[2].is_null());
            assert_eq!(arg[3], 50);
            f.sweep_reads += 1;
            let coins:Vec<_>=sweep_coins(f.token,f.sweep_round).into_iter().filter(|_| !f.poor).map(|(n,b)|json!({
                "coinType":arg[1],"coinObjectId":ObjectID::new([n;32]),"digest":ObjectDigest::new([n+10;32]),"version":"4","balance":if f.sweep_ahead{u64::MAX.to_string()}else{b.to_string()}
            })).collect();
            json!({"data":coins,"hasNextPage":!coins.is_empty() && f.token,"nextCursor":coins.last().map(|c| &c["coinObjectId"])})
        }
        "mgo_getCheckpoint" => {
            json!({"sequenceNumber":"0","digest":if f.wrong{"other"}else{mango_worker::TESTNET_GENESIS}})
        }
        "mgox_getCoins" => {
            assert!(
                arg[2].is_null(),
                "withdrawals must not request a second page"
            );
            assert_eq!(arg[3], 50);
            f.hot_page_reads += 1;
            if let Some(inv) = &f.inventory {
                inv.page(arg)
            } else if f.poor {
                json!({"data":[],"hasNextPage":false,"nextCursor":null})
            } else {
                let is_token = arg[1] == token();
                let coins: Vec<_> = [(1, "700000000"), (2, "400000000")].into_iter().map(|(i, balance)| {
                    let n = if is_token { i + 3 } else { i };
                    json!({"coinType":arg[1],"version":"4","balance":balance,"coinObjectId":ObjectID::new([n;32]),"digest":ObjectDigest::new([n+10;32])})
                }).collect();
                json!({"nextCursor":coins.last().unwrap()["coinObjectId"],"data":coins,"hasNextPage":false})
            }
        }
        "mgox_getBalance" => {
            f.balance_reads += 1;
            assert_eq!(arg[0], json!(f.sender.unwrap()));
            let total = f.inventory.as_ref().unwrap().balance(arg[1] == token());
            json!({"coinType":arg[1], "totalBalance":total.to_string()})
        }
        "mgox_getReferenceGasPrice" => json!("1000"),
        "mgo_dryRunTransactionBlock" => {
            if f.sweep.is_some() && f.token && f.sweep_round == 0 {
                json!({"effects":{"status":{"status":"success"}},"balanceChanges":[]})
            } else {
                json!({"effects":{"status":{"status":if f.bad_dry{"failure"}else{"success"}}},
                "balanceChanges":[{"owner":{"AddressOwner":f.sweep.map(|(_,h,_)|h).unwrap_or_else(||MgoAddress::from_bytes([88;32]).unwrap())},"amount":if f.sweep.is_some(){sweep_coins(f.token,f.sweep_round).iter().map(|c|c.1).sum::<u64>().to_string()}else{"1000000000".into()},"coinType":if f.token {token()}else{NATIVE.into()}}]})
            }
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
                let objs: Vec<_> = sweep_coins(f.token, f.sweep_round)
                    .into_iter()
                    .map(|(i, _)| pt.obj(ObjectArg::ImmOrOwnedObject(reference(i))).unwrap())
                    .collect();
                pt.transfer_args(hot, objs);
                let expected = TransactionData::new_programmable_allow_sponsor(
                    deposit,
                    vec![reference(3 + f.sweep_round)],
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
            } else if f.inventory.is_some() {
                assert_eq!(tx.sender(), sender);
                assert_eq!(tx.gas_owner(), sender);
                assert_eq!(arg[1].as_array().unwrap().len(), 1);
                Signature::from_bytes(&STANDARD.decode(arg[1][0].as_str().unwrap()).unwrap())
                    .unwrap()
                    .verify_secure(
                        &IntentMessage::new(Intent::mgo_transaction(), &tx),
                        sender,
                        SignatureScheme::ED25519,
                    )
                    .unwrap();
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
    Fragmented,
    MergeFailure,
    Paid,
    Failed,
    Conflict,
    NoReceipt,
    WrongAsset,
    Sweep,
    WrongSweepOwner,
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
    let asset_file = tmp.path().join("coins.json");
    let coins = if is_token {
        serde_json::json!([{"coin_type": t, "symbol":"MGO","decimals":9}])
    } else {
        serde_json::json!([])
    };
    std::fs::write(&asset_file, serde_json::to_vec(&coins)?)?;
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
            .env("MANGO_COIN_ALLOWLIST", &asset_file)
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
    let late = is_token && end == End::Sweep;
    if !late {
        custody_store::registry::activate(
            &db,
            MangoTestnet,
            if is_token {
                vec![(key.to_vec(), "MGO".into(), 9)]
            } else {
                vec![]
            },
            |_| Ok(stamp),
        )
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
        openings: vec![],
        network: MangoTestnet,
        height: h.into(),
        digest: [h; 32],
        parent: [h - 1; 32],
        receipts,
    };
    if late {
        custody_store::registry::activate(&db, MangoTestnet, vec![], |_| Ok(None)).await?;
        let mut previous = block(11, vec![]);
        previous.openings.push(custody_store::OpeningDelta {
            address: alice.clone(),
            asset: key.to_vec(),
            delta: "700000000".into(),
        });
        custody_store::apply_with_rules(&db, &previous, None).await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sweep_checks")
                .fetch_one(&db)
                .await?,
            0
        );
        custody_store::registry::activate(
            &db,
            MangoTestnet,
            vec![(key.to_vec(), "MGO".into(), 9)],
            |_| Ok(stamp),
        )
        .await?;
        assert!(run(worker, &["--init-assets"]).await??.status.success());
        std::fs::write(&asset_file, "[]")?;
        assert!(!run(worker, &["--init-assets"]).await??.status.success());
        std::fs::write(&asset_file, serde_json::to_vec(&coins)?)?;
    }
    let merging = matches!(end, End::Fragmented | End::MergeFailure);
    let mut incoming = receipt([11; 32], alice.clone());
    if merging {
        incoming.amount = "2000000000".into();
    }
    custody_store::apply_with_rules(
        &db,
        &block(if late { 12 } else { 11 }, vec![incoming]),
        stamp,
    )
    .await?;
    if matches!(end, End::Sweep | End::WrongSweepOwner) {
        if is_token {
            std::fs::write(&asset_file, "[]")?;
            assert!(!run(&signer, &["mango-sweep-once"]).await??.status.success());
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sweep_orders")
                    .fetch_one(&db)
                    .await?,
                0
            );
            std::fs::write(&asset_file, serde_json::to_vec(&coins)?)?;
        }
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
        sqlx::query("INSERT INTO mango_wallet_coins(object_id,owner,version,digest,balance,changed_height,asset) VALUES($1,$2,4,$3,10000000,11,'')")
            .bind([3u8;32].as_slice()).bind(&gas).bind([13u8;32].as_slice()).execute(&db).await?;
        if late {
            f.lock().unwrap().poor = true;
            let o = run(&signer, &["mango-sweep-once"]).await??;
            ensure!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            assert!(String::from_utf8(o.stdout)?.contains("prepared=false"));
            f.lock().unwrap().poor = false;
            sqlx::query("UPDATE sweep_checks SET next_check=now()")
                .execute(&db)
                .await?;
            f.lock().unwrap().sweep_ahead = true;
            let o = run(&signer, &["mango-sweep-once"]).await??;
            ensure!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            assert!(String::from_utf8(o.stdout)?.contains("prepared=false"));
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sweep_orders")
                    .fetch_one(&db)
                    .await?,
                0
            );
            f.lock().unwrap().sweep_ahead = false;
            sqlx::query("UPDATE sweep_checks SET next_check=now()")
                .execute(&db)
                .await?;
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
        let rounds = if late { 3 } else { 1 };
        for round in 0..rounds {
            if round > 0 {
                // A lagging RPC repeats the just-settled input versions. The reservations must reject them.
                let stale = run(&signer, &["mango-sweep-once"]).await??;
                ensure!(
                    stale.status.success(),
                    "{}",
                    String::from_utf8_lossy(&stale.stderr)
                );
                assert!(String::from_utf8(stale.stdout)?.contains("prepared=false"));
                assert_eq!(
                    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sweep_orders")
                        .fetch_one(&db)
                        .await?,
                    i64::from(round)
                );
                sqlx::query("UPDATE sweep_checks SET next_check=now()")
                    .execute(&db)
                    .await?;
                f.lock().unwrap().sweep_round = round;
                let o = run(&signer, &["mango-sweep-once"]).await??;
                ensure!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
                assert!(String::from_utf8(o.stdout)?.contains("prepared=true"));
                let sent = run(worker, &["--broadcast-once"]).await??;
                ensure!(
                    sent.status.success(),
                    "{}",
                    String::from_utf8_lossy(&sent.stderr)
                );
            }
            let page = sweep_coins(is_token, round);
            let amount: u64 = page.iter().map(|c| c.1).sum();
            let mut consumed: Vec<_> = page.iter().map(|(n, _)| ([*n; 32], 4)).collect();
            consumed.push(([3 + round; 32], 4));
            let (hash,actual,total):(Vec<u8>,String,String)=sqlx::query_as("SELECT p.digest::bytea,s.amount::text,s.observed_balance::text FROM mango_payouts p JOIN sweep_orders s ON s.id=p.sweep_id WHERE p.confirmed_height IS NULL").fetch_one(&db).await?;
            assert_eq!(actual, amount.to_string());
            assert_eq!(
                total,
                if !late {
                    "1000000000"
                } else if round < 2 {
                    "1700000000"
                } else {
                    "1200000000"
                }
            );
            let hash = hash.try_into().unwrap();
            let e = Execution {
                hash,
                sender: alice.as_slice().try_into()?,
                payer: gas.as_slice().try_into()?,
                success: true,
                net_gas: 750,
                consumed,
                owners: page
                    .iter()
                    .map(|(id, _)| {
                        (
                            [*id; 32],
                            if end == End::WrongSweepOwner && *id == page[0].0 {
                                alice.as_slice().try_into().unwrap()
                            } else {
                                hot.as_slice().try_into().unwrap()
                            },
                        )
                    })
                    .collect(),
            };
            let b = block(
                if late { 13 + round } else { 12 },
                if amount == 0 {
                    vec![]
                } else {
                    vec![Receipt {
                        amount: actual,
                        ..receipt(hash, hot.clone())
                    }]
                },
            );
            let mut changes = custody_store::mango_coins::Changes::new();
            changes.insert([3 + round; 32], None);
            changes.insert(
                [4 + round; 32],
                Some(custody_store::mango_coins::Owned {
                    owner: gas.as_slice().try_into()?,
                    version: 4,
                    digest: [14 + round; 32],
                    balance: 10_000_000,
                    asset: vec![],
                }),
            );
            custody_store::apply_mango_with_rules(
                &db,
                &b,
                std::slice::from_ref(&e),
                &changes,
                stamp,
            )
            .await?;
            assert_eq!(
                custody_store::apply_mango_with_rules(&db, &b, &[e], &changes, stamp).await?,
                Applied::AlreadyCommitted
            );
        }
        if end == End::WrongSweepOwner {
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT status FROM sweep_orders")
                    .fetch_one(&db)
                    .await?,
                "review"
            );
            task.abort();
            return Ok(());
        }
        let o = run(&signer, &["mango-sweep-once"]).await??;
        ensure!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(String::from_utf8(o.stdout)?.contains("prepared=false"));
        assert_eq!(
            f.lock().unwrap().sweep_reads,
            usize::from(rounds) + if late { 4 } else { 0 }
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sweep_orders WHERE status='completed'"
            )
            .fetch_one(&db)
            .await?,
            i64::from(rounds)
        );
        let (user,fee,n):(String,String,i64)=sqlx::query_as("SELECT sum(delta) FILTER(WHERE account='user:alice:available')::text,sum(delta) FILTER(WHERE account='platform:gas')::text,(SELECT count(*) FROM internal_receipts) FROM journal_entries").fetch_one(&db).await?;
        assert_eq!(
            (user, fee, n),
            (
                "1000000000".into(),
                (-750 * i64::from(rounds)).to_string(),
                i64::from(rounds) - i64::from(late)
            )
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
    if merging {
        fragmentation::exercise(
            &db,
            &f,
            is_token,
            end == End::MergeFailure,
            &signer,
            worker,
            run,
            stamp,
        )
        .await?;
        task.abort();
        return Ok(());
    }
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
        owners: vec![],
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
        End::Sweep | End::WrongSweepOwner | End::Fragmented | End::MergeFailure => unreachable!(),
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
case!(native_withdrawal_after_repeated_consolidation, Fragmented);
case!(
    failed_merge_never_releases_principal_or_signs_another_payment,
    MergeFailure
);
#[sqlx::test(migrations = "../migrations")]
#[ignore = "run scripts/test-postgres.sh --mango"]
async fn token_withdrawal_after_repeated_consolidation(db: PgPool) -> Result<()> {
    scenario(db, End::Fragmented, true).await
}
case!(confirmed_failure_releases_principal, Failed);
case!(conflicting_object_version_keeps_freeze, Conflict);
case!(missing_finalized_gain_keeps_freeze, NoReceipt);
case!(sweep_requires_each_input_to_reach_hot, WrongSweepOwner);
case!(
    sponsored_sweep_reservations_signatures_rebroadcast_and_settlement,
    Sweep
);
#[sqlx::test(migrations = "../migrations")]
#[ignore = "run scripts/test-postgres.sh --mango"]
async fn late_listed_token_sweeps_first_page_again_only_after_confirmation(
    db: PgPool,
) -> Result<()> {
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
