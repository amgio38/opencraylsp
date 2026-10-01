//! Language-server client machinery shared by the daemon and the embedded
//! mode: the backend contract the tool layer codes against, and (added by
//! later issues) transport, instances, document sync and the shared pool.
//!
//! Design: `dev_docs/OPENCRAYLSP_DESIGN.md`.

pub mod backend;
pub mod client;
pub mod config;
pub mod daemon_guard;
pub mod instance;
pub mod languages;
pub mod manager;
pub mod memory;
pub mod pool;
pub mod progress;
pub mod warmup;

#[cfg(any(test, feature = "testing"))]
pub mod mock;

pub use backend::{
    DiagnosticsReport, LanguageInfo, LspBackend, LspError, PositionEncoding, Served,
};
pub use config::{ConfigError, LspConfig, ServerConfig};
pub use languages::{LanguageSelection, UnknownLanguage};
pub use manager::{BoundBackend, EnabledLanguages};
pub use pool::{DaemonGuard, Pool, PoolOptions};
