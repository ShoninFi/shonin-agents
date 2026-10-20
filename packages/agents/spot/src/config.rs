//! The numbers this agent decides with.
//!
//! They are constants in the code rather than settings anywhere else, so the
//! reviewer reads them with the rest of the source, and the claims in each
//! `shonin.toml` say the same thing in words.

/// Where it trades, by the vault's chain as `shonin.toml` names it: the
/// driver it names there, and the weight of the vault each token is held
/// at. The rest is the dollar the driver trades against.
pub const VENUES: &[Venue] = &[
    Venue {
        chain: "robinhood",
        driver: "robinhood",
        weights: &[
            ("TSLA", "0.15"),
            ("AMD", "0.15"),
            ("AMZN", "0.15"),
            ("PLTR", "0.15"),
        ],
    },
    Venue {
        chain: "base",
        driver: "uniswap-base",
        weights: &[("WETH", "0.50")],
    },
];

/// One chain's driver and weights.
pub struct Venue {
    pub chain: &'static str,
    pub driver: &'static str,
    pub weights: &'static [(&'static str, &'static str)],
}

/// A token is put back to its weight once it is further than this from it,
/// as a fraction of the vault.
pub const BAND: &str = "0.03";
/// No single order above 5% of the vault.
pub const MAX_ORDER_FRACTION: &str = "0.05";
/// Below this many dollars, an order is not worth its fee.
pub const MINIMUM_NOTIONAL: &str = "1";
/// How far below the Chainlink price a swap may fill.
pub const SLIPPAGE: &str = "0.01";
/// A token whose pool is further than this from its Chainlink price is not
/// traded until it comes back.
pub const MAX_POOL_GAP: &str = "0.02";
/// 6% of the vault kept liquid, so an ordinary withdrawal closes nothing.
pub const IDLE_BUFFER: &str = "0.06";
/// Only sells after a 10% fall from the vault's peak.
pub const DRAWDOWN_STOP: &str = "0.10";

/// Looks at most this often, in seconds.
pub const STEP_EVERY: u64 = 60;
