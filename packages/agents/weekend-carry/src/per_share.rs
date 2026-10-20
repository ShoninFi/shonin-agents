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
//! and the stop holds until the agent next starts: nothing lifts it. The
//! measure goes on following what the vault actually holds, so once sold
//! out it stays where the selling left it. A restart starts the measure,
//! and the stop with it, afresh.

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
/// started, the highest it has been since, and whether the stop has come.
#[derive(Clone, Debug, PartialEq)]
pub struct PerShare {
    pub now: Decimal,
    pub peak: Decimal,
    /// The fall from the peak, as a fraction of it, that stops the agent.
    stop: Decimal,
    /// The stop came at a look since the agent started.
    stopped: bool,
    /// The last look taken: the vault's value, and what it held.
    last: Option<(Decimal, Vec<Held>)>,
}

impl PerShare {
    /// A share measured from one, stopped by a fall of `stop` from its peak
    /// (a fraction of the peak).
    pub fn new(stop: &str) -> Self {
        Self {
            now: Decimal::from(1u8),
            peak: Decimal::from(1u8),
            stop: decimal(stop),
            stopped: false,
            last: None,
        }
    }

    /// Whether the stop holds: a share fell by the stop from its peak at a
    /// look since the agent started. Only a restart clears it.
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Take a look at the vault: worth `value` in all as its accounts read,
    /// and `accounted` as its holdings and cash say (none when one of its
    /// accounts couldn't be read), holding `held`. A holding with no value
    /// now counts no move. A look whose holdings don't account for its value
    /// within [`ACCOUNTED_WITHIN`] is skipped: nothing moves, and the next
    /// look counts from the last one taken. Says whether it was taken.
    pub fn look(&mut self, value: Decimal, accounted: Option<Decimal>, held: Vec<Held>) -> bool {
        let zero = Decimal::from(0u8);
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
        if self.peak - self.now >= self.peak * self.stop {
            self.stopped = true;
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
        PerShare::new("0.10")
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
    fn once_stopped_the_stop_holds_and_the_share_follows_only_what_the_vault_holds() {
        let mut share = share();
        look(&mut share, "1000", vec![held("A", "2", "250")]);
        // A down 25%: the share 12.5% down, stopped.
        look(&mut share, "875", vec![held("A", "2", "187.5")]);
        assert!(share.stopped());
        // Sold out: A's recovery is no gain of the vault's, so the share
        // stays where the selling left it, and the stop holds.
        look(&mut share, "875", vec![held("A", "0", "187.5")]);
        look(&mut share, "875", vec![held("A", "0", "250")]);
        look(&mut share, "875", vec![held("A", "0", "400")]);
        assert_eq!(share.now, decimal("0.875"));
        assert!(share.stopped());
        // Even a share back above its peak doesn't lift it.
        look(&mut share, "875", vec![held("A", "1", "400")]);
        look(&mut share, "1075", vec![held("A", "1", "600")]);
        assert!(share.now > share.peak * decimal("0.95"), "{}", share.now);
        assert!(share.stopped());
        // A restart starts the measure, and the stop, afresh.
        let share = PerShare::new("0.10");
        assert!(!share.stopped());
        assert_eq!(share.now, decimal("1"));
    }
}
