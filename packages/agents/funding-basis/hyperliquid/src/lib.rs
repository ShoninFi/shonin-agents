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
mod per_share;

use decide::{
    Leavers, Patience, Plan, Quotes, Snapshot, Top, hedge, held, limits, spot_dollars, trim, unwind,
};
use futures::StreamExt;
use hl::{AccountState, Action, Balance, Order, Receipt, Side};
use per_share::PerShare;
use sdk::streams::{self, HYPERLIQUID, HyperliquidFeed, HyperliquidValue};
use sdk::transfers::{self, Account, Moved, VenueAccount};
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
/// it starts, so losing it to a restart costs a moment and nothing else.
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
}

impl Mind {
    fn new() -> Self {
        Self {
            limits: limits(),
            quotes: Quotes::default(),
            per_share: PerShare::new(config::DRAWDOWN_STOP, config::STOP_LIFTS_WITHIN),
            funding_paid: decide::zero(),
            levered: false,
            busy: false,
            patience: Patience::default(),
            leavers: Leavers::default(),
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

        let mut waking = futures::stream::select_all([
            spot_book.boxed_local(),
            perp_book.boxed_local(),
            funding.boxed_local(),
            account.boxed_local(),
            vault_events.boxed_local(),
        ]);

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
                        step(&mut mind, false).await?;
                    }
                }
                Woke::Venue(event) => {
                    mind.funding_paid += decide::funding_paid(&event);
                    if decide::wakes_for(&event) {
                        step(&mut mind, false).await?
                    }
                }
                Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(&mut mind, false).await?,
                Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                    // As much as the venue will let go of; a claim's close
                    // covers whatever this doesn't.
                    let _ = transfers::move_(venue(), Account::Vault, short).await;
                    step(&mut mind, true).await?
                }
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

fn venue() -> Account {
    Account::Venue(VenueAccount {
        chain: transfers::home_chain(),
        driver: DRIVER.to_string(),
    })
}

/// Look at what is there, work out what should be there, and act on it if
/// that is worth doing. `brought_home` says cash has just come home for
/// leavers.
async fn step(mind: &mut Mind, brought_home: bool) -> Result<(), String> {
    let state = read_state().await?;
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
    if carry_out(mind, &limits, &state, &plan).await? {
        mind.patience.went_through();
    } else {
        mind.patience.refused(at);
    }
    Ok(())
}

async fn read_state() -> Result<AccountState, String> {
    from_payload(&drivers::account_state(DRIVER.to_string(), Vec::new()).await?)
}

/// Do what `plan` says, and say whether any of it went through.
///
/// The spot goes first and the short is sized to what it actually bought,
/// so an unfilled spot never leaves a short on its own, and the short is
/// levelled with the whole of the spot, an order's worth at a time, before
/// the step ends; on the way out the short goes first and the spot follows
/// what was bought back.
async fn carry_out(
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
            let home = transfers::home_chain();
            for (from, amount) in [
                (Account::Station(home), fund.from_station),
                (Account::Vault, fund.from_vault),
            ] {
                if amount > decide::zero() {
                    match transfers::move_(from, venue(), millionths(amount)).await {
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
                    let verdicts = submit(&[Action::MoveDollars {
                        from: Balance::Perp,
                        amount: decide::round_up_cents(cost - spot_cash),
                    }])
                    .await?;
                    if !matches!(verdicts.first(), Some(sdk::Verdict::Accepted(_))) {
                        return Ok(landed);
                    }
                }
                match place(order).await? {
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
                    let verdicts = submit(&[Action::Leverage {
                        market: config::PERP.into(),
                        leverage: config::PERP_LEVERAGE,
                    }])
                    .await?;
                    mind.levered = matches!(verdicts.first(), Some(sdk::Verdict::Accepted(_)));
                }
                match place(&order).await? {
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
                && let Some(size) = place(order).await?
            {
                landed = true;
                perp_held += size;
            }
            if let Some(order) = trim(limits, state, &mind.quotes, spot_held, perp_held) {
                landed |= place(&order).await?.is_some();
            }
        }
    }
    Ok(landed)
}

/// Close everything, perp first, because a halt is an unwind with less time.
/// The per-order cap is lifted: leaving a leg on would cost the depositors
/// more than a wider fill.
async fn flatten(mind: &mut Mind) -> Result<(), String> {
    let limits = Limits {
        max_order_fraction: Decimal::from(1),
        ..mind.limits
    };
    for _ in 0..FLATTEN_STEPS {
        let state = read_state().await?;
        let plan = Plan::Reduce {
            perp: unwind(&limits, &state, &mind.quotes),
        };
        let (spot, perp) = held(&state);
        if perp == decide::zero() && trim(&limits, &state, &mind.quotes, spot, perp).is_none() {
            return Ok(());
        }
        carry_out(mind, &limits, &state, &plan).await?;
    }
    Ok(())
}

/// Send one order, and say how much of it filled, signed as the position
/// moved: nothing if it was refused, missed the book, or went unanswered.
async fn place(order: &Order) -> Result<Option<Decimal>, String> {
    let verdicts = submit(&[Action::Place(order.clone())]).await?;
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
async fn submit(actions: &[Action]) -> Result<Vec<sdk::Verdict>, String> {
    let encoded: Vec<Vec<u8>> = actions.iter().map(to_payload).collect();
    Ok(drivers::submit(DRIVER.to_string(), encoded).await)
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
        assert!(
            manifest.contains(
                "as if the vault had kept what it held when the stop came, is back within 5% of its peak"
            ) && config::STOP_LIFTS_WITHIN == "0.05"
        );
        assert!(manifest.contains("[drivers.hyperliquid]"));
    }
}
