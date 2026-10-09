//! The numbers this agent decides with. The claims in `shonin.toml` say the
//! same things in words.

/// The pool it provides liquidity in, as the driver names it, and the token
/// it pairs with the dollar.
pub const POOL: &str = "SOL/USD";
pub const TOKEN: &str = "SOL";

/// Each band reaches this far either side of the oracle's price, as a
/// fraction of it.
pub const HALF_WIDTH: &str = "0.04";
/// The band is set again once the pool's price is closer than this share of
/// the half-width to either edge (or outside it).
pub const RECENTER_AT: &str = "0.25";

/// At most this much of the vault in the pool.
pub const MAX_DEPLOYED: &str = "0.80";
/// Kept liquid in the vault's own cash, so an ordinary withdrawal takes
/// nothing out of the pool.
pub const IDLE_BUFFER: &str = "0.06";
/// Acts only while the pool's price is within this of the oracle's, as a
/// fraction.
pub const MAX_POOL_GAP: &str = "0.01";
/// How far past the oracle's price a rebalancing swap may fill, as a
/// fraction.
pub const SLIPPAGE: &str = "0.005";
/// Below this, in dollars, nothing is worth a transaction.
pub const MINIMUM_DOLLARS: &str = "5";
