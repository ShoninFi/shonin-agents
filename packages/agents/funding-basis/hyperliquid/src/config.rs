//! The numbers this agent decides with.
//!
//! They are constants in the code rather than settings anywhere else, so the
//! reviewer reads them with the rest of the source, and the claims in
//! `shonin.toml` say the same thing in words.

/// The spot pair held long, as the driver names it; its book is asked for by
/// the same name (the venue numbers its pairs differently on each network).
pub const SPOT: &str = "HYPE/USDC";
/// The token the spot leg is held in.
pub const SPOT_TOKEN: &str = "HYPE";
/// Size decimals the venue allows on the spot pair.
pub const SPOT_SIZE_DECIMALS: u32 = 2;
/// The perpetual shorted against it, and its size decimals.
pub const PERP: &str = "HYPE";
pub const PERP_SIZE_DECIMALS: u32 = 2;
/// The HYPE the account keeps for its sends home, which the strategy never
/// sells. Every send of dollars from HyperCore back to the vault costs the
/// account a little HYPE, and the executor buys it from the account's own
/// dollars, never past one HYPE in all. The strategy can't tell that HYPE
/// from its own spot leg, so it leaves the most the executor may hold.
pub const HYPE_KEPT_FOR_SENDS_HOME: &str = "1";
/// The leverage the perp leg is held at: cross margin, and low, since the
/// spot leg is the hedge and the perp only needs to survive the basis.
pub const PERP_LEVERAGE: u32 = 2;

/// Funding, as a fraction of notional per day, at which carrying is worth it.
pub const ENTRY_FUNDING: &str = "0.0002";
/// And at which an open carry is unwound.
pub const EXIT_FUNDING: &str = "0.00005";
/// How much of the vault to aim to have working.
pub const TARGET_DEPLOYMENT: &str = "0.80";
/// Below this, an order is not worth the spread it would pay (and the venue
/// refuses anything under ten dollars).
pub const MINIMUM_NOTIONAL: &str = "100";
/// How far past the best price an immediate order may reach, as a fraction.
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
