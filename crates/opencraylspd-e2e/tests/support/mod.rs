//! Shared harness for the end-to-end suite.
//!
//! Everything lives in a tempdir: socket, config, workspace, daemon log and
//! `HOME`. Nothing here may touch the real home or the default socket, and no
//! test may leave a daemon or language-server process behind (`pkill -f` and
//! `pgrep -f` are forbidden; processes are addressed by pid).

pub mod binaries;
pub mod env;
pub mod mcp;

pub use env::{Limits, ServerSpec, TestEnv, standard_servers, wait_until};
pub use mcp::{McpClient, is_error, result_text, run_once};
