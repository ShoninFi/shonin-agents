//! Holds spot, shorts the perpetual against it, and collects the funding, at
//! the stand-in venue on Solana.
//!
//! It is the same program as the agent one directory up, published with a
//! vault on Solana: the stand-in driver is the same on every chain, so
//! nothing in the code changes. Its own copy keeps its limits in its own
//! source, as every agent's are.
//!
//! The position has no view on the price: whatever the asset does, the long
//! and the short move against each other and cancel. What is left is the
//! funding the shorts are paid while the perpetual trades above the spot, less
//! the spread it costs to get into both legs and out of them again.
//!
//! The whole strategy is one long-lived program. It waits on the venue's own
//! account stream and on the vault's, and each time it learns something it
//! works out what it would like to hold and takes one step towards it, if
//! that step is worth the gas it costs (`decide.rs` says when it is). It
//! never polls and it never sleeps on a timer, so a quiet market costs
//! nothing.
//!
//! Its limits are in `config.rs` rather than in a platform policy, because
//! there is no platform policy. The manifest claims them in the same words, so
//! a depositor reading the page and a reviewer reading the source are looking
//! at the same promises.

mod config;
mod decide;
mod host;
mod per_share;

use decide::{Leavers, Patience, Plan, Settings, Snapshot, hedge, limits, trim, unwind};
use futures::stream::PollNext;
use futures::{Stream, StreamExt};
use host::{Host, Sdk};
use per_share::PerShare;
use sdk::transfers::{Account, Moved, VenueAccount};
use sdk::{
    AccountState, Action, Decimal, Limits, Order, Query, Receipt, from_payload, millionths,
    to_payload,
};
use sdk::{drivers, vault};
use strategy_sdk as sdk;

/// The driver this agent trades through, as its manifest names it.
const DRIVER: &str = "local";

/// How many steps a halt may take to flatten the book before giving up and
/// letting the contract finish the job.
const FLATTEN_STEPS: usize = 32;

/// How many perpetual orders one step may send to level the short with the
/// spot. One is almost always enough; more are for a gap wider than the
/// order cap, or an order that only partly filled.
const HEDGE_ORDERS: usize = 8;

struct FundingBasis;

/// What the strategy decided last time it looked. It is rebuilt from the venue
/// whenever it runs, so losing it to a restart costs a moment, and the stop
/// (see `per_share.rs`).
struct Mind {
    settings: Settings,
    limits: Limits,
    /// The value of a vault share while this instance watched it, and its
    /// peak: what the drawdown stop is measured on.
    per_share: PerShare,
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
            settings: Settings::shipped(),
            limits: limits(),
            per_share: PerShare::new(config::DRAWDOWN_STOP),
            patience: Patience::default(),
            leavers: Leavers::default(),
            looks_again_at: 0,
        }
    }
}

/// What woke the strategy up.
enum Woke {
    /// The venue says something happened to the account or its markets.
    Venue(Vec<u8>),
    Vault(vault::VaultEvent),
}

impl sdk::Agent for FundingBasis {
    async fn run() -> Result<(), String> {
        let host = Sdk;
        let mut mind = Mind::new();
        let account = drivers::account_events(DRIVER)
            .into_stream()
            .map(Woke::Venue);
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        serve(&host, &mut mind, vault_first(vault_events, account)).await
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
/// goes straight to the halt. Whatever a previous instance left behind is
/// looked at on the first of the venue's events, which come every few
/// seconds: until the agent has read its account it hears every market.
async fn serve(
    host: &impl Host,
    mind: &mut Mind,
    mut waking: impl Stream<Item = Woke> + Unpin,
) -> Result<(), String> {
    while let Some(woke) = waking.next().await {
        match woke {
            // A fill, a funding payment, a price: look again, if it is
            // about this strategy's markets at all.
            Woke::Venue(event) => {
                if decide::wakes_for(&mind.settings, &event) {
                    step(host, mind, false).await?
                }
            }
            // A depositor is leaving, and the contract is closing their
            // fraction of both legs together. Nothing to do but let it,
            // then rebalance what is left.
            Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(host, mind, false).await?,
            // Leavers are waiting on more cash than the vault holds:
            // bring the shortfall home from the venue, so they are paid
            // without waiting for a claim.
            Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                // As much as the venue will let go of: a refusal leaves
                // the claim to the contract, which closes the fraction
                // itself.
                let _ = host.move_(venue(host), Account::Vault, short).await;
                step(host, mind, true).await?
            }
            // Unwind rather than be unwound.
            Woke::Vault(vault::VaultEvent::Halting(_)) => {
                flatten(host, mind, &mut waking).await?;
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The venue account the vault's station owns.
fn venue(host: &impl Host) -> Account {
    Account::Venue(VenueAccount {
        chain: host.home_chain(),
        driver: DRIVER.to_string(),
    })
}

/// Look at what is there, work out what should be there, and act on it if
/// that is worth a transaction. `brought_home` says cash has just come home
/// for leavers.
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
    // The driver reads the account and the vault whole or not at all, so
    // its value is what its holdings account for.
    mind.per_share.look(
        state.vault_value,
        Some(state.vault_value),
        decide::held(&state),
    );
    if brought_home {
        mind.leavers.brought_home(&mind.limits, &state);
    } else {
        mind.leavers.seen(state.idle);
    }
    let plan = decide::decide(
        &mind.settings,
        &mind.limits,
        &mind.per_share,
        mind.leavers.held(),
        &state,
    );
    if plan == Plan::Nothing {
        return Ok(());
    }
    let at = Snapshot::of(&mind.settings, &state);
    if mind.patience.holds_back(&at) {
        return Ok(());
    }
    if let Plan::Build { fund, .. } = &plan {
        mind.leavers.taken(fund.from_vault);
    }
    if carry_out(host, &mind.settings, &mind.limits, &state, &plan).await {
        mind.patience.went_through();
    } else {
        mind.patience.refused(at);
    }
    Ok(())
}

/// The account, asked about the two markets this strategy trades. Naming them
/// also tells the executor which markets' news should wake it, so a price or
/// funding posted for any other market costs nothing.
async fn read_state(host: &impl Host) -> Result<AccountState, String> {
    let query = Query {
        markets: vec![config::SPOT.to_string(), config::PERP.to_string()],
    };
    from_payload(&host.account_state(DRIVER, to_payload(&query)).await?)
}

/// Do what `plan` says, and say whether any of it went through.
///
/// Each leg is its own submission and the second is sized from what the
/// first actually did, so a leg that misses never leaves the other on its
/// own, and a refusal stops the plan where it is: the next event reads the
/// account again. The short is levelled with the whole of the spot, an
/// order's worth at a time, before the step ends.
async fn carry_out(
    host: &impl Host,
    settings: &Settings,
    limits: &Limits,
    state: &AccountState,
    plan: &Plan,
) -> bool {
    let mut landed = false;
    match plan {
        Plan::Nothing => {}
        Plan::Build { fund, spot } => {
            // From the station's idle dollars, or from the vault's cash
            // through its station, into the venue account the station owns.
            // The vault holds its idle floor however much is asked for. A
            // move it refuses, one that stopped on the way, or one whose fate
            // is unknown means trading nothing until the next event says
            // where the money is.
            let home = host.home_chain();
            for (from, amount) in [
                (Account::Station(home), fund.from_station),
                (Account::Vault, fund.from_vault),
            ] {
                if amount > Decimal::from(0) {
                    match host.move_(from, venue(host), millionths(amount)).await {
                        Moved::Done => landed = true,
                        // Some of it reached the station, which the next
                        // step counts and sends on.
                        Moved::Partial(_) => return true,
                        Moved::Refused(_) | Moved::Unknown(_) => return landed,
                    }
                }
            }
            let mut spot_held = state.position(&settings.spot);
            let mut perp_held = state.position(&settings.perp);
            if let Some(order) = spot {
                match place(host, order).await {
                    Some(size) => {
                        landed = true;
                        spot_held += size;
                    }
                    None => return landed,
                }
            }
            for _ in 0..HEDGE_ORDERS {
                let Some(order) = hedge(settings, limits, state, spot_held, perp_held) else {
                    break;
                };
                match place(host, &order).await {
                    Some(size) if size != Decimal::from(0) => {
                        landed = true;
                        perp_held += size;
                    }
                    _ => break,
                }
            }
        }
        Plan::Reduce { perp } => {
            let spot_held = state.position(&settings.spot);
            let mut perp_held = state.position(&settings.perp);
            if let Some(order) = perp
                && let Some(size) = place(host, order).await
            {
                landed = true;
                perp_held += size;
            }
            if let Some(order) = trim(settings, limits, state, spot_held, perp_held) {
                landed |= place(host, &order).await.is_some();
            }
        }
    }
    landed
}

/// Close everything, the short first, because a halt is an unwind with less
/// time. The order cap is lifted: the vault is stopping, and leaving a leg on
/// would cost its depositors more than a wider fill will. A read that fails
/// is looked again at after a pause, within the same bound of looks, rather
/// than ending the agent.
async fn flatten(
    host: &impl Host,
    mind: &Mind,
    waking: &mut (impl Stream<Item = Woke> + Unpin),
) -> Result<(), String> {
    let limits = Limits {
        max_order_fraction: Decimal::from(1),
        ..mind.limits
    };
    let zero = Decimal::from(0);
    for _ in 0..FLATTEN_STEPS {
        let state = match read_state(host).await {
            Ok(state) => state,
            Err(error) => {
                host.note(&format!("halt's look skipped: a read failed: {error}"));
                if !pause(host, waking, config::LOOKS_AGAIN_AFTER).await {
                    break;
                }
                continue;
            }
        };
        if state.position(&mind.settings.spot) == zero
            && state.position(&mind.settings.perp) == zero
        {
            return Ok(());
        }
        let plan = Plan::Reduce {
            perp: unwind(&mind.settings, &limits, &state),
        };
        carry_out(host, &mind.settings, &limits, &state, &plan).await;
    }
    // The market would not take it inside the limits. The contract closes the
    // rest through the same reduce-only path, so stopping here is safe.
    Ok(())
}

/// Send one order, and say how much of it filled, signed as the position
/// moved: nothing if it was refused, missed the market, or went unanswered.
/// A refusal is not a reason to stop: the next event is another chance, and
/// a strategy that gives up on one refusal is a strategy that stops trading
/// the first time a market moves.
async fn place(host: &impl Host, order: &Order) -> Option<Decimal> {
    let verdicts = host
        .submit(DRIVER, vec![to_payload(&Action::Place(order.clone()))])
        .await;
    let Some(sdk::Verdict::Accepted(receipt)) = verdicts.first() else {
        return None;
    };
    let Ok(Receipt::Filled { size, side, .. }) = from_payload::<Receipt>(receipt) else {
        return None;
    };
    Some(match side {
        sdk::Side::Buy => size,
        sdk::Side::Sell => -size,
    })
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

sdk::export_agent!(FundingBasis);

#[cfg(test)]
mod tests {
    use super::{config, limits};
    use crate::decide::pair_notional;
    use futures::StreamExt;
    use strategy_sdk::decimal;

    /// An agent whose own per-order cap is below its own minimum order can
    /// never open a position, and would sit there looking healthy while doing
    /// nothing. The two numbers have to leave room for each other at the size
    /// a vault this agent is meant for actually holds.
    #[test]
    fn the_shipped_limits_can_open_a_position() {
        let minimum = decimal(config::MINIMUM_NOTIONAL);
        let smallest_vault = decimal("2000");
        let wanted = smallest_vault * decimal(config::TARGET_DEPLOYMENT);
        let step = pair_notional(wanted, smallest_vault, decimal("0"), &limits(), minimum);
        assert!(
            step > decimal("0"),
            "a {smallest_vault} vault could not open a position: a step of \
             {step} against a minimum of {minimum}"
        );
    }

    /// The claims in the manifest are the limits in the code, in words.
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
        assert!(manifest.contains(config::SPOT) && manifest.contains(config::PERP));
    }

    /// A stand-in for the host: the venue's account as the test sets it,
    /// every order filled in full, every move landing, and the next
    /// `failing` reads failing.
    struct Fake {
        clock: std::cell::Cell<u64>,
        state: std::cell::RefCell<super::AccountState>,
        failing: std::cell::Cell<u32>,
        /// Orders placed and moves made.
        sent: std::cell::Cell<u32>,
        /// Each order placed, and each move made (as `None`).
        log: std::cell::RefCell<Vec<Option<strategy_sdk::Order>>>,
    }

    impl super::Host for Fake {
        fn home_chain(&self) -> String {
            "base".into()
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
            Ok(strategy_sdk::to_payload(&*self.state.borrow()))
        }

        async fn submit(&self, _: &str, actions: Vec<Vec<u8>>) -> Vec<strategy_sdk::Verdict> {
            self.sent.set(self.sent.get() + 1);
            let super::Action::Place(order) = strategy_sdk::from_payload(&actions[0]).unwrap();
            self.log.borrow_mut().push(Some(order.clone()));
            let filled = super::Receipt::Filled {
                market: order.market,
                side: order.side,
                size: order.size,
                price: order.limit_price,
                notional: order.size * order.limit_price,
            };
            vec![strategy_sdk::Verdict::Accepted(strategy_sdk::to_payload(
                &filled,
            ))]
        }

        async fn move_(&self, _: super::Account, _: super::Account, _: u64) -> super::Moved {
            self.sent.set(self.sent.get() + 1);
            self.log.borrow_mut().push(None);
            super::Moved::Done
        }
    }

    /// A $100,000 vault with $85,000 of cash and $15,000 at the venue,
    /// holding `spot` BTC long and as much BTC-PERP short, BTC's spot marked
    /// at `spot_mark` and its perpetual at $50,000, paid 0.1% a day to short.
    fn account(idle: &str, cash: &str, held: &str, spot_mark: &str) -> super::AccountState {
        use strategy_sdk::{MarketState, PositionState};
        let perp_mark = decimal("50000");
        let market = |name: &str, is_perp: bool, mark| MarketState {
            market: name.to_string(),
            is_perp,
            mark,
            oracle: mark,
            bid: mark - decimal("1"),
            ask: mark + decimal("1"),
            funding_index: decimal("0"),
            funding_rate: if is_perp {
                mark * decimal("0.001")
            } else {
                decimal("0")
            },
        };
        let held = decimal(held);
        let spot_mark = decimal(spot_mark);
        let value = decimal(cash) + held * spot_mark - held * perp_mark;
        super::AccountState {
            cash: decimal(cash),
            value,
            exit_value: value,
            vault_value: decimal(idle) + value,
            idle: decimal(idle),
            station_idle: decimal("0"),
            margin_bps: 1000,
            positions: vec![
                PositionState {
                    market: config::SPOT.to_string(),
                    size: held,
                },
                PositionState {
                    market: config::PERP.to_string(),
                    size: -held,
                },
            ],
            markets: vec![
                market(config::SPOT, false, spot_mark),
                market(config::PERP, true, perp_mark),
            ],
        }
    }

    fn look(host: &Fake, mind: &mut super::Mind) {
        futures::executor::block_on(super::step(host, mind, false)).expect("a step is no fault");
    }

    #[test]
    fn a_read_that_fails_after_the_stop_leaves_the_stop_on_and_opens_nothing() {
        let host = Fake {
            clock: std::cell::Cell::new(1_000_000),
            state: std::cell::RefCell::new(account("85000", "15000", "0.8", "50000")),
            failing: std::cell::Cell::new(0),
            sent: std::cell::Cell::new(0),
            log: std::cell::RefCell::new(Vec::new()),
        };
        let mut mind = super::Mind::new();
        look(&host, &mut mind);
        // The spot falls $14,000 under the perpetual: $11,200 of the vault
        // lost, stopped.
        *host.state.borrow_mut() = account("85000", "15000", "0.8", "36000");
        look(&host, &mut mind);
        assert!(mind.per_share.stopped());
        // Closed out, the spot back where it was, funding well above entry.
        *host.state.borrow_mut() = account("88800", "0", "0", "50000");
        look(&host, &mut mind);
        // A read fails: the step is no fault, so the agent is not started
        // again, and its stop holds.
        host.failing.set(1);
        assert_eq!(
            futures::executor::block_on(super::step(&host, &mut mind, false)),
            Ok(()),
            "a failed read ends the agent"
        );
        assert!(mind.per_share.stopped(), "the stop was cleared");
        let before = host.sent.get();
        for _ in 0..32 {
            look(&host, &mut mind);
        }
        assert!(mind.per_share.stopped());
        assert_eq!(host.sent.get(), before, "opened again");
        // What a measure started afresh would have done there.
        let mut fresh = super::Mind::new();
        look(&host, &mut fresh);
        assert!(host.sent.get() > before);
    }

    #[test]
    fn after_a_read_that_fails_the_next_look_waits_its_pause() {
        let host = Fake {
            clock: std::cell::Cell::new(1_000_000),
            state: std::cell::RefCell::new(account("88800", "0", "0", "50000")),
            failing: std::cell::Cell::new(1),
            sent: std::cell::Cell::new(0),
            log: std::cell::RefCell::new(Vec::new()),
        };
        let mut mind = super::Mind::new();
        look(&host, &mut mind);
        // At once after: not read again.
        look(&host, &mut mind);
        assert_eq!(host.sent.get(), 0);
        host.clock.set(host.clock.get() + config::LOOKS_AGAIN_AFTER);
        look(&host, &mut mind);
        assert!(host.sent.get() > 0, "never looked again");
    }

    #[test]
    fn a_halt_whose_read_fails_looks_again_after_a_pause_and_still_closes() {
        let host = Fake {
            clock: std::cell::Cell::new(1_000_000),
            state: std::cell::RefCell::new(account("85000", "15000", "0.8", "50000")),
            failing: std::cell::Cell::new(2),
            sent: std::cell::Cell::new(0),
            log: std::cell::RefCell::new(Vec::new()),
        };
        let mind = super::Mind::new();
        let mut waking = futures::stream::iter(
            std::iter::repeat_with(|| super::Woke::Venue(Vec::new())).take(1_000),
        );
        let before = host.clock.get();
        futures::executor::block_on(super::flatten(&host, &mind, &mut waking)).unwrap();
        assert!(host.sent.get() > 0, "closed nothing");
        assert!(host.clock.get() - before >= 2 * config::LOOKS_AGAIN_AFTER);
    }

    #[test]
    fn a_start_inside_a_halt_opens_nothing_and_goes_to_flatten() {
        // Short of its target with funding above entry: an ordinary look
        // would add to both legs.
        let host = Fake {
            clock: std::cell::Cell::new(1_000_000),
            state: std::cell::RefCell::new(account("85000", "15000", "0.2", "50000")),
            failing: std::cell::Cell::new(0),
            sent: std::cell::Cell::new(0),
            log: std::cell::RefCell::new(Vec::new()),
        };
        let mut mind = super::Mind::new();
        // Started again within the halt: the notice waits first in the
        // vault's news, and the venue's events are there from the start too.
        let halting = super::Woke::Vault(super::vault::VaultEvent::Halting("halted".into()));
        let vault = futures::stream::iter([halting]).chain(futures::stream::pending());
        let venue = futures::stream::iter(
            std::iter::repeat_with(|| super::Woke::Venue(Vec::new())).take(1_000),
        );
        futures::executor::block_on(super::serve(
            &host,
            &mut mind,
            super::vault_first(vault, venue),
        ))
        .unwrap();
        let log = host.log.borrow();
        assert!(!log.is_empty(), "never went to the halt");
        assert!(
            log.iter()
                .all(|sent| matches!(sent, Some(order) if order.reduce_only)),
            "added within the halt"
        );
    }
}
