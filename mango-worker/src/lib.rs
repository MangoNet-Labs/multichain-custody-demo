//! Decode the public full-node BCS representation with pinned Mango types.
use anyhow::{ensure, Context, Result};
use custody_core::Network::MangoTestnet;
use custody_store::{
    mango_coins::{Changes, Owned},
    FinalizedBlock, Receipt,
};
use mgo_types::{
    base_types::MgoAddress,
    coin::Coin,
    effects::TransactionEffectsAPI,
    full_checkpoint_content::{CheckpointData, CheckpointTransaction},
    object::{Object, Owner},
    transaction::{InputObjectKind, TransactionDataAPI},
};
use std::collections::{BTreeMap, HashMap, HashSet};
pub mod rules;
use rules::Rules;

pub const TESTNET_GENESIS: &str = "ACaDn8ue58z3q7ujnBsDo8SPQTe1SooUH3nsNQXniW1M";

fn add_coin(
    deltas: &mut BTreeMap<(MgoAddress, Vec<u8>), i128>,
    obj: &Object,
    asset: &[u8],
    sign: i128,
) -> Result<()> {
    if let Owner::AddressOwner(address) = obj.owner {
        let amount = Coin::extract_balance_if_coin(obj)?.context("missing coin balance")?;
        *deltas.entry((address, asset.to_vec())).or_default() += sign * i128::from(amount);
    }
    Ok(())
}

fn changes(
    tx: &CheckpointTransaction,
    coins: &mut Changes,
    unlisted: &mut BTreeMap<(MgoAddress, Vec<u8>), i128>,
    rules: &Rules,
    height: u64,
) -> Result<Vec<Receipt>> {
    let fx = &tx.effects;
    let data = tx.transaction.data().transaction_data();
    let modified = fx.modified_at_versions();
    let absent: HashSet<_> = fx
        .unwrapped_then_deleted()
        .into_iter()
        .map(|r| r.0)
        .collect();
    let expected: HashSet<_> = modified
        .iter()
        .copied()
        .chain(
            fx.input_shared_objects()
                .into_iter()
                .map(|o| o.id_and_version()),
        )
        .filter(|(id, _)| !absent.contains(id))
        .collect();
    let inputs: HashMap<_, _> = tx
        .input_objects
        .iter()
        .map(|o| ((o.id(), o.version()), o))
        .collect();
    ensure!(
        inputs.len() == tx.input_objects.len()
            && inputs.len() == expected.len()
            && expected.iter().all(|k| inputs.contains_key(k)),
        "missing, unexpected or duplicate input objects"
    );
    for k in data.input_objects()? {
        if let InputObjectKind::ImmOrOwnedMoveObject((id, version, digest)) = k {
            if let Some(obj) = inputs.get(&(id, version)) {
                ensure!(obj.digest() == digest, "input object digest mismatch");
            }
        }
    }
    let mut net = BTreeMap::new();
    for (id, v) in modified {
        if !absent.contains(&id) {
            let old = inputs[&(id, v)];
            if let Some(asset) = rules.asset(old, height) {
                add_coin(&mut net, old, asset, -1)?;
                coins.insert(id.into_bytes(), None);
            } else if let Some(tag) = old.coin_type_maybe() {
                add_coin(unlisted, old, tag.to_canonical_string(true).as_bytes(), -1)?;
            }
        }
    }
    let mut outputs: HashMap<_, _> = tx
        .output_objects
        .iter()
        .map(|o| ((o.id(), o.version()), o))
        .collect();
    ensure!(
        outputs.len() == tx.output_objects.len(),
        "duplicate output object"
    );
    for ((id, ver, hash), owner, _) in fx.all_changed_objects() {
        let obj = outputs
            .remove(&(id, ver))
            .context("missing output object")?;
        ensure!(
            obj.digest() == hash && obj.owner == owner,
            "output object digest or owner mismatch"
        );
        if let Some(asset) = rules.asset(obj, height) {
            add_coin(&mut net, obj, asset, 1)?;
            let coin = if let Owner::AddressOwner(addr) = owner {
                Some(Owned {
                    owner: addr.to_inner(),
                    version: ver.value(),
                    digest: hash.into_inner(),
                    balance: Coin::extract_balance_if_coin(obj)?.context("missing coin balance")?,
                    asset: asset.to_vec(),
                })
            } else {
                None
            };
            coins.insert(id.into_bytes(), coin);
        } else if let Some(tag) = obj.coin_type_maybe() {
            add_coin(unlisted, obj, tag.to_canonical_string(true).as_bytes(), 1)?;
        }
    }
    ensure!(outputs.is_empty(), "unexpected output object");
    if !fx.status().is_ok() || data.is_system_tx() {
        return Ok(vec![]);
    }
    let payer = data.gas_owner();
    let gas = i128::from(fx.gas_cost_summary().net_gas_usage());
    let mut rows = Vec::new();
    for ((owner, asset), delta) in net {
        let amount = deposit_delta(delta, asset.is_empty() && owner == payer, gas);
        if amount > 0 {
            rows.push(Receipt {
                tx: tx.transaction.digest().into_inner(),
                recipient: owner.to_vec(),
                sender: None,
                asset,
                amount: amount.to_string(),
                log: None,
            });
        }
    }
    Ok(rows)
}

// Do not credit a storage rebate as a deposit, nor credit more than the
// receiver actually gained. The fee payer is not necessarily the sender.
fn deposit_delta(delta: i128, is_payer: bool, gas: i128) -> i128 {
    if is_payer {
        delta + gas.min(0)
    } else {
        delta
    }
}

pub fn decode(bytes: &[u8], expected_height: u64) -> Result<FinalizedBlock> {
    Ok(decode_full(bytes, expected_height)?.0)
}
pub type FullObservation = (
    FinalizedBlock,
    Vec<custody_store::mango::Execution>,
    Changes,
);
pub fn decode_full(bytes: &[u8], expected_height: u64) -> Result<FullObservation> {
    decode_with_rules(bytes, expected_height, &Rules::default())
}
pub fn decode_with_rules(
    bytes: &[u8],
    expected_height: u64,
    rules: &Rules,
) -> Result<FullObservation> {
    let cp: CheckpointData = bcs::from_bytes(bytes).context("decoding CheckpointData")?;
    let summary = cp.checkpoint_summary.data();
    ensure!(
        summary.sequence_number == expected_height,
        "checkpoint height mismatch"
    );
    ensure!(
        summary.previous_digest.is_none() == (expected_height == 0),
        "invalid checkpoint parent"
    );
    ensure!(
        cp.checkpoint_contents.digest() == &summary.content_digest
            && cp.checkpoint_contents.size() == cp.transactions.len(),
        "checkpoint contents mismatch"
    );
    let digest = cp.checkpoint_summary.digest();
    ensure!(
        expected_height != 0 || digest.base58_encode() == TESTNET_GENESIS,
        "wrong Mango genesis"
    );
    let mut receipts = Vec::new();
    let mut coins = Changes::new();
    let mut unlisted = BTreeMap::new();
    let mut executions = Vec::new();
    for (expected, tx) in cp.checkpoint_contents.iter().zip(&cp.transactions) {
        ensure!(
            expected.transaction == *tx.transaction.digest()
                && *expected == tx.effects.execution_digests(),
            "transaction or effects digest mismatch"
        );
        ensure!(
            tx.events.as_ref().map(|e| e.digest()) == tx.effects.events_digest().copied(),
            "event digest mismatch"
        );
        receipts.extend(
            changes(tx, &mut coins, &mut unlisted, rules, expected_height)
                .with_context(|| format!("transaction {}", expected.transaction))?,
        );
        let data = tx.transaction.data().transaction_data();
        let mut consumed = tx
            .effects
            .modified_at_versions()
            .into_iter()
            .map(|(id, v)| (id.into_bytes(), v.value()))
            .collect::<Vec<_>>();
        consumed.sort_unstable();
        let owners = if data.sender() != data.gas_owner() {
            tx.effects
                .mutated()
                .into_iter()
                .filter_map(|((id, _, _), owner)| {
                    if let Owner::AddressOwner(addr) = owner {
                        Some((id.into_bytes(), addr.to_inner()))
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            vec![]
        };
        executions.push(custody_store::mango::Execution {
            hash: expected.transaction.into_inner(),
            sender: data.sender().to_inner(),
            payer: data.gas_owner().to_inner(),
            success: tx.effects.status().is_ok(),
            net_gas: tx.effects.gas_cost_summary().net_gas_usage(),
            consumed,
            owners,
        });
    }
    Ok((
        FinalizedBlock {
            openings: unlisted
                .into_iter()
                .filter(|(_, d)| *d != 0)
                .map(|((owner, asset), d)| custody_store::OpeningDelta {
                    address: owner.to_inner().to_vec(),
                    asset,
                    delta: d.to_string(),
                })
                .collect(),
            network: MangoTestnet,
            height: expected_height.try_into()?,
            digest: digest.into_inner(),
            parent: summary
                .previous_digest
                .map(|d| d.into_inner())
                .unwrap_or_default(),
            receipts,
        },
        executions,
        coins,
    ))
}

#[cfg(test)]
mod tests {
    use super::deposit_delta;
    #[test]
    fn fee_rebate_is_not_external_income() {
        assert_eq!(deposit_delta(50, true, -50), 0);
        assert_eq!(deposit_delta(120, true, -20), 100);
        assert_eq!(deposit_delta(90, true, 10), 90);
        assert_eq!(deposit_delta(100, false, -20), 100);
    }
}
