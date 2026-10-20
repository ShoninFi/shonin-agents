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
mod per_share;
mod plan;
mod venues;

use futures::StreamExt;
use per_share::PerShare;
use plan::{Name, Picture, Step, zero};
use sdk::streams::{
    self, CHAINLINK, ChainlinkFeed, ChainlinkValue, HYPERLIQUID, HyperliquidFeed, HyperliquidValue,
};
use sdk::transfers::{self, Account, Moved, VenueAccount};
use sdk::{Decimal, Verdict, decimal, drivers, from_payload, millionths, to_payload, vault};
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

enum Woke {
    Tick,
    Vault(vault::VaultEvent),
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

impl sdk::Agent for Spot {
    async fn run() -> Result<(), String> {
        let home = transfers::home_chain();
        let venue = config::VENUES
            .iter()
            .find(|venue| venue.chain == home)
            .ok_or_else(|| format!("this agent trades on no venue on {home}"))?;
        let mut mind = Mind {
            venue,
            per_share: PerShare::new(config::DRAWDOWN_STOP),
            last_step: 0,
        };
        let ticks = clock(venue)?;
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        let mut waking = futures::stream::select(ticks, vault_events);

        // Look once at the start: a restart picks up wherever it was.
        step(&mut mind).await?;
        while let Some(woke) = waking.next().await {
            match woke {
                Woke::Tick => {
                    if now() >= mind.last_step + config::STEP_EVERY {
                        step(&mut mind).await?;
                    }
                }
                // A leaver's fraction is being closed by the contracts; look
                // again at what is left.
                Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(&mut mind).await?,
                // Leavers wait on more cash than the vault holds: bring the
                // Safe's idle dollars home.
                Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                    let _ = transfers::move_(safe(venue), Account::Vault, short).await;
                }
                Woke::Vault(vault::VaultEvent::Halting(_)) => {
                    flatten(&mut mind).await?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }
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
fn safe(venue: &config::Venue) -> Account {
    Account::Venue(VenueAccount {
        chain: transfers::home_chain(),
        driver: venue.driver.to_string(),
    })
}

async fn state<Q: serde::Serialize, A: serde::de::DeserializeOwned>(
    venue: &config::Venue,
    query: &Q,
) -> Result<A, String> {
    from_payload(&drivers::account_state(venue.driver.to_string(), to_payload(query)).await?)
}

/// What the agent sees now, and whether the share's measure could take it
/// in: a look one of whose reads failed or came back partial is not decided
/// from.
async fn picture(mind: &mut Mind) -> Result<(Picture, bool), String> {
    let prices: Vec<venues::TokenPrice> = state(mind.venue, &venues::Query::Prices).await?;
    let held: venues::AccountState = state(mind.venue, &venues::Query::Holdings).await?;
    let balances = transfers::balances().await;
    let dollars = |amount: u64| Decimal::from(amount) / Decimal::from(1_000_000u32);
    // Every account the vault has, the Safe valued by its driver.
    let value = dollars(balances.iter().map(|balance| balance.amount).sum());
    let read = |account: &Account| {
        balances
            .iter()
            .find(|balance| same(&balance.account, account))
            .map(|balance| dollars(balance.amount))
    };
    let home = transfers::home_chain();
    // An account whose read failed is left out of the balances, not read
    // as empty: each of the vault's must be there.
    let (cash, idle, in_safe) = (
        read(&Account::Vault),
        read(&Account::Station(home)),
        read(&safe(mind.venue)),
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
async fn step(mind: &mut Mind) -> Result<(), String> {
    mind.last_step = now();
    let (picture, looked) = picture(mind).await?;
    if looked {
        act(mind.venue, &picture, picture.next(false)).await?;
    }
    Ok(())
}

/// Take `step`; whether the driver took it.
async fn act(venue: &config::Venue, picture: &Picture, step: Step) -> Result<bool, String> {
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
                    transfers::move_(Account::Vault, safe(venue), millionths(short)).await,
                    Moved::Done
                ) {
                    return Ok(false);
                }
            }
            submit(
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
async fn submit(venue: &config::Venue, action: &venues::Action) -> Result<bool, String> {
    let verdicts = drivers::submit(venue.driver.to_string(), vec![to_payload(action)]).await;
    Ok(matches!(verdicts.first(), Some(Verdict::Accepted(_))))
}

/// Sell everything, one bounded order at a time, then bring the dollars
/// home: a halt is an unwind with less time.
async fn flatten(mind: &mut Mind) -> Result<(), String> {
    for _ in 0..FLATTEN_STEPS {
        // A halt acts on a look the measure skipped too: see
        // `Picture::halting`.
        let (picture, looked) = picture(mind).await?;
        let step = picture.halting(looked);
        if step == Step::Nothing || !act(mind.venue, &picture, step).await? {
            break;
        }
    }
    let held: venues::AccountState = state(mind.venue, &venues::Query::Holdings).await?;
    if held.dollars > zero() {
        let _ = transfers::move_(safe(mind.venue), Account::Vault, millionths(held.dollars)).await;
    }
    Ok(())
}

sdk::export_agent!(Spot);
