//! What the agent would like to hold, worked out from what it sees: no I/O,
//! so every rule is tested here.

use strategy_sdk::{Decimal, decimal};

use crate::config;

pub fn zero() -> Decimal {
    Decimal::from(0)
}

fn abs(value: Decimal) -> Decimal {
    if value < zero() { -value } else { value }
}

/// Whether `unix_secs` falls in the weekend window: from Friday 21:00 UTC to
/// Monday 13:30 UTC.
pub fn in_weekend(unix_secs: u64) -> bool {
    let days = unix_secs / 86_400;
    // The epoch was a Thursday: 0 is Sunday, 5 is Friday.
    let weekday = ((days + 4) % 7) as u8;
    let second = (unix_secs % 86_400) as u32;
    let (from_day, from_second) = config::WEEKEND_FROM;
    let (until_day, until_second) = config::WEEKEND_UNTIL;
    match weekday {
        d if d == from_day => second >= from_second,
        6 | 0 => true,
        d if d == until_day => second < until_second,
        _ => false,
    }
}

/// One underlying, as seen now.
#[derive(Clone, Debug, PartialEq)]
pub struct Name {
    pub token: String,
    pub perp: String,
    /// The token's market price (its pool's), in dollars per token.
    pub token_price: Decimal,
    /// Shares of the stock per token.
    pub multiplier: Decimal,
    /// The issuer has paused the token's oracle: nothing is opened.
    pub paused: bool,
    /// The perpetual's best bid and ask, per share.
    pub perp_bid: Decimal,
    pub perp_ask: Decimal,
    /// Held now: tokens in the Safe, and the perpetual (negative is short).
    pub tokens: Decimal,
    pub perp_position: Decimal,
}

impl Name {
    /// What one share of the stock costs as the token.
    pub fn share_price(&self) -> Decimal {
        if self.multiplier <= zero() {
            return zero();
        }
        self.token_price / self.multiplier
    }

    /// How far the perpetual trades above the token, per share: the bid
    /// (what shorting it fetches) against the token's price.
    pub fn premium(&self) -> Decimal {
        let share = self.share_price();
        if share <= zero() || self.perp_bid <= zero() {
            return zero();
        }
        (self.perp_bid - share) / share
    }
}

/// The whole vault, as seen now.
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub now: u64,
    /// Everything the vault is worth, everywhere.
    pub vault_value: Decimal,
    pub peak: Decimal,
    pub names: Vec<Name>,
}

/// One step towards what the agent would like to hold.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Buy `tokens` of the token (spending about `dollars`), then short the
    /// matching shares of the perpetual.
    Open {
        index: usize,
        dollars: Decimal,
        tokens: Decimal,
        shares: Decimal,
    },
    /// Buy back `shares` of the perpetual, then sell `tokens`: the short
    /// always goes first.
    Close {
        index: usize,
        shares: Decimal,
        tokens: Decimal,
    },
    Nothing,
}

impl Picture {
    pub fn only_reducing(&self) -> bool {
        self.peak > zero()
            && (self.peak - self.vault_value) / self.peak >= decimal(config::DRAWDOWN_STOP)
    }

    fn gross(&self) -> Decimal {
        self.names
            .iter()
            .map(|name| name.tokens * name.token_price + abs(name.perp_position) * name.perp_ask)
            .fold(zero(), |a, b| a + b)
    }

    /// The next step: a close first wherever one is due, else an opening
    /// where one is worth it, one order's worth at a time.
    pub fn next(&self) -> Step {
        let per_order = self.vault_value * decimal(config::MAX_ORDER_FRACTION);
        let weekend = in_weekend(self.now);
        let reducing = self.only_reducing();
        // Closes come first, everywhere.
        for (index, name) in self.names.iter().enumerate() {
            let carrying = name.tokens > zero() || name.perp_position < zero();
            let due = reducing || !weekend || name.premium() <= decimal(config::EXIT_PREMIUM);
            if carrying && due {
                let short = abs(name.perp_position.min(zero()));
                let per_share = name.perp_ask.max(name.share_price());
                let cap = if per_share > zero() {
                    per_order / per_share
                } else {
                    short
                };
                let shares = short.min(cap);
                // The tokens go only as far as the short already gone leaves
                // them hedged: never a naked short, never more than the
                // shares closed in this step allow.
                let left_short = short - shares;
                let tokens_hedged = if name.multiplier > zero() {
                    left_short / name.multiplier
                } else {
                    zero()
                };
                let tokens = if name.tokens > tokens_hedged {
                    name.tokens - tokens_hedged
                } else {
                    zero()
                };
                if shares > zero() || tokens > zero() {
                    return Step::Close {
                        index,
                        shares,
                        tokens,
                    };
                }
            }
        }
        if !weekend || reducing {
            return Step::Nothing;
        }
        let room = self.vault_value * decimal(config::MAX_GROSS_LEVERAGE) - self.gross();
        let per_name = self.vault_value * decimal(config::TARGET_DEPLOYMENT)
            / Decimal::from(self.names.len().max(1) as u64);
        for (index, name) in self.names.iter().enumerate() {
            if name.paused || name.premium() < decimal(config::ENTRY_PREMIUM) {
                continue;
            }
            let held = name.tokens * name.token_price;
            if held >= per_name {
                continue;
            }
            // Both legs count against the gross limit.
            let dollars = (per_name - held)
                .min(per_order)
                .min(room / Decimal::from(2u8));
            if dollars < decimal(config::MINIMUM_NOTIONAL) || name.token_price <= zero() {
                continue;
            }
            let tokens = dollars / name.token_price;
            return Step::Open {
                index,
                dollars,
                tokens,
                shares: tokens * name.multiplier,
            };
        }
        Step::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saturday 3 October 2026, 12:00 UTC.
    const SATURDAY: u64 = 1_791_028_800;

    fn spy(perp_bid: &str) -> Name {
        Name {
            token: "SPY".into(),
            perp: "SPY".into(),
            token_price: decimal("700"),
            multiplier: decimal("1"),
            paused: false,
            perp_bid: decimal(perp_bid),
            perp_ask: decimal(perp_bid) + decimal("0.05"),
            tokens: zero(),
            perp_position: zero(),
        }
    }

    fn picture(now: u64, names: Vec<Name>) -> Picture {
        Picture {
            now,
            vault_value: decimal("100000"),
            peak: decimal("100000"),
            names,
        }
    }

    #[test]
    fn the_weekend_runs_from_friday_evening_to_mondays_open() {
        assert!(in_weekend(SATURDAY));
        assert!(in_weekend(SATURDAY + 86_400), "Sunday");
        let friday_noon = SATURDAY - 86_400;
        assert!(!in_weekend(friday_noon));
        assert!(in_weekend(friday_noon + 9 * 3600 + 1), "Friday after 21:00");
        let monday_midnight = SATURDAY + 2 * 86_400 - 12 * 3600;
        assert!(in_weekend(monday_midnight + 13 * 3600));
        assert!(
            !in_weekend(monday_midnight + 13 * 3600 + 1800),
            "Monday's open"
        );
        assert!(!in_weekend(SATURDAY + 4 * 86_400), "Wednesday");
    }

    #[test]
    fn a_premium_over_the_entry_opens_one_orders_worth_hedged_share_for_share() {
        let step = picture(SATURDAY, vec![spy("704")]).next();
        let Step::Open {
            dollars,
            tokens,
            shares,
            ..
        } = step
        else {
            panic!("{step:?}")
        };
        assert_eq!(dollars, decimal("5000"), "one order: 5% of the vault");
        assert_eq!(shares, tokens, "a token is one share here");
    }

    #[test]
    fn nothing_opens_outside_the_weekend_below_the_entry_or_on_a_paused_oracle() {
        assert_eq!(
            picture(SATURDAY + 4 * 86_400, vec![spy("704")]).next(),
            Step::Nothing
        );
        assert_eq!(picture(SATURDAY, vec![spy("701")]).next(), Step::Nothing);
        let mut paused = spy("704");
        paused.paused = true;
        assert_eq!(picture(SATURDAY, vec![paused]).next(), Step::Nothing);
    }

    #[test]
    fn a_carry_closes_short_first_and_never_leaves_tokens_short_of_the_hedge() {
        let mut held = spy("700.2");
        held.tokens = decimal("20");
        held.perp_position = decimal("-20");
        let step = picture(SATURDAY, vec![held]).next();
        // The premium is below the exit: close, at most an order's worth.
        let Step::Close { shares, tokens, .. } = step else {
            panic!("{step:?}")
        };
        assert!(shares > zero());
        assert_eq!(tokens, shares, "only as many tokens as the short closed");
    }

    #[test]
    fn after_a_ten_percent_fall_it_only_reduces() {
        let mut held = spy("710");
        held.tokens = decimal("5");
        held.perp_position = decimal("-5");
        let mut p = picture(SATURDAY, vec![held]);
        p.vault_value = decimal("89000");
        assert!(p.only_reducing());
        assert!(matches!(p.next(), Step::Close { .. }));
    }

    #[test]
    fn mondays_open_closes_every_carry() {
        let mut held = spy("710");
        held.tokens = decimal("5");
        held.perp_position = decimal("-5");
        let monday_open = SATURDAY + 2 * 86_400 - 12 * 3600 + 14 * 3600;
        assert!(matches!(
            picture(monday_open, vec![held]).next(),
            Step::Close { .. }
        ));
    }

    #[test]
    fn gross_exposure_stays_within_twice_the_vault() {
        let mut held = spy("710");
        // Already 199k gross of a 100k vault.
        held.tokens = decimal("142");
        held.perp_position = decimal("-142");
        held.token_price = decimal("700");
        let mut other = spy("710");
        other.token = "QQQ".into();
        let step = picture(SATURDAY, vec![held, other]).next();
        assert_eq!(step, Step::Nothing, "{step:?}");
    }
}
