//! Reads company filings as they are published, scores them, and trades the
//! stock token on the news.
//!
//! Every few minutes it reads the SEC's list of each company's recent
//! filings. A current report (Form 8-K) it hasn't seen is turned into eight
//! numbers (`score.rs`) and scored by the bundle's model through the host's
//! inference service; if the model can't be run, the same placeholder score
//! is computed here instead. A strong score buys a bounded amount of the
//! company's stock token through the `robinhood` driver; every position is
//! sold after a day, or sooner after a fall from its entry.
//!
//! The model is a placeholder, and the manifest says so. Its limits are in
//! `config.rs`, and the manifest claims them in the same words.

mod config;
mod edgar;
mod score;
mod venues;

use std::collections::{BTreeMap, BTreeSet};

use futures::stream::PollNext;
use futures::{Stream, StreamExt};
use sdk::inference::{self, NamedTensor, Tensor, TensorData};
use sdk::streams::{self, CHAINLINK, ChainlinkFeed, ChainlinkValue};
use sdk::transfers::{self, Account, Moved, VenueAccount};
use sdk::{Decimal, Verdict, decimal, drivers, from_payload, millionths, to_payload, vault};
use strategy_sdk as sdk;
use venues::robinhood as rh;

struct Catalyst;

/// A position the agent opened: when, and at what price.
struct Entry {
    at: u64,
    price: Decimal,
}

struct Mind {
    /// Every report already read, so each is acted on once.
    seen: BTreeSet<String>,
    entries: BTreeMap<String, Entry>,
    last_read: u64,
    /// The first read only learns what is already out: old news is not
    /// traded.
    primed: bool,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

fn zero() -> Decimal {
    Decimal::from(0)
}

impl sdk::Agent for Catalyst {
    async fn run() -> Result<(), String> {
        let mind = Mind {
            seen: BTreeSet::new(),
            entries: BTreeMap::new(),
            last_read: 0,
            primed: false,
        };
        // The feeds' rounds are the agent's clock: each wakes it to look at
        // its filings and its positions.
        let mut ticks = Vec::new();
        for (token, _) in config::NAMES {
            ticks.push(streams::subscribe::<_, ChainlinkValue>(
                CHAINLINK,
                &ChainlinkFeed::Price {
                    token: token.to_string(),
                },
            )?);
        }
        let ticks = futures::stream::select_all(ticks).map(|_| None);
        let vault_events = vault::events().into_stream().map(Some);
        serve(&mut Live { mind }, vault_first(vault_events, ticks)).await
    }
}

/// What the agent does on a wake.
trait Wakes {
    /// An ordinary step: read new filings, act on any strong one, and close
    /// what is due.
    async fn step(&mut self) -> Result<(), String>;
    /// The vault halts: sell everything.
    async fn halt(&mut self) -> Result<(), String>;
    /// Leavers wait on `short` more cash than the vault holds.
    async fn buffer_low(&mut self, short: u64);
}

/// The agent as it runs, on its venue.
struct Live {
    mind: Mind,
}

impl Wakes for Live {
    async fn step(&mut self) -> Result<(), String> {
        step(&mut self.mind).await
    }

    async fn halt(&mut self) -> Result<(), String> {
        for (token, _) in config::NAMES {
            sell_all(token).await?;
        }
        Ok(())
    }

    async fn buffer_low(&mut self, short: u64) {
        let _ = transfers::move_(venue(), Account::Vault, short).await;
    }
}

/// What wakes the agent (a vault event, or a tick as `None`), the vault's
/// news always taken before anything else that is waiting: an agent started
/// again within a halt finds the halting notice first in the vault's news,
/// and must see it before it takes an ordinary step.
fn vault_first(
    vault: impl Stream<Item = Option<vault::VaultEvent>> + Unpin,
    ticks: impl Stream<Item = Option<vault::VaultEvent>> + Unpin,
) -> impl Stream<Item = Option<vault::VaultEvent>> + Unpin {
    futures::stream::select_with_strategy(vault, ticks, |_: &mut ()| PollNext::Left)
}

/// Act on each wake until the vault halts. There is no step before the
/// first wake: the vault's news is read first, so a start within a halt
/// goes straight to the halt, and the first step comes at the first tick.
async fn serve(
    agent: &mut impl Wakes,
    mut waking: impl Stream<Item = Option<vault::VaultEvent>> + Unpin,
) -> Result<(), String> {
    while let Some(woke) = waking.next().await {
        match woke {
            Some(vault::VaultEvent::Halting(_)) => return agent.halt().await,
            Some(vault::VaultEvent::BufferLow(short)) => agent.buffer_low(short).await,
            _ => agent.step().await?,
        }
    }
    Ok(())
}

fn venue() -> Account {
    Account::Venue(VenueAccount {
        chain: transfers::home_chain(),
        driver: rh::DRIVER.to_string(),
    })
}

async fn state<Q: serde::Serialize, A: serde::de::DeserializeOwned>(
    query: &Q,
) -> Result<A, String> {
    from_payload(&drivers::account_state(rh::DRIVER.to_string(), to_payload(query)).await?)
}

async fn submit(action: &rh::Action) -> bool {
    let verdicts = drivers::submit(rh::DRIVER.to_string(), vec![to_payload(action)]).await;
    matches!(verdicts.first(), Some(Verdict::Accepted(_)))
}

async fn vault_value() -> (Decimal, Decimal) {
    let balances = transfers::balances().await;
    let dollars = |amount: u64| Decimal::from(amount) / Decimal::from(1_000_000u32);
    let total = balances.iter().map(|b| b.amount).sum::<u64>();
    let cash = balances
        .iter()
        .find(|b| matches!(b.account, Account::Vault))
        .map(|b| b.amount)
        .unwrap_or(0);
    (dollars(total), dollars(cash))
}

/// Read new filings, act on any strong one, and close what is due.
async fn step(mind: &mut Mind) -> Result<(), String> {
    let prices: Vec<rh::TokenPrice> = state(&rh::Query::Prices).await?;
    let holdings: rh::AccountState = state(&rh::Query::Holdings).await?;
    let price_of = |token: &str| prices.iter().find(|p| p.token == token);
    let held_of = |token: &str| {
        holdings
            .holdings
            .iter()
            .find(|h| h.token == token)
            .map(|h| h.tokens)
            .unwrap_or_else(zero)
    };

    // Close what is due: a day old, or past the stop.
    for (token, _) in config::NAMES {
        let held = held_of(token);
        let Some(price) = price_of(token) else {
            continue;
        };
        if held <= zero() {
            mind.entries.remove(*token);
            continue;
        }
        // A position this instance didn't open (a restart) is timed from now.
        let entry = mind.entries.entry(token.to_string()).or_insert(Entry {
            at: now(),
            price: price.market_price,
        });
        let stopped =
            price.market_price <= entry.price * (Decimal::from(1u8) - decimal(config::STOP_LOSS));
        if now() >= entry.at + config::HOLD_FOR || stopped {
            sell_all(token).await?;
        }
    }

    if now() < mind.last_read + config::READ_EVERY {
        return Ok(());
    }
    mind.last_read = now();
    for (token, cik) in config::NAMES {
        let document = match edgar::get(&edgar::path(*cik), config::USER_AGENT) {
            Ok(document) => document,
            // The SEC not answering is a quiet minute, not a fault.
            Err(_) => continue,
        };
        for report in edgar::current_reports(&document).into_iter().take(10) {
            if !mind.seen.insert(report.accession.clone()) || !mind.primed {
                continue;
            }
            let score = score_of(&report).await;
            let strong = score >= decimal(config::BUY_SCORE) && held_of(token) <= zero();
            if let (true, Some(price)) = (strong, price_of(token)) {
                buy(token, price).await?;
                mind.entries.insert(
                    token.to_string(),
                    Entry {
                        at: now(),
                        price: price.market_price,
                    },
                );
            }
        }
    }
    mind.primed = true;
    Ok(())
}

/// The model's score for a report, or the same placeholder computed here
/// if the model can't be run (not in the bundle, over its budget).
async fn score_of(report: &edgar::Report) -> Decimal {
    let features = score::features(report);
    let input = NamedTensor {
        name: "features".to_string(),
        tensor: Tensor {
            shape: vec![1, 8],
            data: TensorData::Float32(features.to_vec()),
        },
    };
    let scored = match inference::infer(config::MODEL.to_string(), vec![input]).await {
        Ok(outputs) => outputs
            .into_iter()
            .find_map(|named| match named.tensor.data {
                TensorData::Float32(values) => values.first().copied(),
                _ => None,
            }),
        Err(_) => None,
    };
    let score = scored.unwrap_or_else(|| score::written_down(&features));
    // Two decimal places are all a score is read to.
    Decimal::from((score.clamp(0.0, 1.0) * 100.0).round() as u32) / Decimal::from(100u32)
}

/// Buy one order's worth of `token`, within every limit.
async fn buy(token: &str, price: &rh::TokenPrice) -> Result<(), String> {
    if price.paused || price.market_price <= zero() {
        return Ok(());
    }
    let (value, cash) = vault_value().await;
    let holdings: rh::AccountState = state(&rh::Query::Holdings).await?;
    let exposure = holdings
        .holdings
        .iter()
        .map(|h| h.tokens * h.feed_price)
        .fold(zero(), |a, b| a + b);
    let room = value * decimal(config::MAX_EXPOSURE) - exposure;
    let dollars = (value * decimal(config::MAX_ORDER_FRACTION)).min(room);
    if dollars < decimal(config::MINIMUM_NOTIONAL) {
        return Ok(());
    }
    // The dollars go to the Safe first, the buffer staying home.
    if holdings.dollars < dollars {
        let keep = value * decimal(config::IDLE_BUFFER);
        let short = dollars - holdings.dollars;
        if cash - keep < short {
            return Ok(());
        }
        if !matches!(
            transfers::move_(Account::Vault, venue(), millionths(short)).await,
            Moved::Done
        ) {
            return Ok(());
        }
    }
    let one = Decimal::from(1u8);
    submit(&rh::Action::Buy {
        token: token.to_string(),
        amount: dollars,
        min_tokens: dollars / price.market_price * (one - decimal(config::SLIPPAGE)),
    })
    .await;
    Ok(())
}

/// Sell every token of `token` the Safe holds.
async fn sell_all(token: &str) -> Result<(), String> {
    let prices: Vec<rh::TokenPrice> = state(&rh::Query::Prices).await?;
    let holdings: rh::AccountState = state(&rh::Query::Holdings).await?;
    let (Some(price), Some(held)) = (
        prices.iter().find(|p| p.token == token),
        holdings.holdings.iter().find(|h| h.token == token),
    ) else {
        return Ok(());
    };
    let one = Decimal::from(1u8);
    submit(&rh::Action::Sell {
        token: token.to_string(),
        tokens: held.tokens,
        min_amount: held.tokens * price.market_price * (one - decimal(config::SLIPPAGE)),
    })
    .await;
    Ok(())
}

sdk::export_agent!(Catalyst);

#[cfg(test)]
mod tests {
    use super::*;

    /// Notes what each wake did.
    #[derive(Default)]
    struct Noted(Vec<&'static str>);

    impl Wakes for Noted {
        async fn step(&mut self) -> Result<(), String> {
            self.0.push("step");
            Ok(())
        }

        async fn halt(&mut self) -> Result<(), String> {
            self.0.push("halt");
            Ok(())
        }

        async fn buffer_low(&mut self, _: u64) {
            self.0.push("buffer-low");
        }
    }

    #[test]
    fn a_start_inside_a_halt_buys_nothing_and_goes_to_the_halt() {
        let mut noted = Noted::default();
        // Started again within the halt: the notice waits first in the
        // vault's news, and the feeds' rounds are there from the start too.
        let vault = futures::stream::iter([Some(vault::VaultEvent::Halting("halted".into()))])
            .chain(futures::stream::pending());
        let ticks = futures::stream::iter(std::iter::repeat_with(|| None).take(100));
        futures::executor::block_on(serve(&mut noted, vault_first(vault, ticks))).unwrap();
        assert_eq!(noted.0, ["halt"], "an ordinary step within the halt");
    }
}
