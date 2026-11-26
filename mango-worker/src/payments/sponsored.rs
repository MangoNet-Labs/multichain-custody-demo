use super::{encoded, Node, MAX_INPUTS};
use anyhow::{ensure, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use custody_store::mango::{Input, Quote, SponsoredSweep};
use mgo_types::{
    base_types::{MgoAddress, ObjectID, ObjectRef, SequenceNumber},
    digests::ObjectDigest,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{ObjectArg, TransactionData},
};
use serde_json::{json, Value};
use std::{collections::HashSet, io::Read};

pub(super) fn as_ref(i: &Input) -> ObjectRef {
    (
        ObjectID::new(i.id),
        SequenceNumber::from_u64(i.version),
        ObjectDigest::new(i.digest),
    )
}

pub(super) fn build(s: &SponsoredSweep, coins: &[Input], amount: u64, price: u64) -> Result<Quote> {
    ensure!(
        price > 0 && amount <= s.max_amount && s.budget > 0,
        "invalid sponsored sweep amount or fee"
    );
    let roles = HashSet::from([s.sender, s.recipient, s.sponsor]);
    ensure!(
        roles.len() == 3 && !roles.contains(&[0; 32]),
        "invalid sponsored sweep roles"
    );
    ensure!(
        !coins.is_empty() && !s.gas.is_empty() && coins.len() + s.gas.len() <= MAX_INPUTS,
        "invalid sponsored input count"
    );
    let mut seen = HashSet::new();
    ensure!(
        coins.iter().chain(&s.gas).all(|i| seen.insert(i.id)),
        "overlapping Mango inputs"
    );
    let mut builder = ProgrammableTransactionBuilder::new();
    let coins = coins.iter().map(as_ref).collect::<Vec<_>>();
    let gas = s.gas.iter().map(as_ref).collect::<Vec<_>>();
    let args = coins
        .iter()
        .map(|r| builder.obj(ObjectArg::ImmOrOwnedObject(*r)))
        .collect::<Result<Vec<_>>>()?;
    let recipient = MgoAddress::from_bytes(s.recipient)?;
    builder.transfer_args(recipient, args);
    let tx = TransactionData::new_programmable_allow_sponsor(
        MgoAddress::from_bytes(s.sender)?,
        gas.clone(),
        builder.finish(),
        s.budget,
        price,
        MgoAddress::from_bytes(s.sponsor)?,
    );
    encoded(&tx, &[coins, gas].concat(), amount, false)
}
pub async fn sweep_quote_command() -> Result<()> {
    let mut buf = Vec::new();
    std::io::stdin().take(131073).read_to_end(&mut buf)?;
    ensure!(buf.len() <= 131072, "sweep request too large");
    let s: SponsoredSweep = serde_json::from_slice(&buf)?;
    let node = Node::from_env()?;
    node.verify().await?;
    let quote = quote_page(&node, &s).await?;
    println!("{}", serde_json::to_string(&quote)?);
    Ok(())
}

async fn quote_page(node: &Node, s: &SponsoredSweep) -> Result<Quote> {
    let asset = if s.asset.is_empty() {
        super::MGO
    } else {
        &s.asset
    };
    let tag = mgo_types::parse_mgo_type_tag(asset)?;
    ensure!(
        !s.gas.is_empty() && s.gas.len() < MAX_INPUTS,
        "invalid gas input count"
    );
    let limit = (MAX_INPUTS - s.gas.len()).min(50);
    let page: super::Page = node
        .call(
            "mgox_getCoins",
            json!([MgoAddress::from_bytes(s.sender)?, asset, null, limit]),
        )
        .await?;
    ensure!(page.data.len() <= limit, "oversized Mango coin page");
    if page.data.is_empty() {
        return Ok(Quote::Waiting("no coin objects on the first page".into()));
    }
    let mut coins = Vec::new();
    let mut amount = 0u64;
    let mut ids = HashSet::new();
    for c in page.data {
        ensure!(
            mgo_types::parse_mgo_type_tag(&c.coin_type)? == tag && ids.insert(c.coin_object_id),
            "invalid coin in sweep page"
        );
        let balance = c.balance.parse::<u64>()?;
        if balance > s.max_amount - amount {
            return Ok(Quote::Waiting(
                "coin page exceeds confirmed holdings".into(),
            ));
        }
        coins.push(Input {
            id: c.coin_object_id.into_bytes(),
            version: c.version.parse()?,
            digest: c.digest.into_inner(),
        });
        amount += balance;
    }
    let price: String = node.call("mgox_getReferenceGasPrice", json!([])).await?;
    let mut quote = build(s, &coins, amount, price.parse()?)?;
    if let Quote::Ready { bytes, .. } = &quote {
        let dry: Value = node
            .call(
                "mgo_dryRunTransactionBlock",
                json!([STANDARD.encode(bytes)]),
            )
            .await?;
        if dry["effects"]["status"]["status"] != "success" {
            quote = Quote::Waiting(
                "sponsored sweep dry-run failed; check object versions and gas budget".into(),
            );
        } else {
            // Transferring whole coin objects encodes no numeric amount. Check the
            // recipient's actual balance change before either wallet signs.
            super::verify_gain(&dry, MgoAddress::from_bytes(s.recipient)?, &s.asset, amount)?;
        }
    }
    Ok(quote)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mgo_types::transaction::TransactionDataAPI;
    fn input(n: u8) -> Input {
        Input {
            id: [n; 32],
            version: n.into(),
            digest: [n; 32],
        }
    }
    #[test]
    fn sponsor_pays_and_only_deposit_objects_transfer() -> Result<()> {
        let mut s = SponsoredSweep {
            asset: String::new(),
            sender: [1; 32],
            recipient: [2; 32],
            sponsor: [3; 32],
            max_amount: 100,
            budget: 10000000,
            gas: vec![input(6)],
        };
        let coins = [input(4), input(5)];
        let Quote::Ready { bytes, inputs, .. } = build(&s, &coins, 100, 1000)? else {
            unreachable!()
        };
        let tx: TransactionData = bcs::from_bytes(&bytes)?;
        assert_eq!(tx.sender().to_inner(), s.sender);
        assert_eq!(tx.gas_owner().to_inner(), s.sponsor);
        assert_eq!(tx.gas_data().payment, [as_ref(&s.gas[0])]);
        assert_eq!(inputs.len(), 3);
        s.gas[0] = coins[0].clone();
        assert!(build(&s, &coins, 100, 1000).is_err());
        Ok(())
    }
}
