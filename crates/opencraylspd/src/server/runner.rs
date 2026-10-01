//! The seam between the wire protocol and the tools.
//!
//! The daemon does not know what `lsp_definition` means; it hands a tool name
//! and arguments to a [`ToolRunner`]. Production uses [`LspTools`]; tests plug
//! in slow or scripted runners to exercise cancellation and concurrency
//! without a language server.

use async_trait::async_trait;
use opencraylsp_core::LspBackend;
use opencraylsp_proto::{ToolDef, ToolOutput};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Runs tools on behalf of one connection's backend.
#[async_trait]
pub trait ToolRunner: Send + Sync {
    /// The tools offered to clients.
    fn defs(&self) -> Vec<ToolDef>;

    /// Runs `name`. Never fails at the protocol level: problems are a
    /// [`ToolOutput`] with `is_error = true`.
    async fn call(
        &self,
        backend: &dyn LspBackend,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> ToolOutput;
}

/// The real tool catalog.
#[derive(Debug, Default, Clone, Copy)]
pub struct LspTools;

#[async_trait]
impl ToolRunner for LspTools {
    fn defs(&self) -> Vec<ToolDef> {
        opencraylsp_tools::tool_defs()
    }

    async fn call(
        &self,
        backend: &dyn LspBackend,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        opencraylsp_tools::call_tool(backend, name, arguments, cancel).await
    }
}
