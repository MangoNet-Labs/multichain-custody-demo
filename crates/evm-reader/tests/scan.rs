use alloy_primitives::{keccak256, Address, B256, U256};
use custody_core::AssetId;
use evm_reader::{allowlist, scan, scan_matching, Incoming};
use serde_json::{json, Value};

fn addr(n: u8) -> Address {
    Address::from([n; 20])
}
fn hash(n: u8) -> B256 {
    B256::from([n; 32])
}
fn hex(n: u8) -> String {
    format!("0x{n:x}")
}
fn config(contracts: &[u8]) -> Value {
    let tokens: Vec<_> = contracts
        .iter()
        .map(|n| json!({"contract":addr(*n),"symbol":"MGO","decimals":9}))
        .collect();
    json!({"network":"bsc-testnet","tokens":tokens})
}

#[test]
fn unlisted_erc20_has_signed_deltas_but_neither_nft_nor_ledger_credit() {
    let block = serde_json::from_value(
        json!({"number":"0x7","hash":hash(99),"parentHash":hash(98),"transactions":[tx(0)]}),
    )
    .unwrap();
    let mut nft = transfer(9, 0, 1);
    nft["topics"].as_array_mut().unwrap().push(json!(hash(2)));
    nft["data"] = json!("0x");
    let receipts: Vec<_> =
        serde_json::from_value(json!([receipt(0, vec![transfer(8, 0, 0), nft])])).unwrap();
    let mut observations = vec![];
    let (rows, _) = evm_reader::scan_observed(
        &block,
        &receipts,
        &Default::default(),
        |_| true,
        &mut observations,
    )
    .unwrap();
    assert_eq!(rows.len(), 1); // native transfer only
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].delta, format!("-{}", U256::MAX));
    assert_eq!(observations[1].asset, addr(8).to_vec());
}
fn tx(i: u8) -> Value {
    json!({"hash":hash(i),"transactionIndex":hex(i),"blockHash":hash(99),"blockNumber":"0x7",
        "from":addr(1),"to":addr(2),"value":"0x2a","nonce":hex(i)})
}
fn receipt(i: u8, logs: Vec<Value>) -> Value {
    json!({"transactionHash":hash(i),"transactionIndex":hex(i),"blockHash":hash(99),
        "blockNumber":"0x7","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x1","logs":logs})
}
fn transfer(contract: u8, i: u8, index: u8) -> Value {
    let topic = |n| format!("0x{}{}", "00".repeat(12), format!("{:02x}", n).repeat(20));
    json!({"address":addr(contract),"topics":[keccak256("Transfer(address,address,uint256)"),topic(1),topic(2)],
        "data":format!("0x{}", "ff".repeat(32)),"blockHash":hash(99),"blockNumber":"0x7",
        "transactionHash":hash(i),"transactionIndex":hex(i),"logIndex":hex(index),"removed":false})
}
fn run(txs: Vec<Value>, receipts: Vec<Value>) -> anyhow::Result<Vec<Incoming>> {
    let block = serde_json::from_value(
        json!({"number":"0x7","hash":hash(99),"parentHash":hash(98),"transactions":txs}),
    )?;
    let receipts: Vec<_> = serde_json::from_value(json!(receipts))?;
    scan(
        &block,
        &receipts,
        &allowlist(&serde_json::to_vec(&config(&[3, 4]))?)?,
        &[addr(2)].into(),
    )
}

#[test]
fn matches_by_hash_and_emitter_not_symbol_or_transaction_recipient() {
    let mut call = tx(1);
    call["to"] = json!(addr(8));
    let logs = vec![
        transfer(3, 1, 0),
        transfer(8, 1, 1),
        transfer(4, 1, 2),
        transfer(3, 1, 3),
    ];
    let rows = run(
        vec![tx(0), call],
        vec![receipt(1, logs), receipt(0, vec![])],
    )
    .unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(
        (rows[0].asset.clone(), rows[0].amount, rows[0].log_index),
        (AssetId::BscTestnetBnb, U256::from(42), None)
    );
    for (row, (token, index)) in rows[1..].iter().zip([(3, 0), (4, 2), (3, 3)]) {
        assert_eq!(
            row.asset,
            AssetId::BscTestnetErc20(addr(token).into_array())
        );
        assert_eq!(row.amount, U256::MAX);
        assert_eq!(row.log_index.unwrap().to::<u64>(), index);
    }
}

#[test]
fn failed_zero_self_and_unwatched_transfers_are_not_deposits() {
    let mut failed = receipt(0, vec![]);
    failed["status"] = json!("0x0");
    let mut zero = tx(1);
    zero["value"] = json!("0x0");
    let mut self_tx = tx(2);
    self_tx["from"] = json!(addr(2));
    let mut call = tx(3);
    call["to"] = Value::Null;
    let mut zero_log = transfer(3, 3, 0);
    zero_log["data"] = json!(format!("0x{}", "00".repeat(32)));
    let mut self_log = transfer(3, 3, 1);
    self_log["topics"][1] = self_log["topics"][2].clone();
    let mut outgoing = transfer(3, 3, 2);
    outgoing["topics"][2] = outgoing["topics"][1].clone();
    assert!(run(
        vec![tx(0), zero, self_tx, call],
        vec![
            failed,
            receipt(1, vec![]),
            receipt(2, vec![]),
            receipt(3, vec![zero_log, self_log, outgoing])
        ]
    )
    .unwrap()
    .is_empty());
    assert!(run(vec![], vec![]).unwrap().is_empty());
}

#[test]
fn reverted_receipt_reports_nonce_and_actual_native_fee_without_a_payment() {
    let block = serde_json::from_value(
        json!({"number":"0x7","hash":hash(99),"parentHash":hash(98),"transactions":[tx(0),tx(1)]}),
    )
    .unwrap();
    let mut failed = receipt(1, vec![]);
    failed["status"] = json!("0x0");
    failed["effectiveGasPrice"] = json!("0x3");
    let receipts: Vec<_> = serde_json::from_value(json!([failed, receipt(0, vec![])])).unwrap();
    let (incoming, executions) =
        scan_matching(&block, &receipts, &Default::default(), |_| true).unwrap();
    assert_eq!(incoming.len(), 1);
    let e = &executions[1];
    assert!(!e.success);
    assert_eq!(e.hash, hash(1).0);
    assert_eq!(e.sender, addr(1).into_array());
    assert_eq!(e.nonce, 1);
    assert_eq!(e.fee, "63000");
}

#[test]
fn incomplete_mixed_or_duplicate_receipts_reject_the_whole_block() {
    let good = receipt(0, vec![]);
    assert!(run(vec![tx(0), tx(1)], vec![good.clone()]).is_err());
    assert!(run(vec![tx(0), tx(1)], vec![good.clone(), good.clone()]).is_err());
    for (field, value) in [
        ("blockHash", json!(hash(55))),
        ("blockNumber", json!("0x8")),
        ("transactionHash", json!(hash(11))),
        ("transactionIndex", json!("0x1")),
        ("status", json!("0x2")),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        assert!(run(vec![tx(0)], vec![bad]).is_err(), "{field}");
    }
    let mut missing = tx(0);
    missing.as_object_mut().unwrap().remove("to");
    assert!(run(vec![missing], vec![good]).is_err());
}

#[test]
fn inconsistent_logs_never_deliver_partial_candidates() {
    let log = transfer(3, 0, 0);
    for (field, value) in [
        ("logIndex", json!("0x1")),
        ("removed", json!(true)),
        ("transactionHash", json!(hash(44))),
        (
            "topics",
            json!([keccak256("Transfer(address,address,uint256)")]),
        ),
        ("data", json!("0xff")),
    ] {
        let mut bad = log.clone();
        bad[field] = value;
        assert!(
            run(vec![tx(0)], vec![receipt(0, vec![bad])]).is_err(),
            "{field}"
        );
    }
}

#[test]
fn rejects_ambiguous_or_wrong_network_allowlists() {
    for config in [
        config(&[3, 3]),
        config(&[0]),
        json!({"network":"mainnet","tokens":[]}),
    ] {
        assert!(allowlist(&serde_json::to_vec(&config).unwrap()).is_err());
    }
}
