//! Quotes SOL against the dollar as concentrated liquidity on Orca.
//!
//! The same business as `mm-quoter`, on a pool rather than a book: instead of
//! a resting bid and ask around the mid, one band of liquidity around the
//! oracle's price, which every trade crossing it pays the pool's fee to. When
//! the price nears an edge of the band it takes the liquidity out, rebalances
//! the two tokens and sets a new band; while the pool's price strays from the
//! oracle's it waits, since a pool can be pushed for a moment and the oracle
//! can't.
//!
//! It remembers nothing between steps: each one starts from what the venue
//! says is there. Its limits are in `config.rs`, where the reviewer reads
//! them, and its decisions in `decide.rs`.

mod config;
pub mod decide;

use decide::Plan;
use futures::StreamExt;
use orca::{AccountState, Action};
use sdk::transfers::{self, Account, Moved};
use sdk::{Decimal, drivers, from_payload, millionths, to_payload, vault};
use strategy_sdk as sdk;
use strategy_sdk::orca;

const DRIVER: &str = orca::DRIVER;

/// How many steps one wake may take before waiting for the next: a band set
/// again is a removal, a close, a swap and an open.
const STEPS_PER_WAKE: usize = 6;

struct MmQuoterOrca;

enum Woke {
    Venue,
    Vault(vault::VaultEvent),
}

impl sdk::Agent for MmQuoterOrca {
    async fn run() -> Result<(), String> {
        let account = drivers::account_events(DRIVER)
            .into_stream()
            .map(|_| Woke::Venue);
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        let mut waking = futures::stream::select(account, vault_events);

        // Look once at the start: a restart picks up wherever it was.
        steps(false).await?;
        while let Some(woke) = waking.next().await {
            match woke {
                Woke::Vault(vault::VaultEvent::Halting(_)) => {
                    steps(true).await?;
                    return Ok(());
                }
                // A depositor is leaving and the station is closing their
                // fraction of the position; the band left is looked at again.
                Woke::Venue | Woke::Vault(_) => steps(false).await?,
            }
        }
        Ok(())
    }
}

/// Take steps until there is nothing to do, a step is refused, or the wake's
/// steps are used up.
async fn steps(halting: bool) -> Result<(), String> {
    for _ in 0..STEPS_PER_WAKE {
        let state = read_state().await?;
        let went = match decide::plan(&state, halting) {
            Plan::Nothing => return Ok(()),
            Plan::Fund(amount) => fund(amount).await,
            Plan::Act(actions) => act(&actions).await?,
        };
        if !went {
            return Ok(());
        }
    }
    Ok(())
}

async fn read_state() -> Result<AccountState, String> {
    from_payload(&drivers::account_state(DRIVER.to_string(), Vec::new()).await?)
}

/// Dollars from the vault's cash to its station, for the band to draw on.
async fn fund(amount: Decimal) -> bool {
    let home = transfers::home_chain();
    matches!(
        transfers::move_(Account::Vault, Account::Station(home), millionths(amount)).await,
        Moved::Done
    )
}

/// Submit `actions`, and say whether every one went through.
async fn act(actions: &[Action]) -> Result<bool, String> {
    let payloads: Vec<Vec<u8>> = actions.iter().map(to_payload).collect();
    let verdicts = drivers::submit(DRIVER.to_string(), payloads).await;
    Ok(verdicts.len() == actions.len()
        && verdicts
            .iter()
            .all(|verdict| matches!(verdict, sdk::Verdict::Accepted(_))))
}

sdk::export_agent!(MmQuoterOrca);
