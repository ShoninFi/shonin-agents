//! The config a strategy is started with.
//!
//! The host hands `run` the bytes its manifest declared, as canonical CBOR of
//! a map of strings. Decimals arrive as text so that two machines cannot round
//! the same number differently.

use std::collections::BTreeMap;

use strategy_sdk::{Decimal, from_payload};

/// The manifest's `[config]` table, as the strategy reads it.
pub struct Config(BTreeMap<String, String>);

impl Config {
    /// Read what `run` was handed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.is_empty() {
            return Ok(Self(BTreeMap::new()));
        }
        from_payload(bytes).map(Self)
    }

    /// A decimal setting, or the default the strategy names.
    pub fn decimal(&self, key: &str, fallback: &str) -> Decimal {
        self.0
            .get(key)
            .and_then(|text| text.parse().ok())
            .unwrap_or_else(|| fallback.parse().expect("a default the strategy wrote down"))
    }

    pub fn text(&self, key: &str, fallback: &str) -> String {
        self.0
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strategy_sdk::{decimal, to_payload};

    #[test]
    fn a_setting_is_read_or_defaulted() {
        let mut map = BTreeMap::new();
        map.insert("entry_apr".to_string(), "0.12".to_string());
        let config = Config::from_bytes(&to_payload(&map)).expect("it reads");
        assert_eq!(config.decimal("entry_apr", "0.05"), decimal("0.12"));
        assert_eq!(config.decimal("exit_apr", "0.04"), decimal("0.04"));
        assert_eq!(config.text("market", "BTC"), "BTC");
    }

    #[test]
    fn no_config_at_all_is_every_default() {
        let config = Config::from_bytes(&[]).expect("it reads");
        assert_eq!(config.decimal("entry_apr", "0.05"), decimal("0.05"));
    }

    #[test]
    fn a_setting_that_is_not_a_number_falls_back_rather_than_trapping() {
        let mut map = BTreeMap::new();
        map.insert("entry_apr".to_string(), "soon".to_string());
        let config = Config::from_bytes(&to_payload(&map)).expect("it reads");
        assert_eq!(config.decimal("entry_apr", "0.05"), decimal("0.05"));
    }
}
