use super::Signer;
use alloy_consensus::TxLegacy;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256, U64};
use alloy_sol_types::{sol, SolCall};
use anyhow::{ensure, Context, Result};
use custody_core::Network;
use custody_store::Cursor;
use evm_reader::rpc::BscRpc;
use serde_json::json;
use uuid::Uuid;

sol! {
    function balanceOf(address account) view returns(uint256);
    function transfer(address to,uint256 amount) returns(bool);
}
#[derive(Clone, Copy)]
struct Wallets {
    deposit: Address,
    hot: Address,
    gas: Address,
}
struct Intent {
    topup: bool,
    trigger: Vec<u8>,
    minimum: U256,
    observed: U256,
    amount: U256,
    tx: TxLegacy,
}
enum Decision {
    Dormant,
    Retry(&'static str),
    Ready(Box<Intent>),
}
fn balance(node: &BscRpc, token: Option<Address>, owner: Address, tag: &str) -> Result<U256> {
    if let Some(token) = token {
        let data = Bytes::from(balanceOfCall { account: owner }.abi_encode());
        let res: Bytes = node.call("eth_call", json!([{"to":token,"data":data},tag]))?;
        ensure!(res.len() == 32, "invalid token balance result");
        Ok(U256::from_be_slice(&res))
    } else {
        node.call("eth_getBalance", json!([owner, tag]))
    }
}
fn gas_limit(node: &BscRpc, from: Address, to: Address, value: U256, data: &Bytes) -> Result<u64> {
    // Do not require the deposit address to already have BNB to simulate a token transfer.
    let est: U64 = node.call(
        "eth_estimateGas",
        json!([{"from":from,"to":to,"data":data,"value":value,"gasPrice":"0x0"}]),
    )?;
    Ok(est
        .to::<u64>()
        .checked_mul(120)
        .context("gas estimate overflow")?
        .div_ceil(100))
}
fn plan(
    node: &BscRpc,
    w: Wallets,
    c: Cursor,
    rules: Vec<(Vec<u8>, String)>,
    cap: U256,
) -> Result<Decision> {
    if rules.is_empty() {
        return Ok(Decision::Dormant);
    }
    node.verify()?;
    let tag = format!("0x{:x}", c.height);
    #[derive(serde::Deserialize)]
    struct Header {
        number: U64,
        hash: B256,
    }
    let header: Header = node.call("eth_getBlockByNumber", json!([tag, false]))?;
    ensure!(
        header.hash.0 == c.digest && header.number == U64::from(u64::try_from(c.height)?),
        "sweep cursor does not match the node"
    );
    for (asset, min) in rules {
        let token = (!asset.is_empty())
            .then(|| Address::try_from(asset.as_slice()))
            .transpose()?;
        let minimum = min.parse::<U256>()?;
        let observed = balance(node, token, w.deposit, &tag)?;
        if observed < minimum {
            continue;
        }
        let native = if token.is_some() {
            balance(node, None, w.deposit, &tag)?
        } else {
            observed
        };
        let pending = balance(node, None, w.deposit, "pending")?;
        if pending < native
            || (token.is_some() && balance(node, token, w.deposit, "pending")? < observed)
        {
            return Ok(Decision::Retry("waiting for deposit spending to finalize"));
        }
        let price: U256 = node.call("eth_gasPrice", json!([]))?;
        let price = price
            .checked_mul(U256::from(110))
            .context("gas price overflow")?
            .div_ceil(U256::from(100));
        let price = u128::try_from(price)?;
        let fee = |limit: u64| U256::from(price) * U256::from(limit);
        let mut data = token
            .map(|_| {
                Bytes::from(
                    transferCall {
                        to: w.hot,
                        amount: observed,
                    }
                    .abi_encode(),
                )
            })
            .unwrap_or_default();
        let mut to = token.unwrap_or(w.hot);
        let mut limit = gas_limit(
            node,
            w.deposit,
            to,
            if token.is_some() {
                U256::ZERO
            } else {
                U256::from(1)
            },
            &data,
        )?;
        let reserve = fee(limit);
        if reserve.is_zero() || reserve > cap {
            return Ok(Decision::Retry("estimated fee outside signer policy"));
        }
        let topup = token.is_some() && native < reserve;
        let (from, amount, value) = if topup {
            if pending != native {
                return Ok(Decision::Retry("waiting for incoming gas to finalize"));
            }
            to = w.deposit;
            data = Bytes::new();
            let delta = reserve - native;
            let gas_funds =
                balance(node, None, w.gas, &tag)?.min(balance(node, None, w.gas, "pending")?);
            if gas_funds <= delta {
                return Ok(Decision::Retry("waiting for platform gas wallet funding"));
            }
            limit = gas_limit(node, w.gas, to, delta, &data)?;
            let charge = fee(limit);
            if charge.is_zero() || charge > cap || gas_funds - delta < charge {
                return Ok(Decision::Retry(
                    "insufficient gas wallet funds or fee outside policy",
                ));
            }
            (w.gas, delta, delta)
        } else if token.is_some() {
            (w.deposit, observed, U256::ZERO)
        } else {
            if native <= reserve {
                return Ok(Decision::Retry("native balance does not cover fee"));
            }
            (w.deposit, native - reserve, native - reserve)
        };
        let nonce: U64 = node.call("eth_getTransactionCount", json!([from, tag]))?;
        let mempool: U64 = node.call("eth_getTransactionCount", json!([from, "pending"]))?;
        if nonce != mempool {
            return Ok(Decision::Retry("waiting for sender nonce to finalize"));
        }
        return Ok(Decision::Ready(Box::new(Intent {
            topup,
            trigger: asset,
            minimum,
            observed,
            amount,
            tx: TxLegacy {
                chain_id: Some(97),
                nonce: nonce.to(),
                gas_price: price,
                gas_limit: limit,
                to: TxKind::Call(to),
                input: data,
                value,
            },
        })));
    }
    Ok(Decision::Dormant)
}

impl Signer {
    pub async fn prepare_bsc_sweep(&self, node: &BscRpc, cap: U256) -> Result<bool> {
        let lease = Uuid::new_v4().to_string();
        let row:Option<(Vec<u8>,i64)>=sqlx::query_as("WITH due AS (
            SELECT c.address FROM sweep_checks c WHERE c.network='bsc-testnet' AND next_check<=now()
            AND (lease_until IS NULL OR lease_until<now()) AND NOT (SELECT paused FROM payment_control WHERE singleton)
            AND NOT EXISTS(SELECT 1 FROM sweep_orders s WHERE s.network=c.network AND s.deposit_address=c.address AND s.status IN ('signed','submitted','review'))
            ORDER BY next_check,c.address LIMIT 1 FOR UPDATE OF c SKIP LOCKED)
            UPDATE sweep_checks c SET lease=$1::text::uuid,lease_until=now()+interval '2 minutes' FROM due
            WHERE c.network='bsc-testnet' AND c.address=due.address RETURNING c.address,c.revision")
            .bind(&lease).fetch_optional(&self.db).await?;
        let Some((address, revision)) = row else {
            return Ok(false);
        };
        let (hot,gas):(Vec<u8>,Vec<u8>)=sqlx::query_as("SELECT h.address,g.address FROM wallets h, wallets g WHERE h.network='bsc-testnet' AND g.network=h.network AND h.role='hot' AND g.role='gas'").fetch_one(&self.db).await?;
        let w = Wallets {
            deposit: Address::try_from(address.as_slice())?,
            hot: Address::try_from(hot.as_slice())?,
            gas: Address::try_from(gas.as_slice())?,
        };
        let rules=sqlx::query_as("SELECT asset,minimum::text FROM sweep_rules WHERE network='bsc-testnet' ORDER BY (octet_length(asset)=0),asset").fetch_all(&self.db).await?;
        let cursor = custody_store::cursor(&self.db, Network::BscTestnet)
            .await?
            .context("missing BSC scan cursor")?;
        let rpc = node.clone();
        let result = tokio::task::spawn_blocking(move || plan(&rpc, w, cursor, rules, cap)).await?;
        let mut tx = self.db.begin().await?;
        let current:Option<i64>=sqlx::query_scalar("SELECT revision FROM sweep_checks WHERE network='bsc-testnet' AND address=$1 AND lease=$2::text::uuid FOR UPDATE")
            .bind(&address).bind(&lease).fetch_optional(&mut *tx).await?;
        let Some(current) = current else {
            return Ok(false);
        };
        let paused: bool =
            sqlx::query_scalar("SELECT paused FROM payment_control WHERE singleton FOR SHARE")
                .fetch_one(&mut *tx)
                .await?;
        let outcome = if current != revision || paused {
            Ok(Decision::Retry("state changed; rechecking"))
        } else {
            result
        };
        let mut signed = false;
        let mut err = None;
        let (retry, detail) = match outcome {
            Err(e) => {
                err = Some(e);
                (true, "RPC preflight failed; nothing signed")
            }
            Ok(Decision::Dormant) => (false, "below configured thresholds"),
            Ok(Decision::Retry(reason)) => (true, reason),
            Ok(Decision::Ready(i)) => {
                let sender = if i.topup { w.gas } else { w.deposit };
                let recipient = if i.topup { w.deposit } else { w.hot };
                let (key,role,owner):(String,String,Option<String>)=sqlx::query_as("SELECT key_ref,role,owner_id FROM wallets WHERE network='bsc-testnet' AND address=$1 FOR UPDATE")
                    .bind(sender.as_slice()).fetch_one(&mut *tx).await?;
                ensure!(
                    role == if i.topup { "gas" } else { "deposit" },
                    "unexpected sweep signer role"
                );
                let nonce = i64::try_from(i.tx.nonce)?;
                let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sweep_orders WHERE network='bsc-testnet' AND sender=$1 AND status IN ('signed','submitted','review')) OR EXISTS(SELECT 1 FROM evm_payouts WHERE sender=$1 AND nonce=$2)")
                    .bind(sender.as_slice()).bind(nonce).fetch_one(&mut *tx).await?;
                if busy {
                    (true, "waiting for previous sender operation")
                } else {
                    let asset = if i.topup { &[][..] } else { &i.trigger };
                    let id:String=sqlx::query_scalar("INSERT INTO sweep_orders(network,deposit_address,sender,recipient,kind,trigger_asset,asset,amount,minimum,observed_balance,observed_height,observed_hash)
                        VALUES('bsc-testnet',$1,$2,$3,$4,$5,$6,$7::text::positive_raw,$8::text::positive_raw,$9::text::positive_raw,$10,$11) RETURNING id::text")
                        .bind(&address).bind(sender.as_slice()).bind(recipient.as_slice()).bind(if i.topup{"gas_topup"}else{"sweep"}).bind(&i.trigger).bind(asset)
                        .bind(i.amount.to_string()).bind(i.minimum.to_string()).bind(i.observed.to_string()).bind(cursor.height).bind(cursor.digest.as_slice()).fetch_one(&mut *tx).await?;
                    let max_fee = U256::from(i.tx.gas_limit) * U256::from(i.tx.gas_price);
                    let slot = owner.map(|u| format!("deposit:{u}")).unwrap_or(role);
                    let vault = self.vault.clone();
                    let raw = tokio::task::spawn_blocking(move || {
                        vault.sign_bsc(Uuid::parse_str(&key)?, &slot, sender, i.tx)
                    })
                    .await??;
                    sqlx::query("INSERT INTO evm_payouts(sweep_id,sender,nonce,digest,signed_tx,max_fee) VALUES($1::text::uuid,$2,$3,$4,$5,$6::text::positive_raw)")
                        .bind(id).bind(sender.as_slice()).bind(nonce).bind(keccak256(&raw).as_slice()).bind(raw).bind(max_fee.to_string()).execute(&mut *tx).await?;
                    signed = true;
                    (false, "waiting for finalized internal transfer")
                }
            }
        };
        sqlx::query("UPDATE sweep_checks SET lease=NULL,lease_until=NULL,detail=$2,next_check=CASE WHEN $3 THEN now()+interval '30 seconds' ELSE NULL END WHERE network='bsc-testnet' AND address=$1")
            .bind(address).bind(detail).bind(retry).execute(&mut *tx).await?;
        tx.commit().await?;
        if let Some(e) = err {
            return Err(e);
        }
        Ok(signed)
    }
}
