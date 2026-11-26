use crate::FinalizedBlock;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub id: [u8; 32],
    pub version: u64,
    pub digest: [u8; 32],
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SponsoredSweep {
    #[serde(default)]
    pub asset: String,
    pub sender: [u8; 32],
    pub recipient: [u8; 32],
    pub sponsor: [u8; 32],
    pub max_amount: u64,
    pub budget: u64,
    pub gas: Vec<Input>,
}
#[derive(Serialize, Deserialize)]
pub enum Quote {
    Waiting(String),
    Ready {
        merge: bool,
        amount: u64,
        gas_count: usize,
        bytes: Vec<u8>,
        digest: [u8; 32],
        intent: [u8; 32],
        inputs: Vec<Input>,
    },
}
#[derive(Debug, Serialize)]
pub struct Execution {
    pub hash: [u8; 32],
    pub sender: [u8; 32],
    pub payer: [u8; 32],
    pub success: bool,
    pub net_gas: i64,
    pub consumed: Vec<([u8; 32], u64)>,
    /// Address-owned outputs of this sponsored transaction, not the checkpoint's final inventory.
    pub owners: Vec<([u8; 32], [u8; 32])>,
}
#[derive(sqlx::FromRow)]
struct Attempt {
    order_id: String,
    merge: bool,
    asset: Vec<u8>,
    id: String,
    user_id: Option<String>,
    sponsor: Option<Vec<u8>>,
    recipient: Vec<u8>,
    amount: String,
    sender: Vec<u8>,
    digest: Vec<u8>,
    budget: String,
}

pub(crate) async fn settle(
    tx: &mut Transaction<'_, Postgres>,
    b: &FinalizedBlock,
    executions: &[Execution],
) -> Result<()> {
    if executions.is_empty() {
        return Ok(());
    }
    let hashes: Vec<_> = executions.iter().map(|e| e.hash.as_slice()).collect();
    let mut spent = HashMap::new();
    for e in executions {
        for (id, v) in &e.consumed {
            ensure!(
                spent.insert((*id, *v), e).is_none(),
                "object version consumed twice"
            );
        }
    }
    let ids: Vec<_> = spent.keys().map(|(id, _)| id.as_slice()).collect();
    let rows:Vec<Attempt>=sqlx::query_as("SELECT p.payout_id::text AS id,coalesce(s.id,w.id)::text AS order_id,p.merge_for IS NOT NULL AS merge,w.user_id,p.sponsor,coalesce(s.asset,w.asset) AS asset,coalesce(s.recipient,w.recipient) AS recipient,coalesce(s.amount,w.amount)::text AS amount,p.sender,p.digest::bytea,p.budget::text
        FROM mango_payouts p LEFT JOIN withdrawals w ON w.id=coalesce(p.withdrawal_id,p.merge_for) LEFT JOIN sweep_orders s ON s.id=p.sweep_id WHERE p.confirmed_height IS NULL AND coalesce(s.status,w.status) IN ('signed','submitted','consolidating')
        AND (p.digest=ANY($1::bytea[]) OR EXISTS(SELECT 1 FROM mango_payout_inputs i WHERE i.payout_id=p.payout_id AND i.object_id=ANY($2::bytea[]))) FOR UPDATE OF p")
        .bind(hashes).bind(ids).fetch_all(&mut **tx).await?;
    for p in rows {
        let inputs:Vec<(Vec<u8>,i64,bool)>=sqlx::query_as("SELECT object_id::bytea,version,is_gas FROM mango_payout_inputs WHERE payout_id=$1::text::uuid ORDER BY object_id")
            .bind(&p.id).fetch_all(&mut **tx).await?;
        let mut consumed = Vec::new();
        for (id, v, _) in &inputs {
            let id: [u8; 32] = id.as_slice().try_into()?;
            if let Some(e) = spent.get(&(id, *v as u64)) {
                consumed.push(*e);
            }
        }
        let own = executions.iter().find(|e| e.hash.as_slice() == p.digest);
        let Some(e) = own.or(consumed.first().copied()) else {
            continue;
        };
        let mut posting = None;
        let mut gas = None;
        let mut net_gas = None;
        let (status, detail) = if own.is_none() {
            (
                "review",
                "reserved Mango object version consumed by another transaction",
            )
        } else {
            ensure!(
                p.sender == e.sender
                    && p.sponsor.as_ref().unwrap_or(&p.sender).as_slice() == e.payer,
                "Mango sender or gas owner differs from signed payout"
            );
            ensure!(
                i128::from(e.net_gas) <= p.budget.parse::<i128>()?,
                "Mango gas exceeds signed budget"
            );
            net_gas = Some(e.net_gas);
            if e.net_gas != 0 {
                let (debit, credit) = if e.net_gas > 0 {
                    ("platform:gas", "custody")
                } else {
                    ("custody", "platform:gas")
                };
                gas=Some(sqlx::query_scalar::<_,i64>("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES('mango-testnet','',$1,$2,$3::text::positive_raw) RETURNING id")
                    .bind(debit).bind(credit).bind(e.net_gas.unsigned_abs().to_string()).fetch_one(&mut **tx).await?);
            }
            let mut gains = b.receipts.iter().filter(|r| r.tx == e.hash);
            let paid = if p.merge {
                gains.next().is_none()
            } else {
                match gains.next() {
                    Some(r) => {
                        r.recipient == p.recipient
                            && r.amount == p.amount
                            && r.asset == p.asset
                            && gains.next().is_none()
                    }
                    None => p.amount == "0",
                }
            };
            let delivered = p.sponsor.is_none() || {
                let owners: HashMap<_, _> = e
                    .owners
                    .iter()
                    .map(|(id, owner)| (id.as_slice(), owner.as_slice()))
                    .collect();
                inputs
                    .iter()
                    .filter(|(_, _, gas)| !gas)
                    .all(|(id, _, _)| owners.get(id.as_slice()) == Some(&p.recipient.as_slice()))
            };
            if inputs.is_empty()
                || consumed.iter().any(|x| x.hash != e.hash)
                || (e.success && (!paid || !delivered || consumed.len() != inputs.len()))
            {
                (
                    "review",
                    "finalized effects do not prove the ordered Mango payment",
                )
            } else if p.merge {
                if e.success {
                    (
                        "consolidating",
                        "coin consolidation confirmed; preparing the original withdrawal",
                    )
                } else {
                    (
                        "review",
                        "coin consolidation failed on chain; withdrawal remains frozen",
                    )
                }
            } else {
                if let Some(user) = &p.user_id {
                    let credit = if e.success {
                        "custody".into()
                    } else {
                        format!("user:{user}:available")
                    };
                    posting=Some(sqlx::query_scalar::<_,i64>("INSERT INTO journal(network,asset,debit_account,credit_account,amount) VALUES('mango-testnet',$4,$1,$2,$3::text::positive_raw) RETURNING id")
                    .bind(format!("user:{user}:frozen")).bind(credit).bind(&p.amount).bind(&p.asset).fetch_one(&mut **tx).await?);
                }
                if e.success {
                    ("completed", "finalized Mango payment matched")
                } else {
                    for (obj, v, _) in &inputs {
                        let key: [u8; 32] = obj.as_slice().try_into()?;
                        if !spent.contains_key(&(key, *v as u64)) {
                            sqlx::query("UPDATE mango_payout_inputs SET reserved=false WHERE payout_id=$1::text::uuid AND object_id=$2 AND version=$3")
                                .bind(&p.id).bind(obj).bind(v).execute(&mut **tx).await?;
                        }
                    }
                    ("failed", "finalized Mango execution failed")
                }
            }
        };
        let table = if p.sponsor.is_some() {
            "sweep_orders"
        } else {
            "withdrawals"
        };
        sqlx::query(&format!(
            "UPDATE {table} SET status=$2,detail=$3 WHERE id=$1::text::uuid"
        ))
        .bind(&p.order_id)
        .bind(status)
        .bind(detail)
        .execute(&mut **tx)
        .await?;
        sqlx::query("UPDATE mango_payouts SET confirmed_height=$2,observed_tx=$3,net_gas=$4,settlement_journal=$5,gas_journal=$6 WHERE payout_id=$1::text::uuid")
            .bind(&p.id).bind(b.height).bind(e.hash.as_slice()).bind(net_gas).bind(posting).bind(gas).execute(&mut **tx).await?;
        if p.sponsor.is_some() {
            crate::sweeps::schedule_after_settlement(
                tx,
                "mango-testnet",
                &p.sender,
                status,
                detail,
                false,
            )
            .await?;
        }
    }
    Ok(())
}
