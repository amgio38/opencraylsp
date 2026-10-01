//! `opencraylsp-mcp`: MCP stdio server. `mcp` holds the hand-written protocol layer,
//! `options` resolves the command line, `daemon_host` and `embedded` are the
//! two backends, and `main` owns the process entry point.

/// Test scaffolding. Always compiled so integration tests can reach it; the
/// `--fake-host` flag that puts it behind the binary is gated on the
/// `test-fake-host` feature, so a release build has no way in.
pub mod fake_host;

pub mod daemon_host;
pub mod mcp;
pub mod options;

#[cfg(feature = "embedded")]
pub mod embedded;
