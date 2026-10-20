//! What this agent says to its driver, and hears back: the `robinhood`
//! driver's payloads, which the same driver's contract on Base (named
//! `uniswap-base` there) speaks too.
//!
//! These are the driver's own published payload types, copied verbatim (the
//! same names, fields and serde attributes, so they encode to the same
//! canonical CBOR), with the dollar the driver trades against named in its
//! account state, since it differs by chain (USDG on Robinhood Chain, USDC
//! on Base).

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
pub use strategy_sdk::Decimal;

/// What a strategy submits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Spend `amount` of the dollar on token `token`, receiving at least
    /// `min_tokens`.
    Buy {
        token: String,
        amount: Decimal,
        min_tokens: Decimal,
    },
    /// Sell `tokens` of token `token` for at least `min_amount` dollars.
    Sell {
        token: String,
        tokens: Decimal,
        min_amount: Decimal,
    },
}

/// What came back inside an accepted verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Receipt {
    /// The swap landed: what was spent and what came back.
    Swapped {
        token: String,
        spent: Decimal,
        received: Decimal,
        transaction: String,
    },
}

/// What `account-state` is asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Query {
    /// The Safe's holdings, priced.
    Holdings,
    /// Every listed token's prices: its feed's and its pool's.
    Prices,
}

/// One listed token's prices, as `Query::Prices` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TokenPrice {
    pub token: String,
    /// Chainlink's price for the token (the multiplier already applied).
    pub feed_price: Decimal,
    /// When the feed last moved, in seconds since the Unix epoch.
    pub feed_updated_at: u64,
    /// What the token's pool says, averaged over the driver's window.
    pub market_price: Decimal,
    /// Shares of the underlying per token.
    pub multiplier: Decimal,
    /// The issuer has paused the token's oracle.
    pub paused: bool,
}

/// One token the Safe holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Holding {
    pub token: String,
    /// In the token's own units.
    pub tokens: Decimal,
    /// The token's Chainlink price.
    pub feed_price: Decimal,
    /// When the feed last moved, in seconds since the Unix epoch.
    pub feed_updated_at: u64,
    /// Shares of the underlying per token: for display, never for valuing.
    pub multiplier: Decimal,
}

/// What `account-state` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountState {
    /// The Safe's address.
    pub safe: String,
    /// The symbol of the dollar the driver trades against on this chain, as
    /// its token names itself.
    #[serde(default)]
    pub dollar: String,
    /// Idle dollars in the Safe.
    pub dollars: Decimal,
    pub holdings: Vec<Holding>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use strategy_sdk::{decimal, from_payload, to_payload};

    #[test]
    fn an_action_round_trips_through_its_canonical_payload() {
        let action = Action::Buy {
            token: "WETH".into(),
            amount: decimal("25.5"),
            min_tokens: decimal("0.0101"),
        };
        assert_eq!(from_payload::<Action>(&to_payload(&action)), Ok(action));
    }

    #[test]
    fn an_account_state_names_its_dollar_and_one_that_doesn_t_still_reads() {
        let state = AccountState {
            safe: "0x01".into(),
            dollar: "USDC".into(),
            dollars: decimal("12.5"),
            holdings: Vec::new(),
        };
        let back: AccountState = from_payload(&to_payload(&state)).unwrap();
        assert_eq!(back.dollar, "USDC");

        /// The account state as a driver that doesn't name its dollar sends it.
        #[derive(Serialize)]
        struct Older {
            safe: String,
            dollars: Decimal,
            holdings: Vec<Holding>,
        }
        let older = Older {
            safe: "0x01".into(),
            dollars: decimal("1"),
            holdings: Vec::new(),
        };
        let back: AccountState = from_payload(&to_payload(&older)).unwrap();
        assert_eq!(back.dollar, "");
        assert_eq!(back.dollars, decimal("1"));
    }
}
