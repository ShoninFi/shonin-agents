//! Holds Robinhood stock tokens against Lighter perpetuals over the weekend.
//!
//! A stock token trades around the clock; the stock behind it, and the
//! channel that creates and redeems tokens against it, don't. From Friday's
//! close to Monday's open nothing pulls a token back to its stock, and the
//! perpetual on the same stock moves with the weekend's flow. When the
//! perpetual trades far enough above the token, this agent buys the token
//! and shorts the perpetual in matching shares, and carries the pair into
//! Monday, when the gap usually closes.
//!
//! The whole strategy is one long-lived program. It listens to the shared
//! Lighter books for its three stocks and to the vault's events, and at most
//! every `STEP_EVERY` seconds it works out what it would like to hold
//! (`plan.rs`) and takes one step towards it. Its limits are in `config.rs`,
//! and the manifest claims them in the same words.

mod config;
mod per_share;
mod plan;
mod venues;

use std::collections::HashMap;

use futures::StreamExt;
use sdk::streams::{self, LIGHTER, LighterFeed, LighterValue};
use sdk::transfers::{self, Account, Moved, VenueAccount};
use sdk::{Decimal, Verdict, decimal, drivers, from_payload, millionths, to_payload, vault};
use strategy_sdk as sdk;
use venues::{lighter as lt, robinhood as rh};

use per_share::PerShare;
use plan::{Name, Picture, Step, zero};

/// How many steps a halt may take to close everything before leaving the
/// rest to the contracts.
const FLATTEN_STEPS: usize = 64;

struct WeekendCarry;

/// What the agent remembers between steps: rebuilt from the venues whenever
/// it starts, so a restart costs a moment and nothing else.
struct Mind {
    /// The value of a vault share since the agent started, and its peak:
    /// what the drawdown stop is measured on.
    per_share: PerShare,
    /// Each perpetual's best bid and ask, by Lighter market number.
    books: HashMap<u32, (Decimal, Decimal)>,
    last_step: u64,
}

enum Woke {
    Book(u32, Decimal, Decimal),
    Vault(vault::VaultEvent),
    Nothing,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

impl sdk::Agent for WeekendCarry {
    async fn run() -> Result<(), String> {
        let mut mind = Mind {
            per_share: PerShare::default(),
            books: HashMap::new(),
            last_step: 0,
        };
        let mut books = Vec::new();
        for (_, _, market) in config::NAMES {
            let stream = streams::subscribe::<_, LighterValue>(
                LIGHTER,
                &LighterFeed::Book { market: *market },
            )?;
            let market = *market;
            books.push(stream.map(move |value| match value {
                Ok(stamped) => {
                    let LighterValue::Book(book) = stamped.value;
                    match (book.bids.first(), book.asks.first()) {
                        (Some(bid), Some(ask)) => Woke::Book(market, bid.price, ask.price),
                        _ => Woke::Nothing,
                    }
                }
                Err(_) => Woke::Nothing,
            }));
        }
        let books = futures::stream::select_all(books);
        let vault_events = vault::events().into_stream().map(Woke::Vault);
        let mut waking = futures::stream::select(books, vault_events);

        while let Some(woke) = waking.next().await {
            match woke {
                Woke::Book(market, bid, ask) => {
                    mind.books.insert(market, (bid, ask));
                    if now() >= mind.last_step + config::STEP_EVERY {
                        mind.last_step = now();
                        step(&mut mind).await?;
                    }
                }
                // A leaver's fraction of both legs is being closed by the
                // contracts; look again at what is left.
                Woke::Vault(vault::VaultEvent::ClaimPending(_)) => step(&mut mind).await?,
                // Leavers wait on more cash than the vault holds: bring idle
                // dollars home from the Safe, then from Lighter.
                Woke::Vault(vault::VaultEvent::BufferLow(short)) => {
                    let mut owed = short;
                    for driver in [rh::DRIVER, lt::DRIVER] {
                        if owed == 0 {
                            break;
                        }
                        if matches!(
                            transfers::move_(venue(driver), Account::Vault, owed).await,
                            Moved::Done
                        ) {
                            owed = 0;
                        }
                    }
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

fn venue(driver: &str) -> Account {
    Account::Venue(VenueAccount {
        chain: transfers::home_chain(),
        driver: driver.to_string(),
    })
}

/// Everything the vault is worth, everywhere, in dollars.
async fn vault_value() -> Decimal {
    let total: u64 = transfers::balances()
        .await
        .iter()
        .map(|balance| balance.amount)
        .sum();
    Decimal::from(total) / Decimal::from(1_000_000u32)
}

async fn state<Q: serde::Serialize, A: serde::de::DeserializeOwned>(
    driver: &str,
    query: &Q,
) -> Result<A, String> {
    from_payload(&drivers::account_state(driver.to_string(), to_payload(query)).await?)
}

/// What the agent sees now.
async fn picture(mind: &mut Mind) -> Result<Picture, String> {
    let prices: Vec<rh::TokenPrice> = state(rh::DRIVER, &rh::Query::Prices).await?;
    let holdings: rh::AccountState = state(rh::DRIVER, &rh::Query::Holdings).await?;
    // No Lighter account before the first move to it: nothing is held there.
    let perps: Option<lt::AccountState> = state(lt::DRIVER, &lt::Query::Account).await.ok();
    let value = vault_value().await;
    let mut names = Vec::new();
    for (token, perp, market) in config::NAMES {
        let Some(price) = prices.iter().find(|p| p.token == *token) else {
            continue;
        };
        let Some((bid, ask)) = mind.books.get(market) else {
            continue;
        };
        let tokens = holdings
            .holdings
            .iter()
            .find(|h| h.token == *token)
            .map(|h| h.tokens)
            .unwrap_or_else(zero);
        let perp_position = perps
            .as_ref()
            .and_then(|state| state.positions.iter().find(|p| p.market == *perp))
            .map(|p| p.size)
            .unwrap_or_else(zero);
        names.push(Name {
            token: token.to_string(),
            perp: perp.to_string(),
            token_price: price.market_price,
            feed_price: price.feed_price,
            multiplier: price.multiplier,
            paused: price.paused,
            perp_bid: *bid,
            perp_ask: *ask,
            tokens,
            perp_position,
        });
    }
    mind.per_share.look(value, plan::held(&names));
    Ok(Picture {
        now: now(),
        vault_value: value,
        share: mind.per_share.now,
        peak: mind.per_share.peak,
        names,
    })
}

/// Look, decide, and take one step.
async fn step(mind: &mut Mind) -> Result<(), String> {
    let picture = picture(mind).await?;
    act(&picture, picture.next()).await
}

async fn act(picture: &Picture, step: Step) -> Result<(), String> {
    let slippage = decimal(config::SLIPPAGE);
    let one = Decimal::from(1u8);
    match step {
        Step::Nothing => Ok(()),
        Step::Open {
            index,
            dollars,
            tokens,
            shares,
        } => {
            let name = &picture.names[index];
            // The money has to be at each venue before it can trade there;
            // the buffer stays home, where a withdrawal reaches it.
            let margin = shares * name.perp_bid * decimal(config::PERP_MARGIN);
            if !fund(rh::DRIVER, dollars, picture.vault_value).await
                || !fund(lt::DRIVER, margin, picture.vault_value).await
            {
                return Ok(());
            }
            let bought = submit(
                rh::DRIVER,
                &rh::Action::Buy {
                    token: name.token.clone(),
                    amount: dollars,
                    min_tokens: tokens * (one - slippage),
                },
            )
            .await?;
            // No token, no short: a naked short is the one thing this
            // strategy never holds.
            if !bought {
                return Ok(());
            }
            submit(
                lt::DRIVER,
                &lt::Action::Place(lt::Order {
                    market: name.perp.clone(),
                    side: lt::Side::Sell,
                    size: shares,
                    limit_price: name.perp_bid * (one - slippage),
                    tif: lt::Tif::Ioc,
                    reduce_only: false,
                    client_id: None,
                }),
            )
            .await
            .map(|_| ())
        }
        Step::Close {
            index,
            shares,
            tokens,
        } => {
            let name = &picture.names[index];
            if shares > zero() {
                let bought_back = submit(
                    lt::DRIVER,
                    &lt::Action::Place(lt::Order {
                        market: name.perp.clone(),
                        side: lt::Side::Buy,
                        size: shares,
                        limit_price: name.perp_ask * (one + slippage),
                        tif: lt::Tif::Ioc,
                        reduce_only: true,
                        client_id: None,
                    }),
                )
                .await?;
                // The token waits for the short: the next step sees what
                // the buy-back really closed.
                if !bought_back {
                    return Ok(());
                }
            }
            if tokens > zero() {
                submit(
                    rh::DRIVER,
                    &rh::Action::Sell {
                        token: name.token.clone(),
                        tokens,
                        min_amount: tokens * name.token_price * (one - slippage),
                    },
                )
                .await?;
            }
            Ok(())
        }
    }
}

/// Make sure `driver`'s account holds `wanted` dollars, moving the shortfall
/// from the vault's cash without going below the buffer. Whether it holds
/// enough now.
async fn fund(driver: &str, wanted: Decimal, vault_value: Decimal) -> bool {
    let balances = transfers::balances().await;
    let amount_of = |wanted: &dyn Fn(&Account) -> bool| -> Decimal {
        balances
            .iter()
            .find(|b| wanted(&b.account))
            .map(|b| Decimal::from(b.amount) / Decimal::from(1_000_000u32))
            .unwrap_or_else(zero)
    };
    let there = amount_of(&|account| matches!(account, Account::Venue(v) if v.driver == driver));
    if there >= wanted {
        return true;
    }
    let cash = amount_of(&|account| matches!(account, Account::Vault));
    let keep = vault_value * decimal(config::IDLE_BUFFER);
    let spare = if cash > keep { cash - keep } else { zero() };
    let short = wanted - there;
    if spare < short {
        return false;
    }
    matches!(
        transfers::move_(Account::Vault, venue(driver), millionths(short)).await,
        Moved::Done
    )
}

/// Submit one action; whether the venue took it.
async fn submit<A: serde::Serialize>(driver: &str, action: &A) -> Result<bool, String> {
    let verdicts = drivers::submit(driver.to_string(), vec![to_payload(action)]).await;
    Ok(matches!(verdicts.first(), Some(Verdict::Accepted(_))))
}

/// Close everything, short first, because a halt is an unwind with less
/// time.
async fn flatten(mind: &mut Mind) -> Result<(), String> {
    for _ in 0..FLATTEN_STEPS {
        let mut picture = picture(mind).await?;
        // A halt closes regardless of the weekend or the premium: as if the
        // peak were far above.
        picture.peak = picture.share * Decimal::from(10u8);
        match picture.next() {
            Step::Nothing => return Ok(()),
            close @ Step::Close { .. } => act(&picture, close).await?,
            Step::Open { .. } => return Ok(()),
        }
    }
    Ok(())
}

sdk::export_agent!(WeekendCarry);
