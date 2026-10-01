//! The numbers this agent decides with.
//!
//! They are constants in the code rather than settings anywhere else, so the
//! reviewer reads them with the rest of the source, and the claims in
//! `shonin.toml` say the same thing in words.

/// The market held long, and the perpetual shorted against it.
pub const SPOT: &str = "BTC";
pub const PERP: &str = "BTC-PERP";

/// Funding, as a fraction of notional per day, at which carrying is worth it.
pub const ENTRY_FUNDING: &str = "0.0002";
/// And at which an open carry is unwound.
pub const EXIT_FUNDING: &str = "0.00005";
/// How much of the vault to aim to have working.
pub const TARGET_DEPLOYMENT: &str = "0.80";
/// Below this, an order is not worth the spread it would pay.
pub const MINIMUM_NOTIONAL: &str = "100";

/// Gross exposure at most twice the vault's value.
pub const MAX_GROSS_LEVERAGE: &str = "2";
/// No single order above 5% of the vault.
pub const MAX_ORDER_FRACTION: &str = "0.05";
/// 5% of the vault kept liquid, so an ordinary withdrawal closes nothing.
pub const IDLE_BUFFER: &str = "0.05";
/// Only reduces after a 10% fall from the vault's peak.
pub const DRAWDOWN_STOP: &str = "0.10";
