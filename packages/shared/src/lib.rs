//! What every bot here needs and the SDK does not provide: reading the config
//! its manifest declares, and sizing a position against limits it wrote down
//! itself.
//!
//! None of this is a platform rule. The platform has no risk policy; a
//! strategy's limits live in its own source, where a reader and a reviewer can
//! both see them.

pub mod config;
pub mod sizing;

pub use config::Config;
pub use sizing::{Limits, size_for};
