//! Quotes both sides of one Hyperliquid perpetual and manages the inventory
//! it takes on.
//!
//! It reads the market's book from Shonin's shared stream and keeps one
//! resting bid and one resting ask around the mid, leaning both against its
//! inventory so fills that bring it back to flat are the likelier ones. It
//! renews its quotes only when the mid has moved far enough to matter.
//!
//! Behind the quotes stands the venue's own cancel-everything deadline,
//! renewed as the books arrive once less than half of it is left (not with
//! every requote: each renewal is an action the venue counts), so if the
//! agent stops, the venue pulls its quotes within a minute. The venue fires
//! that deadline at most ten times a UTC day; the agent counts each time it
//! ran out and, after eight in a day, pulls its quotes and stands aside until
//! the next day, so no quote of its rests without the deadline behind it.
//!
//! Its limits are in `config.rs`, where the reviewer reads them.

mod config;

use futures::StreamExt;
use hl::{AccountState, Action, Order, OrderRef, Side, Tif};
use sdk::streams::{self, HYPERLIQUID, HyperliquidFeed, HyperliquidValue};
use sdk::{Decimal, decimal, drivers, from_payload, to_payload, vault};
use strategy_sdk as sdk;
use strategy_sdk::hyperliquid as hl;

const DRIVER: &str = hl::DRIVER;
/// How many steps a halt may take to close the inventory.
const FLATTEN_STEPS: usize = 16;

struct MmQuoter;

fn zero() -> Decimal {
    Decimal::from(0)
}

fn one() -> Decimal {
    Decimal::from(1)
}

/// The quotes resting now, by the agent's own ids.
#[derive(Default)]
struct Quotes {
    bid: Option<u128>,
    ask: Option<u128>,
    /// The mid they were set around.
    mid: Option<Decimal>,
}

struct Mind {
    best_bid: Option<Decimal>,
    best_ask: Option<Decimal>,
    /// When the last book arrived, in milliseconds.
    now_ms: u64,
    quotes: Quotes,
    next_id: u128,
    levered: bool,
    dead_man: DeadMan,
}

const DAY_MS: u64 = 86_400_000;

/// The venue's cancel-everything deadline, as the agent has set it, and how
/// often it has run out today.
#[derive(Default, Debug)]
struct DeadMan {
    /// When the venue will cancel everything, unless renewed first.
    deadline_ms: Option<u64>,
    /// The UTC day the count is for, in days since 1970.
    day: u64,
    /// How many times the deadline ran out on that day.
    fired: u32,
}

impl DeadMan {
    /// Take note of the time: a deadline that has passed fired, and the
    /// venue cancelled every quote then. True if it did.
    fn observe(&mut self, now_ms: u64) -> bool {
        let fired = match self.deadline_ms {
            Some(deadline) if now_ms >= deadline => {
                self.deadline_ms = None;
                self.count(deadline / DAY_MS);
                true
            }
            _ => false,
        };
        // A new UTC day: the venue's count starts again.
        if now_ms / DAY_MS > self.day {
            self.day = now_ms / DAY_MS;
            self.fired = 0;
        }
        fired
    }

    fn count(&mut self, day: u64) {
        if day > self.day {
            self.day = day;
            self.fired = 0;
        }
        if day == self.day {
            self.fired += 1;
        }
    }

    /// Whether quotes may rest: the deadline has run out fewer times today
    /// than the agent allows itself, so the venue can still fire it.
    fn may_quote(&self) -> bool {
        self.fired < config::STOP_AFTER_FIRES
    }

    /// The deadline to renew to now, if a renewal is due: none set, or less
    /// than [`config::RENEW_WHEN_LEFT_MS`] left on it.
    fn renewal(&self, now_ms: u64) -> Option<u64> {
        self.deadline_ms
            .is_none_or(|deadline| deadline.saturating_sub(now_ms) < config::RENEW_WHEN_LEFT_MS)
            .then_some(now_ms + config::DEAD_MAN_MS)
    }

    /// The venue took a renewal to `at_ms`. One it refused leaves the
    /// deadline it had.
    fn renewed(&mut self, at_ms: u64) {
        self.deadline_ms = Some(at_ms);
    }

    /// Take the deadline off: nothing is resting.
    fn clear(&mut self) -> Action {
        self.deadline_ms = None;
        Action::CancelAllAt { at_ms: None }
    }
}

impl Mind {
    fn mid(&self) -> Option<Decimal> {
        match (self.best_bid, self.best_ask) {
            (Some(bid), Some(ask)) if bid > zero() && ask > bid => {
                Some((bid + ask) / Decimal::from(2))
            }
            _ => None,
        }
    }

    fn id(&mut self) -> u128 {
        self.next_id += 1;
        self.next_id
    }
}

enum Woke {
    Book {
        bid: Option<Decimal>,
        ask: Option<Decimal>,
        at_ms: u64,
    },
    Venue,
    Vault(vault::VaultEvent),
    Nothing,
}

impl sdk::Agent for MmQuoter {
    async fn run() -> Result<(), String> {
        let mut mind = Mind {
            best_bid: None,
            best_ask: None,
            now_ms: 0,
            quotes: Quotes::default(),
            next_id: 0,
            levered: false,
            dead_man: DeadMan::default(),
        };
        let book = streams::subscribe::<_, HyperliquidValue>(
            HYPERLIQUID,
            &HyperliquidFeed::Book {
                coin: config::MARKET.into(),
            },
        )?
        .map(|value| match value {
            Ok(stamped) => match stamped.value {
                HyperliquidValue::Book(book) => Woke::Book {
                    bid: book.bids.first().map(|level| level.price),
                    ask: book.asks.first().map(|level| level.price),
                    at_ms: stamped.received_ms,
                },
                _ => Woke::Nothing,
            },
            Err(_) => Woke::Nothing,
        });
        let account = drivers::account_events(DRIVER)
            .into_stream()
            .map(|_| Woke::Venue);
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        let mut waking = futures::stream::select_all([
            book.boxed_local(),
            account.boxed_local(),
            vault_events.boxed_local(),
        ]);

        while let Some(woke) = waking.next().await {
            match woke {
                Woke::Book { bid, ask, at_ms } => {
                    mind.best_bid = bid;
                    mind.best_ask = ask;
                    mind.now_ms = at_ms;
                    // Ids carry the time, so a restarted agent never reuses
                    // one its predecessor left resting.
                    if mind.next_id == 0 {
                        mind.next_id = u128::from(at_ms) << 32;
                    }
                    if mind.dead_man.observe(at_ms) {
                        // The venue cancelled everything: nothing rests.
                        mind.quotes = Quotes::default();
                    }
                    if !mind.dead_man.may_quote() {
                        stand_aside(&mut mind).await?;
                    } else if moved_enough(&mind) {
                        requote(&mut mind).await?;
                    } else if let Some(until) = mind.dead_man.renewal(at_ms) {
                        let verdicts =
                            submit(&[Action::CancelAllAt { at_ms: Some(until) }]).await?;
                        if matches!(verdicts.first(), Some(sdk::Verdict::Accepted(_))) {
                            mind.dead_man.renewed(until);
                        }
                    }
                }
                // A fill changes the inventory: lean the quotes again.
                Woke::Venue | Woke::Vault(vault::VaultEvent::ClaimPending(_)) => {
                    if mind.mid().is_some() && mind.dead_man.may_quote() {
                        requote(&mut mind).await?;
                    }
                }
                Woke::Vault(vault::VaultEvent::BufferLow(_)) => {}
                Woke::Vault(vault::VaultEvent::Halting(_)) => {
                    flatten(&mut mind).await?;
                    return Ok(());
                }
                Woke::Nothing => {}
            }
        }
        Ok(())
    }
}

/// Whether the mid has moved far enough from where the quotes were set.
fn moved_enough(mind: &Mind) -> bool {
    let Some(mid) = mind.mid() else {
        return false;
    };
    match mind.quotes.mid {
        None => true,
        Some(quoted) => {
            let moved = if mid > quoted {
                mid - quoted
            } else {
                quoted - mid
            };
            moved / quoted >= decimal(config::REQUOTE_MOVE)
        }
    }
}

async fn read_state() -> Result<AccountState, String> {
    from_payload(&drivers::account_state(DRIVER.to_string(), Vec::new()).await?)
}

fn inventory(state: &AccountState) -> Decimal {
    state
        .positions
        .iter()
        .find(|position| position.market == config::MARKET)
        .map_or_else(zero, |position| position.size)
}

/// One side's quote: its price and size.
type Quote = Option<(Decimal, Decimal)>;

/// Where the two quotes go, and how big each is: `None` for a side that
/// would add to inventory already at its cap, or that is too small to post.
fn quotes_for(mid: Decimal, inventory: Decimal, vault_value: Decimal) -> (Quote, Quote) {
    if mid <= zero() || vault_value <= zero() {
        return (None, None);
    }
    let cap = vault_value * decimal(config::INVENTORY_CAP);
    let held = inventory * mid;
    // -1 at the cap short, +1 at the cap long.
    let mut lean = held / cap;
    if lean > one() {
        lean = one();
    }
    if lean < zero() - one() {
        lean = zero() - one();
    }
    let skew = lean * decimal(config::SKEW_AT_CAP);
    let half = decimal(config::HALF_SPREAD);
    let size = round_down(
        vault_value * decimal(config::QUOTE_FRACTION) / mid,
        config::SIZE_DECIMALS,
    );
    if size * mid < decimal(config::MINIMUM_NOTIONAL) {
        return (None, None);
    }
    let bid = (held < cap).then(|| (five_figures(mid * (one() - half - skew), Side::Buy), size));
    let ask = (held > zero() - cap)
        .then(|| (five_figures(mid * (one() + half - skew), Side::Sell), size));
    (bid, ask)
}

/// Cancel the quotes resting, push the venue's deadline back, and post new
/// ones around the mid.
async fn requote(mind: &mut Mind) -> Result<(), String> {
    let Some(mid) = mind.mid() else {
        return Ok(());
    };
    let state = read_state().await?;
    if !mind.levered {
        let verdicts = submit(&[Action::Leverage {
            market: config::MARKET.into(),
            leverage: config::LEVERAGE,
        }])
        .await?;
        mind.levered = matches!(verdicts.first(), Some(sdk::Verdict::Accepted(_)));
    }
    let mut actions = cancels(&mut mind.quotes);
    // Nothing is posted without the venue's deadline behind it, and the
    // deadline is renewed only when due.
    let renewal = mind.dead_man.renewal(mind.now_ms).map(|until| {
        actions.push(Action::CancelAllAt { at_ms: Some(until) });
        (actions.len() - 1, until)
    });
    let (bid, ask) = quotes_for(mid, inventory(&state), state.vault_value);
    if let Some((price, size)) = bid {
        let id = mind.id();
        mind.quotes.bid = Some(id);
        actions.push(Action::Place(quote(Side::Buy, price, size, id)));
    }
    if let Some((price, size)) = ask {
        let id = mind.id();
        mind.quotes.ask = Some(id);
        actions.push(Action::Place(quote(Side::Sell, price, size, id)));
    }
    mind.quotes.mid = Some(mid);
    let verdicts = submit(&actions).await?;
    if let Some((at, until)) = renewal
        && matches!(verdicts.get(at), Some(sdk::Verdict::Accepted(_)))
    {
        mind.dead_man.renewed(until);
    }
    Ok(())
}

/// Pull the quotes and take the deadline off, once: the deadline has run out
/// too often today for the venue to be relied on to fire it again.
async fn stand_aside(mind: &mut Mind) -> Result<(), String> {
    if mind.quotes.bid.is_none() && mind.quotes.ask.is_none() && mind.dead_man.deadline_ms.is_none()
    {
        return Ok(());
    }
    let mut actions = cancels(&mut mind.quotes);
    actions.push(mind.dead_man.clear());
    mind.quotes.mid = None;
    submit(&actions).await.map(|_| ())
}

/// Cancels for the quotes resting now, which are then forgotten.
fn cancels(quotes: &mut Quotes) -> Vec<Action> {
    [quotes.bid.take(), quotes.ask.take()]
        .into_iter()
        .flatten()
        .map(|id| Action::Cancel {
            market: config::MARKET.into(),
            order: OrderRef::Client(id),
        })
        .collect()
}

fn quote(side: Side, price: Decimal, size: Decimal, id: u128) -> Order {
    Order {
        market: config::MARKET.into(),
        side,
        size,
        limit_price: price,
        tif: Tif::PostOnly,
        reduce_only: false,
        client_id: Some(id),
    }
}

/// Pull the quotes and close the inventory, reduce-only, at the book.
async fn flatten(mind: &mut Mind) -> Result<(), String> {
    let mut actions = cancels(&mut mind.quotes);
    actions.push(mind.dead_man.clear());
    submit(&actions).await?;
    for _ in 0..FLATTEN_STEPS {
        let held = inventory(&read_state().await?);
        if held == zero() {
            return Ok(());
        }
        let (side, size, price) = if held > zero() {
            (Side::Sell, held, mind.best_bid)
        } else {
            (Side::Buy, zero() - held, mind.best_ask)
        };
        let Some(price) = price else {
            return Ok(());
        };
        // A wide reach: the vault is stopping, and the contract closes
        // whatever this leaves inside its own band.
        let reach = decimal("0.01");
        let limit = match side {
            Side::Sell => price * (one() - reach),
            Side::Buy => price * (one() + reach),
        };
        submit(&[Action::Place(Order {
            market: config::MARKET.into(),
            side,
            size,
            limit_price: five_figures(limit, side),
            tif: Tif::Ioc,
            reduce_only: true,
            client_id: None,
        })])
        .await?;
    }
    Ok(())
}

fn round_down(size: Decimal, decimals: u32) -> Decimal {
    let scale = Decimal::from(10u64.pow(decimals));
    (size * scale).floor() / scale
}

/// Five significant figures, the most the venue takes, rounded away from
/// the market: down for a bid, up for an ask.
fn five_figures(price: Decimal, side: Side) -> Decimal {
    if price <= zero() {
        return price;
    }
    let ten = Decimal::from(10);
    let mut scale = one();
    while price * scale < Decimal::from(10_000) {
        scale *= ten;
    }
    while price * scale >= Decimal::from(100_000) {
        scale /= ten;
    }
    let scaled = price * scale;
    let rounded = match side {
        Side::Buy => scaled.floor(),
        Side::Sell => scaled.ceil(),
    };
    rounded / scale
}

async fn submit(actions: &[Action]) -> Result<Vec<sdk::Verdict>, String> {
    let encoded: Vec<Vec<u8>> = actions.iter().map(to_payload).collect();
    Ok(drivers::submit(DRIVER.to_string(), encoded).await)
}

sdk::export_agent!(MmQuoter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_inventory_quotes_symmetrically_around_the_mid() {
        let (bid, ask) = quotes_for(decimal("2700"), zero(), decimal("10000"));
        let (bid, size) = bid.unwrap();
        let (ask, _) = ask.unwrap();
        assert!(bid < decimal("2700") && ask > decimal("2700"));
        assert_eq!(
            decimal("2700") - bid,
            ask - decimal("2700") - (ask - bid - (ask - bid))
        );
        // 2% of 10,000 at 2,700, to four decimals.
        assert_eq!(size, decimal("0.074"));
    }

    #[test]
    fn a_long_inventory_leans_both_quotes_down() {
        let (flat_bid, flat_ask) = quotes_for(decimal("2700"), zero(), decimal("10000"));
        let (long_bid, long_ask) = quotes_for(decimal("2700"), decimal("0.5"), decimal("10000"));
        assert!(long_bid.unwrap().0 < flat_bid.unwrap().0);
        assert!(long_ask.unwrap().0 < flat_ask.unwrap().0);
    }

    #[test]
    fn a_side_at_its_inventory_cap_is_not_quoted() {
        // 2 ETH at 2,700 is 5,400, past half of 10,000.
        let (bid, ask) = quotes_for(decimal("2700"), decimal("2"), decimal("10000"));
        assert!(bid.is_none(), "no more buying at the cap long");
        assert!(ask.is_some());
        let (bid, ask) = quotes_for(decimal("2700"), decimal("-2"), decimal("10000"));
        assert!(bid.is_some());
        assert!(ask.is_none(), "no more selling at the cap short");
    }

    #[test]
    fn a_vault_too_small_for_the_venues_minimum_posts_nothing() {
        assert_eq!(
            quotes_for(decimal("2700"), zero(), decimal("100")),
            (None, None)
        );
    }

    #[test]
    fn the_deadline_is_renewed_only_once_less_than_half_of_it_is_left() {
        let mut dead_man = DeadMan::default();
        let start = 1_000 * DAY_MS;
        dead_man.observe(start);
        let until = dead_man.renewal(start).expect("none set: renewed");
        dead_man.renewed(until);
        // Books every second for the next half minute renew nothing.
        for second in 1..30 {
            let now = start + second * 1_000;
            assert!(!dead_man.observe(now));
            assert!(dead_man.renewal(now).is_none(), "renewed at {second}s");
        }
        // With less than half left, it is renewed, and pushed a minute on.
        let now = start + 30_001;
        assert_eq!(dead_man.renewal(now), Some(now + config::DEAD_MAN_MS));
    }

    #[test]
    fn renewals_a_day_stay_far_below_one_per_requote() {
        // A book every 100 ms for a day.
        let mut dead_man = DeadMan::default();
        let mut renewals = 0;
        for tick in 0..(DAY_MS / 100) {
            let now = tick * 100;
            dead_man.observe(now);
            if let Some(until) = dead_man.renewal(now) {
                dead_man.renewed(until);
                renewals += 1;
            }
        }
        assert!(renewals <= 2 * 24 * 60 + 1, "{renewals}");
        assert_eq!(dead_man.fired, 0, "a renewed deadline never fires");
    }

    #[test]
    fn a_deadline_that_ran_out_is_counted_and_the_agent_stands_aside_after_eight_a_day() {
        let mut dead_man = DeadMan::default();
        let mut now = 2_000 * DAY_MS;
        for fired in 1..=config::STOP_AFTER_FIRES {
            assert!(dead_man.may_quote());
            let until = dead_man.renewal(now).unwrap();
            dead_man.renewed(until);
            // The agent falls silent past the deadline.
            now += config::DEAD_MAN_MS + 5_000;
            assert!(dead_man.observe(now), "a deadline that passed fired");
            assert_eq!(dead_man.fired, fired);
        }
        assert!(!dead_man.may_quote(), "stands aside, leaving the venue two");
        // The next UTC day it quotes again.
        let next_day = (now / DAY_MS + 1) * DAY_MS;
        assert!(!dead_man.observe(next_day));
        assert!(dead_man.may_quote());
    }

    #[test]
    fn a_deadline_cleared_never_counts_as_fired() {
        let mut dead_man = DeadMan::default();
        dead_man.renewed(dead_man.renewal(0).unwrap());
        assert_eq!(dead_man.clear(), Action::CancelAllAt { at_ms: None });
        assert!(!dead_man.observe(10 * config::DEAD_MAN_MS));
        assert_eq!(dead_man.fired, 0);
    }

    #[test]
    fn the_manifest_claims_the_limits_the_code_keeps() {
        let manifest = include_str!("../shonin.toml");
        assert!(manifest.contains("at most 50% of the vault") && config::INVENTORY_CAP == "0.50");
        assert!(manifest.contains("at most 2% of the vault") && config::QUOTE_FRACTION == "0.02");
        assert!(manifest.contains("within 60 seconds") && config::DEAD_MAN_MS == 60_000);
        assert!(manifest.contains("ten times a day") && manifest.contains("eight"));
        assert_eq!(config::STOP_AFTER_FIRES, 8);
        assert!(manifest.contains("leverage 3") && config::LEVERAGE == 3);
        assert!(manifest.contains("ETH perpetual") && config::MARKET == "ETH");
    }
}
