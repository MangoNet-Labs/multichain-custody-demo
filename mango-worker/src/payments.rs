use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use custody_store::{
    mango::{Input, Quote},
    PgPool,
};
use mgo_types::{
    base_types::{MgoAddress, ObjectID, ObjectRef, SequenceNumber},
    crypto::{default_hash, MgoSignature, Signature, SignatureScheme, ToFromBytes},
    digests::{ObjectDigest, TransactionDigest},
    parse_mgo_type_tag,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{Command, ObjectArg, TransactionData, TransactionDataAPI},
    Identifier, MGO_FRAMEWORK_PACKAGE_ID,
};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};
use shared_crypto::intent::{Intent, IntentMessage};
use std::{collections::HashSet, env, time::Duration};

pub struct Node {
    url: String,
    client: reqwest::Client,
}
impl Node {
    fn from_env() -> Result<Self> {
        Ok(Self {
            url: env::var("MANGO_RPC_URL")
                .unwrap_or_else(|_| "https://fullnode.testnet2.mangonetwork.io".into()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
        })
    }
    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let mut response = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await?
            .error_for_status()?;
        let mut body = Vec::new();
        while let Some(c) = response.chunk().await? {
            ensure!(
                c.len() <= 4 * 1024 * 1024 - body.len(),
                "Mango RPC response too large"
            );
            body.extend_from_slice(&c);
        }
        let r: Value = serde_json::from_slice(&body)?;
        ensure!(
            r["jsonrpc"] == "2.0" && r["id"] == 1 && r.get("error").is_none(),
            "{method}: unsuccessful Mango RPC reply"
        );
        Ok(serde_json::from_value(
            r.get("result").context("missing Mango RPC result")?.clone(),
        )?)
    }
    async fn verify(&self) -> Result<()> {
        let g: Value = self.call("mgo_getCheckpoint", json!(["0"])).await?;
        ensure!(
            g["digest"] == mango_worker::TESTNET_GENESIS && g["sequenceNumber"] == "0",
            "not Mango Testnet"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Coin {
    coin_object_id: ObjectID,
    digest: ObjectDigest,
    version: String,
    balance: String,
    coin_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    data: Vec<Coin>,
    has_next_page: bool,
}
const HOT_PAGE_SIZE: usize = 50;
const MAX_INPUTS: usize = 128;
fn build(
    sender: MgoAddress,
    recipient: MgoAddress,
    amount: u64,
    budget: u64,
    price: u64,
    coins: &[ObjectRef],
) -> Result<Quote> {
    ensure!(
        amount > 0
            && budget > 0
            && price > 0
            && sender != recipient
            && recipient != MgoAddress::ZERO,
        "invalid payout fields"
    );
    let (gas, rest) = coins.split_first().context("missing gas coin")?;
    let tx = TransactionData::new_pay_mgo(
        sender,
        rest.to_vec(),
        vec![recipient],
        vec![amount],
        *gas,
        budget,
        price,
    )?;
    encoded(&tx, coins, amount, false)
}
fn encoded(tx: &TransactionData, coins: &[ObjectRef], amount: u64, merge: bool) -> Result<Quote> {
    let intent = Keccak256::digest(bcs::to_bytes(&IntentMessage::new(
        Intent::mgo_transaction(),
        tx,
    ))?)
    .into();
    Ok(Quote::Ready {
        merge,
        amount,
        gas_count: tx.gas_data().payment.len(),
        bytes: bcs::to_bytes(tx)?,
        digest: default_hash(tx),
        intent,
        inputs: coins
            .iter()
            .map(|(id, v, d)| Input {
                id: id.into_bytes(),
                version: v.value(),
                digest: d.into_inner(),
            })
            .collect(),
    })
}

mod sponsored;
pub use sponsored::sweep_quote_command;

const MGO: &str = "0x2::mgo::MGO";

fn build_token(
    sender: MgoAddress,
    recipient: MgoAddress,
    amount: u64,
    budget: u64,
    price: u64,
    asset: &str,
    coins: &[ObjectRef],
    gas: &[ObjectRef],
) -> Result<Quote> {
    let tag = parse_mgo_type_tag(asset)?;
    ensure!(
        matches!(&tag, mgo_types::TypeTag::Struct(_))
            && !mgo_types::gas_coin::GAS::is_gas_type(&tag)
            && tag.to_canonical_string(true) == asset,
        "invalid registered Mango token type"
    );
    ensure!(
        sender != recipient
            && recipient != MgoAddress::ZERO
            && amount > 0
            && budget > 0
            && price > 0,
        "invalid token withdrawal fields"
    );
    ensure!(
        !coins.is_empty() && !gas.is_empty() && coins.len() + gas.len() <= MAX_INPUTS,
        "invalid token withdrawal inputs"
    );
    let mut seen = HashSet::new();
    ensure!(
        coins.iter().chain(gas).all(|r| seen.insert(r.0)),
        "overlapping Mango payment inputs"
    );
    let mut pt = ProgrammableTransactionBuilder::new();
    let mut args = coins
        .iter()
        .map(|r| pt.obj(ObjectArg::ImmOrOwnedObject(*r)))
        .collect::<Result<Vec<_>>>()?;
    let first = args.remove(0);
    if !args.is_empty() {
        pt.command(Command::MergeCoins(first, args));
    }
    let qty = pt.pure(amount)?;
    let dest = pt.pure(recipient)?;
    pt.programmable_move_call(
        MGO_FRAMEWORK_PACKAGE_ID,
        Identifier::new("pay")?,
        Identifier::new("split_and_transfer")?,
        vec![tag],
        vec![first, qty, dest],
    );
    let tx = TransactionData::new_programmable(sender, gas.to_vec(), pt.finish(), budget, price);
    encoded(&tx, &[coins, gas].concat(), amount, false)
}

fn build_merge(
    sender: MgoAddress,
    amount: u64,
    budget: u64,
    price: u64,
    coins: &[ObjectRef],
    gas: &[ObjectRef],
) -> Result<Quote> {
    ensure!(
        budget > 0
            && price > 0
            && amount > 0
            && !gas.is_empty()
            && coins.len() + gas.len() <= MAX_INPUTS,
        "invalid merge inputs or budget"
    );
    let mut ids = HashSet::new();
    ensure!(
        coins.iter().chain(gas).all(|r| ids.insert(r.0)),
        "duplicate merge input"
    );
    let tx = if coins.is_empty() {
        ensure!(gas.len() >= 2, "native merge needs two coins");
        // The protocol combines the gas payments; the surviving coin stays with hot.
        TransactionData::new_pay_all_mgo(sender, gas[1..].to_vec(), sender, gas[0], budget, price)
    } else {
        ensure!(coins.len() >= 2, "token merge needs two coins");
        let mut pt = ProgrammableTransactionBuilder::new();
        let first = pt.obj(ObjectArg::ImmOrOwnedObject(coins[0]))?;
        let rest = coins[1..]
            .iter()
            .map(|c| pt.obj(ObjectArg::ImmOrOwnedObject(*c)))
            .collect::<Result<Vec<_>>>()?;
        pt.command(Command::MergeCoins(first, rest));
        TransactionData::new_programmable(sender, gas.to_vec(), pt.finish(), budget, price)
    };
    encoded(&tx, &[coins, gas].concat(), amount, true)
}

fn verify_gain(dry: &Value, recipient: MgoAddress, asset: &str, amount: u64) -> Result<()> {
    let expected = parse_mgo_type_tag(if asset.is_empty() { MGO } else { asset })?;
    let mut received = None;
    for r in dry["balanceChanges"]
        .as_array()
        .context("missing dry-run balance changes")?
    {
        let Some(addr) = r["owner"]["AddressOwner"].as_str() else {
            continue;
        };
        if addr.parse::<MgoAddress>()? != recipient {
            continue;
        }
        ensure!(
            parse_mgo_type_tag(r["coinType"].as_str().context("missing coin type")?)? == expected
                && received.is_none(),
            "unexpected recipient asset or duplicate change"
        );
        received = Some(
            r["amount"]
                .as_str()
                .context("invalid dry-run amount")?
                .parse::<u64>()?,
        );
    }
    ensure!(
        received.unwrap_or_default() == amount,
        "dry-run recipient gain differs from order"
    );
    Ok(())
}

enum Selection {
    Enough(Vec<ObjectRef>),
    Merge(Vec<ObjectRef>),
    Short,
}

async fn first_page_inputs(
    node: &Node,
    sender: MgoAddress,
    asset: &str,
    required: u64,
    merge_gas: u64,
) -> Result<Selection> {
    let tag = parse_mgo_type_tag(asset)?;
    let page: Page = node
        .call("mgox_getCoins", json!([sender, asset, null, HOT_PAGE_SIZE]))
        .await?;
    ensure!(
        page.data.len() <= HOT_PAGE_SIZE,
        "oversized Mango coin page"
    );
    let mut best = Vec::with_capacity(page.data.len());
    let mut seen = HashSet::new();
    for c in page.data {
        ensure!(
            parse_mgo_type_tag(&c.coin_type)? == tag && seen.insert(c.coin_object_id),
            "unexpected Mango coin type or duplicate coin"
        );
        best.push((
            c.balance.parse::<u64>()?,
            (
                c.coin_object_id,
                SequenceNumber::from_u64(c.version.parse()?),
                c.digest,
            ),
        ));
    }
    best.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1 .0.cmp(&b.1 .0)));
    let mut total = 0u128;
    if let Some(last) = best.iter().position(|(b, _)| {
        total += u128::from(*b);
        total >= u128::from(required)
    }) {
        return Ok(Selection::Enough(
            best.into_iter().take(last + 1).map(|(_, r)| r).collect(),
        ));
    }
    if !page.has_next_page || best.len() < 2 || total < u128::from(merge_gas) {
        return Ok(Selection::Short);
    }
    // Check solvency in one call; never traverse the remaining coin objects.
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Balance {
        coin_type: String,
        total_balance: String,
    }
    let balance: Balance = node.call("mgox_getBalance", json!([sender, asset])).await?;
    ensure!(
        parse_mgo_type_tag(&balance.coin_type)? == tag,
        "wrong Mango balance type"
    );
    if balance.total_balance.parse::<u128>()? < u128::from(required) {
        return Ok(Selection::Short);
    }
    Ok(Selection::Merge(best.into_iter().map(|(_, r)| r).collect()))
}

async fn quote(
    node: &Node,
    sender: MgoAddress,
    recipient: MgoAddress,
    amount: u64,
    budget: u64,
    asset: &str,
) -> Result<Quote> {
    node.verify().await?;
    let native = asset.is_empty();
    let required = if native {
        amount
            .checked_add(budget)
            .context("amount plus gas exceeds native range")?
    } else {
        budget
    };
    let (gas, mut merge) = match first_page_inputs(node, sender, MGO, required, budget).await? {
        Selection::Enough(gas) => (gas, false),
        Selection::Merge(gas) => (gas, true),
        Selection::Short => {
            return Ok(Quote::Waiting(
                "waiting for spendable hot wallet MGO for payment or gas".into(),
            ))
        }
    };
    let coins = if native {
        vec![]
    } else {
        match first_page_inputs(node, sender, asset, amount, 0).await? {
            Selection::Enough(coins) => coins,
            Selection::Merge(coins) => {
                merge = true;
                coins
            }
            Selection::Short => {
                return Ok(Quote::Waiting(
                    "waiting for spendable hot wallet token balance".into(),
                ))
            }
        }
    };
    let price: String = node.call("mgox_getReferenceGasPrice", json!([])).await?;
    let price = price.parse()?;
    let q = if merge {
        build_merge(sender, amount, budget, price, &coins, &gas)?
    } else if native {
        build(sender, recipient, amount, budget, price, &gas)?
    } else {
        build_token(
            sender, recipient, amount, budget, price, asset, &coins, &gas,
        )?
    };
    if let Quote::Ready { bytes, merge, .. } = &q {
        let dry: Value = node
            .call(
                "mgo_dryRunTransactionBlock",
                json!([STANDARD.encode(bytes)]),
            )
            .await?;
        if dry["effects"]["status"]["status"] != "success" {
            return Ok(Quote::Waiting(
                "Mango dry-run failed; check gas budget and coin liquidity".into(),
            ));
        }
        if !native && !merge {
            verify_gain(&dry, recipient, asset, amount)?;
        }
    }
    Ok(q)
}
pub async fn quote_command(args: &[String]) -> Result<()> {
    ensure!(
        (5..=6).contains(&args.len()),
        "usage: --quote-withdrawal sender recipient amount_raw budget_raw [canonical_coin_type]"
    );
    let q = quote(
        &Node::from_env()?,
        args[1].parse()?,
        args[2].parse()?,
        args[3].parse()?,
        args[4].parse()?,
        args.get(5).map(String::as_str).unwrap_or(""),
    )
    .await?;
    println!("{}", serde_json::to_string(&q)?);
    Ok(())
}

pub async fn broadcast(db: &PgPool, node: &Node) -> Result<bool> {
    let mut lock = db.begin().await?;
    if sqlx::query_scalar::<_, bool>("SELECT paused FROM payment_control WHERE singleton FOR SHARE")
        .fetch_one(&mut *lock)
        .await?
    {
        return Ok(false);
    }
    #[derive(sqlx::FromRow)]
    struct Signed {
        order_id: String,
        merge: bool,
        asset: String,
        id: String,
        digest: Vec<u8>,
        tx_bytes: Vec<u8>,
        signature: Vec<u8>,
        sender: Vec<u8>,
        recipient: Vec<u8>,
        amount: String,
        budget: String,
        sponsor: Option<Vec<u8>>,
        sponsor_signature: Option<Vec<u8>>,
    }
    let row:Option<Signed>=sqlx::query_as("SELECT p.payout_id::text AS id,coalesce(w.id,s.id)::text AS order_id,p.merge_for IS NOT NULL AS merge,p.digest::bytea,p.tx_bytes,p.signature,p.sender,p.sponsor,p.sponsor_signature,convert_from(coalesce(s.asset,w.asset),'UTF8') AS asset,coalesce(s.recipient,w.recipient) AS recipient,coalesce(s.amount,w.amount)::text AS amount,p.budget::text FROM mango_payouts p LEFT JOIN withdrawals w ON w.id=coalesce(p.withdrawal_id,p.merge_for) LEFT JOIN sweep_orders s ON s.id=p.sweep_id
        WHERE coalesce(s.status,w.status) IN ('signed','submitted','consolidating') AND p.confirmed_height IS NULL AND p.next_send<=now() ORDER BY p.next_send,p.payout_id LIMIT 1 FOR UPDATE OF p SKIP LOCKED")
        .fetch_optional(&mut *lock).await?;
    let Some(Signed {
        order_id,
        merge,
        asset,
        id,
        digest: hash,
        tx_bytes: bytes,
        signature: sig,
        sender,
        recipient,
        amount,
        budget,
        sponsor,
        sponsor_signature,
    }) = row
    else {
        return Ok(false);
    };
    let tx: TransactionData = bcs::from_bytes(&bytes)?;
    ensure!(
        default_hash(&tx).as_slice() == hash,
        "stored Mango transaction digest mismatch"
    );
    let sender = MgoAddress::from_bytes(&sender)?;
    let order = if merge || sponsor.is_some() || !asset.is_empty() {
        let rows:Vec<(Vec<u8>,i64,Vec<u8>,bool)>=sqlx::query_as("SELECT object_id::bytea,version,object_digest::bytea,is_gas FROM mango_payout_inputs WHERE payout_id=$1::text::uuid AND input_index IS NOT NULL AND reserved ORDER BY input_index")
            .bind(&id).fetch_all(&mut *lock).await?;
        let mut coins = vec![];
        let mut gas = vec![];
        for (id, v, d, is_gas) in rows {
            let i = Input {
                id: id.as_slice().try_into()?,
                version: v.try_into()?,
                digest: d.as_slice().try_into()?,
            };
            if is_gas {
                gas.push(i)
            } else {
                coins.push(i)
            }
        }
        if merge {
            ensure!(
                sponsor.is_none() && (coins.is_empty() || !asset.is_empty()),
                "invalid hot-wallet merge"
            );
            build_merge(
                sender,
                amount.parse()?,
                budget.parse()?,
                tx.gas_price(),
                &coins.iter().map(sponsored::as_ref).collect::<Vec<_>>(),
                &gas.iter().map(sponsored::as_ref).collect::<Vec<_>>(),
            )?
        } else if let Some(gas_owner) = &sponsor {
            sponsored::build(
                &custody_store::mango::SponsoredSweep {
                    asset,
                    sender: sender.to_inner(),
                    recipient: recipient.as_slice().try_into()?,
                    sponsor: gas_owner.as_slice().try_into()?,
                    budget: budget.parse()?,
                    max_amount: amount.parse()?,
                    gas,
                },
                &coins,
                amount.parse()?,
                tx.gas_price(),
            )?
        } else {
            build_token(
                sender,
                MgoAddress::from_bytes(&recipient)?,
                amount.parse()?,
                budget.parse()?,
                tx.gas_price(),
                &asset,
                &coins.iter().map(sponsored::as_ref).collect::<Vec<_>>(),
                &gas.iter().map(sponsored::as_ref).collect::<Vec<_>>(),
            )?
        }
    } else {
        build(
            sender,
            MgoAddress::from_bytes(&recipient)?,
            amount.parse()?,
            budget.parse()?,
            tx.gas_price(),
            &tx.gas_data().payment,
        )?
    };
    let Quote::Ready {
        bytes: expected,
        inputs,
        ..
    } = order
    else {
        unreachable!()
    };
    ensure!(
        bytes == expected,
        "signed Mango transaction differs from registered order"
    );
    let stored:Vec<(Vec<u8>,i64,Vec<u8>)>=sqlx::query_as("SELECT object_id::bytea,version,object_digest::bytea FROM mango_payout_inputs WHERE payout_id=$1::text::uuid ORDER BY object_id")
        .bind(&id).fetch_all(&mut *lock).await?;
    let mut refs = inputs
        .into_iter()
        .map(|i| Ok((i.id.to_vec(), i64::try_from(i.version)?, i.digest.to_vec())))
        .collect::<Result<Vec<_>>>()?;
    refs.sort_unstable();
    ensure!(
        refs == stored,
        "Mango input reservations differ from signed payment"
    );
    Signature::from_bytes(&sig)?.verify_secure(
        &IntentMessage::new(Intent::mgo_transaction(), &tx),
        sender,
        SignatureScheme::ED25519,
    )?;
    let mut signatures = vec![STANDARD.encode(sig)];
    if let Some(gas_owner) = &sponsor {
        let sig = sponsor_signature.context("missing Mango sponsor signature")?;
        Signature::from_bytes(&sig)?.verify_secure(
            &IntentMessage::new(Intent::mgo_transaction(), &tx),
            MgoAddress::from_bytes(gas_owner)?,
            SignatureScheme::ED25519,
        )?;
        signatures.push(STANDARD.encode(sig));
    }
    sqlx::query("UPDATE mango_payouts SET send_count=send_count+1,next_send=now()+interval '30 seconds' WHERE payout_id=$1::text::uuid")
        .bind(&id).execute(&mut *lock).await?;
    lock.commit().await?;
    node.verify().await?;
    let resp: Result<Value> = node
        .call(
            "mgo_executeTransactionBlock",
            json!([STANDARD.encode(bytes), signatures, {}, "WaitForEffectsCert"]),
        )
        .await;
    let matched = resp
        .as_ref()
        .ok()
        .and_then(|r| r["digest"].as_str())
        .and_then(|s| s.parse::<TransactionDigest>().ok())
        .is_some_and(|d| d.inner().as_slice() == hash);
    let detail = if matched {
        "waiting for finalized Mango checkpoint"
    } else {
        "broadcast unconfirmed; retrying the same signed bytes"
    };
    let table = if sponsor.is_some() {
        "sweep_orders"
    } else {
        "withdrawals"
    };
    if !merge {
        sqlx::query(&format!("UPDATE {table} SET status=CASE WHEN $2 THEN 'submitted' ELSE status END,detail=$3 WHERE id=$1::text::uuid AND status IN ('signed','submitted')"))
            .bind(order_id).bind(matched).bind(detail).execute(db).await?;
    }
    resp?;
    ensure!(matched, "Mango broadcast digest mismatch");
    Ok(true)
}
pub async fn relay(once: bool) -> Result<()> {
    let db = PgPool::connect(&env::var("DATABASE_URL")?).await?;
    custody_store::MIGRATIONS.run(&db).await?;
    let node = Node::from_env()?;
    loop {
        let result = broadcast(&db, &node).await;
        if once {
            return result.map(|_| ());
        }
        if let Err(e) = result {
            eprintln!("Mango broadcast deferred: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
