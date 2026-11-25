use crate::Signer;
use alloy_consensus::TxLegacy;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, U256, U64};
use alloy_sol_types::{sol, SolCall};
use anyhow::{ensure, Context, Result};
use evm_reader::rpc::BscRpc;
use serde_json::json;
use uuid::Uuid;

sol! {
    function transfer(address to, uint256 amount) returns (bool);
    function balanceOf(address account) view returns (uint256);
}
struct Payment {
    sender: Address,
    recipient: Address,
    token: Option<Address>,
    amount: U256,
}
enum Quote {
    Ready(TxLegacy),
    Wait(&'static str),
}

fn quote(node: &BscRpc, p: &Payment, max_fee: U256) -> Result<Quote> {
    node.verify()?;
    let nonce: U64 = node.call("eth_getTransactionCount", json!([p.sender, "finalized"]))?;
    let pending: U64 = node.call("eth_getTransactionCount", json!([p.sender, "pending"]))?;
    if nonce != pending {
        return Ok(Quote::Wait(
            "hot wallet nonce has an unfinalized transaction",
        ));
    }
    let funds: U256 = node.call("eth_getBalance", json!([p.sender, "pending"]))?;
    let (to, value, input) = if let Some(token) = p.token {
        let data = Bytes::from(balanceOfCall { account: p.sender }.abi_encode());
        let balance: Bytes = node.call("eth_call", json!([{"to":token,"data":data},"pending"]))?;
        ensure!(balance.len() == 32, "invalid ERC-20 balance result");
        if U256::from_be_slice(&balance) < p.amount {
            return Ok(Quote::Wait("waiting for hot wallet token liquidity"));
        }
        (
            token,
            U256::ZERO,
            Bytes::from(
                transferCall {
                    to: p.recipient,
                    amount: p.amount,
                }
                .abi_encode(),
            ),
        )
    } else {
        (p.recipient, p.amount, Bytes::new())
    };
    if funds <= value {
        return Ok(Quote::Wait("waiting for hot wallet BNB liquidity or gas"));
    }
    let call = json!({"from":p.sender,"to":to,"value":value,"data":input});
    let estimate: U64 = node.call("eth_estimateGas", json!([call]))?;
    let gas_limit = estimate
        .to::<u64>()
        .checked_mul(120)
        .context("gas estimate overflow")?
        .div_ceil(100);
    let price: U256 = node.call("eth_gasPrice", json!([]))?;
    let gas_price = price
        .checked_mul(U256::from(110))
        .context("gas price overflow")?
        .div_ceil(U256::from(100));
    let fee = gas_price
        .checked_mul(U256::from(gas_limit))
        .context("fee overflow")?;
    if fee.is_zero() || fee > max_fee {
        return Ok(Quote::Wait("estimated fee outside signer policy"));
    }
    if funds - value < fee {
        return Ok(Quote::Wait("waiting for hot wallet gas"));
    }
    Ok(Quote::Ready(TxLegacy {
        chain_id: Some(97),
        nonce: nonce.to(),
        gas_price: u128::try_from(gas_price)?,
        gas_limit,
        to: TxKind::Call(to),
        value,
        input,
    }))
}

impl Signer {
    /// Reads the payment intent from its frozen order; callers cannot supply
    /// arbitrary calldata, nonce, amount, recipient or key reference.
    pub async fn prepare_bsc(&self, node: &BscRpc, max_fee: U256) -> Result<bool> {
        let mut tx = self.db.begin().await?;
        let hot:Option<(String,Vec<u8>)>=sqlx::query_as("SELECT key_ref,address FROM wallets WHERE network='bsc-testnet' AND role='hot' FOR UPDATE SKIP LOCKED")
            .fetch_optional(&mut *tx).await?;
        let Some((key, sender)) = hot else {
            return Ok(false);
        };
        let paused: bool = sqlx::query_scalar("SELECT paused FROM payment_control WHERE singleton")
            .fetch_one(&mut *tx)
            .await?;
        let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM evm_payouts p JOIN withdrawals w ON w.id=p.withdrawal_id WHERE p.sender=$1 AND w.status IN ('signed','submitted','review'))")
            .bind(&sender).fetch_one(&mut *tx).await?;
        if paused || busy {
            return Ok(false);
        }
        #[derive(sqlx::FromRow)]
        struct Intent {
            id: String,
            recipient: Vec<u8>,
            asset: Vec<u8>,
            amount: String,
            valid: bool,
        }
        let order:Option<Intent>=sqlx::query_as("SELECT w.id::text,w.recipient,w.asset,w.amount::text,
            (j.network=w.network AND j.asset=w.asset AND j.amount=w.amount AND j.debit_account='user:' || w.user_id || ':available' AND j.credit_account='user:' || w.user_id || ':frozen'
            AND NOT EXISTS(SELECT 1 FROM wallets x WHERE x.network=w.network AND x.address=w.recipient)) AS valid
            FROM withdrawals w JOIN journal j ON j.id=w.freeze_id WHERE w.network='bsc-testnet' AND w.status='queued' AND w.next_prepare<=now() ORDER BY w.created_at,w.id LIMIT 1 FOR UPDATE OF w SKIP LOCKED")
            .fetch_optional(&mut *tx).await?;
        let Some(o) = order else { return Ok(false) };
        ensure!(
            o.valid,
            "order does not match its freeze or recipient became internal"
        );
        let token = if o.asset.is_empty() {
            None
        } else {
            Some(Address::try_from(o.asset.as_slice())?)
        };
        let sender = Address::try_from(sender.as_slice())?;
        let p = Payment {
            sender,
            recipient: Address::try_from(o.recipient.as_slice())?,
            token,
            amount: o.amount.parse()?,
        };
        ensure!(
            !p.amount.is_zero() && !p.recipient.is_zero(),
            "invalid payout"
        );
        let rpc = node.clone();
        let q = tokio::task::spawn_blocking(move || quote(&rpc, &p, max_fee)).await?;
        let quoted = match q {
            Ok(Quote::Ready(q)) => q,
            other => {
                let reason = match other {
                    Ok(Quote::Wait(s)) => s,
                    Err(_) => "RPC preflight failed; nothing was signed",
                    _ => unreachable!(),
                };
                sqlx::query("UPDATE withdrawals SET detail=$2,next_prepare=now()+interval '30 seconds' WHERE id=$1::text::uuid")
                    .bind(&o.id).bind(reason).execute(&mut *tx).await?;
                tx.commit().await?;
                return match other {
                    Err(e) => Err(e),
                    _ => Ok(false),
                };
            }
        };
        let paused: bool =
            sqlx::query_scalar("SELECT paused FROM payment_control WHERE singleton FOR SHARE")
                .fetch_one(&mut *tx)
                .await?;
        if paused {
            return Ok(false);
        }
        let nonce = i64::try_from(quoted.nonce)?;
        let fee = U256::from(quoted.gas_price) * U256::from(quoted.gas_limit);
        let vault = self.vault.clone();
        let raw = tokio::task::spawn_blocking(move || {
            vault.sign_hot_bsc(Uuid::parse_str(&key)?, sender, quoted)
        })
        .await??;
        let hash = keccak256(&raw);
        sqlx::query("INSERT INTO evm_payouts(withdrawal_id,sender,nonce,digest,signed_tx,max_fee) VALUES($1::text::uuid,$2,$3,$4,$5,$6::text::positive_raw)")
            .bind(&o.id).bind(sender.as_slice()).bind(nonce).bind(hash.as_slice()).bind(raw).bind(fee.to_string()).execute(&mut *tx).await?;
        sqlx::query("UPDATE withdrawals SET status='signed',detail=NULL WHERE id=$1::text::uuid")
            .bind(&o.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
}
