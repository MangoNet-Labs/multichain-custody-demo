use super::*;
use mgo_types::transaction::{Argument, Command as PtCommand, TransactionKind};
use std::{collections::BTreeMap, process::Output};

type Id = [u8; 32];
type Reserved = (Vec<u8>, i64, bool);
pub(super) struct Inventory(BTreeMap<Id, (bool, u64, u64)>);

impl Inventory {
    fn new(token: bool) -> Self {
        let mut coins = BTreeMap::new();
        for i in 1..=10_000u32 {
            let mut id = [0; 32];
            id[28..].copy_from_slice(&i.to_be_bytes());
            coins.insert(id, (token, 1, 3_500_000));
        }
        if token {
            coins.insert([255; 32], (false, 1, 100_000_000));
        }
        Self(coins)
    }

    pub(super) fn page(&self, arg: &Value) -> Value {
        let is_token = arg[1] == token();
        let mut matching = self.0.iter().filter(|(_, (t, _, _))| *t == is_token);
        let data:Vec<_> = matching.by_ref().take(arg[3].as_u64().unwrap() as usize)
            .map(|(id,(_,v,b))| json!({"coinObjectId":ObjectID::new(*id),"version":v.to_string(),
                "digest":ObjectDigest::new([*v as u8;32]),"balance":b.to_string(),"coinType":arg[1]})).collect();
        json!({"hasNextPage":matching.next().is_some(),"nextCursor":data.last().map(|r| &r["coinObjectId"]),"data":data})
    }

    pub(super) fn balance(&self, token: bool) -> u128 {
        self.0
            .values()
            .filter(|(t, _, _)| *t == token)
            .map(|(_, _, b)| u128::from(*b))
            .sum()
    }

    fn confirm_merge(&mut self, inputs: &[Reserved]) {
        for gas in [true, false] {
            let mut group = inputs.iter().filter(|r| r.2 == gas);
            let Some(first) = group.next() else { continue };
            let id: Id = first.0.as_slice().try_into().unwrap();
            let (token, v, mut total) = self.0.remove(&id).unwrap();
            assert_eq!(v, first.1 as u64);
            for (obj, v, _) in group {
                let (t, old, b) = self
                    .0
                    .remove(<&Id>::try_from(obj.as_slice()).unwrap())
                    .unwrap();
                assert_eq!((token, *v as u64), (t, old));
                total += b;
            }
            self.0
                .insert(id, (token, v + 1, total - if gas { 750 } else { 0 }));
        }
    }
}

pub(super) async fn exercise(
    db: &PgPool,
    fixture: &Arc<Mutex<Fixture>>,
    is_token: bool,
    fail: bool,
    signer: &str,
    worker: &str,
    run: impl Fn(&str, &[&str]) -> tokio::task::JoinHandle<std::io::Result<Output>>,
    stamp: Option<Id>,
) -> Result<()> {
    let asset = if is_token {
        AssetId::MangoTestnetCoin(token())
    } else {
        AssetId::MangoTestnetMgo
    };
    let hot = fixture.lock().unwrap().sender.unwrap().to_inner();
    let prepare = || async {
        let out = run(signer, &["mango-prepare-once"]).await??;
        ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok::<_, anyhow::Error>(String::from_utf8(out.stdout)?.contains("prepared=true"))
    };
    let mut underfunded = Inventory::new(is_token);
    for (index, (token, _, balance)) in underfunded.0.values_mut().enumerate() {
        if *token == is_token {
            *balance = if index < 50 { 300_000 } else { 1 };
        }
    }
    fixture.lock().unwrap().inventory = Some(underfunded);
    assert!(!prepare().await?);
    assert_eq!(fixture.lock().unwrap().balance_reads, 1);
    // Exactly 50 objects, but no next page: wait without even querying the total.
    {
        let mut f = fixture.lock().unwrap();
        let inv = f.inventory.as_mut().unwrap();
        inv.0.retain(|id, (t, _, _)| {
            *t != is_token || u32::from_be_bytes(id[28..].try_into().unwrap()) <= 50
        });
    }
    sqlx::query("UPDATE withdrawals SET next_prepare=now() WHERE request_id='test'")
        .execute(db)
        .await?;
    assert!(!prepare().await?);
    assert_eq!(fixture.lock().unwrap().balance_reads, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mango_payouts")
            .fetch_one(db)
            .await?,
        0
    );
    fixture.lock().unwrap().inventory = Some(Inventory::new(is_token));
    sqlx::query("UPDATE withdrawals SET next_prepare=now() WHERE request_id='test'")
        .execute(db)
        .await?;
    // Each invocation starts a fresh signer process; only PostgreSQL remembers the active batch.
    for round in 0..6u8 {
        let reads = {
            let f = fixture.lock().unwrap();
            (f.hot_page_reads, f.balance_reads)
        };
        if round == 0 {
            // A checkpoint insert holds a foreign-key reference to hot. It must
            // not force the signer to skip an otherwise eligible withdrawal.
            let mut scan = db.begin().await?;
            sqlx::query("INSERT INTO opening_balances(network,address,asset,amount) VALUES('mango-testnet',$1,'test',1)")
                .bind(&hot[..]).execute(&mut *scan).await?;
            assert!(prepare().await?);
            scan.rollback().await?;
        } else {
            assert!(prepare().await?);
        }
        if round == 0 {
            withdrawals::submit(
                db,
                withdrawals::Request {
                    user: "alice",
                    request_id: "second",
                    asset: asset.clone(),
                    recipient: &[89; 32],
                    amount: "0.1",
                },
            )
            .await?;
        }
        assert!(!prepare().await?);
        let (id,digest,bytes,merge,parent):(String, Vec<u8>,Vec<u8>,bool,String)=sqlx::query_as(
            "SELECT payout_id::text,digest::bytea,tx_bytes,merge_for IS NOT NULL,w.request_id FROM mango_payouts p
             JOIN withdrawals w ON w.id=coalesce(p.merge_for,p.withdrawal_id) WHERE confirmed_height IS NULL")
            .fetch_one(db).await?;
        assert_eq!(parent, "test");
        assert_eq!(merge, round < 5);
        {
            let f = fixture.lock().unwrap();
            assert_eq!(f.hot_page_reads - reads.0, if is_token { 2 } else { 1 });
            assert_eq!(f.balance_reads - reads.1, usize::from(merge));
        }
        let hash: Id = digest.try_into().unwrap();
        let inputs:Vec<Reserved>=sqlx::query_as("SELECT object_id::bytea,version,is_gas FROM mango_payout_inputs WHERE payout_id=$1::text::uuid ORDER BY input_index")
            .bind(&id).fetch_all(db).await?;
        assert!(inputs.len() <= if is_token { 51 } else { 50 });
        if merge {
            assert_eq!(inputs.len(), if is_token { 51 } else { 50 });
            let tx: TransactionData = bcs::from_bytes(&bytes)?;
            let TransactionKind::ProgrammableTransaction(pt) = tx.kind() else {
                panic!("not a PTB")
            };
            assert_eq!(pt.commands.len(), 1);
            match &pt.commands[0] {
                PtCommand::MergeCoins(_, rest) => {
                    assert!(is_token && !rest.is_empty());
                }
                PtCommand::TransferObjects(args, _) => {
                    assert!(!is_token);
                    assert_eq!(args, &[Argument::GasCoin]);
                    assert!(tx.gas_data().payment.len() > 1);
                }
                _ => panic!("unexpected consolidation command"),
            }
        }
        if round == 0 {
            fixture.lock().unwrap().lost = true;
            assert!(!run(worker, &["--broadcast-once"]).await??.status.success());
            assert!(!prepare().await?);
            sqlx::query("UPDATE mango_payouts SET next_send=now() WHERE payout_id=$1::text::uuid")
                .bind(&id)
                .execute(db)
                .await?;
        }
        let out = run(worker, &["--broadcast-once"]).await??;
        ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!prepare().await?);
        if round == 0 {
            let f = fixture.lock().unwrap();
            assert_eq!(f.sends[0], f.sends[1]);
        }
        let b = FinalizedBlock {
            openings: vec![],
            network: MangoTestnet,
            height: i64::from(12 + round),
            digest: [12 + round; 32],
            parent: [11 + round; 32],
            receipts: if merge {
                vec![]
            } else {
                vec![Receipt {
                    tx: hash,
                    sender: None,
                    recipient: vec![88; 32],
                    asset: if is_token {
                        token().into_bytes()
                    } else {
                        vec![]
                    },
                    amount: "1000000000".into(),
                    log: None,
                }]
            },
        };
        let e = Execution {
            hash,
            sender: hot,
            payer: hot,
            success: !fail,
            net_gas: 750,
            owners: vec![],
            consumed: inputs
                .iter()
                .map(|(i, v, _)| (i.as_slice().try_into().unwrap(), *v as u64))
                .collect(),
        };
        let changes = Default::default();
        custody_store::apply_mango_with_rules(db, &b, std::slice::from_ref(&e), &changes, stamp)
            .await?;
        assert_eq!(
            custody_store::apply_mango_with_rules(db, &b, &[e], &changes, stamp).await?,
            Applied::AlreadyCommitted
        );
        let (first,second,frozen,gas):(String,String,String,String)=sqlx::query_as(
            "SELECT (SELECT status FROM withdrawals WHERE request_id='test'),(SELECT status FROM withdrawals WHERE request_id='second'),
             sum(delta) FILTER(WHERE account='user:alice:frozen')::text,sum(delta) FILTER(WHERE account='platform:gas')::text FROM journal_entries")
            .fetch_one(db).await?;
        assert_eq!(second, "queued");
        assert_eq!(gas, (-750 * (1 + i64::from(round))).to_string());
        assert_eq!(frozen, if merge { "1100000000" } else { "100000000" });
        if fail {
            assert_eq!(first, "review");
            assert!(!prepare().await?);
            return Ok(());
        }
        assert_eq!(first, if merge { "consolidating" } else { "completed" });
        if merge {
            fixture
                .lock()
                .unwrap()
                .inventory
                .as_mut()
                .unwrap()
                .confirm_merge(&inputs);
        }
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM mango_payouts WHERE settlement_journal IS NOT NULL"
        )
        .fetch_one(db)
        .await?,
        1
    );
    Ok(())
}
