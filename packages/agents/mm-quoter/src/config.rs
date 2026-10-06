//! The numbers this agent decides with. The claims in `shonin.toml` say the
//! same things in words.

/// The perpetual it quotes, and the size decimals the venue allows on it.
pub const MARKET: &str = "ETH";
pub const SIZE_DECIMALS: u32 = 4;
/// Cross margin at this leverage.
pub const LEVERAGE: u32 = 3;

/// Each side's distance from the mid, as a fraction of it.
pub const HALF_SPREAD: &str = "0.0008";
/// How far the quotes lean against inventory at the cap, as a fraction of
/// the mid: at full inventory long, both quotes sit this much lower.
pub const SKEW_AT_CAP: &str = "0.0010";
/// Quotes are renewed when the mid moves this far from where they were set.
pub const REQUOTE_MOVE: &str = "0.0004";
/// The venue cancels every quote this long after it was last renewed.
pub const DEAD_MAN_MS: u64 = 15_000;

/// Each quote at most 2% of the vault's value.
pub const QUOTE_FRACTION: &str = "0.02";
/// Inventory at most 50% of the vault's value.
pub const INVENTORY_CAP: &str = "0.50";
/// The venue refuses an order under ten dollars; below this a quote isn't
/// posted.
pub const MINIMUM_NOTIONAL: &str = "12";
