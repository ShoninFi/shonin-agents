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
/// How far past the quoted price an order may fill, as a fraction. The
/// market moves between reading a price and the order landing; an order held
/// to the exact quote misses whenever it does, and a miss costs as much gas
/// as a fill.
pub const SLIPPAGE: &str = "0.002";
/// How far short of its target the carry, or the money behind it at the
/// venue, may fall before anything is done about it, as a fraction of the
/// target. Drift inside it is not worth the transactions it would take.
pub const REBALANCE_BAND: &str = "0.10";

/// Gross exposure at most twice the vault's value.
pub const MAX_GROSS_LEVERAGE: &str = "2";
/// No single order above 5% of the vault.
pub const MAX_ORDER_FRACTION: &str = "0.05";
/// 6% of the vault kept liquid, so an ordinary withdrawal closes nothing:
/// a little above the vault's own 5% idle floor, which it never moves past.
pub const IDLE_BUFFER: &str = "0.06";
/// Only reduces after a 10% fall in a vault share's value from its peak since
/// the agent last started, as it measures it (`per_share.rs`).
pub const DRAWDOWN_STOP: &str = "0.10";
/// Once stopped, opens again only when a share, measured as if the vault had
/// kept what it held when the stop came, is back within 5% of its peak.
pub const STOP_LIFTS_WITHIN: &str = "0.05";
