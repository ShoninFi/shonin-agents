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
//! leg as they arrive, and takes one step towards what it wants to hold each
//! time funding changes or its account does. It never polls.

mod config;

use futures::StreamExt;
use hl::{AccountState, Action, Balance, Order, Side, Tif};
use sdk::streams::{self, HYPERLIQUID, HyperliquidFeed, HyperliquidValue};
use sdk::transfers::{self, Account, Moved, VenueAccount};
use sdk::{
    Decimal, Limits, decimal, drivers, from_payload, millionths, size_for, to_payload, vault,
};
use strategy_sdk as sdk;
use strategy_sdk::hyperliquid as hl;

/// The driver this agent trades through, as its manifest names it.
const DRIVER: &str = hl::DRIVER;

/// How many steps a halt may take to flatten the book before leaving the
/// rest to the contract.
const FLATTEN_STEPS: usize = 32;

struct FundingBasis;

/// The best bid and ask of one leg, as last heard.
#[derive(Clone, Copy, Default)]
struct Top {
    bid: Option<Decimal>,
    ask: Option<Decimal>,
}

/// What the strategy knows. Rebuilt from the venue and the stream whenever
/// it starts, so losing it to a restart costs a moment and nothing else.
struct Mind {
    limits: Limits,
    spot: Top,
    perp: Top,
    /// Funding, as a fraction of notional per day.
    funding: Option<Decimal>,
    /// The most the vault has been worth while this instance watched it.
    peak: Decimal,
    /// Whether the perp's leverage has been set since this instance began.
    levered: bool,
}

fn zero() -> Decimal {
    Decimal::from(0)
}

fn limits() -> Limits {
    Limits {
        max_gross_leverage: decimal(config::MAX_GROSS_LEVERAGE),
        max_order_fraction: decimal(config::MAX_ORDER_FRACTION),
        idle_buffer: decimal(config::IDLE_BUFFER),
        drawdown_stop: decimal(config::DRAWDOWN_STOP),
    }
}

impl Mind {
    fn new() -> Self {
        Self {
            limits: limits(),
            spot: Top::default(),
            perp: Top::default(),
            funding: None,
            peak: zero(),
            levered: false,
        }
    }

    fn only_reducing(&self, value: Decimal) -> bool {
        self.peak > zero() && (self.peak - value) / self.peak >= self.limits.drawdown_stop
    }
}

/// What woke the strategy up.
enum Woke {
    Venue,
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
                coin: config::SPOT_BOOK.into(),
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
            .map(|_| Woke::Venue);
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
                Woke::SpotBook(top) => mind.spot = top,
                Woke::PerpBook(top) => mind.perp = top,
                Woke::Funding(daily) => {
                    mind.funding = Some(daily);
                    step(&mut mind).await?;
                }
                Woke::Venue | Woke::Vault(vault::VaultEvent::ClaimPending(_)) => {
                    step(&mut mind).await?
                }
                Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                    // As much as the venue will let go of; a claim's close
                    // covers whatever this doesn't.
                    let _ = transfers::move_(venue(), Account::Vault, short).await;
                    step(&mut mind).await?
                }
                Woke::Vault(vault::VaultEvent::Halting(_)) => {
                    flatten(&mind).await?;
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

/// The legs as the account holds them: spot tokens, and the perp position
/// (negative when short).
fn held(state: &AccountState) -> (Decimal, Decimal) {
    let spot = state
        .spot
        .iter()
        .find(|balance| balance.token == config::SPOT_TOKEN)
        .map_or_else(zero, |balance| balance.total);
    let perp = state
        .positions
        .iter()
        .find(|position| position.market == config::PERP)
        .map_or_else(zero, |position| position.size);
    (spot, perp)
}

fn spot_dollars(state: &AccountState) -> Decimal {
    state
        .spot
        .iter()
        .find(|balance| balance.token == "USDC")
        .map_or_else(zero, |balance| balance.total - balance.hold)
}

/// Look at what is there, work out what should be there, and take one step.
async fn step(mind: &mut Mind) -> Result<(), String> {
    let state = read_state().await?;
    if state.vault_value > mind.peak {
        mind.peak = state.vault_value;
    }
    let Some(funding) = mind.funding else {
        return Ok(());
    };
    let (spot, perp) = held(&state);
    let carrying = spot > zero() && perp < zero();
    let reducing = mind.only_reducing(state.vault_value);

    if carrying && (funding < decimal(config::EXIT_FUNDING) || reducing) {
        return unwind_one_step(mind, &state, mind.limits.max_order_fraction).await;
    }
    if funding >= decimal(config::ENTRY_FUNDING) && !reducing {
        return open_one_step(mind, &state).await;
    }
    Ok(())
}

async fn read_state() -> Result<AccountState, String> {
    from_payload(&drivers::account_state(DRIVER.to_string(), Vec::new()).await?)
}

/// `size` rounded down to `decimals` places.
fn round_down(size: Decimal, decimals: u32) -> Decimal {
    let scale = Decimal::from(10u64.pow(decimals));
    (size * scale).floor() / scale
}

/// Add one step's worth of carry, inside every limit the strategy set.
async fn open_one_step(mind: &mut Mind, state: &AccountState) -> Result<(), String> {
    let (Some(spot_ask), Some(perp_bid)) = (mind.spot.ask, mind.perp.bid) else {
        return Ok(());
    };
    if spot_ask <= zero() || perp_bid <= zero() {
        return Ok(());
    }
    let (spot, perp) = held(state);
    let gross = spot * spot_ask + (zero() - perp) * perp_bid;
    let working = state.vault_value * decimal(config::TARGET_DEPLOYMENT);
    let wanted = if working > gross {
        working - gross
    } else {
        zero()
    };
    let notional = size_for(
        wanted,
        state.vault_value,
        gross,
        &mind.limits,
        decimal(config::MINIMUM_NOTIONAL),
    );
    if notional <= zero() {
        return Ok(());
    }

    // The money has to be at the venue before it can trade; the buffer
    // stays in the vault, where a withdrawal reaches it without a close.
    let keep = state.vault_value * mind.limits.idle_buffer;
    let spare = if state.idle > keep {
        state.idle - keep
    } else {
        zero()
    };
    if spare > zero() {
        let moving = if notional < spare { notional } else { spare };
        if !matches!(
            transfers::move_(Account::Vault, venue(), millionths(moving)).await,
            Moved::Done
        ) {
            return Ok(());
        }
    }

    // The spot leg is paid for from the account's spot dollars; the perp
    // leg's margin stays on the perp side.
    let spot_cash = spot_dollars(state);
    if spot_cash < notional {
        let moving = round_down(notional - spot_cash, 2);
        let verdicts = submit(&[Action::MoveDollars {
            from: Balance::Perp,
            amount: moving,
        }])
        .await?;
        if !matches!(verdicts.first(), Some(sdk::Verdict::Accepted(_))) {
            return Ok(());
        }
    }
    if !mind.levered {
        let verdicts = submit(&[Action::Leverage {
            market: config::PERP.into(),
            leverage: config::PERP_LEVERAGE,
        }])
        .await?;
        mind.levered = matches!(verdicts.first(), Some(sdk::Verdict::Accepted(_)));
    }

    let slip = decimal(config::SLIPPAGE);
    let spot_size = round_down(notional / spot_ask, config::SPOT_SIZE_DECIMALS);
    let perp_size = round_down(notional / perp_bid, config::PERP_SIZE_DECIMALS);
    if spot_size <= zero() || perp_size <= zero() {
        return Ok(());
    }
    // The spot first: an unfilled spot beside a filled short is a short, so
    // the short is sized to what the spot actually bought, on the next step.
    submit(&[
        Action::Place(order(
            config::SPOT,
            Side::Buy,
            spot_size,
            spot_ask * (Decimal::from(1) + slip),
            false,
        )),
        Action::Place(order(
            config::PERP,
            Side::Sell,
            perp_size.min(round_down(spot + spot_size, config::PERP_SIZE_DECIMALS) + perp),
            perp_bid * (Decimal::from(1) - slip),
            false,
        )),
    ])
    .await
    .map(|_| ())
}

/// An immediate-or-cancel order, its limit written as the venue takes it.
fn order(market: &str, side: Side, size: Decimal, limit: Decimal, reduce_only: bool) -> Order {
    Order {
        market: market.to_string(),
        side,
        size,
        limit_price: five_figures(limit, side),
        tif: Tif::Ioc,
        reduce_only,
        client_id: None,
    }
}

/// A price at five significant figures, the most the venue takes, rounded
/// away from the market (down for a buy, up for a sell) so it never reaches
/// further than meant.
fn five_figures(price: Decimal, side: Side) -> Decimal {
    if price <= zero() {
        return price;
    }
    let mut scale = Decimal::from(1);
    let ten = Decimal::from(10);
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

/// Take one step out of the carry: the perp first, then the spot.
async fn unwind_one_step(
    mind: &Mind,
    state: &AccountState,
    order_fraction: Decimal,
) -> Result<(), String> {
    let (spot, perp) = held(state);
    let per_order = state.vault_value * order_fraction;
    let slip = decimal(config::SLIPPAGE);
    let mut actions = Vec::new();
    if let Some(ask) = mind.perp.ask.filter(|ask| *ask > zero() && perp < zero()) {
        let size = round_down(
            (zero() - perp).min(per_order / ask),
            config::PERP_SIZE_DECIMALS,
        );
        if size > zero() {
            actions.push(Action::Place(order(
                config::PERP,
                Side::Buy,
                size,
                ask * (Decimal::from(1) + slip),
                true,
            )));
        }
    }
    if let Some(bid) = mind.spot.bid.filter(|bid| *bid > zero() && spot > zero()) {
        let size = round_down(spot.min(per_order / bid), config::SPOT_SIZE_DECIMALS);
        if size > zero() {
            actions.push(Action::Place(order(
                config::SPOT,
                Side::Sell,
                size,
                bid * (Decimal::from(1) - slip),
                false,
            )));
        }
    }
    if actions.is_empty() {
        return Ok(());
    }
    submit(&actions).await.map(|_| ())
}

/// Close everything, perp first, because a halt is an unwind with less time.
/// The per-order cap is lifted: leaving a leg on would cost the depositors
/// more than a wider fill.
async fn flatten(mind: &Mind) -> Result<(), String> {
    for _ in 0..FLATTEN_STEPS {
        let state = read_state().await?;
        let (spot, perp) = held(&state);
        if round_down(spot, config::SPOT_SIZE_DECIMALS) <= zero() && perp == zero() {
            return Ok(());
        }
        unwind_one_step(mind, &state, Decimal::from(1)).await?;
    }
    Ok(())
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

    #[test]
    fn the_shipped_limits_can_open_a_position() {
        let minimum = decimal(config::MINIMUM_NOTIONAL);
        let smallest_vault = decimal("2000");
        let wanted = smallest_vault * decimal(config::TARGET_DEPLOYMENT);
        let step = size_for(wanted, smallest_vault, zero(), &limits(), minimum);
        assert!(
            step > zero(),
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
        assert!(manifest.contains("a 10% fall") && config::DRAWDOWN_STOP == "0.10");
        assert!(manifest.contains("[drivers.hyperliquid]"));
    }

    #[test]
    fn prices_keep_five_figures_and_never_reach_further_than_meant() {
        assert_eq!(
            five_figures(decimal("22.51234"), Side::Buy),
            decimal("22.512")
        );
        assert_eq!(
            five_figures(decimal("22.51234"), Side::Sell),
            decimal("22.513")
        );
        assert_eq!(
            five_figures(decimal("2710.55"), Side::Buy),
            decimal("2710.5")
        );
        assert_eq!(
            five_figures(decimal("123456"), Side::Sell),
            decimal("123460")
        );
    }

    #[test]
    fn sizes_round_down_to_the_venues_decimals() {
        assert_eq!(round_down(decimal("1.239"), 2), decimal("1.23"));
        assert_eq!(round_down(decimal("7"), 0), decimal("7"));
    }
}
