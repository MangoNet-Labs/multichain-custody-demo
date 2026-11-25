use alloy_primitives::{b256, B256, U64};
use anyhow::{bail, ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::{env, time::Duration};

pub const GENESIS: B256 = b256!("6d3c66c5357ec91d5c43af47e234a939b22557cbb552dc45bebbceeed90fbe34");
#[derive(Clone)]
pub struct BscRpc {
    agent: ureq::Agent,
    url: String,
}
impl BscRpc {
    pub fn from_env() -> Result<Self> {
        Ok(Self::new(env::var("BSC_TESTNET_RPC_URL").unwrap_or_else(
            |_| "https://bsc-testnet-rpc.publicnode.com".into(),
        )))
    }
    pub fn new(url: String) -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(15)))
                .build()
                .into(),
            url,
        }
    }
    pub fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        rpc(&self.agent, &self.url, method, params)
    }
    pub fn verify(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct Header {
            number: U64,
            hash: B256,
        }
        let chain: U64 = self.call("eth_chainId", json!([]))?;
        ensure!(chain == U64::from(97), "not BSC Testnet");
        let genesis: Header = self.call("eth_getBlockByNumber", json!(["0x0", false]))?;
        ensure!(
            genesis.number == U64::ZERO && genesis.hash == GENESIS,
            "BSC Testnet genesis mismatch"
        );
        Ok(())
    }
}
#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct Reply<T> {
    jsonrpc: String,
    id: u64,
    result: Option<T>,
    error: Option<RpcError>,
}

fn rpc<T: DeserializeOwned>(
    client: &ureq::Agent,
    url: &str,
    method: &str,
    params: Value,
) -> Result<T> {
    let reply: Reply<T> = client
        .post(url)
        .send_json(json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .with_context(|| format!("{method}: HTTP request failed"))?
        .body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .read_json()
        .with_context(|| format!("{method}: unreadable or oversized response"))?;
    ensure!(
        reply.id == 1 && reply.jsonrpc == "2.0",
        "{method}: invalid RPC envelope"
    );
    if let Some(error) = reply.error {
        bail!("{method}: RPC {}: {}", error.code, error.message);
    }
    reply
        .result
        .with_context(|| format!("{method}: missing result"))
}
