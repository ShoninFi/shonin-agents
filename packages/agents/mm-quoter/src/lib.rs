//! Quotes both sides of one Hyperliquid perpetual and manages the inventory
//! it takes on.
//!
//! It reads the market's book from Shonin's shared stream and keeps one
//! resting bid and one resting ask around the mid, leaning both against its
//! inventory so fills that bring it back to flat are the likelier ones. It
//! renews its quotes only when the mid has moved far enough to matter, and
//! each renewal also pushes back the venue's own cancel-everything deadline:
//! if the agent stops, the venue pulls its quotes a few seconds later.
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
                    if moved_enough(&mind) {
                        requote(&mut mind).await?;
                    }
                }
                // A fill changes the inventory: lean the quotes again.
                Woke::Venue | Woke::Vault(vault::VaultEvent::ClaimPending(_)) => {
                    if mind.mid().is_some() {
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
    actions.push(Action::CancelAllAt {
        at_ms: Some(mind.now_ms + config::DEAD_MAN_MS),
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
    actions.push(Action::CancelAllAt { at_ms: None });
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
    fn the_manifest_claims_the_limits_the_code_keeps() {
        let manifest = include_str!("../shonin.toml");
        assert!(manifest.contains("at most 50% of the vault") && config::INVENTORY_CAP == "0.50");
        assert!(manifest.contains("at most 2% of the vault") && config::QUOTE_FRACTION == "0.02");
        assert!(manifest.contains("within 15 seconds") && config::DEAD_MAN_MS == 15_000);
        assert!(manifest.contains("leverage 3") && config::LEVERAGE == 3);
        assert!(manifest.contains("ETH perpetual") && config::MARKET == "ETH");
    }
}
