//! The value of a vault share, as the agent measures it, and the stop on it.
//!
//! The agent can't read the vault's share price, so it measures a share's
//! value itself: from one look to the next, a share gains (or loses) what
//! the moves of the prices since the last look did to what the vault held
//! then, as a fraction of the vault's value then. Deposits and withdrawals
//! change how much the vault holds but no price, so they never move it. It
//! starts at one each time the agent starts, since the agent's state starts
//! empty: a fall from before a restart is not remembered.
//!
//! A look whose holdings don't account for the vault's value is skipped and
//! counts nothing: one of the reads behind it failed or came back partial,
//! and a move divided by a value read short would count many times over.
//!
//! Once a share has fallen by the stop from its peak, the agent is stopped,
//! and sells what the stop has it sell. From then on, since what it sold no
//! longer moves, a share is measured as if the vault had kept what it held
//! at the look that stopped it. The stop lifts once that measure is back
//! within a smaller fall of the peak, so the agent buys back in only after
//! what it held has recovered most of the fall.

use strategy_sdk::{Decimal, decimal};

/// How far the vault's value as read may be from what its holdings account
/// for, as a fraction of the value read, before a look is skipped.
const ACCOUNTED_WITHIN: &str = "0.05";

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
/// started, the highest it has been since, and whether the stop holds.
#[derive(Clone, Debug, PartialEq)]
pub struct PerShare {
    pub now: Decimal,
    pub peak: Decimal,
    /// The fall from the peak, as a fraction of it, that stops the agent.
    stop: Decimal,
    /// The fall from the peak, as a fraction of it, within which the
    /// measure must be back for the stop to lift.
    lift: Decimal,
    /// The last look taken: the vault's value, and what it held.
    last: Option<(Decimal, Vec<Held>)>,
    /// While stopped: what the vault held at the look that stopped it, and
    /// what that would be worth now with the vault's value then.
    kept: Option<(Decimal, Vec<Held>)>,
}

impl PerShare {
    /// A share measured from one, stopped by a fall of `stop` from its peak
    /// and lifted once back within `lift` of it (fractions of the peak).
    pub fn new(stop: &str, lift: &str) -> Self {
        Self {
            now: Decimal::from(1u8),
            peak: Decimal::from(1u8),
            stop: decimal(stop),
            lift: decimal(lift),
            last: None,
            kept: None,
        }
    }

    /// Whether the stop holds: a share fell by the stop from its peak, and
    /// hasn't come back within the lift of it since.
    pub fn stopped(&self) -> bool {
        self.kept.is_some()
    }

    /// Take a look at the vault: worth `value` in all as its accounts read,
    /// and `accounted` as its holdings and cash say (none when one of its
    /// accounts couldn't be read), holding `held`. A holding with no value
    /// now counts no move. A look whose holdings don't account for its value
    /// within [`ACCOUNTED_WITHIN`] is skipped: nothing moves, and the next
    /// look counts from the last one taken. Says whether it was taken.
    pub fn look(&mut self, value: Decimal, accounted: Option<Decimal>, held: Vec<Held>) -> bool {
        let zero = Decimal::from(0u8);
        let one = Decimal::from(1u8);
        let Some(accounted) = accounted else {
            return false;
        };
        let gap = if value > accounted {
            value - accounted
        } else {
            accounted - value
        };
        if value <= zero || gap > value * decimal(ACCOUNTED_WITHIN) {
            return false;
        }
        let moved = |then: &[Held]| {
            then.iter().fold(zero, |sum, old| {
                match held.iter().find(|new| new.what == old.what) {
                    Some(new) => sum + old.quantity * (new.unit_value - old.unit_value),
                    None => sum,
                }
            })
        };
        let grown = match (&mut self.kept, &self.last) {
            (Some((worth, kept)), _) => {
                let made = moved(kept);
                let grown = (*worth > zero).then(|| self.now * (one + made / *worth));
                *worth += made;
                for old in kept.iter_mut() {
                    if let Some(new) = held.iter().find(|new| new.what == old.what) {
                        old.unit_value = new.unit_value;
                    }
                }
                grown
            }
            (None, Some((before, then))) if *before > zero => {
                Some(self.now * (one + moved(then) / *before))
            }
            _ => None,
        };
        if let Some(grown) = grown {
            self.now = if grown > zero { grown } else { zero };
            if self.now > self.peak {
                self.peak = self.now;
            }
        }
        let fallen = self.peak - self.now;
        if self.kept.is_none() && fallen >= self.peak * self.stop {
            self.kept = Some((value, held.clone()));
        } else if self.kept.is_some() && fallen <= self.peak * self.lift {
            self.kept = None;
        }
        self.last = Some((value, held));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(what: &str, quantity: &str, unit_value: &str) -> Held {
        Held {
            what: what.into(),
            quantity: decimal(quantity),
            unit_value: decimal(unit_value),
        }
    }

    fn share() -> PerShare {
        PerShare::new("0.10", "0.05")
    }

    /// A look whose holdings account for its value exactly.
    fn look(share: &mut PerShare, value: &str, held: Vec<Held>) -> bool {
        share.look(decimal(value), Some(decimal(value)), held)
    }

    #[test]
    fn deposits_and_withdrawals_never_move_a_share_and_price_moves_do() {
        let mut share = share();
        look(&mut share, "1000", vec![held("A", "2", "250")]);
        // Half the vault leaves, and twice as much comes in: no price moved.
        look(&mut share, "500", vec![held("A", "1", "250")]);
        look(&mut share, "1500", vec![held("A", "3", "250")]);
        assert_eq!(share.now, decimal("1"));
        // A up 10%: three units of it in a $1,500 vault, $75 gained, 5%.
        look(&mut share, "1575", vec![held("A", "3", "275")]);
        assert_eq!(share.now, decimal("1.05"));
        assert_eq!(share.peak, decimal("1.05"));
        // A short gains when the price falls.
        look(&mut share, "1575", vec![held("A", "-3", "275")]);
        look(&mut share, "1650", vec![held("A", "-3", "250")]);
        assert!(share.now > decimal("1.05"));
    }

    #[test]
    fn a_look_whose_holdings_do_not_account_for_its_value_never_moves_a_share() {
        let mut share = share();
        // $1,000: $500 of A, $500 of cash.
        look(&mut share, "1000", vec![held("A", "2", "250")]);
        // One account went unread: the value read is the cash alone.
        assert!(!share.look(decimal("500"), None, vec![held("A", "2", "250")]));
        // Read short without saying so: $60 against the $1,000 held.
        assert!(!share.look(
            decimal("60"),
            Some(decimal("1000")),
            vec![held("A", "2", "240")]
        ));
        assert_eq!(share.now, decimal("1"));
        // Read whole again, A 1.5% under where the last look taken saw it:
        // the move counts once, against the $1,000 of that look.
        assert!(look(&mut share, "992.5", vec![held("A", "2", "246.25")]));
        assert_eq!(share.now, decimal("0.9925"));
        assert!(!share.stopped());
    }

    #[test]
    fn once_stopped_a_share_moves_with_what_was_held_and_the_stop_lifts_near_the_peak() {
        let mut share = share();
        look(&mut share, "1000", vec![held("A", "2", "250")]);
        // A down 25%: the share 12.5% down, stopped.
        look(&mut share, "875", vec![held("A", "2", "187.5")]);
        assert!(share.stopped());
        // Sold out: nothing held moves, but the measure follows A as held.
        look(&mut share, "875", vec![held("A", "0", "187.5")]);
        look(&mut share, "875", vec![held("A", "0", "200")]);
        assert!(share.stopped(), "back to {} only", share.now);
        // A back where it was: the share would be whole again; lifted.
        look(&mut share, "875", vec![held("A", "0", "250")]);
        assert!(share.now > decimal("0.9999"), "{}", share.now);
        assert!(!share.stopped());
        // From here on, what the vault holds moves it again.
        look(&mut share, "875", vec![held("A", "1", "250")]);
        look(&mut share, "885", vec![held("A", "1", "260")]);
        assert!(share.now > decimal("1"));
    }
}
