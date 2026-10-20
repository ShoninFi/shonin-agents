//! The value of a vault share, as the agent measures it.
//!
//! The agent can't read the vault's share price, so it measures a share's
//! value itself: from one look to the next, a share gains (or loses) what
//! the moves of the prices since the last look did to what the vault held
//! then, as a fraction of the vault's value then. Deposits and withdrawals
//! change how much the vault holds but no price, so they never move it. It
//! starts at one each time the agent starts, since the agent's state starts
//! empty: a fall from before a restart is not remembered.

use strategy_sdk::Decimal;

/// One thing the vault holds, at one look.
#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    /// What it is: a token, a market.
    pub what: String,
    /// How much is held: negative when short.
    pub quantity: Decimal,
    /// What one unit is worth now. Its move from one look to the next, times
    /// the quantity, is what holding it made.
    pub unit_value: Decimal,
}

/// The value of a vault share as a multiple of its value when the agent
/// started, and the highest it has been since.
#[derive(Clone, Debug, PartialEq)]
pub struct PerShare {
    pub now: Decimal,
    pub peak: Decimal,
    /// The last look: the vault's value, and what it held.
    last: Option<(Decimal, Vec<Held>)>,
}

impl Default for PerShare {
    fn default() -> Self {
        Self {
            now: Decimal::from(1u8),
            peak: Decimal::from(1u8),
            last: None,
        }
    }
}

impl PerShare {
    /// Take a look at the vault, worth `value` in all, holding `held`. A
    /// holding with no value now counts no move.
    pub fn look(&mut self, value: Decimal, held: Vec<Held>) {
        let zero = Decimal::from(0u8);
        if let Some((before, then)) = &self.last
            && *before > zero
        {
            let moved = then.iter().fold(zero, |sum, old| {
                match held.iter().find(|new| new.what == old.what) {
                    Some(new) => sum + old.quantity * (new.unit_value - old.unit_value),
                    None => sum,
                }
            });
            let grown = self.now * (Decimal::from(1u8) + moved / *before);
            self.now = if grown > zero { grown } else { zero };
            if self.now > self.peak {
                self.peak = self.now;
            }
        }
        self.last = Some((value, held));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strategy_sdk::decimal;

    fn held(what: &str, quantity: &str, unit_value: &str) -> Held {
        Held {
            what: what.into(),
            quantity: decimal(quantity),
            unit_value: decimal(unit_value),
        }
    }

    #[test]
    fn deposits_and_withdrawals_never_move_a_share_and_price_moves_do() {
        let mut share = PerShare::default();
        share.look(decimal("1000"), vec![held("A", "2", "250")]);
        // Half the vault leaves, and twice as much comes in: no price moved.
        share.look(decimal("500"), vec![held("A", "1", "250")]);
        share.look(decimal("1500"), vec![held("A", "3", "250")]);
        assert_eq!(share.now, decimal("1"));
        // A up 10%: three units of it in a $1,500 vault, $75 gained, 5%.
        share.look(decimal("1575"), vec![held("A", "3", "275")]);
        assert_eq!(share.now, decimal("1.05"));
        assert_eq!(share.peak, decimal("1.05"));
        // A short gains when the price falls.
        share.look(decimal("1575"), vec![held("A", "-3", "275")]);
        share.look(decimal("1650"), vec![held("A", "-3", "250")]);
        assert!(share.now > decimal("1.05"));
    }
}
