//! What the agent would like to hold, worked out from what it sees: no I/O,
//! so every rule is tested here.

use strategy_sdk::{Decimal, decimal};

use crate::config;
use crate::per_share::Held;

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
    /// Chainlink's price for the token, which the vault is valued at.
    pub feed_price: Decimal,
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
    /// The value of a vault share now, and the highest it has been since
    /// the agent started, each as a multiple of its value then (see
    /// [`PerShare`](crate::per_share::PerShare)).
    pub share: Decimal,
    pub peak: Decimal,
    /// The stop came at a look since the agent started: it holds until the
    /// agent restarts (see [`PerShare`](crate::per_share::PerShare)).
    pub stop_holds: bool,
    /// The least the vault is surely worth, from the accounts that read
    /// (see `vault_value`): at a look the measure skipped, `vault_value` may
    /// be read short of it.
    pub least_value: Decimal,
    /// What the vault is short is known: Lighter's positions were read, or
    /// Lighter holds nothing.
    pub shorts_known: bool,
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

/// What the vault holds of `names`, for
/// [`PerShare`](crate::per_share::PerShare): each token at its Chainlink
/// price, and each perpetual at the middle of its book. One without a price
/// counts no move.
pub fn held(names: &[Name]) -> Vec<Held> {
    let mut held = Vec::new();
    for name in names {
        if name.feed_price > zero() {
            held.push(Held {
                what: name.token.clone(),
                quantity: name.tokens,
                unit_value: name.feed_price,
            });
        }
        if name.perp_bid > zero() && name.perp_ask > zero() {
            held.push(Held {
                what: format!("perp:{}", name.perp),
                quantity: name.perp_position,
                unit_value: (name.perp_bid + name.perp_ask) / Decimal::from(2u8),
            });
        }
    }
    held
}

impl Picture {
    /// Whether a vault share's value has fallen by the stop from its peak,
    /// now or at any earlier look since the agent started.
    pub fn only_reducing(&self) -> bool {
        self.stop_holds
            || (self.peak > zero()
                && (self.peak - self.share) / self.peak >= decimal(config::DRAWDOWN_STOP))
    }

    fn gross(&self) -> Decimal {
        self.names
            .iter()
            .map(|name| name.tokens * name.token_price + abs(name.perp_position) * name.perp_ask)
            .fold(zero(), |a, b| a + b)
    }

    /// The next step of a halt, at a look the share's measure took or
    /// skipped (`looked`): a close wherever anything is carried, whatever the
    /// weekend, the premium or the share's measure. It reads what the Safe
    /// holds, Lighter's positions, the prices and books, and the vault's value
    /// for the order cap. At a skipped look the value read may be short, so
    /// the cap is taken from no less than [`Picture::least_value`]. While the shorts aren't known nothing is
    /// closed, since selling a token whose short is still open would leave
    /// the short naked.
    pub fn halting(&self, looked: bool) -> Step {
        if !self.shorts_known {
            return Step::Nothing;
        }
        let mut picture = self.clone();
        picture.stop_holds = true;
        if !looked {
            picture.vault_value = self.vault_value.max(self.least_value);
        }
        picture.next()
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
            feed_price: decimal("700"),
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
            share: decimal("1"),
            peak: decimal("1"),
            stop_holds: false,
            least_value: zero(),
            shorts_known: true,
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
        p.share = decimal("0.89");
        assert!(p.only_reducing());
        assert!(matches!(p.next(), Step::Close { .. }));
    }

    #[test]
    fn the_manifest_claims_the_stop_as_a_fall_in_a_share_s_value() {
        let manifest = include_str!("../shonin.toml");
        assert!(
            manifest.contains("a 10% fall in a vault share's value")
                && config::DRAWDOWN_STOP == "0.10"
        );
        assert!(manifest.contains("a fall from before a restart is not remembered"));
        assert!(!manifest.contains("fall from the vault's peak"));
        assert!(manifest.contains("couldn't be read"));
        assert!(
            manifest.contains("the stop holds until then, and a restart measures a share afresh")
        );
        assert!(manifest.contains("opens nothing until the agent restarts"));
        assert!(!manifest.contains("lifts"));
    }

    /// The share's measure, as the agent keeps it.
    fn measure() -> crate::per_share::PerShare {
        crate::per_share::PerShare::new(config::DRAWDOWN_STOP)
    }

    /// `p` with the share's value, peak and stop `per_share` has after a
    /// look at it whose accounts all read and add up.
    fn looked(per_share: &mut crate::per_share::PerShare, mut p: Picture) -> Picture {
        per_share.look(p.vault_value, Some(p.vault_value), held(&p.names));
        p.share = per_share.now;
        p.peak = per_share.peak;
        p.stop_holds = per_share.stopped();
        p
    }

    #[test]
    fn a_look_with_an_account_unread_never_moves_a_share() {
        let mut per_share = measure();
        let mut carry = spy("710");
        carry.tokens = decimal("100");
        carry.perp_position = decimal("-100");
        looked(&mut per_share, picture(SATURDAY, vec![carry.clone()]));
        // The Safe's value went unread: the balances add up to $6,000, and
        // the perpetual has moved $5 against the short meanwhile.
        let mut moved = carry.clone();
        moved.perp_bid = decimal("715");
        moved.perp_ask = decimal("715.05");
        assert!(!per_share.look(decimal("6000"), None, held(&[moved.clone()])));
        assert_eq!(per_share.now, decimal("1"));
        // Read whole again: the $500 lost counts once, on $100,000.
        let mut whole = picture(SATURDAY, vec![moved]);
        whole.vault_value = decimal("99500");
        let whole = looked(&mut per_share, whole);
        assert_eq!(whole.share, decimal("0.995"));
        assert!(!whole.only_reducing());
    }

    #[test]
    fn once_both_legs_are_cut_the_stop_holds_whatever_the_pair_does_after() {
        let mut per_share = measure();
        let mut carry = spy("710");
        carry.tokens = decimal("100");
        carry.perp_position = decimal("-100");
        looked(&mut per_share, picture(SATURDAY, vec![carry.clone()]));
        // The gap widens $110 a share: 11% lost, stopped.
        carry.perp_bid = decimal("820");
        carry.perp_ask = decimal("820.05");
        let mut cut = picture(SATURDAY, vec![carry.clone()]);
        cut.vault_value = decimal("89000");
        assert!(looked(&mut per_share, cut.clone()).only_reducing());
        // Both legs cut: nothing held moves.
        carry.tokens = zero();
        carry.perp_position = zero();
        cut.names = vec![carry.clone()];
        let share = looked(&mut per_share, cut.clone()).share;
        // The gap closes again, wide enough to open on: the pair wasn't
        // held, so the share stays where the cut left it, and nothing opens.
        for bid in ["710", "760", "710"] {
            carry.perp_bid = decimal(bid);
            carry.perp_ask = decimal(bid) + decimal("0.05");
            cut.names = vec![carry.clone()];
            let now = looked(&mut per_share, cut.clone());
            assert!(now.only_reducing(), "lifted at {bid}, share {}", now.share);
            assert_eq!(now.share, share);
            assert_eq!(now.next(), Step::Nothing, "at {bid}");
        }
        // A restart measures afresh, and may open again.
        let mut restarted = measure();
        assert!(!looked(&mut restarted, cut).only_reducing());
    }

    #[test]
    fn a_halt_closes_at_a_skipped_look_but_never_sells_a_token_whose_short_is_unknown() {
        let mut carry = spy("710");
        carry.tokens = decimal("100");
        carry.perp_position = decimal("-100");
        // A look the measure skipped: the vault's value read as nothing,
        // though the Safe's holdings say it holds at least $70,000.
        let mut skipped = picture(SATURDAY, vec![carry]);
        skipped.vault_value = zero();
        skipped.least_value = decimal("70000");
        // The cap comes from the $70,000: $3,500, about 4 shares.
        let Step::Close { shares, .. } = skipped.halting(false) else {
            panic!("{:?}", skipped.halting(false))
        };
        assert!(shares > zero() && shares < decimal("5"), "{shares}");
        // A halt closes outside the weekend and at any premium too.
        skipped.now = SATURDAY + 4 * 86_400;
        assert!(matches!(skipped.halting(false), Step::Close { .. }));
        // Lighter's positions unread: nothing closes, not even the token.
        skipped.shorts_known = false;
        assert_eq!(skipped.halting(false), Step::Nothing);
    }

    #[test]
    fn a_leaver_taking_a_tenth_of_the_vault_does_not_stop_the_agent() {
        let mut per_share = measure();
        let mut looked = |p: Picture| looked(&mut per_share, p);
        // A $100,000 vault carrying 100 SPY tokens against 100 shorted.
        let mut carry = spy("710");
        carry.tokens = decimal("100");
        carry.perp_position = decimal("-100");
        let before = looked(picture(SATURDAY, vec![carry.clone()]));
        assert_eq!(before.next(), Step::Nothing, "{:?}", before.next());
        // A holder of 15% of the shares leaves, and their fraction of both
        // legs is closed: $85,000 carrying 85 against 85. No price moved.
        carry.tokens = decimal("85");
        carry.perp_position = decimal("-85");
        let mut after = picture(SATURDAY, vec![carry]);
        after.vault_value = decimal("85000");
        let after = looked(after);
        assert_eq!(after.share, decimal("1"));
        assert!(!after.only_reducing());
        assert_eq!(after.next(), Step::Nothing, "{:?}", after.next());
    }

    #[test]
    fn a_fall_in_a_share_s_value_past_the_stop_only_reduces() {
        let mut per_share = measure();
        let mut carry = spy("710");
        carry.tokens = decimal("100");
        carry.perp_position = decimal("-100");
        looked(&mut per_share, picture(SATURDAY, vec![carry.clone()]));
        // The gap widens: the perpetual $110 a share higher, the token
        // where it was, on 100 shorted in a $100,000 vault: 11% lost.
        carry.perp_bid = decimal("820");
        carry.perp_ask = decimal("820.05");
        let mut after = picture(SATURDAY, vec![carry]);
        after.vault_value = decimal("89000");
        let after = looked(&mut per_share, after);
        assert!(after.only_reducing(), "{:?}", after.share);
        assert!(matches!(after.next(), Step::Close { .. }));
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
