//! Holds spot, shorts the perpetual against it, and collects the funding.
//!
//! The position has no view on the price: whatever the asset does, the long
//! and the short move against each other and cancel. What is left is the
//! funding the shorts are paid while the perpetual trades above the spot, less
//! the spread it costs to get into both legs and out of them again.
//!
//! The whole strategy is one long-lived program. It waits on the venue's own
//! account stream and on the vault's, and each time it learns something it
//! works out what it would like to hold and takes one step towards it. It
//! never polls and it never sleeps on a timer, so a quiet market costs
//! nothing.
//!
//! Its limits are in this file rather than in a platform policy, because there
//! is no platform policy. The manifest lists them in the same words, so a
//! depositor reading the page and a reviewer reading the source are looking at
//! the same promises.

use agent_shared::{Config, Limits, size_for};
use futures::StreamExt;
use sdk::{
    Account, AccountState, Action, Decimal, Order, Side, Verdict, from_payload, to_payload, vault,
    venues,
};
use strategy_sdk as sdk;

/// The venue this strategy trades, as its manifest names it.
const VENUE: &str = "local";

/// How many steps a halt may take to flatten the book before giving up and
/// letting the contract finish the job.
const FLATTEN_STEPS: usize = 32;

struct FundingBasis;

/// What the strategy decided last time it looked. It is rebuilt from the venue
/// whenever it runs, so losing it to a restart costs a moment and nothing else.
struct Mind {
    settings: Settings,
    limits: Limits,
    /// The most the vault has been worth while this instance watched it: what
    /// the drawdown stop is measured from.
    peak: Decimal,
}

struct Settings {
    spot: String,
    perp: String,
    /// Funding, as a fraction of notional per day, worth carrying.
    entry_funding: Decimal,
    /// And below which an open carry is unwound.
    exit_funding: Decimal,
    /// How much of the vault to aim to have working.
    target_deployment: Decimal,
    /// Below this, an order is not worth the spread it would pay.
    minimum_notional: Decimal,
}

fn zero() -> Decimal {
    Decimal::from(0)
}

impl Mind {
    fn new(config: &Config) -> Self {
        Self {
            settings: Settings {
                spot: config.text("market_spot", "BTC"),
                perp: config.text("market_perp", "BTC-PERP"),
                entry_funding: config.decimal("entry_funding", "0.0002"),
                exit_funding: config.decimal("exit_funding", "0.00005"),
                target_deployment: config.decimal("target_deployment", "0.80"),
                minimum_notional: config.decimal("minimum_notional", "250"),
            },
            limits: Limits {
                max_gross_leverage: config.decimal("max_gross_leverage", "2"),
                max_order_fraction: config.decimal("max_order_fraction", "0.05"),
                idle_buffer: config.decimal("idle_buffer", "0.05"),
                drawdown_stop: config.decimal("drawdown_stop", "0.10"),
            },
            peak: zero(),
        }
    }

    /// Whether the vault has fallen far enough from its peak that this
    /// strategy should stop opening anything and only get smaller.
    fn only_reducing(&self, value: Decimal) -> bool {
        self.peak > zero() && (self.peak - value) / self.peak >= self.limits.drawdown_stop
    }
}

/// What woke the strategy up.
enum Woke {
    /// The venue says something happened to the account.
    Venue,
    Vault(vault::VaultEvent),
}

impl sdk::Strategy for FundingBasis {
    async fn run(config: Vec<u8>) -> Result<(), String> {
        let config = Config::from_bytes(&config)?;
        let mut mind = Mind::new(&config);

        // Whatever a previous instance left behind is still on the venue, so
        // look before doing anything.
        step(&mut mind).await?;

        let account = venues::account_events(VENUE)
            .into_stream()
            .map(|_| Woke::Venue);
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        let mut waking = futures::stream::select(account, vault_events);

        while let Some(woke) = waking.next().await {
            match woke {
                // A fill, a funding payment, a price: look again.
                Woke::Venue => step(&mut mind).await?,
                // A depositor is leaving, and the contract is closing their
                // fraction of both legs together. Nothing to do but let it,
                // then rebalance what is left.
                Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(&mut mind).await?,
                // Unwind rather than be unwound.
                Woke::Vault(vault::VaultEvent::Halting(_)) => {
                    flatten(&mind).await?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// Look at what is there, work out what should be there, and take one step.
async fn step(mind: &mut Mind) -> Result<(), String> {
    let state = read_state().await?;
    if state.vault_value > mind.peak {
        mind.peak = state.vault_value;
    }

    let (Some(spot), Some(perp)) = (
        state.market(&mind.settings.spot),
        state.market(&mind.settings.perp),
    ) else {
        // The venue has not listed what the manifest named. Saying nothing is
        // right: the next event is another chance, and trading something else
        // would not be this strategy.
        return Ok(());
    };

    let funding = perp.funding_fraction();
    let carrying = state.position(&mind.settings.spot) > zero()
        && state.position(&mind.settings.perp) < zero();
    let reducing = mind.only_reducing(state.vault_value);

    if carrying && (funding < mind.settings.exit_funding || reducing) {
        return unwind_one_step(&mind.settings, &mind.limits, &state).await;
    }
    if funding >= mind.settings.entry_funding && !reducing {
        return open_one_step(mind, &state, spot.ask, perp.bid).await;
    }
    Ok(())
}

async fn read_state() -> Result<AccountState, String> {
    from_payload(&venues::account_state(VENUE.to_string(), Vec::new()).await?)
}

/// Add one step's worth of carry, inside every limit the strategy set itself.
async fn open_one_step(
    mind: &Mind,
    state: &AccountState,
    spot_ask: Decimal,
    perp_bid: Decimal,
) -> Result<(), String> {
    let working = state.vault_value * mind.settings.target_deployment;
    let gross = gross_notional(state);
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
        mind.settings.minimum_notional,
    );
    if notional <= zero() || spot_ask <= zero() || perp_bid <= zero() {
        return Ok(());
    }

    // The venue holds the money, so it has to be there before it can be
    // traded; the buffer stays at home, where a withdrawal can reach it
    // without closing anything.
    let keep = state.vault_value * mind.limits.idle_buffer;
    let spare = if state.idle > keep {
        state.idle - keep
    } else {
        zero()
    };
    if spare > zero() {
        let moving = if notional < spare { notional } else { spare };
        submit(&[Action::MoveCapital {
            from: Account::Idle,
            to: Account::Venue,
            amount: moving,
        }])
        .await?;
    }

    // Both legs in one submission. A filled long beside an unfilled short is
    // a directional bet, which is the one thing this strategy never means to
    // hold, so the venue answers for both together.
    submit(&[
        Action::Place(Order {
            market: mind.settings.spot.clone(),
            side: Side::Buy,
            size: notional / spot_ask,
            limit_price: spot_ask,
            reduce_only: false,
        }),
        Action::Place(Order {
            market: mind.settings.perp.clone(),
            side: Side::Sell,
            size: notional / perp_bid,
            limit_price: perp_bid,
            reduce_only: false,
        }),
    ])
    .await
    .map(|_| ())
}

/// Take one step out of the carry: the perp first, then the spot.
async fn unwind_one_step(
    settings: &Settings,
    limits: &Limits,
    state: &AccountState,
) -> Result<(), String> {
    let (Some(spot), Some(perp)) = (state.market(&settings.spot), state.market(&settings.perp))
    else {
        return Ok(());
    };
    let per_order = state.vault_value * limits.max_order_fraction;

    // Closing the spot first would leave a naked short until the next step,
    // which is exactly the exposure this strategy exists to avoid.
    let perp_size = cap_by_notional(-state.position(&settings.perp), perp.ask, per_order);
    let spot_size = cap_by_notional(state.position(&settings.spot), spot.bid, per_order);

    let mut actions = Vec::new();
    if perp_size > zero() {
        actions.push(Action::Place(Order {
            market: settings.perp.clone(),
            side: Side::Buy,
            size: perp_size,
            limit_price: perp.ask,
            reduce_only: true,
        }));
    }
    if spot_size > zero() {
        actions.push(Action::Place(Order {
            market: settings.spot.clone(),
            side: Side::Sell,
            size: spot_size,
            limit_price: spot.bid,
            reduce_only: true,
        }));
    }
    if actions.is_empty() {
        return Ok(());
    }
    submit(&actions).await.map(|_| ())
}

/// Close everything, in the same order, because a halt is an unwind with less
/// time. The order cap is lifted: the vault is stopping, and leaving a leg on
/// would cost its depositors more than a wider fill will.
async fn flatten(mind: &Mind) -> Result<(), String> {
    let limits = Limits {
        max_order_fraction: Decimal::from(1),
        ..mind.limits
    };
    for _ in 0..FLATTEN_STEPS {
        let state = read_state().await?;
        if state.position(&mind.settings.spot) == zero()
            && state.position(&mind.settings.perp) == zero()
        {
            return Ok(());
        }
        unwind_one_step(&mind.settings, &limits, &state).await?;
    }
    // The market would not take it inside the limits. The contract closes the
    // rest through the same reduce-only path, so stopping here is safe.
    Ok(())
}

/// What every open position is worth at mark, added up without regard to sign.
fn gross_notional(state: &AccountState) -> Decimal {
    state
        .positions
        .iter()
        .map(|held| {
            let price = state
                .market(&held.market)
                .map(|market| market.mark)
                .unwrap_or_else(zero);
            let size = if held.size < zero() {
                -held.size
            } else {
                held.size
            };
            size * price
        })
        .fold(zero(), |total, one| total + one)
}

/// How much of a position one order may close, so no single order is worth
/// more than the strategy allows.
fn cap_by_notional(size: Decimal, price: Decimal, max_notional: Decimal) -> Decimal {
    if size <= zero() || price <= zero() {
        return zero();
    }
    let allowed = max_notional / price;
    if size > allowed { allowed } else { size }
}

/// Send actions. A rejection is not a reason to stop: the next event is
/// another chance, and a strategy that gives up on one refusal is a strategy
/// that stops trading the first time a market moves.
async fn submit(actions: &[Action]) -> Result<Vec<Verdict>, String> {
    let encoded: Vec<Vec<u8>> = actions.iter().map(to_payload).collect();
    Ok(venues::submit(VENUE.to_string(), encoded).await)
}

sdk::export_strategy!(FundingBasis);

#[cfg(test)]
mod tests {
    use agent_shared::{Limits, size_for};
    use strategy_sdk::{Decimal, decimal};

    /// The settings this strategy actually ships with, read from its own
    /// manifest so a test cannot drift from what runs.
    fn shipped(key: &str) -> Decimal {
        let manifest = include_str!("../shonin.toml");
        let line = manifest
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{key} =")))
            .unwrap_or_else(|| panic!("the manifest declares {key}"));
        let value = line.split('=').nth(1).expect("a value").trim();
        value.trim_matches('"').parse().expect("a decimal")
    }

    /// A strategy whose own per-order cap is below its own minimum order can
    /// never open a position, and would sit there looking healthy while doing
    /// nothing. The two numbers have to leave room for each other at the size
    /// a vault this strategy is meant for actually holds.
    #[test]
    fn the_shipped_limits_can_open_a_position() {
        let limits = Limits {
            max_gross_leverage: shipped("max_gross_leverage"),
            max_order_fraction: shipped("max_order_fraction"),
            idle_buffer: shipped("idle_buffer"),
            drawdown_stop: shipped("drawdown_stop"),
        };
        let minimum = shipped("minimum_notional");
        let smallest_vault = decimal("2000");

        let wanted = smallest_vault * shipped("target_deployment");
        let step = size_for(wanted, smallest_vault, decimal("0"), &limits, minimum);
        assert!(
            step > decimal("0"),
            "a {smallest_vault} vault could not open a position: a step of \
             {step} against a minimum of {minimum}"
        );
    }
}
