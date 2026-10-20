//! Holds spot, shorts the perpetual against it, and collects the funding, on
//! Hyperliquid.
//!
//! The same strategy as `funding-basis`, on the venue it was meant for: both
//! legs in one HyperCore account that the vault's station owns. The account
//! is the station's, so the agent's key can trade it and move dollars
//! between its own perp and spot balances, and nothing else.
//!
//! Prices and funding come from Shonin's shared Hyperliquid stream; the
//! account comes from the driver. The agent keeps the latest book of each
//! leg as they arrive, and takes one step towards what it wants to hold when
//! funding crosses a threshold or its account changes, if that step is worth
//! doing (`decide.rs` says when it is). It never polls.

mod config;
mod decide;
mod host;
mod per_share;

use decide::{
    Leavers, Patience, Plan, Quotes, Snapshot, Top, hedge, held, limits, spot_dollars, trim, unwind,
};
use futures::stream::PollNext;
use futures::{Stream, StreamExt};
use hl::{AccountState, Action, Balance, Order, Receipt, Side};
use host::{Host, Sdk};
use per_share::PerShare;
use sdk::streams::{self, HYPERLIQUID, HyperliquidFeed, HyperliquidValue};
use sdk::transfers::{Account, Moved, VenueAccount};
use sdk::{Decimal, Limits, drivers, from_payload, millionths, to_payload, vault};
use strategy_sdk as sdk;
use strategy_sdk::hyperliquid as hl;

/// The driver this agent trades through, as its manifest names it.
const DRIVER: &str = hl::DRIVER;

/// How many steps a halt may take to flatten the book before leaving the
/// rest to the contract.
const FLATTEN_STEPS: usize = 32;

/// How many perpetual orders one step may send to level the short with the
/// spot. One is almost always enough; more are for a gap wider than the
/// order cap, or an order that only partly filled.
const HEDGE_ORDERS: usize = 8;

struct FundingBasis;

/// What the strategy knows. Rebuilt from the venue and the stream whenever
/// it starts, so losing it to a restart costs a moment, and the stop (see
/// `per_share.rs`).
struct Mind {
    limits: Limits,
    quotes: Quotes,
    /// The value of a vault share while this instance watched it, and its
    /// peak: what the drawdown stop is measured on.
    per_share: PerShare,
    /// The funding the account has been paid since this instance began
    /// (negative when it paid), as the venue's events said.
    funding_paid: Decimal,
    /// Whether the perp's leverage has been set since this instance began.
    levered: bool,
    /// Whether the last look found something to do, so the next funding
    /// update is worth another.
    busy: bool,
    /// How long to leave a plan the venue or the station refused.
    patience: Patience,
    /// The vault's cash that leavers are waiting on. Lost to a restart, which
    /// at worst sends it out once more for the next `buffer-low` to bring
    /// back.
    leavers: Leavers,
    /// After a read that failed, the agent looks again no sooner than this
    /// (seconds since the Unix epoch).
    looks_again_at: u64,
}

impl Mind {
    fn new() -> Self {
        Self {
            limits: limits(),
            quotes: Quotes::default(),
            per_share: PerShare::new(config::DRAWDOWN_STOP),
            funding_paid: decide::zero(),
            levered: false,
            busy: false,
            patience: Patience::default(),
            leavers: Leavers::default(),
            looks_again_at: 0,
        }
    }
}

/// What woke the strategy up.
enum Woke {
    Venue(Vec<u8>),
    Vault(vault::VaultEvent),
    SpotBook(Top),
    PerpBook(Top),
    Funding(Decimal),
    /// A stream value that didn't read: ignored.
    Nothing,
}

fn top_of(value: Result<streams::Stamped<HyperliquidValue>, String>) -> Option<Top> {
    match value.ok()?.value {
        HyperliquidValue::Book(book) => Some(Top {
            bid: book.bids.first().map(|level| level.price),
            ask: book.asks.first().map(|level| level.price),
        }),
        _ => None,
    }
}

impl sdk::Agent for FundingBasis {
    async fn run() -> Result<(), String> {
        let host = Sdk;
        let mut mind = Mind::new();

        let spot_book = streams::subscribe::<_, HyperliquidValue>(
            HYPERLIQUID,
            &HyperliquidFeed::Book {
                coin: config::SPOT.into(),
            },
        )?
        .map(|value| top_of(value).map_or(Woke::Nothing, Woke::SpotBook));
        let perp_book = streams::subscribe::<_, HyperliquidValue>(
            HYPERLIQUID,
            &HyperliquidFeed::Book {
                coin: config::PERP.into(),
            },
        )?
        .map(|value| top_of(value).map_or(Woke::Nothing, Woke::PerpBook));
        let funding = streams::subscribe::<_, HyperliquidValue>(
            HYPERLIQUID,
            &HyperliquidFeed::Funding {
                coin: config::PERP.into(),
            },
        )?
        .map(|value| match value.map(|stamped| stamped.value) {
            Ok(HyperliquidValue::Funding(funding)) => {
                Woke::Funding(funding.hourly_rate * Decimal::from(24))
            }
            _ => Woke::Nothing,
        });
        let account = drivers::account_events(DRIVER)
            .into_stream()
            .map(Woke::Venue);
        let vault_events = vault::events().into_stream().map(Woke::Vault);

        let others = futures::stream::select_all([
            spot_book.boxed_local(),
            perp_book.boxed_local(),
            funding.boxed_local(),
            account.boxed_local(),
        ]);
        serve(&host, &mut mind, vault_first(vault_events, others)).await
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
            // Prices only update what the agent knows; it acts when
            // funding or its account moves, not on every quote.
            Woke::SpotBook(top) => mind.quotes.spot = top,
            Woke::PerpBook(top) => mind.quotes.perp = top,
            // Funding arrives every few seconds. It is looked at when it
            // crosses a threshold, or while there is still something to
            // do; otherwise the account's own events (fills, the hourly
            // funding payment) are what wake the strategy.
            Woke::Funding(daily) => {
                let before = mind.quotes.funding.replace(daily);
                if decide::steps_on_funding(before, daily, mind.busy) {
                    step(host, mind, false).await?;
                }
            }
            Woke::Venue(event) => {
                mind.funding_paid += decide::funding_paid(&event);
                if decide::wakes_for(&event) {
                    step(host, mind, false).await?
                }
            }
            Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(host, mind, false).await?,
            Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                // As much as the venue will let go of; a claim's close
                // covers whatever this doesn't.
                let _ = host.move_(venue(host), Account::Vault, short).await;
                step(host, mind, true).await?
            }
            Woke::Vault(vault::VaultEvent::Halting(_)) => {
                flatten(host, mind, &mut waking).await?;
                return Ok(());
            }
            Woke::Nothing => {}
        }
    }
    Ok(())
}

fn venue(host: &impl Host) -> Account {
    Account::Venue(VenueAccount {
        chain: host.home_chain(),
        driver: DRIVER.to_string(),
    })
}

/// Look at what is there, work out what should be there, and act on it if
/// that is worth doing. `brought_home` says cash has just come home for
/// leavers.
///
/// A read that fails skips the look, and is no fault: a fault ends `run`,
/// and the platform starts the agent again with its measure, and the stop
/// with it, afresh. The agent looks again at the first wake
/// `LOOKS_AGAIN_AFTER` on.
async fn step(host: &impl Host, mind: &mut Mind, brought_home: bool) -> Result<(), String> {
    if host.now() < mind.looks_again_at {
        return Ok(());
    }
    let state = match read_state(host).await {
        Ok(state) => state,
        Err(error) => {
            host.note(&format!("look skipped: a read failed: {error}"));
            mind.looks_again_at = host.now() + config::LOOKS_AGAIN_AFTER;
            return Ok(());
        }
    };
    let holdings = decide::share_holdings(&state, &mind.quotes, mind.funding_paid);
    // The driver reads the account and the vault whole or not at all, so
    // its value is what its holdings account for.
    mind.per_share
        .look(state.vault_value, Some(state.vault_value), holdings);
    if brought_home {
        mind.leavers.brought_home(&mind.limits, &state);
    } else {
        mind.leavers.seen(state.idle);
    }
    let plan = decide::decide(
        &mind.limits,
        &mind.per_share,
        mind.leavers.held(),
        &state,
        &mind.quotes,
    );
    mind.busy = plan != Plan::Nothing;
    if !mind.busy {
        return Ok(());
    }
    let at = Snapshot::of(&state);
    if mind.patience.holds_back(&at) {
        return Ok(());
    }
    if let Plan::Build { fund, .. } = &plan {
        mind.leavers.taken(fund.from_vault);
    }
    let limits = mind.limits;
    if carry_out(host, mind, &limits, &state, &plan).await? {
        mind.patience.went_through();
    } else {
        mind.patience.refused(at);
    }
    Ok(())
}

async fn read_state(host: &impl Host) -> Result<AccountState, String> {
    from_payload(&host.account_state(DRIVER, Vec::new()).await?)
}

/// Do what `plan` says, and say whether any of it went through.
///
/// The spot goes first and the short is sized to what it actually bought,
/// so an unfilled spot never leaves a short on its own, and the short is
/// levelled with the whole of the spot, an order's worth at a time, before
/// the step ends; on the way out the short goes first and the spot follows
/// what was bought back.
async fn carry_out(
    host: &impl Host,
    mind: &mut Mind,
    limits: &Limits,
    state: &AccountState,
    plan: &Plan,
) -> Result<bool, String> {
    let mut landed = false;
    match plan {
        Plan::Nothing => {}
        Plan::Build { fund, spot } => {
            // The money has to be at the venue before it can trade: the
            // station's idle dollars first, then the vault's cash; the
            // buffer stays in the vault, where a withdrawal reaches it
            // without a close. A move that stopped in the station, or whose
            // fate is unknown, is counted as outside the vault from then on,
            // so it is never sent twice.
            let home = host.home_chain();
            for (from, amount) in [
                (Account::Station(home), fund.from_station),
                (Account::Vault, fund.from_vault),
            ] {
                if amount > decide::zero() {
                    match host.move_(from, venue(host), millionths(amount)).await {
                        Moved::Done => landed = true,
                        Moved::Partial(_) => return Ok(true),
                        Moved::Refused(_) | Moved::Unknown(_) => return Ok(landed),
                    }
                }
            }
            let (mut spot_held, mut perp_held) = held(state);
            if let Some(order) = spot {
                // The spot leg is paid for from the account's spot dollars;
                // the perp leg's margin stays on the perp side.
                let cost = order.size * order.limit_price;
                let spot_cash = spot_dollars(state);
                if spot_cash < cost {
                    let verdicts = submit(
                        host,
                        &[Action::MoveDollars {
                            from: Balance::Perp,
                            amount: decide::round_up_cents(cost - spot_cash),
                        }],
                    )
                    .await?;
                    if !matches!(verdicts.first(), Some(sdk::Verdict::Accepted(_))) {
                        return Ok(landed);
                    }
                }
                match place(host, order).await? {
                    Some(size) => {
                        landed = true;
                        spot_held += size;
                    }
                    None => return Ok(landed),
                }
            }
            for _ in 0..HEDGE_ORDERS {
                let Some(order) = hedge(limits, state, &mind.quotes, spot_held, perp_held) else {
                    break;
                };
                if !mind.levered {
                    let verdicts = submit(
                        host,
                        &[Action::Leverage {
                            market: config::PERP.into(),
                            leverage: config::PERP_LEVERAGE,
                        }],
                    )
                    .await?;
                    mind.levered = matches!(verdicts.first(), Some(sdk::Verdict::Accepted(_)));
                }
                match place(host, &order).await? {
                    Some(size) if size != decide::zero() => {
                        landed = true;
                        perp_held += size;
                    }
                    _ => break,
                }
            }
        }
        Plan::Reduce { perp } => {
            let (spot_held, mut perp_held) = held(state);
            if let Some(order) = perp
                && let Some(size) = place(host, order).await?
            {
                landed = true;
                perp_held += size;
            }
            if let Some(order) = trim(limits, state, &mind.quotes, spot_held, perp_held) {
                landed |= place(host, &order).await?.is_some();
            }
        }
    }
    Ok(landed)
}

/// Close everything, perp first, because a halt is an unwind with less time.
/// The per-order cap is lifted: leaving a leg on would cost the depositors
/// more than a wider fill. A read that fails is looked again at after a
/// pause, within the same bound of looks, rather than ending the agent.
async fn flatten(
    host: &impl Host,
    mind: &mut Mind,
    waking: &mut (impl Stream<Item = Woke> + Unpin),
) -> Result<(), String> {
    let limits = Limits {
        max_order_fraction: Decimal::from(1),
        ..mind.limits
    };
    for _ in 0..FLATTEN_STEPS {
        let state = match read_state(host).await {
            Ok(state) => state,
            Err(error) => {
                host.note(&format!("halt's look skipped: a read failed: {error}"));
                if !pause(host, mind, waking, config::LOOKS_AGAIN_AFTER).await {
                    break;
                }
                continue;
            }
        };
        let plan = Plan::Reduce {
            perp: unwind(&limits, &state, &mind.quotes),
        };
        let (spot, perp) = held(&state);
        if perp == decide::zero() && trim(&limits, &state, &mind.quotes, spot, perp).is_none() {
            return Ok(());
        }
        carry_out(host, mind, &limits, &state, &plan).await?;
    }
    Ok(())
}

/// Send one order, and say how much of it filled, signed as the position
/// moved: nothing if it was refused, missed the book, or went unanswered.
async fn place(host: &impl Host, order: &Order) -> Result<Option<Decimal>, String> {
    let verdicts = submit(host, &[Action::Place(order.clone())]).await?;
    let Some(sdk::Verdict::Accepted(receipt)) = verdicts.first() else {
        return Ok(None);
    };
    let Ok(Receipt::Filled { size, .. }) = from_payload::<Receipt>(receipt) else {
        return Ok(None);
    };
    Ok(Some(match order.side {
        Side::Buy => size,
        Side::Sell => -size,
    }))
}

/// Send actions. A refusal is not a reason to stop: the next event is
/// another chance.
async fn submit(host: &impl Host, actions: &[Action]) -> Result<Vec<sdk::Verdict>, String> {
    let encoded: Vec<Vec<u8>> = actions.iter().map(to_payload).collect();
    Ok(host.submit(DRIVER, encoded).await)
}

/// Wait at least `seconds` on whatever wakes the agent, which has no timer
/// of its own, keeping each quote that arrives meanwhile; false once
/// nothing will wake it again.
async fn pause(
    host: &impl Host,
    mind: &mut Mind,
    waking: &mut (impl Stream<Item = Woke> + Unpin),
    seconds: u64,
) -> bool {
    let until = host.now() + seconds;
    while host.now() < until {
        match waking.next().await {
            Some(Woke::SpotBook(top)) => mind.quotes.spot = top,
            Some(Woke::PerpBook(top)) => mind.quotes.perp = top,
            Some(Woke::Funding(daily)) => mind.quotes.funding = Some(daily),
            Some(_) => {}
            None => return false,
        }
    }
    true
}

sdk::export_agent!(FundingBasis);

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::decimal;

    #[test]
    fn the_shipped_limits_can_open_a_position() {
        let minimum = decimal(config::MINIMUM_NOTIONAL);
        let smallest_vault = decimal("2000");
        let wanted = smallest_vault * decimal(config::TARGET_DEPLOYMENT);
        let step =
            decide::pair_notional(wanted, smallest_vault, decide::zero(), &limits(), minimum);
        assert!(
            step > decide::zero(),
            "a {smallest_vault} vault could not open a position"
        );
    }

    #[test]
    fn the_manifest_claims_the_limits_the_code_keeps() {
        let manifest = include_str!("../shonin.toml");
        assert!(
            manifest.contains("at most twice the vault's value")
                && config::MAX_GROSS_LEVERAGE == "2"
        );
        assert!(manifest.contains("above 5% of the vault") && config::MAX_ORDER_FRACTION == "0.05");
        assert!(manifest.contains("6% of the vault kept liquid") && config::IDLE_BUFFER == "0.06");
        assert!(
            manifest.contains("a 10% fall in a vault share's value")
                && config::DRAWDOWN_STOP == "0.10"
        );
        assert!(manifest.contains("a fall from before a restart is not remembered"));
        assert!(!manifest.contains("fall from the vault's peak"));
        assert!(manifest.contains(
            "the stop holds until then, a read that fails never restarts it, and a restart \
             measures a share afresh"
        ));
        assert!(manifest.contains("opens nothing until the platform restarts the agent"));
        assert!(manifest.contains("the stop is cleared when the platform restarts"));
        assert!(manifest.contains("A read that fails is a look skipped"));
        assert!(manifest.contains("save a halt's closing orders"));
        assert!(!manifest.contains("lifts") && !manifest.contains("Only reduces"));
        assert!(manifest.contains("[drivers.hyperliquid]"));
    }

    use hl::{Position, SpotBalance};
    use std::cell::{Cell, RefCell};

    /// A stand-in for the host: the account as the test sets it, every
    /// order filled in full, every move landing, and the next `failing`
    /// reads failing.
    struct Fake {
        clock: Cell<u64>,
        state: RefCell<AccountState>,
        failing: Cell<u32>,
        /// Actions sent and moves made.
        sent: Cell<u32>,
        /// Each action sent, and each move made (as `None`).
        log: RefCell<Vec<Option<Action>>>,
    }

    impl Fake {
        fn new(state: AccountState) -> Self {
            Self {
                clock: Cell::new(1_000_000),
                state: RefCell::new(state),
                failing: Cell::new(0),
                sent: Cell::new(0),
                log: RefCell::new(Vec::new()),
            }
        }
    }

    impl Host for Fake {
        fn home_chain(&self) -> String {
            "hyperevm".into()
        }

        fn now(&self) -> u64 {
            // Each look at the clock is a second on.
            self.clock.set(self.clock.get() + 1);
            self.clock.get()
        }

        fn note(&self, _: &str) {}

        async fn account_state(&self, _: &str, _: Vec<u8>) -> Result<Vec<u8>, String> {
            if self.failing.get() > 0 {
                self.failing.set(self.failing.get() - 1);
                return Err("the venue timed out".into());
            }
            Ok(to_payload(&*self.state.borrow()))
        }

        async fn submit(&self, _: &str, actions: Vec<Vec<u8>>) -> Vec<sdk::Verdict> {
            self.sent.set(self.sent.get() + 1);
            let action = from_payload::<Action>(&actions[0]).unwrap();
            self.log.borrow_mut().push(Some(action.clone()));
            let receipt = match action {
                Action::Place(order) => Receipt::Filled {
                    order_id: 1,
                    size: order.size,
                    average_price: order.limit_price,
                },
                _ => Receipt::Done,
            };
            vec![sdk::Verdict::Accepted(to_payload(&receipt))]
        }

        async fn move_(&self, _: Account, _: Account, _: u64) -> Moved {
            self.sent.set(self.sent.get() + 1);
            self.log.borrow_mut().push(None);
            Moved::Done
        }
    }

    /// A vault with $85,000 of cash and $15,000 on the account's perp side,
    /// holding `held` HYPE against as much shorted, HYPE's spot at `spot`.
    fn account(held: &str, spot: &str) -> AccountState {
        let held = decimal(held);
        AccountState {
            vault_value: decimal("100000") + held * decimal(spot) - held * decimal("25"),
            idle: decimal("85000"),
            station_idle: decide::zero(),
            account_value: decimal("15000"),
            withdrawable: decimal("15000"),
            positions: vec![Position {
                market: config::PERP.to_string(),
                size: -held,
                entry_price: None,
                leverage: config::PERP_LEVERAGE,
            }],
            spot: vec![
                SpotBalance {
                    token: "USDC".to_string(),
                    total: decide::zero(),
                    hold: decide::zero(),
                },
                SpotBalance {
                    token: config::SPOT_TOKEN.to_string(),
                    total: held,
                    hold: decide::zero(),
                },
            ],
            open_orders: Vec::new(),
        }
    }

    /// Both books about `spot` and $25, and funding well above entry.
    fn quotes(spot: &str) -> Quotes {
        let top = |mid: Decimal| Top {
            bid: Some(mid - decimal("0.01")),
            ask: Some(mid + decimal("0.01")),
        };
        Quotes {
            spot: top(decimal(spot)),
            perp: top(decimal("25")),
            funding: Some(decimal("0.001")),
        }
    }

    fn look(host: &Fake, mind: &mut Mind, spot: &str) {
        mind.quotes = quotes(spot);
        futures::executor::block_on(step(host, mind, false)).expect("a step is no fault");
    }

    #[test]
    fn a_read_that_fails_after_the_stop_leaves_the_stop_on_and_opens_nothing() {
        let host = Fake::new(account("1000", "25"));
        let mut mind = Mind::new();
        look(&host, &mut mind, "25");
        // HYPE's spot falls $13 under the perpetual: $13,000 of the vault
        // lost, stopped.
        *host.state.borrow_mut() = account("1000", "12");
        look(&host, &mut mind, "12");
        assert!(mind.per_share.stopped());
        // Closed out, and the spot back where it was.
        *host.state.borrow_mut() = account("0", "25");
        look(&host, &mut mind, "25");
        // A read fails: the step is no fault, so the agent is not started
        // again, and its stop holds.
        host.failing.set(1);
        assert_eq!(
            futures::executor::block_on(step(&host, &mut mind, false)),
            Ok(()),
            "a failed read ends the agent"
        );
        assert!(mind.per_share.stopped(), "the stop was cleared");
        let before = host.sent.get();
        for _ in 0..32 {
            look(&host, &mut mind, "25");
        }
        assert!(mind.per_share.stopped());
        assert_eq!(host.sent.get(), before, "opened again");
        // What a measure started afresh would have done there.
        let mut fresh = Mind::new();
        look(&host, &mut fresh, "25");
        assert!(host.sent.get() > before);
    }

    #[test]
    fn a_halt_whose_read_fails_looks_again_after_a_pause_and_still_closes() {
        let host = Fake::new(account("1000", "25"));
        host.failing.set(2);
        let mut mind = Mind::new();
        mind.quotes = quotes("25");
        let mut waking = futures::stream::iter(
            std::iter::repeat_with(|| Woke::Funding(decimal("0.001"))).take(1_000),
        );
        let before = host.clock.get();
        futures::executor::block_on(flatten(&host, &mut mind, &mut waking)).unwrap();
        assert!(host.sent.get() > 0, "closed nothing");
        assert!(host.clock.get() - before >= 2 * config::LOOKS_AGAIN_AFTER);
    }

    #[test]
    fn a_start_inside_a_halt_opens_nothing_and_goes_to_flatten() {
        // Holding a little, with funding above entry: an ordinary look
        // would add to both legs.
        let host = Fake::new(account("100", "25"));
        let mut mind = Mind::new();
        mind.quotes = quotes("25");
        // No funding heard yet: the first to come is looked at.
        mind.quotes.funding = None;
        // Started again within the halt: the notice waits first in the
        // vault's news, and funding is there from the start too.
        let halting = Woke::Vault(vault::VaultEvent::Halting("halted".into()));
        let vault = futures::stream::iter([halting]).chain(futures::stream::pending());
        let funding = futures::stream::iter(
            std::iter::repeat_with(|| Woke::Funding(decimal("0.001"))).take(1_000),
        );
        futures::executor::block_on(serve(&host, &mut mind, vault_first(vault, funding))).unwrap();
        let log = host.log.borrow();
        assert!(!log.is_empty(), "never went to the halt");
        assert!(
            // Only closes: the short bought back, the spot sold.
            log.iter().all(|sent| matches!(
                sent,
                Some(Action::Place(order))
                    if order.reduce_only || (order.market == config::SPOT && order.side == Side::Sell)
            )),
            "added within the halt"
        );
    }
}
