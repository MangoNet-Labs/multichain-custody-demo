use mango_worker::decode;
use mgo_types::{
    base_types::MgoAddress,
    digests::{CheckpointDigest, TransactionDigest},
    full_checkpoint_content::CheckpointData,
};
const FIXTURE: &[u8] = include_bytes!("fixtures/testnet-106658.bcs");
#[test]
fn real_testnet_transfer_matches_rpc_amount_and_owner() {
    let b = decode(FIXTURE, 106658).unwrap();
    assert_eq!(
        CheckpointDigest::new(b.digest).base58_encode(),
        "BNzDy5JxW5Gqutq8PZnuSMtKf9YRhJquPZgBeFCfj8ru"
    );
    assert_eq!(b.receipts.len(), 1);
    let r = &b.receipts[0];
    assert_eq!(
        TransactionDigest::new(r.tx).to_string(),
        "ES7MPDmSJECqCobxLToBbkJBuVj3rCRR23CBjYxegywR"
    );
    assert_eq!(
        MgoAddress::from_bytes(&r.recipient).unwrap(),
        "0xc70a44bfedc8c36d775cd2775c67845ac2e25694a094fea601966786a708435a"
            .parse()
            .unwrap()
    );
    assert_eq!(r.amount, "100000000000");
}
#[test]
fn rejects_missing_or_wrong_data_even_when_header_is_intact() {
    assert!(decode(FIXTURE, 106659).is_err());
    assert!(decode(&FIXTURE[..FIXTURE.len() - 1], 106658).is_err());
    for defect in 0..4 {
        let mut cp: CheckpointData = bcs::from_bytes(FIXTURE).unwrap();
        let transfer = cp.transactions.last_mut().unwrap();
        match defect {
            0 => transfer.input_objects.clear(),
            1 => {
                transfer.output_objects.pop().unwrap();
            }
            2 => {
                transfer.output_objects[0] = transfer.input_objects[0].clone();
            }
            _ => cp.transactions.reverse(),
        }
        assert!(decode(&bcs::to_bytes(&cp).unwrap(), 106658).is_err());
    }
}

#[test]
fn same_symbol_foreign_coin_is_not_native_income_or_spendable_inventory() {
    // Node balance changes: 3 native MGO and 50 OTHER (also named MGO) to Alice.
    let (b, _, coins) =
        mango_worker::decode_full(include_bytes!("fixtures/localnet-mixed-assets.bcs"), 142)
            .unwrap();
    assert_eq!(b.receipts.len(), 1);
    let r = &b.receipts[0];
    assert_eq!(
        TransactionDigest::new(r.tx).to_string(),
        "56jGHUbG4VGPyfy3RtShxvxnV4DhSEqTUrujXfn5vQ7f"
    );
    assert_eq!(r.recipient, [0xaa; 32]);
    assert!(r.asset.is_empty());
    assert_eq!(r.amount, "3000000000");
    assert_eq!(
        coins
            .values()
            .flatten()
            .filter(|c| c.owner == [0xaa; 32])
            .map(|c| c.balance)
            .collect::<Vec<_>>(),
        [3_000_000_000]
    );
}

#[test]
fn restructuring_owned_coins_never_credits_gas_or_storage_rebates() {
    for (bytes, height, tx, fee) in [
        (
            &include_bytes!("fixtures/localnet-split.bcs")[..],
            162,
            "BjPvoLs6MNpXoywE5hMZxWV9pkEZDqFtZTBio8QX5xkb",
            7_935_760,
        ),
        (
            &include_bytes!("fixtures/localnet-rebate.bcs")[..],
            172,
            "8R1WrxmxRqiqXYJsSdjyFgQ1qtwuYLAZmv15DXSP5ffa",
            -6_815_080,
        ),
    ] {
        let (b, exec, _) = mango_worker::decode_full(bytes, height).unwrap();
        let e = exec
            .iter()
            .find(|e| TransactionDigest::new(e.hash).to_string() == tx)
            .unwrap();
        assert!(e.success);
        assert_eq!(e.sender, e.payer);
        assert_eq!(e.net_gas, fee);
        assert!(b.receipts.is_empty());
    }
}
