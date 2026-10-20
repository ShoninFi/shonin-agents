//! The numbers this agent decides with.
//!
//! They are constants in the code rather than settings anywhere else, so the
//! reviewer reads them with the rest of the source, and the claims in
//! `shonin.toml` say the same thing in words.

/// The underlyings it trades: the stock token's symbol, the Lighter
/// perpetual's symbol, and the perpetual's market number on Lighter's
/// Robinhood Chain exchange (read from its order books).
pub const NAMES: &[(&str, &str, u32)] =
    &[("SPY", "SPY", 26), ("QQQ", "QQQ", 25), ("NVDA", "NVDA", 15)];

/// The perpetual's premium over the token (per share of the stock) at which
/// a carry is opened, and below which an open one is closed.
pub const ENTRY_PREMIUM: &str = "0.005";
pub const EXIT_PREMIUM: &str = "0.001";
/// How much of the vault to aim to have working across all names: each name
/// gets an equal part.
pub const TARGET_DEPLOYMENT: &str = "0.60";
/// Below this, an order is not worth the spread it would pay.
pub const MINIMUM_NOTIONAL: &str = "50";
/// What the short is margined with at Lighter, as a fraction of its notional:
/// well above the exchange's 2% so a weekend move doesn't liquidate it.
pub const PERP_MARGIN: &str = "0.25";
/// How far from the price seen a closing or opening order may fill.
pub const SLIPPAGE: &str = "0.01";

/// Gross exposure at most twice the vault's value.
pub const MAX_GROSS_LEVERAGE: &str = "2";
/// No single order above 5% of the vault, save a halt's closing orders
/// (a halt's cap may come from a value read high: see `Picture::halting`).
pub const MAX_ORDER_FRACTION: &str = "0.05";
/// 6% of the vault kept liquid, so an ordinary withdrawal closes nothing.
pub const IDLE_BUFFER: &str = "0.06";
/// Both legs cut together after a 10% fall in a vault share's value from its
/// peak since the agent last started, as it measures it (`per_share.rs`),
/// and nothing opened again until the agent restarts.
pub const DRAWDOWN_STOP: &str = "0.10";

/// The weekend, in UTC: from Friday 21:00 (the cash market has closed) to
/// Monday 13:30 (it opens). Outside it, an open carry is closed.
pub const WEEKEND_FROM: (u8, u32) = (5, 21 * 3600);
pub const WEEKEND_UNTIL: (u8, u32) = (1, 13 * 3600 + 1800);

/// Steps at most this often, in seconds, however busy the books are.
pub const STEP_EVERY: u64 = 30;

/// During a halt, a look that couldn't see everything held (a read failed,
/// or a book not yet seen) is taken again no sooner than this, in seconds.
pub const HALT_LOOKS_AGAIN_AFTER: u64 = 10;
