//! How big a position an agent will take, and what stops it.
//!
//! The limits here are the strategy's own, written where a reader and a
//! reviewer can both find them. They are deliberately dull: a ceiling on
//! leverage, a ceiling on any one order, a buffer kept liquid so most
//! withdrawals never have to close anything, and a fall from the peak after
//! which the strategy stops opening.

use strategy_sdk::Decimal;

/// What a strategy will not do, whatever it sees.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Gross exposure, as a multiple of the vault's value.
    pub max_gross_leverage: Decimal,
    /// The most any one order may be worth, as a fraction of the vault.
    pub max_order_fraction: Decimal,
    /// How much of the vault to leave liquid, so an ordinary withdrawal is
    /// paid without closing anything.
    pub idle_buffer: Decimal,
    /// A fall from the vault's peak after which the strategy only reduces.
    pub drawdown_stop: Decimal,
}

/// How much notional to add, given what the vault is worth, what is already
/// on, and the limits above.
///
/// It never returns more than the limits allow, and it returns nothing at all
/// rather than a size too small to be worth the spread.
pub fn size_for(
    wanted_notional: Decimal,
    vault_value: Decimal,
    gross_open: Decimal,
    limits: &Limits,
    minimum_notional: Decimal,
) -> Decimal {
    let zero = Decimal::from(0);
    if vault_value <= zero || wanted_notional <= zero {
        return zero;
    }

    let ceiling = vault_value * limits.max_gross_leverage;
    let headroom = if gross_open >= ceiling {
        zero
    } else {
        ceiling - gross_open
    };
    let per_order = vault_value * limits.max_order_fraction;

    let mut size = wanted_notional;
    if size > headroom {
        size = headroom;
    }
    if size > per_order {
        size = per_order;
    }
    if size < minimum_notional { zero } else { size }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strategy_sdk::decimal;

    fn limits() -> Limits {
        Limits {
            max_gross_leverage: decimal("2"),
            max_order_fraction: decimal("0.05"),
            idle_buffer: decimal("0.05"),
            drawdown_stop: decimal("0.10"),
        }
    }

    #[test]
    fn an_order_is_capped_at_its_share_of_the_vault() {
        let size = size_for(
            decimal("50000"),
            decimal("100000"),
            decimal("0"),
            &limits(),
            decimal("100"),
        );
        assert_eq!(size, decimal("5000"));
    }

    #[test]
    fn nothing_is_added_once_the_leverage_ceiling_is_reached() {
        let size = size_for(
            decimal("1000"),
            decimal("100000"),
            decimal("200000"),
            &limits(),
            decimal("100"),
        );
        assert_eq!(size, decimal("0"));
    }

    #[test]
    fn the_headroom_left_is_what_can_still_be_added() {
        let size = size_for(
            decimal("5000"),
            decimal("100000"),
            decimal("198000"),
            &limits(),
            decimal("100"),
        );
        assert_eq!(size, decimal("2000"));
    }

    #[test]
    fn an_order_too_small_to_be_worth_the_spread_is_not_sent() {
        let size = size_for(
            decimal("50"),
            decimal("100000"),
            decimal("0"),
            &limits(),
            decimal("100"),
        );
        assert_eq!(size, decimal("0"));
    }

    #[test]
    fn an_empty_vault_trades_nothing() {
        let size = size_for(
            decimal("1000"),
            decimal("0"),
            decimal("0"),
            &limits(),
            decimal("100"),
        );
        assert_eq!(size, decimal("0"));
    }
}
