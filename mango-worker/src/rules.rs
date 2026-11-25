use anyhow::{ensure, Context, Result};
use mgo_types::{coin::Coin, gas_coin::GAS, object::Object, parse_mgo_type_tag, TypeTag};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub coin_type: String,
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Default, Clone)]
pub struct Rules(BTreeMap<TypeTag, (Token, i64)>);

impl Rules {
    pub fn new(entries: impl IntoIterator<Item = (Token, i64)>) -> Result<Self> {
        let mut rules = Self::default();
        for (mut token, height) in entries {
            ensure!(
                height >= 0 && token.coin_type.len() <= 512,
                "invalid Mango coin configuration"
            );
            let tag = parse_mgo_type_tag(&token.coin_type).context("invalid Mango coin type")?;
            ensure!(
                matches!(&tag, TypeTag::Struct(s) if !Coin::is_coin(s)) && !GAS::is_gas_type(&tag),
                "expected the non-native type T, not Coin<T> or native MGO"
            );
            ensure!(
                !token.symbol.is_empty()
                    && token.symbol.len() <= 32
                    && !token
                        .symbol
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control()),
                "invalid token symbol"
            );
            token.coin_type = tag.to_canonical_string(true);
            ensure!(
                token.coin_type.len() <= 512,
                "canonical coin type exceeds 512 bytes"
            );
            ensure!(
                rules.0.insert(tag, (token, height)).is_none(),
                "duplicate Mango coin type"
            );
        }
        Ok(rules)
    }
    pub fn entries(&self) -> impl Iterator<Item = &(Token, i64)> {
        self.0.values()
    }
    pub fn fingerprint(&self) -> Option<[u8; 32]> {
        if self.0.is_empty() {
            return None;
        }
        Some(
            Sha3_256::digest(
                serde_json::to_vec(&self.entries().collect::<Vec<_>>())
                    .expect("serializable token rules"),
            )
            .into(),
        )
    }
    pub(crate) fn asset(&self, obj: &Object, height: u64) -> Option<&[u8]> {
        if obj.is_gas_coin() {
            return Some(&[]);
        }
        if self.0.is_empty() {
            return None;
        }
        let (token, from) = self.0.get(&obj.coin_type_maybe()?)?;
        (height >= *from as u64).then_some(token.coin_type.as_bytes())
    }
}
