//! Holds a few spot tokens at fixed weights against the dollar, and puts
//! each back to its weight when the market moves it outside its band.
//!
//! One agent for two chains: on Robinhood Chain it holds four stock tokens
//! against USDG through the `robinhood` driver, on Base it holds WETH
//! against USDC through `uniswap-base`, the same driver's contract there.
//! Which one it runs is the vault's chain (`transfers::home_chain`), and
//! which dollar it holds is whatever the driver says it trades against, so
//! nothing here assumes a chain's dollar.
//!
//! It wakes on the prices of what it holds where a shared stream carries
//! them (Chainlink's feeds of the stock tokens), on ether's book on
//! Hyperliquid where none does (WETH on Base, read as a heartbeat, never as
//! a price), and on the vault's events; at most every `STEP_EVERY` seconds
//! it looks at its Safe and takes one step towards its weights (`plan.rs`).
//! Its limits are in `config.rs`, and each manifest claims them in the same
//! words.

mod config;
mod host;
mod per_share;
mod plan;
mod venues;

use futures::stream::PollNext;
use futures::{Stream, StreamExt};
use host::{Host, Sdk};
use per_share::PerShare;
use plan::{Name, Picture, Step, zero};
use sdk::streams::{
    self, CHAINLINK, ChainlinkFeed, ChainlinkValue, HYPERLIQUID, HyperliquidFeed, HyperliquidValue,
};
use sdk::transfers::{Account, Moved, VenueAccount};
use sdk::{Decimal, Verdict, decimal, from_payload, millionths, to_payload, vault};
use strategy_sdk as sdk;

/// How many steps a halt may take to sell everything before leaving the
/// rest to the contracts.
const FLATTEN_STEPS: usize = 64;

struct Spot;

/// What the agent remembers between steps: its venue, and the value of a
/// vault share since it started, and its peak.
struct Mind {
    venue: &'static config::Venue,
    per_share: PerShare,
    last_step: u64,
}

impl Mind {
    fn new(venue: &'static config::Venue) -> Self {
        Self {
            venue,
            per_share: PerShare::new(config::DRAWDOWN_STOP),
            last_step: 0,
        }
    }
}

enum Woke {
    Tick,
    Vault(vault::VaultEvent),
}

impl sdk::Agent for Spot {
    async fn run() -> Result<(), String> {
        let host = Sdk;
        let home = host.home_chain();
        let venue = config::VENUES
            .iter()
            .find(|venue| venue.chain == home)
            .ok_or_else(|| format!("this agent trades on no venue on {home}"))?;
        let mut mind = Mind::new(venue);
        let ticks = clock(venue)?;
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        serve(&host, &mut mind, vault_first(vault_events, ticks)).await
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
/// first wake: the vault's news is read first, so a start within a halt
/// goes straight to the halt, and the first ordinary look (a restart
/// picking up wherever it was) comes at the first tick after it.
async fn serve(
    host: &impl Host,
    mind: &mut Mind,
    mut waking: impl Stream<Item = Woke> + Unpin,
) -> Result<(), String> {
    while let Some(woke) = waking.next().await {
        match woke {
            Woke::Tick => {
                if host.now() >= mind.last_step + config::STEP_EVERY {
                    step(host, mind).await?;
                }
            }
            // A leaver's fraction is being closed by the contracts; look
            // again at what is left.
            Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(host, mind).await?,
            // Leavers wait on more cash than the vault holds: bring the
            // Safe's idle dollars home.
            Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                let _ = host
                    .move_(safe(host, mind.venue), Account::Vault, short)
                    .await;
            }
            Woke::Vault(vault::VaultEvent::Halting(_)) => {
                flatten(host, mind, &mut waking).await?;
                return Ok(());
            }
        }
    }
    Ok(())
}

/// What wakes the agent: each new Chainlink round of a token it holds, where
/// the shared stream has the token's feed, else ether's book on Hyperliquid
/// as a heartbeat.
fn clock(venue: &config::Venue) -> Result<futures::stream::LocalBoxStream<'static, Woke>, String> {
    let mut ticks = Vec::new();
    if venue.chain == "robinhood" {
        for (token, _) in venue.weights {
            let stream = streams::subscribe::<_, ChainlinkValue>(
                CHAINLINK,
                &ChainlinkFeed::Price {
                    token: token.to_string(),
                },
            )?;
            ticks.push(stream.map(|_| Woke::Tick).boxed_local());
        }
    } else {
        let stream = streams::subscribe::<_, HyperliquidValue>(
            HYPERLIQUID,
            &HyperliquidFeed::Book {
                coin: "ETH".to_string(),
            },
        )?;
        ticks.push(stream.map(|_| Woke::Tick).boxed_local());
    }
    Ok(futures::stream::select_all(ticks).boxed_local())
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

/// The vault's account at its driver: the station's Safe.
fn safe(host: &impl Host, venue: &config::Venue) -> Account {
    Account::Venue(VenueAccount {
        chain: host.home_chain(),
        driver: venue.driver.to_string(),
    })
}

async fn state<Q: serde::Serialize, A: serde::de::DeserializeOwned>(
    host: &impl Host,
    venue: &config::Venue,
    query: &Q,
) -> Result<A, String> {
    from_payload(&host.account_state(venue.driver, to_payload(query)).await?)
}

/// What the agent sees now, and whether the share's measure could take it
/// in: a look one of whose reads failed or came back partial is not decided
/// from.
async fn picture(host: &impl Host, mind: &mut Mind) -> Result<(Picture, bool), String> {
    let prices: Vec<venues::TokenPrice> = state(host, mind.venue, &venues::Query::Prices).await?;
    let held: venues::AccountState = state(host, mind.venue, &venues::Query::Holdings).await?;
    let balances = host.balances().await;
    let dollars = |amount: u64| Decimal::from(amount) / Decimal::from(1_000_000u32);
    // Every account the vault has, the Safe valued by its driver.
    let value = dollars(balances.iter().map(|balance| balance.amount).sum());
    let read = |account: &Account| {
        balances
            .iter()
            .find(|balance| same(&balance.account, account))
            .map(|balance| dollars(balance.amount))
    };
    let home = host.home_chain();
    // An account whose read failed is left out of the balances, not read
    // as empty: each of the vault's must be there.
    let (cash, idle, in_safe) = (
        read(&Account::Vault),
        read(&Account::Station(home)),
        read(&safe(host, mind.venue)),
    );
    // What the Safe's holdings and the vault's dollars say it is worth.
    let accounted = match (cash, idle, in_safe) {
        (Some(cash), Some(idle), Some(_)) => Some((cash, idle)),
        _ => None,
    };
    let cash = cash.unwrap_or_else(zero);
    let mut names = Vec::new();
    for (token, weight) in mind.venue.weights {
        // A token the driver doesn't list is not traded.
        let Some(price) = prices.iter().find(|price| price.token == *token) else {
            continue;
        };
        let tokens = held
            .holdings
            .iter()
            .find(|holding| holding.token == *token)
            .map_or_else(zero, |holding| holding.tokens);
        names.push(Name {
            token: token.to_string(),
            weight: decimal(weight),
            tokens,
            feed_price: price.feed_price,
            market_price: price.market_price,
            paused: price.paused,
        });
    }
    let accounted =
        accounted.map(|(cash, idle)| plan::accounted(&names, cash + idle + held.dollars));
    let looked = mind.per_share.look(value, accounted, plan::held(&names));
    Ok((
        Picture {
            value,
            cash,
            safe_dollars: held.dollars,
            share: mind.per_share.now,
            peak: mind.per_share.peak,
            stop_holds: mind.per_share.stopped(),
            names,
        },
        looked,
    ))
}

/// Look, decide, and take one step; none from a look the share's measure
/// skipped, since what it would be decided from was read short.
///
/// A read that fails skips the look too, and is no fault: a fault ends
/// `run`, and the platform starts the agent again with its measure, and the
/// stop with it, afresh. The next wake at least `STEP_EVERY` on looks again.
async fn step(host: &impl Host, mind: &mut Mind) -> Result<(), String> {
    mind.last_step = host.now();
    let (picture, looked) = match picture(host, mind).await {
        Ok(seen) => seen,
        Err(error) => {
            host.note(&format!("look skipped: a read failed: {error}"));
            return Ok(());
        }
    };
    if looked {
        act(host, mind.venue, &picture, picture.next(false)).await?;
    }
    Ok(())
}

/// Take `step`; whether the driver took it.
async fn act(
    host: &impl Host,
    venue: &config::Venue,
    picture: &Picture,
    step: Step,
) -> Result<bool, String> {
    match step {
        Step::Nothing => Ok(false),
        Step::Buy {
            index,
            dollars,
            min_tokens,
        } => {
            // The dollars go to the Safe first, from the vault's cash.
            if picture.safe_dollars < dollars {
                let short = dollars - picture.safe_dollars;
                if !matches!(
                    host.move_(Account::Vault, safe(host, venue), millionths(short))
                        .await,
                    Moved::Done
                ) {
                    return Ok(false);
                }
            }
            submit(
                host,
                venue,
                &venues::Action::Buy {
                    token: picture.names[index].token.clone(),
                    amount: dollars,
                    min_tokens,
                },
            )
            .await
        }
        Step::Sell {
            index,
            tokens,
            min_dollars,
        } => {
            submit(
                host,
                venue,
                &venues::Action::Sell {
                    token: picture.names[index].token.clone(),
                    tokens,
                    min_amount: min_dollars,
                },
            )
            .await
        }
    }
}

/// Submit one action; whether the venue took it.
async fn submit(
    host: &impl Host,
    venue: &config::Venue,
    action: &venues::Action,
) -> Result<bool, String> {
    let verdicts = host.submit(venue.driver, vec![to_payload(action)]).await;
    Ok(matches!(verdicts.first(), Some(Verdict::Accepted(_))))
}

/// Wait at least `seconds` on whatever wakes the agent, which has no timer
/// of its own; false once nothing will wake it again.
async fn pause(
    host: &impl Host,
    waking: &mut (impl Stream<Item = Woke> + Unpin),
    seconds: u64,
) -> bool {
    let until = host.now() + seconds;
    while host.now() < until {
        if waking.next().await.is_none() {
            return false;
        }
    }
    true
}

/// Sell everything, one bounded order at a time, then bring the dollars
/// home: a halt is an unwind with less time. A read that fails is looked
/// again at after a pause, within the same bound of steps; if the steps run
/// out with the last look unread, the halt ends with an error rather than as
/// wound down, so the platform starts the agent again within the halt's
/// grace.
async fn flatten(
    host: &impl Host,
    mind: &mut Mind,
    waking: &mut (impl Stream<Item = Woke> + Unpin),
) -> Result<(), String> {
    let mut unread = None;
    for _ in 0..FLATTEN_STEPS {
        // A halt acts on a look the measure skipped too: see
        // `Picture::halting`.
        let (picture, looked) = match picture(host, mind).await {
            Ok(seen) => seen,
            Err(error) => {
                host.note(&format!("halt's look skipped: a read failed: {error}"));
                unread = Some(error);
                if !pause(host, waking, config::HALT_LOOKS_AGAIN_AFTER).await {
                    break;
                }
                continue;
            }
        };
        unread = None;
        let step = picture.halting(looked);
        if step == Step::Nothing || !act(host, mind.venue, &picture, step).await? {
            break;
        }
    }
    let held: Result<venues::AccountState, String> =
        state(host, mind.venue, &venues::Query::Holdings).await;
    if let Ok(held) = held
        && held.dollars > zero()
    {
        let _ = host
            .move_(
                safe(host, mind.venue),
                Account::Vault,
                millionths(held.dollars),
            )
            .await;
    }
    match unread {
        Some(error) => Err(format!("the halt could not read what is held: {error}")),
        None => Ok(()),
    }
}

sdk::export_agent!(Spot);

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::transfers::Balance;
    use std::cell::{Cell, RefCell};

    fn d(text: &str) -> Decimal {
        decimal(text)
    }

    /// A stand-in for the host: a vault on Base whose Safe holds WETH,
    /// whose driver fills every order at the price, and whose next
    /// `failing` reads fail.
    struct Fake {
        clock: Cell<u64>,
        price: Cell<Decimal>,
        weth: Cell<Decimal>,
        safe_dollars: Cell<Decimal>,
        cash: Cell<Decimal>,
        failing: Cell<u32>,
        sent: RefCell<Vec<venues::Action>>,
    }

    impl Fake {
        /// $1,000: 0.2 WETH at $2,500, and $500 of cash.
        fn new() -> Self {
            Self {
                clock: Cell::new(1_000_000),
                price: Cell::new(d("2500")),
                weth: Cell::new(d("0.2")),
                safe_dollars: Cell::new(zero()),
                cash: Cell::new(d("500")),
                failing: Cell::new(0),
                sent: RefCell::new(Vec::new()),
            }
        }

        fn venue() -> &'static config::Venue {
            &config::VENUES[1]
        }

        fn buys(&self) -> usize {
            self.sent
                .borrow()
                .iter()
                .filter(|action| matches!(action, venues::Action::Buy { .. }))
                .count()
        }
    }

    impl Host for Fake {
        fn home_chain(&self) -> String {
            "base".into()
        }

        fn now(&self) -> u64 {
            // Each look at the clock is a second on.
            self.clock.set(self.clock.get() + 1);
            self.clock.get()
        }

        fn note(&self, _: &str) {}

        async fn account_state(&self, _: &str, query: Vec<u8>) -> Result<Vec<u8>, String> {
            if self.failing.get() > 0 {
                self.failing.set(self.failing.get() - 1);
                return Err("the driver timed out".into());
            }
            Ok(match from_payload::<venues::Query>(&query)? {
                venues::Query::Prices => to_payload(&vec![venues::TokenPrice {
                    token: "WETH".into(),
                    feed_price: self.price.get(),
                    feed_updated_at: 0,
                    market_price: self.price.get(),
                    multiplier: Decimal::from(1u8),
                    paused: false,
                }]),
                venues::Query::Holdings => to_payload(&venues::AccountState {
                    safe: "0x01".into(),
                    dollar: "USDC".into(),
                    dollars: self.safe_dollars.get(),
                    holdings: vec![venues::Holding {
                        token: "WETH".into(),
                        tokens: self.weth.get(),
                        feed_price: self.price.get(),
                        feed_updated_at: 0,
                        multiplier: Decimal::from(1u8),
                    }],
                }),
            })
        }

        async fn submit(&self, _: &str, actions: Vec<Vec<u8>>) -> Vec<Verdict> {
            let action: venues::Action = from_payload(&actions[0]).unwrap();
            let price = self.price.get();
            match &action {
                venues::Action::Buy { amount, .. } => {
                    self.safe_dollars.set(self.safe_dollars.get() - *amount);
                    self.weth.set(self.weth.get() + *amount / price);
                }
                venues::Action::Sell { tokens, .. } => {
                    self.weth.set(self.weth.get() - *tokens);
                    self.safe_dollars
                        .set(self.safe_dollars.get() + *tokens * price);
                }
            }
            self.sent.borrow_mut().push(action);
            vec![Verdict::Accepted(Vec::new())]
        }

        async fn move_(&self, from: Account, _: Account, amount: u64) -> Moved {
            let dollars = Decimal::from(amount) / Decimal::from(1_000_000u32);
            let (from, to) = match from {
                Account::Vault => (&self.cash, &self.safe_dollars),
                _ => (&self.safe_dollars, &self.cash),
            };
            from.set(from.get() - dollars);
            to.set(to.get() + dollars);
            Moved::Done
        }

        async fn balances(&self) -> Vec<Balance> {
            let millionths = |dollars: Decimal| sdk::millionths(dollars);
            vec![
                Balance {
                    account: Account::Vault,
                    amount: millionths(self.cash.get()),
                },
                Balance {
                    account: Account::Station("base".into()),
                    amount: 0,
                },
                Balance {
                    account: safe(self, Fake::venue()),
                    amount: millionths(
                        self.weth.get() * self.price.get() + self.safe_dollars.get(),
                    ),
                },
            ]
        }
    }

    fn look(host: &Fake, mind: &mut Mind) {
        futures::executor::block_on(step(host, mind)).expect("a step is no fault");
    }

    #[test]
    fn a_read_that_fails_after_the_stop_leaves_the_stop_on_and_buys_nothing() {
        let host = Fake::new();
        let mut mind = Mind::new(Fake::venue());
        look(&host, &mut mind);
        // Ether falls 40%: a share by 20%, past the stop. It sells out.
        host.price.set(d("1500"));
        for _ in 0..64 {
            look(&host, &mut mind);
        }
        assert!(mind.per_share.stopped());
        assert_eq!(host.weth.get(), zero(), "sold out");
        // A read fails: the step is no fault, so the agent is not started
        // again, and its stop holds.
        host.failing.set(1);
        assert_eq!(
            futures::executor::block_on(step(&host, &mut mind)),
            Ok(()),
            "a failed read ends the agent"
        );
        assert!(mind.per_share.stopped(), "the stop was cleared");
        // Ether recovers past where it was: nothing is bought back.
        host.price.set(d("3000"));
        for _ in 0..8 {
            look(&host, &mut mind);
        }
        assert!(mind.per_share.stopped());
        assert_eq!(host.buys(), 0, "bought back in");
        // What a measure started afresh would have done there.
        let mut fresh = Mind::new(Fake::venue());
        look(&host, &mut fresh);
        assert_eq!(host.buys(), 1);
    }

    #[test]
    fn a_halt_whose_read_fails_looks_again_after_a_pause_and_still_sells_everything() {
        let host = Fake::new();
        let mut mind = Mind::new(Fake::venue());
        host.failing.set(3);
        let mut waking = futures::stream::iter(std::iter::repeat_with(|| Woke::Tick).take(1_000));
        let before = host.clock.get();
        futures::executor::block_on(flatten(&host, &mut mind, &mut waking)).unwrap();
        assert_eq!(host.weth.get(), zero(), "left WETH in the Safe");
        assert!(
            host.clock.get() - before >= 3 * config::HALT_LOOKS_AGAIN_AFTER,
            "looked again without a pause"
        );
        assert_eq!(host.safe_dollars.get(), zero(), "the dollars stayed out");
    }

    #[test]
    fn a_start_inside_a_halt_buys_nothing_and_goes_to_flatten() {
        // Under its weight: an ordinary look would buy WETH.
        let host = Fake::new();
        host.weth.set(d("0.1"));
        host.cash.set(d("750"));
        let mut mind = Mind::new(Fake::venue());
        // Started again within the halt: the notice waits first in the
        // vault's news, and the ticks are there from the start too.
        let vault =
            futures::stream::iter([Woke::Vault(vault::VaultEvent::Halting("halted".into()))]);
        let ticks = futures::stream::iter(std::iter::repeat_with(|| Woke::Tick).take(1_000));
        futures::executor::block_on(serve(
            &host,
            &mut mind,
            vault_first(then_waits(vault), ticks),
        ))
        .unwrap();
        assert_eq!(host.buys(), 0, "bought within the halt");
        assert_eq!(host.weth.get(), zero(), "never went to the halt");
    }

    /// The vault's news, as a stream that then waits for ever, as the
    /// vault's does.
    fn then_waits(vault: impl Stream<Item = Woke> + Unpin) -> impl Stream<Item = Woke> + Unpin {
        vault.chain(futures::stream::pending())
    }

    #[test]
    fn a_halt_that_cannot_read_what_is_held_never_ends_as_wound_down() {
        let host = Fake::new();
        let mut mind = Mind::new(Fake::venue());
        host.failing.set(u32::MAX);
        let mut waking = futures::stream::iter(std::iter::repeat_with(|| Woke::Tick).take(10_000));
        let ended = futures::executor::block_on(flatten(&host, &mut mind, &mut waking));
        assert!(ended.is_err(), "ended as wound down without reading");
        assert_eq!(host.weth.get(), d("0.2"));
    }
}
