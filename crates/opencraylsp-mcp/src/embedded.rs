//! The in-process backend for `--embedded`.
//!
//! No daemon: the tools run against a [`Pool`] bound to this process's
//! workspace. `opencraylsp-mcp` only uses this when `--embedded` is passed; it never
//! falls back to it because a failed daemon connection means "retry later",
//! not "start every language server twice".

use std::sync::Arc;

use async_trait::async_trait;
use opencraylsp_core::{BoundBackend, LspBackend, Pool};
use opencraylsp_proto::{HostError, ToolDef, ToolHost, ToolOutput};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// A [`ToolHost`] backed by an in-process pool.
pub struct EmbeddedHost {
    backend: Arc<dyn LspBackend>,
    /// Present when this host owns a pool, so it can stop it on the way out.
    pool: Option<Arc<Pool>>,
}

impl std::fmt::Debug for EmbeddedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedHost")
            .field("owns_pool", &self.pool.is_some())
            .finish_non_exhaustive()
    }
}

impl EmbeddedHost {
    /// Wraps a backend without pool ownership. Used by tests and by callers
    /// that manage the pool themselves.
    pub fn new(backend: Arc<dyn LspBackend>) -> Self {
        Self {
            backend,
            pool: None,
        }
    }

    /// Wraps a pool-bound backend; [`Self::shutdown`] stops that pool.
    pub fn from_pool(backend: Arc<BoundBackend>) -> Self {
        let pool = backend.pool().clone();
        Self {
            backend,
            pool: Some(pool),
        }
    }

    /// Stops the pool, if this host owns one. Call after the stdio loop ends.
    pub async fn shutdown(&self) {
        if let Some(pool) = &self.pool {
            pool.shutdown().await;
        }
    }
}

#[async_trait]
impl ToolHost for EmbeddedHost {
    async fn list_tools(&self) -> Result<Vec<ToolDef>, HostError> {
        Ok(opencraylsp_tools::tool_defs())
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, HostError> {
        // `call_tool` never returns a protocol error: a failing tool is a
        // `ToolOutput` with `is_error = true`.
        Ok(opencraylsp_tools::call_tool(self.backend.as_ref(), name, arguments, cancel).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencraylsp_core::mock::MockBackend;
    use serde_json::json;

    fn host() -> EmbeddedHost {
        EmbeddedHost::new(Arc::new(MockBackend::new("/ws")))
    }

    #[tokio::test]
    async fn the_catalogue_is_the_built_in_one() {
        let tools = host().list_tools().await.expect("list");
        let names: Vec<String> = tools.into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            opencraylsp_tools::tool_defs()
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        );
        assert!(!names.is_empty());
    }

    #[tokio::test]
    async fn an_unknown_tool_is_a_tool_error_not_a_host_error() {
        let out = host()
            .call_tool("lsp_nope", json!({}), &CancellationToken::new())
            .await
            .expect("the host answers");
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_args]"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_backend_without_a_pool_shuts_down_cleanly() {
        let host = host();
        assert!(format!("{host:?}").contains("EmbeddedHost"));
        host.shutdown().await;
    }

    #[tokio::test]
    async fn a_pool_backed_host_owns_its_pool_and_shuts_it_down() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(opencraylsp_core::LspConfig::default()));
        let backend = pool.bind(
            dir.path().to_owned(),
            opencraylsp_core::LanguageSelection::Auto,
        );
        let host = EmbeddedHost::from_pool(backend);
        assert!(!host.list_tools().await.expect("list").is_empty());
        host.shutdown().await;
    }
}
