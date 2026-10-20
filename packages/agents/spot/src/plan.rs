//! What to do next, from what the agent sees: no venue, no clock, so every
//! rule here is checked by the tests below.
//!
//! Each token has a weight, a share of the vault's value. When the market
//! moves a token further than the band from its weight, one order puts it
//! back, no larger than the order cap; the token furthest out goes first.
//! After a fall from the vault's peak past the stop, or when the vault is
//! halting, every weight is zero and the agent only sells.

use strategy_sdk::{Decimal, decimal};

use crate::config;

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
    /// The highest value seen since the agent started.
    pub peak: Decimal,
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

impl Picture {
    /// Whether the vault has fallen past the stop from its peak.
    pub fn stopped(&self) -> bool {
        self.peak > zero() && self.value < self.peak * (one() - decimal(config::DRAWDOWN_STOP))
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
            peak: value,
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
            peak: d("1000"),
            names,
        };
        assert!(matches!(picture.next(false), Step::Sell { index: 1, .. }));
    }

    #[test]
    fn past_the_stop_or_halting_it_only_sells_and_sells_everything() {
        let mut picture = base("0", "1000");
        picture.peak = d("1200");
        assert_eq!(picture.next(false), Step::Nothing, "16% down: no buying");
        // Holding WETH past the stop: sold, $50 an order, whatever the band.
        let mut held = base("0.2", "500");
        held.peak = d("1200");
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

    #[test]
    fn an_order_below_the_least_worth_sending_is_not_sent() {
        // A $10 vault: 5% is 50 cents, under the dollar an order must be.
        let picture = base("0", "10");
        assert_eq!(picture.next(false), Step::Nothing);
    }
}
