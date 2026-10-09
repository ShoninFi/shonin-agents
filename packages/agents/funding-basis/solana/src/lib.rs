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

use decide::{Leavers, Patience, Plan, Settings, Snapshot, hedge, limits, trim, unwind};
use futures::StreamExt;
use sdk::transfers::{self, Account, Moved, VenueAccount};
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
/// whenever it runs, so losing it to a restart costs a moment and nothing else.
struct Mind {
    settings: Settings,
    limits: Limits,
    /// The most the vault has been worth while this instance watched it: what
    /// the drawdown stop is measured from.
    peak: Decimal,
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
            settings: Settings::shipped(),
            limits: limits(),
            peak: Decimal::from(0),
            patience: Patience::default(),
            leavers: Leavers::default(),
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
        let mut mind = Mind::new();

        // Whatever a previous instance left behind is still on the venue, so
        // look before doing anything.
        step(&mut mind, false).await?;

        let account = drivers::account_events(DRIVER)
            .into_stream()
            .map(Woke::Venue);
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        let mut waking = futures::stream::select(account, vault_events);

        while let Some(woke) = waking.next().await {
            match woke {
                // A fill, a funding payment, a price: look again, if it is
                // about this strategy's markets at all.
                Woke::Venue(event) => {
                    if decide::wakes_for(&mind.settings, &event) {
                        step(&mut mind, false).await?
                    }
                }
                // A depositor is leaving, and the contract is closing their
                // fraction of both legs together. Nothing to do but let it,
                // then rebalance what is left.
                Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(&mut mind, false).await?,
                // Leavers are waiting on more cash than the vault holds:
                // bring the shortfall home from the venue, so they are paid
                // without waiting for a claim.
                Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                    // As much as the venue will let go of: a refusal leaves
                    // the claim to the contract, which closes the fraction
                    // itself.
                    let _ = transfers::move_(venue(), Account::Vault, short).await;
                    step(&mut mind, true).await?
                }
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

/// The venue account the vault's station owns.
fn venue() -> Account {
    Account::Venue(VenueAccount {
        chain: transfers::home_chain(),
        driver: DRIVER.to_string(),
    })
}

/// Look at what is there, work out what should be there, and act on it if
/// that is worth a transaction. `brought_home` says cash has just come home
/// for leavers.
async fn step(mind: &mut Mind, brought_home: bool) -> Result<(), String> {
    let state = read_state().await?;
    if state.vault_value > mind.peak {
        mind.peak = state.vault_value;
    }
    if brought_home {
        mind.leavers.brought_home(&mind.limits, &state);
    } else {
        mind.leavers.seen(state.idle);
    }
    let plan = decide::decide(
        &mind.settings,
        &mind.limits,
        mind.peak,
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
    if carry_out(&mind.settings, &mind.limits, &state, &plan).await {
        mind.patience.went_through();
    } else {
        mind.patience.refused(at);
    }
    Ok(())
}

/// The account, asked about the two markets this strategy trades. Naming them
/// also tells the executor which markets' news should wake it, so a price or
/// funding posted for any other market costs nothing.
async fn read_state() -> Result<AccountState, String> {
    let query = Query {
        markets: vec![config::SPOT.to_string(), config::PERP.to_string()],
    };
    from_payload(&drivers::account_state(DRIVER.to_string(), to_payload(&query)).await?)
}

/// Do what `plan` says, and say whether any of it went through.
///
/// Each leg is its own submission and the second is sized from what the
/// first actually did, so a leg that misses never leaves the other on its
/// own, and a refusal stops the plan where it is: the next event reads the
/// account again. The short is levelled with the whole of the spot, an
/// order's worth at a time, before the step ends.
async fn carry_out(
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
            let home = transfers::home_chain();
            for (from, amount) in [
                (Account::Station(home), fund.from_station),
                (Account::Vault, fund.from_vault),
            ] {
                if amount > Decimal::from(0) {
                    match transfers::move_(from, venue(), millionths(amount)).await {
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
                match place(order).await {
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
                match place(&order).await {
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
                && let Some(size) = place(order).await
            {
                landed = true;
                perp_held += size;
            }
            if let Some(order) = trim(settings, limits, state, spot_held, perp_held) {
                landed |= place(&order).await.is_some();
            }
        }
    }
    landed
}

/// Close everything, the short first, because a halt is an unwind with less
/// time. The order cap is lifted: the vault is stopping, and leaving a leg on
/// would cost its depositors more than a wider fill will.
async fn flatten(mind: &Mind) -> Result<(), String> {
    let limits = Limits {
        max_order_fraction: Decimal::from(1),
        ..mind.limits
    };
    let zero = Decimal::from(0);
    for _ in 0..FLATTEN_STEPS {
        let state = read_state().await?;
        if state.position(&mind.settings.spot) == zero
            && state.position(&mind.settings.perp) == zero
        {
            return Ok(());
        }
        let plan = Plan::Reduce {
            perp: unwind(&mind.settings, &limits, &state),
        };
        carry_out(&mind.settings, &limits, &state, &plan).await;
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
async fn place(order: &Order) -> Option<Decimal> {
    let verdicts = drivers::submit(
        DRIVER.to_string(),
        vec![to_payload(&Action::Place(order.clone()))],
    )
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

sdk::export_agent!(FundingBasis);

#[cfg(test)]
mod tests {
    use super::{config, limits};
    use crate::decide::pair_notional;
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
        assert!(manifest.contains("a 10% fall") && config::DRAWDOWN_STOP == "0.10");
        assert!(manifest.contains(config::SPOT) && manifest.contains(config::PERP));
    }
}
