//! Holds Robinhood stock tokens against Lighter perpetuals over the weekend.
//!
//! A stock token trades around the clock; the stock behind it, and the
//! channel that creates and redeems tokens against it, don't. From Friday's
//! close to Monday's open nothing pulls a token back to its stock, and the
//! perpetual on the same stock moves with the weekend's flow. When the
//! perpetual trades far enough above the token, this agent buys the token
//! and shorts the perpetual in matching shares, and carries the pair into
//! Monday, when the gap usually closes.
//!
//! The whole strategy is one long-lived program. It listens to the shared
//! Lighter books for its three stocks and to the vault's events, and at most
//! every `STEP_EVERY` seconds it works out what it would like to hold
//! (`plan.rs`) and takes one step towards it. Its limits are in `config.rs`,
//! and the manifest claims them in the same words.

mod config;
mod host;
mod per_share;
mod plan;
mod venues;

use std::collections::HashMap;

use futures::stream::PollNext;
use futures::{Stream, StreamExt};
use host::{Host, Sdk};
use sdk::streams::{self, LIGHTER, LighterFeed, LighterValue};
use sdk::transfers::{Account, Moved, VenueAccount};
use sdk::{Decimal, Verdict, decimal, from_payload, millionths, to_payload, vault};
use strategy_sdk as sdk;
use venues::{lighter as lt, robinhood as rh};

use per_share::PerShare;
use plan::{Name, Picture, Step, zero};

/// How many looks a halt may take to close everything before leaving the
/// rest to the contracts.
const FLATTEN_STEPS: usize = 64;

struct WeekendCarry;

/// What the agent remembers between steps: rebuilt from the venues whenever
/// it starts, so a restart costs a moment, and the stop (see `per_share.rs`).
struct Mind {
    /// The value of a vault share since the agent started, and its peak:
    /// what the drawdown stop is measured on.
    per_share: PerShare,
    /// Each perpetual's best bid and ask, by Lighter market number.
    books: HashMap<u32, (Decimal, Decimal)>,
    last_step: u64,
}

impl Mind {
    fn new() -> Self {
        Self {
            per_share: PerShare::new(config::DRAWDOWN_STOP),
            books: HashMap::new(),
            last_step: 0,
        }
    }
}

enum Woke {
    Book(u32, Decimal, Decimal),
    Vault(vault::VaultEvent),
    Nothing,
}

impl sdk::Agent for WeekendCarry {
    async fn run() -> Result<(), String> {
        let host = Sdk;
        let mut mind = Mind::new();
        let mut books = Vec::new();
        for (_, _, market) in config::NAMES {
            let stream = streams::subscribe::<_, LighterValue>(
                LIGHTER,
                &LighterFeed::Book { market: *market },
            )?;
            let market = *market;
            books.push(stream.map(move |value| match value {
                Ok(stamped) => {
                    let LighterValue::Book(book) = stamped.value;
                    match (book.bids.first(), book.asks.first()) {
                        (Some(bid), Some(ask)) => Woke::Book(market, bid.price, ask.price),
                        _ => Woke::Nothing,
                    }
                }
                Err(_) => Woke::Nothing,
            }));
        }
        let books = futures::stream::select_all(books);
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        serve(&host, &mut mind, vault_first(vault_events, books)).await
    }
}

/// What wakes the agent, the vault's news always taken before anything
/// else that is waiting: an agent started again within a halt finds the
/// halting notice first in the vault's news, and must see it before it
/// takes an ordinary step.
fn vault_first(
    vault: impl Stream<Item = Woke> + Unpin,
    others: impl Stream<Item = Woke> + Unpin,
) -> impl Stream<Item = Woke> + Unpin {
    futures::stream::select_with_strategy(vault, others, |_: &mut ()| PollNext::Left)
}

/// Act on each wake until the vault halts. There is no look before the
/// first wake, and the vault's news is read before anything else, so a
/// start within a halt goes straight to the halt.
async fn serve(
    host: &impl Host,
    mind: &mut Mind,
    mut waking: impl Stream<Item = Woke> + Unpin,
) -> Result<(), String> {
    while let Some(woke) = waking.next().await {
        match woke {
            Woke::Book(market, bid, ask) => {
                mind.books.insert(market, (bid, ask));
                if host.now() >= mind.last_step + config::STEP_EVERY {
                    mind.last_step = host.now();
                    step(host, mind).await?;
                }
            }
            // A leaver's fraction of both legs is being closed by the
            // contracts; look again at what is left.
            Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(host, mind).await?,
            // Leavers wait on more cash than the vault holds: bring idle
            // dollars home from the Safe, then from Lighter.
            Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                let mut owed = short;
                for driver in [rh::DRIVER, lt::DRIVER] {
                    if owed == 0 {
                        break;
                    }
                    if matches!(
                        host.move_(venue(host, driver), Account::Vault, owed).await,
                        Moved::Done
                    ) {
                        owed = 0;
                    }
                }
            }
            Woke::Vault(vault::VaultEvent::Halting(_)) => {
                return flatten(host, mind, &mut waking).await;
            }
            Woke::Nothing => {}
        }
    }
    Ok(())
}

fn venue(host: &impl Host, driver: &str) -> Account {
    Account::Venue(VenueAccount {
        chain: host.home_chain(),
        driver: driver.to_string(),
    })
}

/// Whether two of the vault's accounts are the same one.
fn same(one: &Account, other: &Account) -> bool {
    match (one, other) {
        (Account::Vault, Account::Vault) => true,
        (Account::Station(one), Account::Station(other)) => one == other,
        (Account::Venue(one), Account::Venue(other)) => {
            one.chain == other.chain && one.driver == other.driver
        }
        _ => false,
    }
}

/// Everything the vault is worth, everywhere, in dollars; and what its
/// holdings say it is worth, the Safe's tokens at their Chainlink prices:
/// none when one of its accounts couldn't be read, since the balances leave
/// such an account out rather than read it as empty, or when Lighter holds
/// something and its positions (`perps_read`) couldn't be read.
async fn vault_value(host: &impl Host, holdings: &rh::AccountState, perps_read: bool) -> Value {
    let balances = host.balances().await;
    let dollars = |amount: u64| Decimal::from(amount) / Decimal::from(1_000_000u32);
    let total = dollars(balances.iter().map(|balance| balance.amount).sum());
    let read = |account: &Account| {
        balances
            .iter()
            .find(|balance| same(&balance.account, account))
            .map(|balance| dollars(balance.amount))
    };
    let in_safe = holdings
        .holdings
        .iter()
        .fold(holdings.dollars, |sum, held| {
            sum + held.tokens * held.feed_price
        });
    let least = match read(&venue(host, rh::DRIVER)) {
        Some(_) => total,
        None => total + in_safe,
    };
    let shorts_known =
        perps_read || matches!(read(&venue(host, lt::DRIVER)), Some(perps) if perps <= zero());
    let accounted = match (
        read(&Account::Vault),
        read(&Account::Station(host.home_chain())),
        read(&venue(host, rh::DRIVER)),
        read(&venue(host, lt::DRIVER)),
    ) {
        (Some(cash), Some(idle), Some(_), Some(perps)) if perps_read || perps <= zero() => {
            Some(cash + idle + perps + in_safe)
        }
        _ => None,
    };
    Value {
        total,
        accounted,
        least,
        shorts_known,
    }
}

/// The vault's value, as [`vault_value`] reads it.
struct Value {
    /// Everything the vault is worth, everywhere, in dollars.
    total: Decimal,
    /// What its holdings say it is worth: none when they can't say.
    accounted: Option<Decimal>,
    /// The least it is surely worth: every account read, and the Safe by
    /// what it holds when its own balance went unread.
    least: Decimal,
    /// What it is short is known: Lighter's positions read, or Lighter's
    /// balance read as nothing.
    shorts_known: bool,
}

async fn state<Q: serde::Serialize, A: serde::de::DeserializeOwned>(
    host: &impl Host,
    driver: &str,
    query: &Q,
) -> Result<A, String> {
    from_payload(&host.account_state(driver, to_payload(query)).await?)
}

/// What the agent sees: the picture, whether the share's measure could take
/// it in (a look one of whose reads failed or came back partial is not
/// decided from), and the perpetuals of the names the vault holds something
/// of whose book hasn't been seen since the agent started, which the
/// picture leaves out.
struct Seen {
    picture: Picture,
    looked: bool,
    unpriced: Vec<&'static str>,
}

async fn picture(host: &impl Host, mind: &mut Mind) -> Result<Seen, String> {
    let prices: Vec<rh::TokenPrice> = state(host, rh::DRIVER, &rh::Query::Prices).await?;
    let holdings: rh::AccountState = state(host, rh::DRIVER, &rh::Query::Holdings).await?;
    // No Lighter account before the first move to it: nothing is held there.
    let perps: Option<lt::AccountState> = state(host, lt::DRIVER, &lt::Query::Account).await.ok();
    let Value {
        total: value,
        accounted,
        least,
        shorts_known,
    } = vault_value(host, &holdings, perps.is_some()).await;
    let mut names = Vec::new();
    let mut unpriced = Vec::new();
    for (token, perp, market) in config::NAMES {
        let Some(price) = prices.iter().find(|p| p.token == *token) else {
            continue;
        };
        let tokens = holdings
            .holdings
            .iter()
            .find(|h| h.token == *token)
            .map(|h| h.tokens)
            .unwrap_or_else(zero);
        let perp_position = perps
            .as_ref()
            .and_then(|state| state.positions.iter().find(|p| p.market == *perp))
            .map(|p| p.size)
            .unwrap_or_else(zero);
        let Some((bid, ask)) = mind.books.get(market) else {
            if tokens != zero() || perp_position != zero() {
                unpriced.push(*perp);
            }
            continue;
        };
        names.push(Name {
            token: token.to_string(),
            perp: perp.to_string(),
            token_price: price.market_price,
            feed_price: price.feed_price,
            multiplier: price.multiplier,
            paused: price.paused,
            perp_bid: *bid,
            perp_ask: *ask,
            tokens,
            perp_position,
        });
    }
    let looked = mind.per_share.look(value, accounted, plan::held(&names));
    Ok(Seen {
        picture: Picture {
            now: host.now(),
            vault_value: value,
            share: mind.per_share.now,
            peak: mind.per_share.peak,
            stop_holds: mind.per_share.stopped(),
            least_value: least,
            shorts_known,
            names,
        },
        looked,
        unpriced,
    })
}

/// Look, decide, and take one step; none from a look the share's measure
/// skipped, since what it would be decided from was read short.
///
/// A read that fails skips the look too, and is no fault: a fault ends
/// `run`, and the platform starts the agent again with its measure, and the
/// stop with it, afresh. The next book at least `STEP_EVERY` on looks again.
async fn step(host: &impl Host, mind: &mut Mind) -> Result<(), String> {
    let seen = match picture(host, mind).await {
        Ok(seen) => seen,
        Err(error) => {
            host.note(&format!("look skipped: a read failed: {error}"));
            return Ok(());
        }
    };
    if !seen.looked {
        return Ok(());
    }
    act(host, &seen.picture, seen.picture.next()).await?;
    Ok(())
}

/// Take `step`; whether the venues took any of it.
async fn act(host: &impl Host, picture: &Picture, step: Step) -> Result<bool, String> {
    let slippage = decimal(config::SLIPPAGE);
    let one = Decimal::from(1u8);
    match step {
        Step::Nothing => Ok(false),
        Step::Open {
            index,
            dollars,
            tokens,
            shares,
        } => {
            let name = &picture.names[index];
            // The money has to be at each venue before it can trade there;
            // the buffer stays home, where a withdrawal reaches it.
            let margin = shares * name.perp_bid * decimal(config::PERP_MARGIN);
            if !fund(host, rh::DRIVER, dollars, picture.vault_value).await
                || !fund(host, lt::DRIVER, margin, picture.vault_value).await
            {
                return Ok(false);
            }
            let bought = submit(
                host,
                rh::DRIVER,
                &rh::Action::Buy {
                    token: name.token.clone(),
                    amount: dollars,
                    min_tokens: tokens * (one - slippage),
                },
            )
            .await?;
            // No token, no short: a naked short is the one thing this
            // strategy never holds.
            if !bought {
                return Ok(false);
            }
            submit(
                host,
                lt::DRIVER,
                &lt::Action::Place(lt::Order {
                    market: name.perp.clone(),
                    side: lt::Side::Sell,
                    size: shares,
                    limit_price: name.perp_bid * (one - slippage),
                    tif: lt::Tif::Ioc,
                    reduce_only: false,
                    client_id: None,
                }),
            )
            .await?;
            Ok(true)
        }
        Step::Close {
            index,
            shares,
            tokens,
        } => {
            let name = &picture.names[index];
            let mut took = false;
            if shares > zero() {
                let bought_back = submit(
                    host,
                    lt::DRIVER,
                    &lt::Action::Place(lt::Order {
                        market: name.perp.clone(),
                        side: lt::Side::Buy,
                        size: shares,
                        limit_price: name.perp_ask * (one + slippage),
                        tif: lt::Tif::Ioc,
                        reduce_only: true,
                        client_id: None,
                    }),
                )
                .await?;
                // The token waits for the short: the next step sees what
                // the buy-back really closed.
                if !bought_back {
                    return Ok(false);
                }
                took = true;
            }
            if tokens > zero() {
                took |= submit(
                    host,
                    rh::DRIVER,
                    &rh::Action::Sell {
                        token: name.token.clone(),
                        tokens,
                        min_amount: tokens * name.token_price * (one - slippage),
                    },
                )
                .await?;
            }
            Ok(took)
        }
    }
}

/// Make sure `driver`'s account holds `wanted` dollars, moving the shortfall
/// from the vault's cash without going below the buffer. Whether it holds
/// enough now.
async fn fund(host: &impl Host, driver: &str, wanted: Decimal, vault_value: Decimal) -> bool {
    let balances = host.balances().await;
    let amount_of = |wanted: &dyn Fn(&Account) -> bool| -> Decimal {
        balances
            .iter()
            .find(|b| wanted(&b.account))
            .map(|b| Decimal::from(b.amount) / Decimal::from(1_000_000u32))
            .unwrap_or_else(zero)
    };
    let there = amount_of(&|account| matches!(account, Account::Venue(v) if v.driver == driver));
    if there >= wanted {
        return true;
    }
    let cash = amount_of(&|account| matches!(account, Account::Vault));
    let keep = vault_value * decimal(config::IDLE_BUFFER);
    let spare = if cash > keep { cash - keep } else { zero() };
    let short = wanted - there;
    if spare < short {
        return false;
    }
    matches!(
        host.move_(Account::Vault, venue(host, driver), millionths(short))
            .await,
        Moved::Done
    )
}

/// Submit one action; whether the venue took it.
async fn submit<A: serde::Serialize>(
    host: &impl Host,
    driver: &str,
    action: &A,
) -> Result<bool, String> {
    let verdicts = host.submit(driver, vec![to_payload(action)]).await;
    Ok(matches!(verdicts.first(), Some(Verdict::Accepted(_))))
}

/// Wait at least `seconds` on whatever wakes the agent, which has no timer
/// of its own, keeping each book that arrives meanwhile; false once nothing
/// will wake it again.
async fn pause(
    host: &impl Host,
    mind: &mut Mind,
    waking: &mut (impl Stream<Item = Woke> + Unpin),
    seconds: u64,
) -> bool {
    let until = host.now() + seconds;
    while host.now() < until {
        match waking.next().await {
            Some(Woke::Book(market, bid, ask)) => {
                mind.books.insert(market, (bid, ask));
            }
            Some(_) => {}
            None => return false,
        }
    }
    true
}

/// Close everything, short first, because a halt is an unwind with less
/// time.
///
/// It closes nothing it can't see whole: while a read fails, Lighter's
/// positions are unread, or something held has no book yet (an agent
/// started again within a halt has seen none), it waits for the next look
/// and closes what it can see meanwhile; a close the venues refuse is tried
/// again after the same pause. It ends cleanly only once nothing
/// is left to close, or the market would not take the closes; if the looks
/// run out with something held still unread or unpriced, it ends with an
/// error rather than as wound down, so the platform starts it again to
/// close the rest within the halt's grace.
async fn flatten(
    host: &impl Host,
    mind: &mut Mind,
    waking: &mut (impl Stream<Item = Woke> + Unpin),
) -> Result<(), String> {
    let mut unseen = String::new();
    for _ in 0..FLATTEN_STEPS {
        unseen.clear();
        match picture(host, mind).await {
            Err(error) => unseen = format!("a read failed: {error}"),
            // A halt acts on a look the measure skipped too: see
            // `Picture::halting`.
            Ok(seen) => {
                if !seen.picture.shorts_known {
                    unseen = "Lighter's positions are unread".into();
                } else if !seen.unpriced.is_empty() {
                    unseen = format!("no book yet for {}", seen.unpriced.join(", "));
                }
                match seen.picture.halting(seen.looked) {
                    close @ Step::Close { .. } => {
                        if act(host, &seen.picture, close).await? {
                            continue;
                        }
                        // Refused: try again after a pause, not at once.
                    }
                    _ if unseen.is_empty() => return Ok(()),
                    _ => {}
                }
            }
        }
        if !unseen.is_empty() {
            host.note(&format!("halt waiting to look again: {unseen}"));
        }
        if !pause(host, mind, waking, config::HALT_LOOKS_AGAIN_AFTER).await {
            break;
        }
    }
    if unseen.is_empty() {
        // The market would not take the closes: the contracts close the
        // rest.
        return Ok(());
    }
    Err(format!("the halt left something held unclosed: {unseen}"))
}

sdk::export_agent!(WeekendCarry);

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::transfers::Balance;
    use std::cell::{Cell, RefCell};

    fn d(text: &str) -> Decimal {
        decimal(text)
    }

    /// Saturday 3 October 2026, 12:00 UTC.
    const SATURDAY: u64 = 1_791_028_800;

    /// SPY's perpetual on Lighter.
    const SPY: u32 = 26;

    /// A stand-in for the host: a vault on Robinhood Chain carrying SPY
    /// tokens against SPY's perpetual, whose venues fill every order at the
    /// price, and whose reads fail while told to.
    struct Fake {
        clock: Cell<u64>,
        /// SPY's token price, and its perpetual's bid (the ask 5 cents up).
        price: Cell<Decimal>,
        bid: Cell<Decimal>,
        tokens: Cell<Decimal>,
        safe_dollars: Cell<Decimal>,
        /// Lighter: the short (negative), and the dollars that with it at
        /// the middle of the book make the account's worth.
        short: Cell<Decimal>,
        lighter_dollars: Cell<Decimal>,
        cash: Cell<Decimal>,
        /// How many of the next reads of each driver fail.
        robinhood_failing: Cell<u32>,
        lighter_failing: Cell<u32>,
        sent: RefCell<Vec<String>>,
    }

    impl Fake {
        /// About $100,000: 100 SPY tokens at $700 hedged by 100 shorted at
        /// $710, with $17,500 of margin, and $12,000 of cash.
        fn carrying() -> Self {
            Self {
                clock: Cell::new(SATURDAY),
                price: Cell::new(d("700")),
                bid: Cell::new(d("710")),
                tokens: Cell::new(d("100")),
                safe_dollars: Cell::new(zero()),
                short: Cell::new(d("-100")),
                lighter_dollars: Cell::new(d("88500")),
                cash: Cell::new(d("12000")),
                robinhood_failing: Cell::new(0),
                lighter_failing: Cell::new(0),
                sent: RefCell::new(Vec::new()),
            }
        }

        fn ask(&self) -> Decimal {
            self.bid.get() + d("0.05")
        }

        fn book(&self) -> Woke {
            Woke::Book(SPY, self.bid.get(), self.ask())
        }

        fn lighter_worth(&self) -> Decimal {
            self.lighter_dollars.get() + self.short.get() * (self.bid.get() + self.ask()) / d("2")
        }

        fn opened(&self) -> usize {
            self.sent
                .borrow()
                .iter()
                .filter(|sent| sent.as_str() == "buy")
                .count()
        }

        fn failing(count: &Cell<u32>) -> bool {
            let failing = count.get() > 0;
            if failing {
                count.set(count.get() - 1);
            }
            failing
        }
    }

    impl Host for Fake {
        fn home_chain(&self) -> String {
            "robinhood".into()
        }

        fn now(&self) -> u64 {
            // Each look at the clock is a second on.
            self.clock.set(self.clock.get() + 1);
            self.clock.get()
        }

        fn note(&self, _: &str) {}

        async fn account_state(&self, driver: &str, query: Vec<u8>) -> Result<Vec<u8>, String> {
            if driver == lt::DRIVER {
                if Fake::failing(&self.lighter_failing) {
                    return Err("Lighter timed out".into());
                }
                return Ok(to_payload(&lt::AccountState {
                    account_index: 1,
                    equity: self.lighter_worth(),
                    available: zero(),
                    positions: vec![lt::Position {
                        market: "SPY".into(),
                        size: self.short.get(),
                        entry_price: d("710"),
                        unrealized_pnl: zero(),
                    }],
                }));
            }
            if Fake::failing(&self.robinhood_failing) {
                return Err("the RPC timed out".into());
            }
            Ok(match from_payload::<rh::Query>(&query)? {
                rh::Query::Prices => to_payload(&vec![rh::TokenPrice {
                    token: "SPY".into(),
                    feed_price: self.price.get(),
                    feed_updated_at: 0,
                    market_price: self.price.get(),
                    multiplier: d("1"),
                    paused: false,
                }]),
                rh::Query::Holdings => to_payload(&rh::AccountState {
                    safe: "0x01".into(),
                    dollars: self.safe_dollars.get(),
                    holdings: vec![rh::Holding {
                        token: "SPY".into(),
                        tokens: self.tokens.get(),
                        feed_price: self.price.get(),
                        feed_updated_at: 0,
                        multiplier: d("1"),
                    }],
                }),
            })
        }

        async fn submit(&self, driver: &str, actions: Vec<Vec<u8>>) -> Vec<Verdict> {
            let price = self.price.get();
            let sent = if driver == lt::DRIVER {
                let lt::Action::Place(order) = from_payload(&actions[0]).unwrap() else {
                    panic!("only orders are placed")
                };
                match order.side {
                    lt::Side::Buy => {
                        self.short.set(self.short.get() + order.size);
                        self.lighter_dollars
                            .set(self.lighter_dollars.get() - order.size * self.ask());
                        "buy-back"
                    }
                    lt::Side::Sell => {
                        self.short.set(self.short.get() - order.size);
                        self.lighter_dollars
                            .set(self.lighter_dollars.get() + order.size * self.bid.get());
                        "short"
                    }
                }
            } else {
                match from_payload(&actions[0]).unwrap() {
                    rh::Action::Buy { amount, .. } => {
                        self.safe_dollars.set(self.safe_dollars.get() - amount);
                        self.tokens.set(self.tokens.get() + amount / price);
                        "buy"
                    }
                    rh::Action::Sell { tokens, .. } => {
                        self.tokens.set(self.tokens.get() - tokens);
                        self.safe_dollars
                            .set(self.safe_dollars.get() + tokens * price);
                        "sell"
                    }
                }
            };
            self.sent.borrow_mut().push(sent.into());
            vec![Verdict::Accepted(Vec::new())]
        }

        async fn move_(&self, from: Account, to: Account, amount: u64) -> Moved {
            let dollars = Decimal::from(amount) / Decimal::from(1_000_000u32);
            let account = |account: &Account| match account {
                Account::Vault => &self.cash,
                Account::Venue(venue) if venue.driver == lt::DRIVER => &self.lighter_dollars,
                _ => &self.safe_dollars,
            };
            account(&from).set(account(&from).get() - dollars);
            account(&to).set(account(&to).get() + dollars);
            Moved::Done
        }

        async fn balances(&self) -> Vec<Balance> {
            let millionths = |dollars: Decimal| sdk::millionths(dollars.max(zero()));
            vec![
                Balance {
                    account: Account::Vault,
                    amount: millionths(self.cash.get()),
                },
                Balance {
                    account: Account::Station("robinhood".into()),
                    amount: 0,
                },
                Balance {
                    account: venue(self, rh::DRIVER),
                    amount: millionths(
                        self.tokens.get() * self.price.get() + self.safe_dollars.get(),
                    ),
                },
                Balance {
                    account: venue(self, lt::DRIVER),
                    amount: millionths(self.lighter_worth()),
                },
            ]
        }
    }

    fn look(host: &Fake, mind: &mut Mind) {
        mind.books.insert(SPY, (host.bid.get(), host.ask()));
        futures::executor::block_on(step(host, mind)).expect("a step is no fault");
    }

    /// What wakes the agent while it waits: SPY's book, over and over.
    fn books(host: &Fake) -> impl Stream<Item = Woke> + Unpin {
        let book = host.book();
        futures::stream::iter(std::iter::repeat_with(move || match book {
            Woke::Book(market, bid, ask) => Woke::Book(market, bid, ask),
            _ => Woke::Nothing,
        }))
    }

    #[test]
    fn a_read_that_fails_after_the_stop_leaves_the_stop_on_and_opens_nothing() {
        let host = Fake::carrying();
        let mut mind = Mind::new();
        look(&host, &mut mind);
        // The gap widens $110 a share: 11% lost, stopped. Both legs close.
        host.bid.set(d("820"));
        for _ in 0..64 {
            look(&host, &mut mind);
        }
        assert!(mind.per_share.stopped());
        assert_eq!(
            (host.tokens.get(), host.short.get()),
            (zero(), zero()),
            "both legs cut"
        );
        // A read fails: the step is no fault, so the agent is not started
        // again, and its stop holds.
        host.robinhood_failing.set(1);
        mind.books.insert(SPY, (host.bid.get(), host.ask()));
        assert_eq!(
            futures::executor::block_on(step(&host, &mut mind)),
            Ok(()),
            "a failed read ends the agent"
        );
        assert!(mind.per_share.stopped(), "the stop was cleared");
        // The perpetual comes back to a premium worth opening on.
        host.bid.set(d("704"));
        for _ in 0..8 {
            look(&host, &mut mind);
        }
        assert!(mind.per_share.stopped());
        assert_eq!(host.opened(), 0, "opened again");
        // What a measure started afresh would have done there.
        let mut fresh = Mind::new();
        look(&host, &mut fresh);
        assert_eq!(host.opened(), 1);
    }

    #[test]
    fn a_halt_handed_to_a_freshly_started_agent_with_no_books_yet_closes_what_the_vault_holds() {
        let host = Fake::carrying();
        // Started again within the halt: no book seen yet.
        let mut mind = Mind::new();
        let mut waking = books(&host);
        assert_eq!(
            futures::executor::block_on(flatten(&host, &mut mind, &mut waking)),
            Ok(())
        );
        assert_eq!(
            (host.tokens.get(), host.short.get()),
            (zero(), zero()),
            "left the carry open"
        );
        // Short first, every time.
        let sent = host.sent.borrow();
        assert_eq!(sent.first().map(String::as_str), Some("buy-back"));
    }

    #[test]
    fn a_halt_that_never_sees_a_book_for_what_it_holds_never_ends_as_wound_down() {
        let host = Fake::carrying();
        let mut mind = Mind::new();
        // Nothing but empty books wakes it.
        let mut waking = futures::stream::iter(std::iter::repeat_with(|| Woke::Nothing));
        let before = host.clock.get();
        let ended = futures::executor::block_on(flatten(&host, &mut mind, &mut waking));
        assert!(ended.is_err(), "wound down with the carry unpriced");
        assert!(host.sent.borrow().is_empty(), "traded unpriced");
        // Its looks were paced, not spun through.
        assert!(host.clock.get() - before >= FLATTEN_STEPS as u64 * config::HALT_LOOKS_AGAIN_AFTER);
    }

    #[test]
    fn a_halt_whose_reads_fail_looks_again_after_a_pause_and_closes_short_first() {
        let host = Fake::carrying();
        let mut mind = Mind::new();
        mind.books.insert(SPY, (host.bid.get(), host.ask()));
        // The Safe's reads fail twice, then Lighter's positions go unread
        // twice: no token is sold while the short is unknown.
        host.robinhood_failing.set(2);
        host.lighter_failing.set(2);
        let mut waking = books(&host);
        let before = host.clock.get();
        assert_eq!(
            futures::executor::block_on(flatten(&host, &mut mind, &mut waking)),
            Ok(())
        );
        assert_eq!((host.tokens.get(), host.short.get()), (zero(), zero()));
        assert_eq!(
            host.sent.borrow().first().map(String::as_str),
            Some("buy-back")
        );
        assert!(host.clock.get() - before >= 4 * config::HALT_LOOKS_AGAIN_AFTER);
    }

    #[test]
    fn a_start_inside_a_halt_opens_nothing_and_goes_to_flatten() {
        // Nothing carried, at a premium worth opening on, on a Saturday:
        // an ordinary look would open.
        let host = Fake::carrying();
        host.tokens.set(zero());
        host.short.set(zero());
        host.lighter_dollars.set(d("17500"));
        host.bid.set(d("704"));
        let mut mind = Mind::new();
        // Started again within the halt: the notice waits first in the
        // vault's news, and the books are there from the start too.
        let halting = Woke::Vault(vault::VaultEvent::Halting("halted".into()));
        let vault = futures::stream::iter([halting]).chain(futures::stream::pending());
        assert_eq!(
            futures::executor::block_on(serve(&host, &mut mind, vault_first(vault, books(&host)))),
            Ok(())
        );
        assert_eq!(host.opened(), 0, "opened within the halt");
        // And with a carry on, the start goes to the halt and closes it.
        let host = Fake::carrying();
        let mut mind = Mind::new();
        let halting = Woke::Vault(vault::VaultEvent::Halting("halted".into()));
        let vault = futures::stream::iter([halting]).chain(futures::stream::pending());
        futures::executor::block_on(serve(&host, &mut mind, vault_first(vault, books(&host))))
            .unwrap();
        assert_eq!((host.tokens.get(), host.short.get()), (zero(), zero()));
        assert_eq!(host.opened(), 0);
    }
}
