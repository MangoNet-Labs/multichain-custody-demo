//! Identifiers for the demo's initial test networks; not a deposit allowlist.

pub mod pipeline;

/// An unset or blank Compose variable means "anchor at the finalized head".
pub fn start_height(key: &str) -> std::io::Result<Option<u64>> {
    let invalid =
        || std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("invalid {key}"));
    match std::env::var(key) {
        Ok(raw) if raw.trim().is_empty() => Ok(None),
        Ok(raw) => Ok(Some(raw.trim().parse().map_err(|_| invalid())?)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(invalid()),
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Network {
    MangoTestnet,
    BscTestnet,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum AssetId {
    MangoTestnetMgo,
    MangoTestnetCoin(String),
    BscTestnetBnb,
    BscTestnetErc20([u8; 20]),
}

impl AssetId {
    pub const fn network(&self) -> Network {
        match self {
            Self::MangoTestnetMgo | Self::MangoTestnetCoin(_) => Network::MangoTestnet,
            Self::BscTestnetBnb | Self::BscTestnetErc20(_) => Network::BscTestnet,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_assets_belong_to_their_own_network() {
        assert_eq!(AssetId::MangoTestnetMgo.network(), Network::MangoTestnet);
        assert_eq!(AssetId::BscTestnetBnb.network(), Network::BscTestnet);
    }

    #[test]
    fn evm_token_identity_depends_on_the_contract() {
        let token = AssetId::BscTestnetErc20([1; 20]);
        assert_eq!(token.network(), Network::BscTestnet);
        assert_ne!(token, AssetId::BscTestnetErc20([2; 20]));
        assert_ne!(token, AssetId::BscTestnetBnb);
    }
}
