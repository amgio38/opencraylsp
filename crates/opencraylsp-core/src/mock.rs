//! A scripted [`LspBackend`] for testing the tool layer without a real server.
//!
//! Responses are keyed by LSP method; every call is recorded so a test can
//! assert which methods were sent with which params. Path resolution mirrors
//! the production rules closely enough for tool tests: relative paths join the
//! boundary, and anything that does not end up under it is refused — a mock
//! looser than production is how a test suite goes green on a broken feature.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use opencraylsp_proto::{DaemonInfo, Indexing, LanguageMode, Limits, StatusReport};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::backend::{
    DiagnosticsReport, LanguageInfo, LspBackend, LspError, PositionEncoding, Served,
};

/// One recorded `request` / `request_workspace` call.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCall {
    /// The file for `request`; empty for `request_workspace`.
    pub file: PathBuf,
    /// The server name for `request_workspace`; `None` for `request`.
    pub server: Option<String>,
    pub method: String,
    pub params: Value,
}

/// See the module docs.
#[derive(Debug)]
pub struct MockBackend {
    boundary: PathBuf,
    encoding: PositionEncoding,
    indexing: Mutex<Option<Indexing>>,
    responses: Mutex<HashMap<String, Result<Value, LspError>>>,
    diagnostics: Mutex<Option<Result<DiagnosticsReport, LspError>>>,
    languages: Mutex<Vec<LanguageInfo>>,
    status: Mutex<Option<StatusReport>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl MockBackend {
    /// A mock rooted at `boundary`, answering in UTF-32 unless changed.
    pub fn new(boundary: impl Into<PathBuf>) -> Self {
        Self {
            boundary: boundary.into(),
            encoding: PositionEncoding::Utf32,
            indexing: Mutex::new(None),
            responses: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(None),
            languages: Mutex::new(Vec::new()),
            status: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Positions in every response are in `encoding`.
    pub fn with_encoding(mut self, encoding: PositionEncoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// Answers `method` with `result` (a `Value` for success, an error otherwise).
    pub fn respond(&self, method: &str, result: Result<Value, LspError>) {
        self.responses
            .lock()
            .expect("mock lock")
            .insert(method.to_owned(), result);
    }

    /// Every later successful response carries `indexing`.
    pub fn set_indexing(&self, indexing: Option<Indexing>) {
        *self.indexing.lock().expect("mock lock") = indexing;
    }

    /// Answers `diagnostics` with `report`.
    pub fn respond_diagnostics(&self, report: Result<DiagnosticsReport, LspError>) {
        *self.diagnostics.lock().expect("mock lock") = Some(report);
    }

    /// The languages `languages()` reports.
    pub fn set_languages(&self, languages: Vec<LanguageInfo>) {
        *self.languages.lock().expect("mock lock") = languages;
    }

    /// The report `status()` returns (a minimal empty one when never set).
    pub fn set_status(&self, status: StatusReport) {
        *self.status.lock().expect("mock lock") = Some(status);
    }

    /// Every `request` / `request_workspace` call so far, in order.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("mock lock").clone()
    }

    fn served(&self, reply: Result<Value, LspError>) -> Result<Served, LspError> {
        reply.map(|value| Served {
            value,
            encoding: self.encoding,
            server: "mock".to_owned(),
            root: self.boundary.clone(),
            indexing: self.indexing.lock().expect("mock lock").clone(),
        })
    }

    fn scripted(&self, method: &str) -> Result<Value, LspError> {
        self.responses
            .lock()
            .expect("mock lock")
            .get(method)
            .cloned()
            .unwrap_or(Ok(Value::Null))
    }
}

#[async_trait]
impl LspBackend for MockBackend {
    async fn request(
        &self,
        file: &Path,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Served, LspError> {
        self.calls.lock().expect("mock lock").push(RecordedCall {
            file: file.to_owned(),
            server: None,
            method: method.to_owned(),
            params,
        });
        if cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        self.served(self.scripted(method))
    }

    async fn request_workspace(
        &self,
        server: &str,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Served, LspError> {
        self.calls.lock().expect("mock lock").push(RecordedCall {
            file: PathBuf::new(),
            server: Some(server.to_owned()),
            method: method.to_owned(),
            params,
        });
        if cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        self.served(self.scripted(method))
    }

    async fn diagnostics(
        &self,
        _file: &Path,
        cancel: &CancellationToken,
    ) -> Result<DiagnosticsReport, LspError> {
        if cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        self.diagnostics
            .lock()
            .expect("mock lock")
            .clone()
            .unwrap_or_else(|| {
                Ok(DiagnosticsReport {
                    items: Vec::new(),
                    encoding: self.encoding,
                    received_for_version: false,
                    timed_out: true,
                    server: "mock".to_owned(),
                })
            })
    }

    fn resolve_path(&self, file_path: &str) -> Result<PathBuf, LspError> {
        let joined = if Path::new(file_path).is_absolute() {
            PathBuf::from(file_path)
        } else {
            self.boundary.join(file_path)
        };
        // Lexical normalization only: the mock must not touch the disk.
        let mut normalized = PathBuf::new();
        for part in joined.components() {
            match part {
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::CurDir => {}
                other => normalized.push(other),
            }
        }
        if normalized.starts_with(&self.boundary) {
            Ok(normalized)
        } else {
            Err(LspError::OutsideWorkspace {
                path: file_path.to_owned(),
                boundary: self.boundary.display().to_string(),
            })
        }
    }

    fn boundary(&self) -> PathBuf {
        self.boundary.clone()
    }

    fn languages(&self) -> Vec<LanguageInfo> {
        self.languages.lock().expect("mock lock").clone()
    }

    async fn status(&self) -> StatusReport {
        self.status
            .lock()
            .expect("mock lock")
            .clone()
            .unwrap_or_else(|| StatusReport {
                daemon: DaemonInfo {
                    version: "mock".to_owned(),
                    pid: 0,
                    uptime_secs: 0,
                    rss_bytes: None,
                    clients: 0,
                    max_rss_mb: None,
                    rss_over_limit: false,
                },
                limits: Limits {
                    max_instances: 8,
                    max_rss_mb: 6144,
                    idle_shutdown_secs: 900,
                    max_open_docs: 256,
                },
                enabled_languages: Vec::new(),
                language_mode: LanguageMode::Auto,
                not_installed: Vec::new(),
                instances: Vec::new(),
            })
    }

    async fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn records_calls_and_replays_scripted_answers() {
        let mock = MockBackend::new("/ws").with_encoding(PositionEncoding::Utf16);
        mock.respond("textDocument/hover", Ok(json!({ "contents": "x" })));
        mock.respond("textDocument/definition", Err(LspError::Cancelled));
        let cancel = CancellationToken::new();
        let file = Path::new("/ws/a.rs");

        let served = mock
            .request(file, "textDocument/hover", json!({ "p": 1 }), &cancel)
            .await
            .unwrap();
        assert_eq!(served.value, json!({ "contents": "x" }));
        assert_eq!(served.encoding, PositionEncoding::Utf16);
        assert_eq!(served.root, PathBuf::from("/ws"));
        assert_eq!(served.indexing, None);

        let unscripted = mock
            .request(file, "textDocument/references", json!(null), &cancel)
            .await
            .unwrap();
        assert_eq!(unscripted.value, Value::Null);

        assert_eq!(
            mock.request(file, "textDocument/definition", json!(null), &cancel)
                .await,
            Err(LspError::Cancelled)
        );
        let methods: Vec<String> = mock.calls().into_iter().map(|c| c.method).collect();
        assert_eq!(
            methods,
            vec![
                "textDocument/hover",
                "textDocument/references",
                "textDocument/definition"
            ]
        );
    }

    #[tokio::test]
    async fn workspace_requests_are_recorded_with_the_server_name() {
        let mock = MockBackend::new("/ws");
        mock.respond("workspace/symbol", Ok(json!([])));
        let served = mock
            .request_workspace(
                "rust-analyzer",
                "workspace/symbol",
                json!({"query": "x"}),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(served.value, json!([]));
        let calls = mock.calls();
        assert_eq!(calls[0].server.as_deref(), Some("rust-analyzer"));
        assert!(calls[0].file.as_os_str().is_empty());
    }

    #[tokio::test]
    async fn indexing_marker_is_attached_to_every_success() {
        let mock = MockBackend::new("/ws");
        mock.set_indexing(Some(Indexing {
            message: "Roots Scanned".into(),
            percent: Some(40),
        }));
        let served = mock
            .request(
                Path::new("/ws/a.rs"),
                "m",
                Value::Null,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(served.indexing.unwrap().percent, Some(40));
    }

    #[tokio::test]
    async fn cancelled_token_short_circuits() {
        let mock = MockBackend::new("/ws");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let file = Path::new("/ws/a.rs");
        assert_eq!(
            mock.request(file, "m", Value::Null, &cancel).await,
            Err(LspError::Cancelled)
        );
        assert_eq!(
            mock.request_workspace("s", "m", Value::Null, &cancel).await,
            Err(LspError::Cancelled)
        );
        assert_eq!(
            mock.diagnostics(file, &cancel).await,
            Err(LspError::Cancelled)
        );
    }

    #[tokio::test]
    async fn diagnostics_default_is_unknown_not_clean() {
        let mock = MockBackend::new("/ws");
        let report = mock
            .diagnostics(Path::new("/ws/a.rs"), &CancellationToken::new())
            .await
            .unwrap();
        assert!(!report.received_for_version);
        assert!(report.items.is_empty());

        let scripted = DiagnosticsReport {
            items: Vec::new(),
            encoding: PositionEncoding::Utf32,
            received_for_version: true,
            timed_out: false,
            server: "mock".to_owned(),
        };
        mock.respond_diagnostics(Ok(scripted.clone()));
        let report = mock
            .diagnostics(Path::new("/ws/a.rs"), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(report, scripted);
        mock.shutdown().await;
    }

    #[test]
    fn resolve_path_joins_relative_and_refuses_escapes() {
        let mock = MockBackend::new("/ws");
        assert_eq!(
            mock.resolve_path("src/./a.rs").unwrap(),
            PathBuf::from("/ws/src/a.rs")
        );
        assert_eq!(
            mock.resolve_path("/ws/b.rs").unwrap(),
            PathBuf::from("/ws/b.rs")
        );
        assert!(matches!(
            mock.resolve_path("../../etc/passwd"),
            Err(LspError::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            mock.resolve_path("/etc/passwd"),
            Err(LspError::OutsideWorkspace { .. })
        ));
        assert_eq!(mock.boundary(), PathBuf::from("/ws"));
    }

    #[tokio::test]
    async fn languages_and_status_are_scriptable() {
        let mock = MockBackend::new("/ws");
        assert!(mock.languages().is_empty());
        let info = LanguageInfo {
            name: "rust".into(),
            server: "rust-analyzer".into(),
            extensions: vec!["rs".into()],
            root_markers: vec!["Cargo.toml".into()],
            installed: true,
            detected: true,
            enabled: true,
        };
        mock.set_languages(vec![info.clone()]);
        assert_eq!(mock.languages(), vec![info]);

        let default = mock.status().await;
        assert!(default.instances.is_empty());
        let mut custom = default.clone();
        custom.daemon.pid = 42;
        mock.set_status(custom.clone());
        assert_eq!(mock.status().await, custom);
    }
}
