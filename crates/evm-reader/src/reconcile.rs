use super::Header;
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use anyhow::{ensure, Result};
use custody_core::Network;
use custody_store::{
    reconcile::{self, Balances},
    PgPool,
};
use evm_reader::rpc::BscRpc;
use serde_json::json;
sol! { function balanceOf(address owner) view returns(uint256); }

pub async fn run(db: &PgPool, node: BscRpc) -> Result<()> {
    let s = reconcile::snapshot(db, Network::BscTestnet).await?;
    let height = s.cursor.height;
    let (s, observed) = tokio::task::spawn_blocking(move || -> Result<_> {
        node.verify()?;
        let tag = format!("0x{height:x}");
        let head: Header = node.call("eth_getBlockByNumber", json!(["finalized", false]))?;
        ensure!(
            head.number.to::<u64>() >= u64::try_from(height)?,
            "audit node behind snapshot"
        );
        let header = || -> Result<()> {
            let h: Header = node.call("eth_getBlockByNumber", json!([tag, false]))?;
            ensure!(
                h.number.to::<u64>() == height as u64 && h.hash.0 == s.cursor.digest,
                "historical BSC block mismatch"
            );
            Ok(())
        };
        header()?;
        let mut observed = Balances::new();
        for w in &s.wallets {
            let owner = Address::try_from(w.address.as_slice())?;
            let raw: U256 = if w.asset.is_empty() {
                node.call("eth_getBalance", json!([owner, tag]))?
            } else {
                let token = Address::try_from(w.asset.as_slice())?;
                let data = Bytes::from(balanceOfCall { owner }.abi_encode());
                let encoded: Bytes =
                    node.call("eth_call", json!([{"to":token,"data":data},tag]))?;
                ensure!(encoded.len() == 32, "invalid historical ERC-20 balance");
                U256::from_be_slice(&encoded)
            };
            observed.insert((w.address.clone(), w.asset.clone()), raw.to_string());
        }
        header()?;
        Ok((s, observed))
    })
    .await??;
    let (id, matched) = reconcile::finish(db, s, observed).await?;
    println!("BSC reconciliation {id} at finalized block {height}: matched={matched}");
    ensure!(
        matched,
        "BSC reconciliation mismatch; inspect Operations before resuming payments"
    );
    Ok(())
}
