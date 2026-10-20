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
use futures::stream::PollNext;
use futures::{Stream, StreamExt};
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
        serve(&Live, vault_first(vault_events, account)).await
    }
}

/// What the agent does on a wake: its steps, ordinary or a halt's.
trait Steps {
    async fn steps(&self, halting: bool) -> Result<(), String>;
}

/// The agent as it runs, on its venue.
struct Live;

impl Steps for Live {
    async fn steps(&self, halting: bool) -> Result<(), String> {
        steps(halting).await
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
/// goes straight to the halt, and a restart picks up wherever it was at
/// the first of the pool's moves after it.
async fn serve(
    agent: &impl Steps,
    mut waking: impl Stream<Item = Woke> + Unpin,
) -> Result<(), String> {
    while let Some(woke) = waking.next().await {
        match woke {
            Woke::Vault(vault::VaultEvent::Halting(_)) => {
                agent.steps(true).await?;
                return Ok(());
            }
            // A depositor is leaving and the station is closing their
            // fraction of the position; the band left is looked at again.
            Woke::Venue | Woke::Vault(_) => agent.steps(false).await?,
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Notes each wake's steps, and whether they were a halt's.
    #[derive(Default)]
    struct Noted(RefCell<Vec<bool>>);

    impl Steps for Noted {
        async fn steps(&self, halting: bool) -> Result<(), String> {
            self.0.borrow_mut().push(halting);
            Ok(())
        }
    }

    #[test]
    fn a_start_inside_a_halt_trades_nothing_ordinary_and_goes_to_the_halt() {
        let noted = Noted::default();
        // Started again within the halt: the notice waits first in the
        // vault's news, and the pool's moves are there from the start too.
        let halting = Woke::Vault(vault::VaultEvent::Halting("halted".into()));
        let vault = futures::stream::iter([halting]).chain(futures::stream::pending());
        let venue = futures::stream::iter(std::iter::repeat_with(|| Woke::Venue).take(100));
        futures::executor::block_on(serve(&noted, vault_first(vault, venue))).unwrap();
        assert_eq!(
            *noted.0.borrow(),
            vec![true],
            "an ordinary step within the halt"
        );
    }
}
