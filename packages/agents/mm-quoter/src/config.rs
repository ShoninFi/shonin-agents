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
/// The venue's own cancel-everything deadline is set this far ahead of the
/// agent's clock: if the agent stops renewing it, the venue cancels every
/// quote at most this long after the last renewal.
pub const DEAD_MAN_MS: u64 = 60_000;
/// The deadline is renewed once less than this is left on it, so it is sent
/// at most about twice a minute however often the quotes move, rather than
/// with every requote: each renewal is an action the venue counts against
/// the account's request allowance.
pub const RENEW_WHEN_LEFT_MS: u64 = 30_000;
/// The venue fires a scheduled cancel at most ten times a UTC day. Once the
/// deadline has run out this many times in a day (each time the agent fell
/// silent), the agent stops quoting until the next UTC day, so it never
/// rests a quote without the venue's deadline behind it.
pub const STOP_AFTER_FIRES: u32 = 8;
// Always some of the venue's ten left over.
const _: () = assert!(STOP_AFTER_FIRES < 10);

/// Each quote at most 2% of the vault's value.
pub const QUOTE_FRACTION: &str = "0.02";
/// Inventory at most 50% of the vault's value.
pub const INVENTORY_CAP: &str = "0.50";
/// The venue refuses an order under ten dollars; below this a quote isn't
/// posted.
pub const MINIMUM_NOTIONAL: &str = "12";

/// During a halt, a look that couldn't close what it saw (a read failed, an
/// order still rests, no book seen yet, or a close the venue didn't fill) is
/// taken again no sooner than this, in seconds.
pub const HALT_LOOKS_AGAIN_AFTER: u64 = 10;
