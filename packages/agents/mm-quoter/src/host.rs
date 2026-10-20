//! Everything the agent reaches outside itself: its driver, the clock and
//! somewhere to print. The steps take it as a [`Host`], so the tests run
//! them against a stand-in venue.

use strategy_sdk::{Verdict, drivers};

pub trait Host {
    /// Seconds since the Unix epoch.
    fn now(&self) -> u64;
    /// Something worth an operator's reading, such as a read that failed.
    fn note(&self, what: &str);
    async fn account_state(&self, driver: &str, query: Vec<u8>) -> Result<Vec<u8>, String>;
    async fn submit(&self, driver: &str, actions: Vec<Vec<u8>>) -> Vec<Verdict>;
}

/// The host the agent runs in.
pub struct Sdk;

impl Host for Sdk {
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
}
