//! What this agent says to its two drivers, and hears back: the
//! `robinhood` driver's payloads (stock tokens in the station's Safe) and
//! the `lighter` driver's (the station's Lighter account).
//!
//! These are the drivers' own published payload types, copied verbatim (the
//! same names, fields and serde attributes, so they encode to the same
//! canonical CBOR). They move to the SDK beside Hyperliquid's; this file then
//! becomes two `use` lines.

#![allow(dead_code)]

pub mod robinhood {
    use serde::{Deserialize, Serialize};
    pub use strategy_sdk::Decimal;

    /// The driver's name, as `shonin.toml` writes it.
    pub const DRIVER: &str = "robinhood";

    /// The dollar's symbol: USDG, the chain's native dollar.
    pub const DOLLAR: &str = "USDG";

    /// What a strategy submits.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Action {
        /// Spend `amount` of the dollar on stock token `token`, receiving at
        /// least `min_tokens`.
        Buy {
            token: String,
            amount: Decimal,
            min_tokens: Decimal,
        },
        /// Sell `tokens` of stock token `token` for at least `min_amount`
        /// dollars.
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

    /// One stock token the Safe holds.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Holding {
        pub token: String,
        /// In shares of the token (its own units, not multiplied: the
        /// multiplier only matters for what a share is a claim on).
        pub tokens: Decimal,
        /// The token's Chainlink price, which already applies the multiplier.
        pub feed_price: Decimal,
        /// When the feed last moved, in seconds since the Unix epoch.
        pub feed_updated_at: u64,
        /// Shares of the underlying per token, from the token's multiplier:
        /// for display, never for valuing.
        pub multiplier: Decimal,
    }

    /// What `account-state` answers.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AccountState {
        /// The Safe's address.
        pub safe: String,
        /// Idle dollars in the Safe.
        pub dollars: Decimal,
        pub holdings: Vec<Holding>,
    }

    /// Something that happened to the Safe, read from the chain.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum AccountEvent {
        /// Tokens moved into or out of the Safe: a swap, a deposit, a close.
        Moved {
            token: String,
            /// Positive when the Safe received.
            amount: Decimal,
            transaction: String,
            block: u64,
        },
    }
}

pub mod lighter {
    use serde::{Deserialize, Serialize};
    pub use strategy_sdk::Decimal;

    /// The driver's name, as `shonin.toml` writes it.
    pub const DRIVER: &str = "lighter";

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Side {
        Buy,
        Sell,
    }

    /// How long an order stands.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Tif {
        /// Added to the book only, never taking.
        PostOnly,
        /// Stands until `expiry_ms` (Lighter allows up to 28 days) or a cancel.
        GoodTill { expiry_ms: u64 },
        /// Immediate or cancel, at the limit price or better.
        Ioc,
    }

    /// An order on one perp market, by its symbol (`SPY`, `NVDA`).
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Order {
        pub market: String,
        pub side: Side,
        /// In units of the underlying (shares, coins).
        pub size: Decimal,
        pub limit_price: Decimal,
        pub tif: Tif,
        /// Only ever shrinks a position, never past flat.
        pub reduce_only: bool,
        /// The agent's own id for the order (1 to 2^48 - 1), so it can cancel it
        /// before it knows the venue's.
        pub client_id: Option<u64>,
    }

    /// Which order: the venue's index, or the agent's own id.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum OrderRef {
        Venue(u64),
        Client(u64),
    }

    /// What a strategy submits.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Action {
        Place(Order),
        Cancel {
            market: String,
            order: OrderRef,
        },
        /// Change a resting order's size and price.
        Modify {
            market: String,
            order: OrderRef,
            size: Decimal,
            limit_price: Decimal,
        },
        /// Cancel every resting order on the account.
        CancelAll,
    }

    /// What came back inside an accepted verdict.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Receipt {
        /// The exchange took the transaction; what it did with it arrives as
        /// account events.
        Sent { tx_hash: String },
    }

    /// What `account-state` is asked.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Query {
        Account,
    }

    /// One perp position.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Position {
        pub market: String,
        /// Positive is long.
        pub size: Decimal,
        pub entry_price: Decimal,
        pub unrealized_pnl: Decimal,
    }

    /// What `account-state` answers: the station's master account.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AccountState {
        pub account_index: u64,
        /// The account's worth at the mark.
        pub equity: Decimal,
        /// What it could withdraw or put up as margin now.
        pub available: Decimal,
        pub positions: Vec<Position>,
    }

    /// Something that happened to the account, as the venue reported it.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum AccountEvent {
        Filled {
            market: String,
            side: Side,
            size: Decimal,
            price: Decimal,
            time_ms: u64,
        },
        Order {
            market: String,
            order_index: u64,
            client_id: Option<u64>,
            status: String,
            time_ms: u64,
        },
        Funding {
            market: String,
            /// Positive when the account was paid.
            amount: Decimal,
            time_ms: u64,
        },
    }
}
