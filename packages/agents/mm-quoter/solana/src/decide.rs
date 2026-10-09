//! Every decision the agent makes, as plain functions of what the venue
//! reports. Nothing here waits or sends anything.

use orca::{AccountState, Action, PoolState, PositionState, Side};
use sdk::{Decimal, decimal};
use strategy_sdk as sdk;
use strategy_sdk::orca;

use crate::config;

fn zero() -> Decimal {
    Decimal::from(0)
}

fn one() -> Decimal {
    Decimal::from(1)
}

fn abs(value: Decimal) -> Decimal {
    if value < zero() {
        zero() - value
    } else {
        value
    }
}

/// What to do next. One step at a time: every step is followed by a fresh
/// read of the account, so a step that fails or fills differently from what
/// was asked never leaves the agent believing something false.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    Nothing,
    /// Bring this many dollars from the vault's cash to its station.
    Fund(Decimal),
    Act(Vec<Action>),
}

/// The pool's price is close enough to the oracle's to trust it now.
fn pool_agrees(pool: &PoolState) -> bool {
    pool.oracle_price > zero()
        && abs(pool.price - pool.oracle_price) / pool.oracle_price <= decimal(config::MAX_POOL_GAP)
}

/// Whether a position holds liquidity.
fn live(position: &PositionState) -> bool {
    position.liquidity != "0"
}

/// Whether the pool's price is near enough an edge of `position`'s band (or
/// past it) to set the band again.
fn needs_recentring(position: &PositionState, price: Decimal) -> bool {
    let edge = (position.upper_price - position.lower_price) / Decimal::from(2)
        * decimal(config::RECENTER_AT);
    price < position.lower_price + edge || price > position.upper_price - edge
}

/// The step towards one band of liquidity around the oracle's price, or,
/// while `halting`, towards holding nothing but dollars.
pub fn plan(state: &AccountState, halting: bool) -> Plan {
    let Some(pool) = state.pool(config::POOL) else {
        return Plan::Nothing;
    };
    let minimum = decimal(config::MINIMUM_DOLLARS);
    let token = state.token(config::TOKEN);
    let ours: Vec<&PositionState> = state
        .positions
        .iter()
        .filter(|position| position.pool == config::POOL)
        .collect();

    if halting {
        let live: Vec<Action> = ours
            .iter()
            .filter(|position| live(position))
            .map(|position| Action::Remove {
                position: position.position.clone(),
                fraction: one(),
            })
            .collect();
        if !live.is_empty() {
            return Plan::Act(live);
        }
        if let Some(empty) = ours.first() {
            return Plan::Act(vec![Action::Close {
                position: empty.position.clone(),
            }]);
        }
        if token * pool.oracle_price >= minimum && pool_agrees(pool) {
            return Plan::Act(vec![sell(pool, token)]);
        }
        return Plan::Nothing;
    }

    // A pool whose price has strayed from the oracle's may have been pushed:
    // nothing is done in it until it comes back.
    if !pool_agrees(pool) {
        return Plan::Nothing;
    }

    if let Some(position) = ours.first() {
        if !live(position) {
            return Plan::Act(vec![Action::Close {
                position: position.position.clone(),
            }]);
        }
        if needs_recentring(position, pool.price) {
            return Plan::Act(vec![Action::Remove {
                position: position.position.clone(),
                fraction: one(),
            }]);
        }
        return Plan::Nothing;
    }

    // No band: bring the dollars it should hold to the station, balance the
    // two tokens at the pool, and open one around the oracle's price.
    let price = pool.oracle_price;
    let target = state.vault_value * decimal(config::MAX_DEPLOYED);
    let at_station = state.station_idle + token * price;
    let spare = state.idle - state.vault_value * decimal(config::IDLE_BUFFER);
    let short = target - at_station;
    if short >= minimum && spare >= minimum {
        return Plan::Fund(if short < spare { short } else { spare });
    }
    let deployable = if at_station < target {
        at_station
    } else {
        target
    };
    if deployable < minimum + minimum {
        return Plan::Nothing;
    }
    let half = deployable / Decimal::from(2);
    let held = token * price;
    if half - held >= minimum && state.station_idle > zero() {
        let dollars = half - held;
        return Plan::Act(vec![Action::Swap {
            pool: config::POOL.into(),
            side: Side::Buy,
            amount: if dollars < state.station_idle {
                dollars
            } else {
                state.station_idle
            },
            limit_price: price * (one() + decimal(config::SLIPPAGE)),
        }]);
    }
    if held - half >= minimum {
        return Plan::Act(vec![sell(pool, (held - half) / price)]);
    }
    let width = decimal(config::HALF_WIDTH);
    let dollars = if state.station_idle < half {
        state.station_idle
    } else {
        half
    };
    let tokens = if held < half { token } else { half / price };
    Plan::Act(vec![Action::Open {
        pool: config::POOL.into(),
        lower_price: price * (one() - width),
        upper_price: price * (one() + width),
        token: tokens,
        dollars,
    }])
}

fn sell(pool: &PoolState, amount: Decimal) -> Action {
    Action::Swap {
        pool: config::POOL.into(),
        side: Side::Sell,
        amount,
        limit_price: pool.oracle_price * (one() - decimal(config::SLIPPAGE)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca::PoolState;

    fn pool(price: &str, oracle: &str) -> PoolState {
        PoolState {
            pool: config::POOL.into(),
            token: config::TOKEN.into(),
            price: decimal(price),
            oracle_price: decimal(oracle),
            oracle_time: 0,
            fee: decimal("0.002"),
            tick_spacing: 64,
        }
    }

    fn state(vault: &str, idle: &str, station: &str, sol: &str) -> AccountState {
        AccountState {
            vault_value: decimal(vault),
            idle: decimal(idle),
            station_idle: decimal(station),
            value: zero(),
            exit_value: zero(),
            tokens: vec![(config::TOKEN.into(), decimal(sol))],
            positions: Vec::new(),
            pools: vec![pool("110", "110")],
            max_positions: 1,
        }
    }

    fn band(lower: &str, upper: &str, liquidity: &str) -> PositionState {
        PositionState {
            position: "position".into(),
            pool: config::POOL.into(),
            lower_price: decimal(lower),
            upper_price: decimal(upper),
            liquidity: liquidity.into(),
            token: zero(),
            dollars: zero(),
            fee_token: zero(),
            fee_dollars: zero(),
            in_range: true,
        }
    }

    #[test]
    fn a_vault_with_only_cash_brings_what_it_will_deploy_to_the_station_and_keeps_its_buffer() {
        assert_eq!(
            plan(&state("1000", "1000", "0", "0"), false),
            Plan::Fund(decimal("800")),
            "80% of the vault, leaving 6% and more"
        );
        // With little cash spare, only what the buffer allows.
        assert_eq!(
            plan(&state("1000", "100", "0", "0"), false),
            Plan::Fund(decimal("40"))
        );
    }

    #[test]
    fn dollars_at_the_station_are_balanced_into_the_token_before_a_band_opens() {
        let Plan::Act(actions) = plan(&state("1000", "200", "800", "0"), false) else {
            panic!("a swap");
        };
        assert!(matches!(
            &actions[..],
            [Action::Swap { side: Side::Buy, amount, limit_price, .. }]
                if *amount == decimal("400") && *limit_price < decimal("111")
        ));
    }

    #[test]
    fn a_balanced_station_opens_one_band_four_percent_either_side_of_the_oracle() {
        // 400 dollars and 400 dollars of SOL at 110.
        let sol = decimal("400") / decimal("110");
        let mut account = state("1000", "200", "400", "0");
        account.tokens = vec![(config::TOKEN.into(), sol)];
        let Plan::Act(actions) = plan(&account, false) else {
            panic!("an open");
        };
        match &actions[..] {
            [
                Action::Open {
                    lower_price,
                    upper_price,
                    dollars,
                    ..
                },
            ] => {
                assert_eq!(*lower_price, decimal("105.6"));
                assert_eq!(*upper_price, decimal("114.4"));
                assert_eq!(*dollars, decimal("400"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nothing_is_done_while_the_pool_strays_from_the_oracle() {
        let mut account = state("1000", "1000", "0", "0");
        account.pools = vec![pool("113", "110")];
        assert_eq!(plan(&account, false), Plan::Nothing);
    }

    #[test]
    fn a_band_is_taken_out_once_the_price_nears_its_edge_and_kept_while_it_is_central() {
        let mut account = state("1000", "200", "0", "0");
        account.positions = vec![band("105.6", "114.4", "1000")];
        account.pools = vec![pool("110.5", "110.5")];
        assert_eq!(plan(&account, false), Plan::Nothing);
        // A quarter of the half-width from the top edge is 113.3.
        account.pools = vec![pool("113.5", "113.5")];
        assert!(matches!(
            plan(&account, false),
            Plan::Act(actions) if matches!(&actions[..], [Action::Remove { fraction, .. }] if *fraction == one())
        ));
        // Emptied, it is closed.
        account.positions = vec![band("105.6", "114.4", "0")];
        assert!(matches!(
            plan(&account, false),
            Plan::Act(actions) if matches!(&actions[..], [Action::Close { .. }])
        ));
    }

    #[test]
    fn a_halting_vault_takes_its_liquidity_out_closes_its_band_and_sells_its_token() {
        let mut account = state("1000", "200", "0", "0");
        account.positions = vec![band("105.6", "114.4", "1000")];
        assert!(matches!(
            plan(&account, true),
            Plan::Act(actions) if matches!(&actions[..], [Action::Remove { .. }])
        ));
        account.positions = vec![band("105.6", "114.4", "0")];
        assert!(matches!(
            plan(&account, true),
            Plan::Act(actions) if matches!(&actions[..], [Action::Close { .. }])
        ));
        account.positions = Vec::new();
        account.tokens = vec![(config::TOKEN.into(), decimal("3"))];
        assert!(matches!(
            plan(&account, true),
            Plan::Act(actions) if matches!(&actions[..], [Action::Swap { side: Side::Sell, .. }])
        ));
        account.tokens = Vec::new();
        assert_eq!(plan(&account, true), Plan::Nothing);
    }

    #[test]
    fn the_manifest_claims_what_the_settings_hold() {
        let manifest = include_str!("../shonin.toml");
        assert!(manifest.contains("4% either side") && config::HALF_WIDTH == "0.04");
        assert!(manifest.contains("At most 80%") && config::MAX_DEPLOYED == "0.80");
        assert!(manifest.contains("6% of the vault kept liquid") && config::IDLE_BUFFER == "0.06");
        assert!(manifest.contains("within 1%") && config::MAX_POOL_GAP == "0.01");
    }
}
