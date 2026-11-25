//! Read-only processing of a finalized BSC block and its complete receipt set.
pub mod broadcast;
pub mod rpc;

use alloy_primitives::{Address, Bytes, B256, U256, U64};
use alloy_sol_types::{sol, SolEvent};
use anyhow::{ensure, Context, Result};
use custody_core::AssetId;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

sol! { event Transfer(address indexed from, address indexed to, uint256 value); }

#[derive(Deserialize)]
pub struct Token {
    pub contract: Address,
    pub symbol: String,
    pub decimals: u8,
}

pub fn allowlist(json: &[u8]) -> Result<HashMap<Address, Token>> {
    #[derive(Deserialize)]
    struct Config {
        network: String,
        tokens: Vec<Token>,
    }
    let config: Config = serde_json::from_slice(json)?;
    ensure!(config.network == "bsc-testnet", "wrong allowlist network");
    let mut tokens = HashMap::new();
    for token in config.tokens {
        ensure!(
            !token.contract.is_zero() && !token.symbol.is_empty(),
            "invalid token"
        );
        ensure!(
            tokens.insert(token.contract, token).is_none(),
            "duplicate token contract"
        );
    }
    Ok(tokens)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub hash: B256,
    pub parent_hash: B256,
    pub number: U64,
    pub transactions: Vec<Transaction>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    hash: B256,
    transaction_index: U64,
    block_hash: B256,
    block_number: U64,
    from: Address,
    #[serde(deserialize_with = "Option::deserialize")]
    to: Option<Address>,
    value: U256,
    nonce: U64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    block_hash: B256,
    block_number: U64,
    transaction_hash: B256,
    transaction_index: U64,
    status: U64,
    gas_used: U64,
    effective_gas_price: U256,
    logs: Vec<EventLog>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventLog {
    address: Address,
    topics: Vec<B256>,
    data: Bytes,
    block_hash: B256,
    block_number: U64,
    transaction_hash: B256,
    transaction_index: U64,
    log_index: U64,
    removed: bool,
}

/// A transfer to a watched address, not a ledger credit. The caller must
/// classify internal funding and sweep orders before crediting a user.
#[derive(Debug, PartialEq)]
pub struct Incoming {
    pub tx: B256,
    pub asset: AssetId,
    pub from: Address,
    pub to: Address,
    pub amount: U256,
    pub log_index: Option<U64>,
    pub tx_log_index: Option<U64>,
}

pub fn scan(
    block: &Block,
    receipts: &[Receipt],
    tokens: &HashMap<Address, Token>,
    watched: &HashSet<Address>,
) -> Result<Vec<Incoming>> {
    Ok(scan_matching(block, receipts, tokens, |a| watched.contains(a))?.0)
}

pub fn scan_matching(
    block: &Block,
    receipts: &[Receipt],
    tokens: &HashMap<Address, Token>,
    accepts: impl Fn(&Address) -> bool,
) -> Result<(Vec<Incoming>, Vec<custody_store::payouts::Execution>)> {
    ensure!(
        receipts.len() == block.transactions.len(),
        "receipt count mismatch"
    );
    let by_hash: HashMap<_, _> = receipts.iter().map(|r| (r.transaction_hash, r)).collect();
    ensure!(by_hash.len() == receipts.len(), "duplicate receipt");
    let mut incoming = Vec::new();
    let mut executions = Vec::with_capacity(receipts.len());
    let mut next_log = 0u64;
    for (index, tx) in block.transactions.iter().enumerate() {
        let r = by_hash.get(&tx.hash).context("missing receipt")?;
        let index = U64::from(index);
        ensure!(
            tx.block_hash == block.hash
                && r.block_hash == block.hash
                && tx.block_number == block.number
                && r.block_number == block.number
                && tx.transaction_index == index
                && r.transaction_index == index,
            "block or transaction position mismatch"
        );
        let fee = r
            .effective_gas_price
            .checked_mul(U256::from(r.gas_used.to::<u64>()))
            .context("gas fee overflow")?;
        executions.push(custody_store::payouts::Execution {
            hash: tx.hash.0,
            sender: tx.from.into_array(),
            nonce: tx.nonce.to::<u64>(),
            success: r.status == U64::from(1),
            fee: fee.to_string(),
        });
        if r.status == U64::ZERO {
            ensure!(r.logs.is_empty(), "failed receipt with logs");
            continue;
        }
        ensure!(r.status == U64::from(1), "unknown execution status");
        if let Some(to) = tx
            .to
            .filter(|a| !a.is_zero() && accepts(a) && a != &tx.from)
        {
            if tx.value != U256::ZERO {
                incoming.push(Incoming {
                    tx: tx.hash,
                    asset: AssetId::BscTestnetBnb,
                    from: tx.from,
                    to,
                    amount: tx.value,
                    log_index: None,
                    tx_log_index: None,
                });
            }
        }
        for (local_index, log) in r.logs.iter().enumerate() {
            ensure!(
                !log.removed
                    && log.block_hash == block.hash
                    && log.block_number == block.number
                    && log.transaction_hash == tx.hash
                    && log.transaction_index == index
                    && log.log_index == U64::from(next_log),
                "inconsistent receipt log"
            );
            next_log += 1;
            if !tokens.contains_key(&log.address)
                || log.topics.first() != Some(&Transfer::SIGNATURE_HASH)
            {
                continue;
            }
            ensure!(
                log.topics.len() == 3 && log.data.len() == 32,
                "malformed ERC-20 Transfer"
            );
            let e = Transfer::decode_raw_log_validate(log.topics.iter().copied(), &log.data)?;
            if !e.to.is_zero() && accepts(&e.to) && e.to != e.from && e.value != U256::ZERO {
                incoming.push(Incoming {
                    tx: tx.hash,
                    asset: AssetId::BscTestnetErc20(log.address.into_array()),
                    from: e.from,
                    to: e.to,
                    amount: e.value,
                    log_index: Some(log.log_index),
                    tx_log_index: Some(U64::from(local_index)),
                });
            }
        }
    }
    Ok((incoming, executions))
}
