use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::{keccak256, Address, Bytes, TxKind, U256};
use anyhow::Result;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use custody_core::{
    AssetId,
    Network::{BscTestnet, MangoTestnet},
};
use custody_signer::{create_unlock_file, Signer};
use custody_store::{
    apply, apply_evidence, initialize,
    payouts::Execution,
    wallets::Purpose,
    withdrawals::{submit, Request},
    Applied, Cursor, FinalizedBlock, PgPool, Receipt,
};
use evm_reader::{
    broadcast::send_ready,
    rpc::{BscRpc, GENESIS},
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Fixture {
    sent: Vec<Bytes>,
    no_ack: bool,
    poor: bool,
    wrong_chain: bool,
    sweep: Option<SweepState>,
}
struct SweepState {
    deposit: Address,
    native: u128,
    token: u64,
    gas_funds: u128,
    deposit_nonce: u64,
}
async fn rpc(
    State(s): State<Arc<Mutex<Fixture>>>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut s = s.lock().unwrap();
    let ans = match req["method"].as_str().unwrap() {
        "eth_chainId" => json!(if s.wrong_chain { "0x38" } else { "0x61" }),
        "eth_getBlockByNumber" if req["params"][0] != "0x0" && s.sweep.is_some() => {
            let height = u8::from_str_radix(
                req["params"][0].as_str().unwrap().trim_start_matches("0x"),
                16,
            )
            .unwrap();
            json!({"number":req["params"][0],"hash":format!("0x{}",hex::encode([height;32]))})
        }
        "eth_getBlockByNumber" => json!({"number":"0x0","hash":GENESIS}),
        "eth_getTransactionCount" if s.sweep.is_some() => {
            let f = s.sweep.as_ref().unwrap();
            json!(format!(
                "0x{:x}",
                if req["params"][0] == json!(f.deposit) {
                    f.deposit_nonce
                } else {
                    3
                }
            ))
        }
        "eth_getTransactionCount" => json!("0x3"),
        "eth_getBalance" if s.sweep.is_some() => {
            let f = s.sweep.as_ref().unwrap();
            json!(format!(
                "0x{:x}",
                if req["params"][0] == json!(f.deposit) {
                    f.native
                } else {
                    f.gas_funds
                }
            ))
        }
        "eth_getBalance" => json!(if s.poor {
            "0x0"
        } else {
            "0x1bc16d674ec80000" // 2 BNB in the hot wallet
        }),
        "eth_call" if s.sweep.is_some() => {
            json!(format!("0x{:064x}", s.sweep.as_ref().unwrap().token))
        }
        "eth_call" => json!(format!("0x{:064x}", 1_000_000_000_000u64)),
        "eth_estimateGas" => json!("0x10000"),
        "eth_gasPrice" => json!("0x3b9aca00"),
        "eth_sendRawTransaction" => {
            let raw: Bytes = serde_json::from_value(req["params"][0].clone()).unwrap();
            let hash = keccak256(&raw);
            s.sent.push(raw);
            if s.no_ack {
                s.no_ack = false;
                return (StatusCode::BAD_GATEWAY, Json(Value::Null));
            }
            json!(hash)
        }
        other => panic!("unexpected RPC: {other}"),
    };
    (
        StatusCode::OK,
        Json(json!({"id":req["id"],"jsonrpc":"2.0","result":ans})),
    )
}

struct Server {
    rpc: BscRpc,
    state: Arc<Mutex<Fixture>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let rpc_url = format!("http://{}", listener.local_addr()?);
        let state = Arc::new(Mutex::new(Fixture::default()));
        let router = Router::new()
            .route("/", post(rpc))
            .with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Ok(Self {
            rpc: BscRpc::new(rpc_url),
            state,
            task,
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum End {
    Native,
    Token,
    Reverted,
    NoTransfer,
    OtherNonceUser,
}
async fn exercise(db: PgPool, end: End) -> Result<()> {
    let server = Server::new().await?;
    for n in [BscTestnet, MangoTestnet] {
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
    let tmp = tempfile::tempdir()?;
    let unlock = tmp.path().join("secret/unlock");
    create_unlock_file(&unlock)?;
    let vault = tmp.path().join("keys");
    let signer = Signer::open(db.clone(), &vault, &unlock).await?;
    let hot = signer.wallet(BscTestnet, Purpose::Hot).await?;
    let alice = signer.wallet(BscTestnet, Purpose::Deposit("alice")).await?;
    let deposit_address = Address::from_slice(&alice.address);
    let hot_address = Address::from_slice(&hot.address);
    let recipient = Address::from([88; 20]);
    let token = matches!(end, End::Token | End::NoTransfer);
    let asset = if token {
        sqlx::query_scalar("SELECT asset FROM assets WHERE octet_length(asset)=20")
            .fetch_one(&db)
            .await?
    } else {
        Vec::<u8>::new()
    };
    let qty = if token {
        "10000000000"
    } else {
        "1000000000000000000"
    };
    let deposit = Receipt {
        tx: [11; 32],
        recipient: alice.address,
        sender: Some(vec![9; 20]),
        asset: asset.clone(),
        amount: qty.into(),
        log: token.then_some((0, 0)),
    };
    let block = |h: u8, receipts| FinalizedBlock {
        network: BscTestnet,
        height: h.into(),
        digest: [h; 32],
        parent: [h - 1; 32],
        receipts,
    };
    let mut receipts = vec![deposit];
    if end == End::Native {
        receipts.push(Receipt {
            tx: [81; 32],
            recipient: hot.address.clone(),
            sender: Some(vec![9; 20]),
            asset: vec![],
            amount: "2000000000000000000".into(),
            log: None,
        });
    }
    apply(&db, &block(11, receipts)).await?;
    let order = submit(
        &db,
        Request {
            user: "alice",
            request_id: "p1",
            asset: if token {
                AssetId::BscTestnetErc20(asset.clone().try_into().unwrap())
            } else {
                AssetId::BscTestnetBnb
            },
            recipient: recipient.as_slice(),
            amount: if token { "10" } else { "1" },
        },
    )
    .await?;
    let max = U256::from(1_000_000_000_000_000u64);
    if end == End::Native {
        server.state.lock().unwrap().wrong_chain = true;
        assert!(signer.prepare_bsc(&server.rpc, max).await.is_err());
        server.state.lock().unwrap().wrong_chain = false;
        sqlx::query("UPDATE withdrawals SET next_prepare=now()")
            .execute(&db)
            .await?;
        server.state.lock().unwrap().poor = true;
        assert!(!signer.prepare_bsc(&server.rpc, max).await?);
        server.state.lock().unwrap().poor = false;
        sqlx::query("UPDATE withdrawals SET next_prepare=now()")
            .execute(&db)
            .await?;
        assert!(!signer.prepare_bsc(&server.rpc, U256::from(1)).await?);
        sqlx::query("UPDATE withdrawals SET next_prepare=now()")
            .execute(&db)
            .await?;
    }
    assert!(signer.prepare_bsc(&server.rpc, max).await?);
    assert!(!signer.prepare_bsc(&server.rpc, max).await?);
    drop(signer);
    let raw: Vec<u8> = sqlx::query_scalar("SELECT signed_tx FROM evm_payouts")
        .fetch_one(&db)
        .await?;
    let signed = Signed::<TxLegacy>::rlp_decode(&mut &raw[..])?;
    assert_eq!(signed.recover_signer()?.as_slice(), hot.address);
    let tx = signed.tx();
    assert_eq!((tx.chain_id, tx.nonce), (Some(97), 3));
    if token {
        assert_eq!(tx.to, TxKind::Call(Address::from_slice(&asset)));
        assert_eq!(&tx.input[..4], &[0xa9, 0x05, 0x9c, 0xbb]);
        assert_eq!(&tx.input[16..36], recipient.as_slice());
        assert_eq!(U256::from_be_slice(&tx.input[36..]), qty.parse::<U256>()?);
        assert_eq!(tx.value, U256::ZERO);
    } else {
        assert_eq!(tx.to, TxKind::Call(recipient));
        assert_eq!(tx.value, qty.parse::<U256>()?);
        assert!(tx.input.is_empty());
    }
    sqlx::query("UPDATE payment_control SET paused=true")
        .execute(&db)
        .await?;
    assert!(!send_ready(&db, &server.rpc).await?);
    sqlx::query("UPDATE payment_control SET paused=false")
        .execute(&db)
        .await?;
    server.state.lock().unwrap().no_ack = true;
    assert!(send_ready(&db, &server.rpc).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM withdrawals")
            .fetch_one(&db)
            .await?,
        "signed"
    );
    sqlx::query("UPDATE evm_payouts SET next_send=now()")
        .execute(&db)
        .await?;
    assert!(send_ready(&db, &server.rpc).await?);
    assert_eq!(
        server.state.lock().unwrap().sent,
        [Bytes::from(raw.clone()), Bytes::from(raw.clone())]
    );
    let hash = if end == End::OtherNonceUser {
        [200; 32]
    } else {
        keccak256(&raw).0
    };
    let exec = [Execution {
        hash,
        sender: hot.address.clone().try_into().unwrap(),
        nonce: 3,
        success: end != End::Reverted,
        fee: "21000".into(),
    }];
    let paid = matches!(end, End::Native | End::Token);
    let receipts = if paid {
        vec![Receipt {
            tx: hash,
            recipient: recipient.to_vec(),
            sender: Some(hot.address),
            asset,
            amount: qty.into(),
            log: token.then_some((0, 0)),
        }]
    } else {
        vec![]
    };
    let b = block(12, receipts);
    apply_evidence(&db, &b, None, Some(&exec)).await?;
    assert_eq!(
        apply_evidence(&db, &b, None, Some(&exec)).await?,
        Applied::AlreadyCommitted
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM withdrawals WHERE id=$1::text::uuid")
            .bind(order.id)
            .fetch_one(&db)
            .await?;
    let expected = if paid {
        "completed"
    } else if end == End::Reverted {
        "failed"
    } else {
        "review"
    };
    assert_eq!(status, expected);
    let balances:Vec<String>=sqlx::query_scalar("SELECT COALESCE(sum(delta) FILTER (WHERE account='user:alice:available'),0)::text FROM journal_entries UNION ALL SELECT COALESCE(sum(delta) FILTER (WHERE account='user:alice:frozen'),0)::text FROM journal_entries UNION ALL SELECT COALESCE(sum(delta) FILTER (WHERE account='platform:gas'),0)::text FROM journal_entries").fetch_all(&db).await?;
    assert_eq!(
        balances,
        [
            if end == End::Reverted { qty } else { "0" },
            if expected == "review" { qty } else { "0" },
            if end == End::OtherNonceUser {
                "0"
            } else {
                "-21000"
            }
        ]
    );
    assert!(!send_ready(&db, &server.rpc).await?);
    if end == End::Native {
        // The withdrawal already settled from the pre-funded hot wallet; now run the delayed sweep.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sweep_orders")
                .fetch_one(&db)
                .await?,
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM platform_funding")
                .fetch_one(&db)
                .await?,
            1
        );
        let signer = Signer::open(db.clone(), &vault, &unlock).await?;
        signer.wallet(BscTestnet, Purpose::Gas).await?;
        signer
            .configure_sweeps(
                BscTestnet,
                include_str!("../../../config/sweep.example.toml"),
            )
            .await?;
        server.state.lock().unwrap().sweep = Some(SweepState {
            deposit: deposit_address,
            native: qty.parse()?,
            token: 0,
            gas_funds: 0,
            deposit_nonce: 0,
        });
        assert!(signer.prepare_bsc_sweep(&server.rpc, max).await?);
        assert!(send_ready(&db, &server.rpc).await?);
        let raw: Vec<u8> =
            sqlx::query_scalar("SELECT signed_tx FROM evm_payouts WHERE sweep_id IS NOT NULL")
                .fetch_one(&db)
                .await?;
        let signed = Signed::<TxLegacy>::rlp_decode(&mut &raw[..])?;
        assert_eq!(signed.recover_signer()?, deposit_address);
        assert_eq!(signed.tx().to, TxKind::Call(hot_address));
        let hash = keccak256(&raw).0;
        let swept = block(
            13,
            vec![Receipt {
                tx: hash,
                recipient: hot_address.to_vec(),
                sender: Some(deposit_address.to_vec()),
                asset: vec![],
                amount: signed.tx().value.to_string(),
                log: None,
            }],
        );
        let effects = [Execution {
            hash,
            sender: deposit_address.into_array(),
            nonce: signed.tx().nonce,
            success: true,
            fee: "21000".into(),
        }];
        apply_evidence(&db, &swept, None, Some(&effects)).await?;
        let after:(String,i64,i64,String) = sqlx::query_as(
            "SELECT (SELECT status FROM sweep_orders),(SELECT count(*) FROM deposits),(SELECT count(*) FROM internal_receipts),
             (SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:available')").fetch_one(&db).await?;
        assert_eq!(after, ("completed".into(), 1, 1, "0".into()));
    }
    Ok(())
}
macro_rules! scenario {
    ($test:ident,$end:ident) => {
        #[sqlx::test(migrations = "../../migrations")]
        #[ignore = "run sh scripts/test-postgres.sh"]
        async fn $test(db: PgPool) -> Result<()> {
            exercise(db, End::$end).await
        }
    };
}
scenario!(native_payment_and_lost_ack, Native);
scenario!(erc20_payment, Token);
scenario!(finalized_failure_unfreezes, Reverted);
scenario!(missing_token_event_keeps_freeze, NoTransfer);
scenario!(different_nonce_consumer_never_refunds, OtherNonceUser);

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn sweeps_token_gas_topup_then_native_without_user_debit(db: PgPool) -> Result<()> {
    sweep_threshold_case(db, 0).await
}
#[sqlx::test(migrations = "../../migrations")]
#[ignore = "run sh scripts/test-postgres.sh"]
async fn no_native_sweep_if_token_gas_leaves_one_wei_below_threshold(db: PgPool) -> Result<()> {
    sweep_threshold_case(db, 1).await
}
async fn sweep_threshold_case(db: PgPool, shortfall: u128) -> Result<()> {
    let server = Server::new().await?;
    for net in [BscTestnet, MangoTestnet] {
        initialize(
            &db,
            net,
            Cursor {
                height: 10,
                digest: [10; 32],
            },
        )
        .await?;
    }
    let tmp = tempfile::tempdir()?;
    let unlock = tmp.path().join("secret/unlock");
    create_unlock_file(&unlock)?;
    let signer = Signer::open(db.clone(), &tmp.path().join("keys"), &unlock).await?;
    let deposit = Address::from_slice(
        &signer
            .wallet(BscTestnet, Purpose::Deposit("alice"))
            .await?
            .address,
    );
    let hot = Address::from_slice(&signer.wallet(BscTestnet, Purpose::Hot).await?.address);
    let gas = Address::from_slice(&signer.wallet(BscTestnet, Purpose::Gas).await?.address);
    let config = include_str!("../../../config/sweep.example.toml");
    signer.configure_sweeps(MangoTestnet, config).await?;
    signer.configure_sweeps(BscTestnet, config).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM sweep_rules WHERE network='mango-testnet'"
        )
        .fetch_one(&db)
        .await?,
        1
    );
    assert!(signer
        .configure_sweeps(BscTestnet, &config.replace("0.01", "-1"))
        .await
        .is_err());
    let token: Vec<u8> =
        sqlx::query_scalar("SELECT asset FROM assets WHERE octet_length(asset)=20")
            .fetch_one(&db)
            .await?;
    let block = |h: u8, receipts| FinalizedBlock {
        network: BscTestnet,
        height: h.into(),
        digest: [h; 32],
        parent: [h - 1; 32],
        receipts,
    };
    let receipt = |tx, sender: Address, recipient: Address, asset: &[u8], amount: u128| Receipt {
        tx,
        sender: Some(sender.to_vec()),
        recipient: recipient.to_vec(),
        asset: asset.to_vec(),
        amount: amount.to_string(),
        log: (!asset.is_empty()).then_some((0, 0)),
    };
    let donor = Address::from([99; 20]);
    let cap = U256::from(1_000_000_000_000_000u64);
    const TOKEN: u64 = 1_000_000_000;
    const BNB: u128 = 10_000_000_000_000_000;
    const ACTUAL_FEE: u128 = 40_000_000_000_000;
    server.state.lock().unwrap().sweep = Some(SweepState {
        deposit,
        native: 0,
        token: TOKEN - 1,
        gas_funds: 0,
        deposit_nonce: 0,
    });
    apply(
        &db,
        &block(
            11,
            vec![receipt([1; 32], donor, deposit, &token, (TOKEN - 1).into())],
        ),
    )
    .await?;
    assert!(!signer.prepare_bsc_sweep(&server.rpc, cap).await?);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT next_check IS NULL FROM sweep_checks")
            .fetch_one(&db)
            .await?
    );
    server.state.lock().unwrap().sweep.as_mut().unwrap().token = TOKEN;
    apply(
        &db,
        &block(12, vec![receipt([2; 32], donor, deposit, &token, 1)]),
    )
    .await?;
    assert!(!signer.prepare_bsc_sweep(&server.rpc, cap).await?);
    let reason: String = sqlx::query_scalar("SELECT detail FROM sweep_checks")
        .fetch_one(&db)
        .await?;
    assert!(reason.contains("gas wallet funding"), "{reason}");
    server
        .state
        .lock()
        .unwrap()
        .sweep
        .as_mut()
        .unwrap()
        .gas_funds = BNB;
    sqlx::query("UPDATE sweep_checks SET next_check=now()")
        .execute(&db)
        .await?;
    let mut reserve = 0;
    for (step, source, dest, kind) in [
        (0, gas, deposit, "gas_topup"),
        (1, deposit, hot, "sweep"),
        (2, deposit, hot, "sweep"),
    ] {
        if step == 2 && shortfall > 0 {
            assert!(!signer.prepare_bsc_sweep(&server.rpc, cap).await?);
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT detail FROM sweep_checks")
                    .fetch_one(&db)
                    .await?,
                "below configured thresholds"
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sweep_orders")
                    .fetch_one(&db)
                    .await?,
                2
            );
            break;
        }
        assert!(signer.prepare_bsc_sweep(&server.rpc, cap).await?);
        assert!(!signer.prepare_bsc_sweep(&server.rpc, cap).await?);
        let (raw,k,a,amt,fee):(Vec<u8>,String,Vec<u8>,String,String)=sqlx::query_as("SELECT p.signed_tx,s.kind,s.asset,s.amount::text,p.max_fee::text FROM sweep_orders s JOIN evm_payouts p ON p.sweep_id=s.id WHERE s.status='signed'").fetch_one(&db).await?;
        assert_eq!(k, kind);
        let signed = Signed::<TxLegacy>::rlp_decode(&mut &raw[..])?;
        assert_eq!(signed.recover_signer()?, source);
        let hash = keccak256(&raw).0;
        let amount = amt.parse::<u128>()?;
        let maxfee = fee.parse::<u128>()?;
        if step == 1 {
            assert_eq!(a, token);
            assert_eq!(signed.tx().to, TxKind::Call(Address::from_slice(&token)));
            assert_eq!(&signed.tx().input[16..36], hot.as_slice());
            assert_eq!(
                U256::from_be_slice(&signed.tx().input[36..]),
                U256::from(TOKEN)
            );
            assert_eq!(amount, u128::from(TOKEN));
            assert_eq!(reserve, maxfee);
        } else {
            assert!(a.is_empty());
            assert_eq!(signed.tx().to, TxKind::Call(dest));
            assert_eq!(signed.tx().value, U256::from(amount));
            if step == 0 {
                reserve = amount;
                server.state.lock().unwrap().no_ack = true;
                assert!(send_ready(&db, &server.rpc).await.is_err());
                sqlx::query("UPDATE evm_payouts SET next_send=now()")
                    .execute(&db)
                    .await?;
            } else {
                assert_eq!(amount, BNB - maxfee)
            }
        }
        assert!(send_ready(&db, &server.rpc).await?);
        if step == 0 {
            assert_eq!(
                server.state.lock().unwrap().sent[..],
                [Bytes::from(raw.clone()), Bytes::from(raw)]
            )
        }
        let mut receipts = vec![receipt(hash, source, dest, &a, amount)];
        {
            let mut fixture = server.state.lock().unwrap();
            let f = fixture.sweep.as_mut().unwrap();
            if step == 0 {
                // Both native and token now meet their thresholds. Token must go first.
                let extra = BNB + ACTUAL_FEE - reserve - shortfall;
                receipts.push(receipt([66; 32], donor, deposit, &[], extra));
                f.native = reserve + extra;
            } else {
                f.deposit_nonce += 1;
                f.native -= ACTUAL_FEE;
                if step == 1 {
                    f.token = 0
                } else {
                    f.native -= amount
                }
            }
        }
        let b = block(13 + step, receipts);
        let e = [Execution {
            hash,
            sender: source.0 .0,
            nonce: signed.tx().nonce,
            success: true,
            fee: ACTUAL_FEE.to_string(),
        }];
        apply_evidence(&db, &b, None, Some(&e)).await?;
        assert_eq!(
            apply_evidence(&db, &b, None, Some(&e)).await?,
            Applied::AlreadyCommitted
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sweep_orders WHERE status='completed'"
            )
            .fetch_one(&db)
            .await?,
            i64::from(step) + 1
        );
    }
    assert!(!signer.prepare_bsc_sweep(&server.rpc, cap).await?);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM internal_receipts")
            .fetch_one(&db)
            .await?,
        if shortfall == 0 { 3 } else { 2 }
    );
    let balances:Vec<String>=sqlx::query_scalar("SELECT sum(delta)::text FROM journal_entries WHERE account='user:alice:available' GROUP BY asset ORDER BY asset").fetch_all(&db).await?;
    assert_eq!(
        balances,
        [
            (BNB + ACTUAL_FEE - reserve - shortfall).to_string(),
            TOKEN.to_string()
        ]
    );
    Ok(())
}
