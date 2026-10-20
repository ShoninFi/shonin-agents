//! Everything the agent reaches outside itself: its drivers, the vault's
//! accounts, the clock and somewhere to print. The steps take it as a
//! [`Host`], so the tests run them against a stand-in whose reads fail when
//! told to.

use strategy_sdk::transfers::{self, Account, Balance, Moved};
use strategy_sdk::{Verdict, drivers};

pub trait Host {
    /// The vault's own chain.
    fn home_chain(&self) -> String;
    /// Seconds since the Unix epoch.
    fn now(&self) -> u64;
    /// Something worth an operator's reading, such as a read that failed.
    fn note(&self, what: &str);
    async fn account_state(&self, driver: &str, query: Vec<u8>) -> Result<Vec<u8>, String>;
    async fn submit(&self, driver: &str, actions: Vec<Vec<u8>>) -> Vec<Verdict>;
    async fn move_(&self, from: Account, to: Account, amount: u64) -> Moved;
    async fn balances(&self) -> Vec<Balance>;
}

/// The host the agent runs in.
pub struct Sdk;

impl Host for Sdk {
    fn home_chain(&self) -> String {
        transfers::home_chain()
    }

    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0)
    }

    fn note(&self, what: &str) {
        eprintln!("{what}");
    }

    async fn account_state(&self, driver: &str, query: Vec<u8>) -> Result<Vec<u8>, String> {
        drivers::account_state(driver.to_string(), query).await
    }

    async fn submit(&self, driver: &str, actions: Vec<Vec<u8>>) -> Vec<Verdict> {
        drivers::submit(driver.to_string(), actions).await
    }

    async fn move_(&self, from: Account, to: Account, amount: u64) -> Moved {
        transfers::move_(from, to, amount).await
    }

    async fn balances(&self) -> Vec<Balance> {
        transfers::balances().await
    }
}
