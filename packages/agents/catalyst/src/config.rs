//! The numbers this agent decides with.
//!
//! They are constants in the code rather than settings anywhere else, so the
//! reviewer reads them with the rest of the source, and the claims in
//! `shonin.toml` say the same thing in words.

/// The companies it reads: the stock token's symbol and the company's SEC
/// Central Index Key.
pub const NAMES: &[(&str, u64)] = &[("NVDA", 1_045_810), ("TSLA", 1_318_605), ("AAPL", 320_193)];

/// The SEC asks every automated reader to say who it is.
pub const USER_AGENT: &str = "Shonin catalyst agent (contact: agents@shonin.fi)";
/// How often each company's filings are read, in seconds: well inside the
/// SEC's fair-access limit of ten requests a second.
pub const READ_EVERY: u64 = 300;

/// The model's name in the bundle, and the score at which it buys.
pub const MODEL: &str = "scorer";
pub const BUY_SCORE: &str = "0.60";

/// Each position at most this much of the vault, and all of them together
/// at most `MAX_EXPOSURE`.
pub const MAX_ORDER_FRACTION: &str = "0.05";
pub const MAX_EXPOSURE: &str = "0.30";
/// Held at most this long, in seconds, and closed sooner after this fall
/// from its entry price.
pub const HOLD_FOR: u64 = 86_400;
pub const STOP_LOSS: &str = "0.03";
/// How far from the price seen a swap may fill.
pub const SLIPPAGE: &str = "0.01";
/// Below this, an order is not worth what it costs.
pub const MINIMUM_NOTIONAL: &str = "25";
/// 6% of the vault kept liquid.
pub const IDLE_BUFFER: &str = "0.06";
