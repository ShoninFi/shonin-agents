//! What to do next, from what the agent sees: no venue, no clock, so every
//! rule here is checked by the tests below.
//!
//! Each token has a weight, a share of the vault's value. When the market
//! moves a token further than the band from its weight, one order puts it
//! back, no larger than the order cap; the token furthest out goes first.
//! After a fall in the value of a vault share past the stop from its peak,
//! or when the vault is halting, every weight is zero and the agent only
//! sells.
//!
//! A share's value is the agent's own measure of it (`per_share.rs`), from
//! what the vault holds at Chainlink's prices.

use strategy_sdk::{Decimal, decimal};

use crate::config;
use crate::per_share::Held;

/// One token as the agent sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Name {
    pub token: String,
    /// Its weight, as a fraction of the vault's value.
    pub weight: Decimal,
    /// What the Safe holds of it.
    pub tokens: Decimal,
    /// Chainlink's price, and its pool's.
    pub feed_price: Decimal,
    pub market_price: Decimal,
    /// The issuer has paused the token's oracle.
    pub paused: bool,
}

/// Everything a step is decided from.
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    /// The whole vault, everywhere, in dollars.
    pub value: Decimal,
    /// The vault's own cash, and the Safe's idle dollars.
    pub cash: Decimal,
    pub safe_dollars: Decimal,
    /// The value of a vault share now, and the highest it has been since
    /// the agent started, each as a multiple of its value then (see
    /// [`PerShare`](crate::per_share::PerShare)).
    pub share: Decimal,
    pub peak: Decimal,
    /// The stop came at a look since the agent started: it holds until the
    /// agent restarts (see [`PerShare`](crate::per_share::PerShare)).
    pub stop_holds: bool,
    pub names: Vec<Name>,
}

/// One step.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Nothing,
    /// Spend `dollars` (from the Safe, moved there from the vault's cash
    /// first where the Safe holds less) on `names[index]`.
    Buy {
        index: usize,
        dollars: Decimal,
        min_tokens: Decimal,
    },
    /// Sell `tokens` of `names[index]`.
    Sell {
        index: usize,
        tokens: Decimal,
        min_dollars: Decimal,
    },
}

pub fn zero() -> Decimal {
    Decimal::from(0u8)
}

fn one() -> Decimal {
    Decimal::from(1u8)
}

fn abs(x: Decimal) -> Decimal {
    if x < zero() { zero() - x } else { x }
}

impl Name {
    /// Whether it may be traded: its oracle running, and its pool near its
    /// Chainlink price, since a swap far from the feed is refused anyway.
    pub fn tradable(&self) -> bool {
        !self.paused
            && self.feed_price > zero()
            && self.market_price > zero()
            && abs(self.market_price / self.feed_price - one()) <= decimal(config::MAX_POOL_GAP)
    }

    /// What it is worth at Chainlink's price.
    pub fn worth(&self) -> Decimal {
        self.tokens * self.feed_price
    }
}

/// What the vault holds of `names`, for [`PerShare`](crate::per_share::PerShare): each token at its
/// Chainlink price, the price the vault is valued at. A token without a
/// price counts no move.
pub fn held(names: &[Name]) -> Vec<Held> {
    names
        .iter()
        .filter(|name| name.feed_price > zero())
        .map(|name| Held {
            what: name.token.clone(),
            quantity: name.tokens,
            unit_value: name.feed_price,
        })
        .collect()
}

/// What the vault is worth by what it holds of `names`, at Chainlink's
/// prices, and `dollars` besides: what its accounts must add up to.
pub fn accounted(names: &[Name], dollars: Decimal) -> Decimal {
    names.iter().fold(dollars, |sum, name| sum + name.worth())
}

impl Picture {
    /// Whether a vault share's value has fallen past the stop from its
    /// peak, now or at any earlier look since the agent started.
    pub fn stopped(&self) -> bool {
        self.stop_holds
            || (self.peak > zero()
                && self.share < self.peak * (one() - decimal(config::DRAWDOWN_STOP)))
    }

    /// The dollars a buy may spend: the Safe's, and the vault's cash above
    /// the buffer.
    pub fn spendable(&self) -> Decimal {
        let keep = self.value * decimal(config::IDLE_BUFFER);
        let spare = if self.cash > keep {
            self.cash - keep
        } else {
            zero()
        };
        self.safe_dollars + spare
    }

    /// The next step of a halt, at a look the share's measure took or
    /// skipped (`looked`). It reads only what the Safe holds of each token,
    /// their prices, and the vault's value for the order cap; never the
    /// share's measure, so it sells every token whatever the measure made of
    /// the look. At a skipped look the value read may be short, so the cap is
    /// taken from no less than the Safe's tokens and the dollars read add up to.
    pub fn halting(&self, looked: bool) -> Step {
        if looked {
            return self.next(true);
        }
        let mut picture = self.clone();
        picture.value = self
            .value
            .max(accounted(&self.names, self.cash + self.safe_dollars));
        picture.next(true)
    }

    /// The next step: the tradable token furthest from its weight, put back
    /// towards it by one bounded order, if it is outside the band; with
    /// `halting` (or past the stop), the largest holding sold down instead.
    pub fn next(&self, halting: bool) -> Step {
        if self.value <= zero() {
            return Step::Nothing;
        }
        let reducing = halting || self.stopped();
        let band = decimal(config::BAND);
        let cap = self.value * decimal(config::MAX_ORDER_FRACTION);
        let least = decimal(config::MINIMUM_NOTIONAL);
        let slippage = decimal(config::SLIPPAGE);
        // Each tradable token's drift from its weight, as a fraction of the
        // vault: positive when it holds too much.
        let mut drifts: Vec<(usize, Decimal)> = self
            .names
            .iter()
            .enumerate()
            .filter(|(_, name)| name.tradable())
            .map(|(index, name)| {
                let weight = if reducing { zero() } else { name.weight };
                (index, name.worth() / self.value - weight)
            })
            .filter(|(index, drift)| {
                if reducing {
                    self.names[*index].tokens > zero()
                } else {
                    abs(*drift) > band
                }
            })
            .collect();
        drifts.sort_by(|a, b| abs(b.1).cmp(&abs(a.1)));
        for (index, drift) in drifts {
            let name = &self.names[index];
            let dollars = (abs(drift) * self.value).min(cap);
            if drift > zero() {
                let mut tokens = (dollars / name.feed_price).min(name.tokens);
                // A remainder too small to sell on its own goes with this
                // order when every token is being sold.
                if reducing && (name.tokens - tokens) * name.feed_price < least {
                    tokens = name.tokens;
                }
                if tokens * name.feed_price < least && !(reducing && tokens == name.tokens) {
                    continue;
                }
                if tokens <= zero() {
                    continue;
                }
                return Step::Sell {
                    index,
                    tokens,
                    min_dollars: tokens * name.feed_price * (one() - slippage),
                };
            }
            if reducing {
                continue;
            }
            let dollars = dollars.min(self.spendable());
            if dollars < least {
                continue;
            }
            return Step::Buy {
                index,
                dollars,
                min_tokens: dollars / name.feed_price * (one() - slippage),
            };
        }
        Step::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::per_share::PerShare;

    fn d(text: &str) -> Decimal {
        decimal(text)
    }

    fn name(token: &str, weight: &str, tokens: &str, price: &str) -> Name {
        Name {
            token: token.into(),
            weight: d(weight),
            tokens: d(tokens),
            feed_price: d(price),
            market_price: d(price),
            paused: false,
        }
    }

    /// A $1,000 vault holding WETH at $2,500 against USDC, at a 50% weight.
    fn base(weth: &str, cash: &str) -> Picture {
        let name = name("WETH", "0.50", weth, "2500");
        let value = name.worth() + d(cash);
        Picture {
            value,
            cash: d(cash),
            safe_dollars: zero(),
            share: one(),
            peak: one(),
            stop_holds: false,
            names: vec![name],
        }
    }

    #[test]
    fn every_chain_s_weights_leave_the_buffer_and_name_each_token_once() {
        for venue in config::VENUES {
            let total = venue
                .weights
                .iter()
                .fold(zero(), |sum, (_, weight)| sum + d(weight));
            assert!(total <= one() - d(config::IDLE_BUFFER), "{}", venue.chain);
            let mut tokens: Vec<&str> = venue.weights.iter().map(|(token, _)| *token).collect();
            tokens.sort();
            tokens.dedup();
            assert_eq!(tokens.len(), venue.weights.len(), "{}", venue.chain);
        }
        // Each chain once, each with its own driver.
        let chains: Vec<&str> = config::VENUES.iter().map(|venue| venue.chain).collect();
        assert_eq!(chains, vec!["robinhood", "base"]);
        assert_eq!(config::VENUES[1].driver, "uniswap-base");
    }

    #[test]
    fn a_token_inside_its_band_is_left_alone() {
        // 0.212 WETH is $530: 51.5% of $1,030, inside 3 points.
        assert_eq!(base("0.212", "500").next(false), Step::Nothing);
    }

    #[test]
    fn a_token_below_its_band_is_bought_back_towards_its_weight_by_one_capped_order() {
        // No WETH at all in a $1,000 vault: 50 points under, and one order
        // is at most 5% of the vault.
        let picture = base("0", "1000");
        let Step::Buy {
            index,
            dollars,
            min_tokens,
        } = picture.next(false)
        else {
            panic!("{:?}", picture.next(false))
        };
        assert_eq!(index, 0);
        assert_eq!(dollars, d("50"));
        assert_eq!(
            min_tokens,
            d("50") / d("2500") * (one() - d(config::SLIPPAGE))
        );
        // 6 points under in a $1,000 vault: $60 to go, capped at $50.
        let picture = base("0.176", "560");
        assert!(matches!(picture.next(false), Step::Buy { dollars, .. } if dollars == d("50")));
        // 4 points under: $40, under the cap.
        let picture = base("0.184", "540");
        assert!(matches!(picture.next(false), Step::Buy { dollars, .. } if dollars == d("40")));
    }

    #[test]
    fn a_token_above_its_band_is_sold_back_towards_its_weight() {
        // 0.24 WETH is $600 of $1,000: 10 points over; $50 at most.
        let picture = base("0.24", "400");
        let Step::Sell {
            tokens,
            min_dollars,
            ..
        } = picture.next(false)
        else {
            panic!("{:?}", picture.next(false))
        };
        assert_eq!(tokens, d("0.02"));
        assert_eq!(min_dollars, d("50") * (one() - d(config::SLIPPAGE)));
    }

    #[test]
    fn a_buy_never_spends_the_vault_s_buffer() {
        // A $1,000 vault (the rest at its station), WETH 10 points under,
        // but only $70 of cash: $60 is the buffer, so $10 may be spent.
        let mut picture = base("0.16", "70");
        picture.value = d("1000");
        assert!(matches!(picture.next(false), Step::Buy { dollars, .. } if dollars == d("10")));
        // Dollars already in the Safe are spent first, and count too.
        picture.safe_dollars = d("15");
        assert!(matches!(picture.next(false), Step::Buy { dollars, .. } if dollars == d("25")));
        // Nothing to spend: nothing bought.
        picture.cash = d("60");
        picture.safe_dollars = zero();
        assert_eq!(picture.next(false), Step::Nothing);
    }

    #[test]
    fn a_token_whose_pool_strays_from_chainlink_or_whose_oracle_is_paused_is_not_traded() {
        let mut picture = base("0", "1000");
        picture.names[0].market_price = d("2560");
        assert_eq!(picture.next(false), Step::Nothing, "2.4% from the feed");
        picture.names[0].market_price = d("2540");
        assert!(matches!(picture.next(false), Step::Buy { .. }), "1.6%");
        picture.names[0].paused = true;
        assert_eq!(picture.next(false), Step::Nothing);
    }

    #[test]
    fn the_token_furthest_from_its_weight_goes_first() {
        // $1,000: TSLA 5 points under, AMD 8 over.
        let names = vec![
            name("TSLA", "0.15", "0.25", "400"),
            name("AMD", "0.15", "1.15", "200"),
        ];
        let picture = Picture {
            value: d("1000"),
            cash: d("670"),
            safe_dollars: zero(),
            share: one(),
            peak: one(),
            stop_holds: false,
            names,
        };
        assert!(matches!(picture.next(false), Step::Sell { index: 1, .. }));
    }

    #[test]
    fn past_the_stop_or_halting_it_only_sells_and_sells_everything() {
        let mut picture = base("0", "1000");
        picture.peak = d("1.2");
        assert_eq!(picture.next(false), Step::Nothing, "16% down: no buying");
        // Holding WETH past the stop: sold, $50 an order, whatever the band.
        let mut held = base("0.2", "500");
        held.peak = d("1.2");
        assert!(matches!(held.next(false), Step::Sell { tokens, .. } if tokens == d("0.02")));
        // Halting at the peak: the same.
        let halting = base("0.2", "500");
        assert!(matches!(halting.next(true), Step::Sell { tokens, .. } if tokens == d("0.02")));
        // The last of it, under the order's least, goes whole.
        let dust = base("0.0002", "1000");
        assert!(
            matches!(dust.next(true), Step::Sell { tokens, .. } if tokens == d("0.0002")),
            "{:?}",
            dust.next(true)
        );
        assert_eq!(base("0", "1000").next(true), Step::Nothing);
    }

    /// The share's measure, as the agent keeps it.
    fn measure() -> PerShare {
        PerShare::new(config::DRAWDOWN_STOP)
    }

    /// `picture` with the share's value and peak `per_share` has after it
    /// looks at it, its holdings counted as the agent counts them.
    fn looked(per_share: &mut PerShare, mut picture: Picture) -> Picture {
        let accounted = accounted(&picture.names, picture.cash + picture.safe_dollars);
        per_share.look(picture.value, Some(accounted), held(&picture.names));
        picture.share = per_share.now;
        picture.peak = per_share.peak;
        picture.stop_holds = per_share.stopped();
        picture
    }

    /// The vault of [`base`] with WETH at `price`.
    fn at(weth: &str, price: &str, cash: &str) -> Picture {
        let mut picture = base(weth, cash);
        picture.names[0].feed_price = d(price);
        picture.names[0].market_price = d(price);
        picture.value = picture.names[0].worth() + picture.cash;
        picture
    }

    #[test]
    fn a_look_whose_value_misses_an_account_does_not_magnify_a_move_into_the_stop() {
        let mut per_share = measure();
        // $1,000: $500 of WETH at $2,500, $500 of cash.
        looked(&mut per_share, base("0.2", "500"));
        // One look at which the Safe's value went unread: the vault's
        // balances add up to its own cash alone, $60.
        let mut unread = base("0.2", "500");
        unread.value = d("60");
        looked(&mut per_share, unread);
        // Everything read again; WETH 1.5% lower, $7.50 of a $992.50 vault.
        let next = looked(&mut per_share, at("0.2", "2462.5", "500"));
        assert!(
            !next.stopped(),
            "share {} of peak {}",
            next.share,
            next.peak
        );
        assert_eq!(next.share, d("0.9925"));
    }

    #[test]
    fn once_sold_out_after_the_stop_it_buys_nothing_until_a_restart() {
        let mut per_share = measure();
        looked(&mut per_share, at("0.2", "2500", "500"));
        // WETH down 25%: the share 12.5% down, stopped, and it sells out.
        assert!(looked(&mut per_share, at("0.2", "1875", "500")).stopped());
        let out = looked(&mut per_share, at("0", "1875", "875"));
        assert!(out.stopped());
        // WETH all the way back, and past its peak: none of it was held, so
        // the share stays where the selling left it, and nothing is bought.
        for price in ["2000", "2500", "3000"] {
            let back = looked(&mut per_share, at("0", price, "875"));
            assert!(back.stopped(), "lifted at ${price}, share {}", back.share);
            assert_eq!(back.share, d("0.875"));
            assert_eq!(back.next(false), Step::Nothing);
        }
        // A restart measures afresh: the vault as it is, at its weights.
        let mut restarted = measure();
        let fresh = looked(&mut restarted, at("0", "3000", "875"));
        assert!(!fresh.stopped());
        assert!(matches!(fresh.next(false), Step::Buy { .. }));
    }

    #[test]
    fn a_market_swinging_after_the_stop_never_buys_back_in() {
        let mut per_share = measure();
        looked(&mut per_share, at("0.2", "2500", "500"));
        // WETH down 25%: stopped, and it sells out at $1,875.
        assert!(looked(&mut per_share, at("0.2", "1875", "500")).stopped());
        let (mut weth, mut cash) = (zero(), d("875"));
        looked(&mut per_share, holding(weth, "1875", cash));
        for cycle in 0..3 {
            // WETH rallies to $2,300, then falls back to $2,000. Whatever a
            // step would buy is bought back to the weight at once.
            for price in ["2300", "2000"] {
                let now = looked(&mut per_share, holding(weth, price, cash));
                assert!(now.stopped(), "cycle {cycle}: lifted at ${price}");
                if let Step::Buy { .. } = now.next(false) {
                    let bought = now.value / d("2") / d(price);
                    cash -= bought * d(price);
                    weth += bought;
                }
            }
        }
        let real = cash + weth * d("2000");
        assert!(
            real >= d("850"),
            "the vault fell to ${real} from $1,000, the stop measuring {} of a peak of {}",
            per_share.now,
            per_share.peak
        );
        assert_eq!(real, d("875"));
    }

    /// The vault of [`base`] holding `weth` at `price`, and `cash`.
    fn holding(weth: Decimal, price: &str, cash: Decimal) -> Picture {
        let mut picture = base("0", "0");
        picture.names[0].tokens = weth;
        picture.names[0].feed_price = d(price);
        picture.names[0].market_price = d(price);
        picture.cash = cash;
        picture.value = picture.names[0].worth() + cash;
        picture
    }

    #[test]
    fn a_halt_at_a_look_the_measure_skipped_still_sells_by_what_the_safe_holds() {
        // The Safe's value went unread: the vault reads as its cash alone.
        let mut skipped = base("0.2", "500");
        skipped.value = d("500");
        skipped.peak = d("2");
        // Its order cap is 5% of the $1,000 it holds, not of the $500 read,
        // and the share's measure (here far past the stop) plays no part.
        assert!(
            matches!(skipped.halting(false), Step::Sell { tokens, .. } if tokens == d("0.02")),
            "{:?}",
            skipped.halting(false)
        );
        // Read as nothing at all: still sells.
        skipped.value = zero();
        assert!(matches!(skipped.halting(false), Step::Sell { .. }));
        // At a look taken, it is the halt's step as ever.
        let taken = base("0.2", "500");
        assert_eq!(taken.halting(true), taken.next(true));
    }

    #[test]
    fn a_leaver_taking_a_tenth_of_the_vault_does_not_stop_the_agent() {
        let mut per_share = measure();
        // A $1,000 vault at its weights: $500 of WETH, $500 of cash.
        let before = looked(&mut per_share, base("0.2", "500"));
        assert_eq!(before.next(false), Step::Nothing);
        // A holder of 15% of the shares leaves: their claim is paid, half
        // from cash and half from a sale, so the vault is $850, still at
        // its weights. Nothing about the market moved.
        let after = looked(&mut per_share, base("0.17", "425"));
        assert_eq!(after.value, d("850"));
        assert_eq!(after.share, one());
        assert_eq!(after.next(false), Step::Nothing, "{:?}", after.next(false));
        // Nor does a deposit that comes and goes again (WETH under its
        // weight meanwhile is bought, as ever).
        let deposited = looked(&mut per_share, base("0.17", "2425"));
        assert_eq!(deposited.share, one());
        assert!(matches!(deposited.next(false), Step::Buy { .. }));
        let left = looked(&mut per_share, base("0.17", "425"));
        assert_eq!(left.share, one());
        assert_eq!(left.next(false), Step::Nothing, "{:?}", left.next(false));
    }

    #[test]
    fn a_fall_in_a_share_s_value_past_the_stop_sells() {
        let mut per_share = measure();
        let at = |price: &str| {
            let mut picture = base("0.2", "500");
            picture.names[0].feed_price = d(price);
            picture.names[0].market_price = d(price);
            picture.value = picture.names[0].worth() + picture.cash;
            picture
        };
        // Up to $2,750 first: the share is worth 5% more, its new peak.
        looked(&mut per_share, at("2500"));
        let up = looked(&mut per_share, at("2750"));
        assert_eq!(up.peak, d("1.05"));
        // WETH from $2,750 to $2,200: the vault from $1,050 to $940, a
        // share 10.5% under its peak.
        let down = looked(&mut per_share, at("2200"));
        assert!(down.stopped(), "{:?}", down.share);
        assert!(
            matches!(down.next(false), Step::Sell { index: 0, .. }),
            "{:?}",
            down.next(false)
        );
        // Not quite 10% under: not stopped.
        let mut per_share = measure();
        looked(&mut per_share, at("2750"));
        let nearly = looked(&mut per_share, at("2240"));
        assert!(!nearly.stopped(), "{:?}", nearly.share);
    }

    #[test]
    fn each_manifest_claims_the_stop_as_a_fall_in_a_share_s_value() {
        assert_eq!(config::DRAWDOWN_STOP, "0.10");
        for manifest in [
            include_str!("../shonin.toml"),
            include_str!("../base/shonin.toml"),
        ] {
            assert!(manifest.contains("a 10% fall in a vault share's value"));
            assert!(manifest.contains("a fall from before a restart is not remembered"));
            assert!(!manifest.contains("fall from the vault's peak"));
            assert!(manifest.contains("couldn't be read"));
            assert!(manifest.contains("Sells everything only after a 10% fall"));
            assert!(
                manifest
                    .contains("the stop holds until then, and a restart measures a share afresh")
            );
            assert!(manifest.contains("buys nothing until the agent restarts"));
            assert!(!manifest.contains("Only sells after"));
            assert!(!manifest.contains("lifts"));
        }
    }

    #[test]
    fn an_order_below_the_least_worth_sending_is_not_sent() {
        // A $10 vault: 5% is 50 cents, under the dollar an order must be.
        let picture = base("0", "10");
        assert_eq!(picture.next(false), Step::Nothing);
    }
}
