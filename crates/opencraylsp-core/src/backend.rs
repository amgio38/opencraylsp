//! The contract between the tool layer (`opencraylsp-tools`) and the language-server
//! layer .
//!
//! Everything a tool needs from a server goes through [`LspBackend`]; nothing
//! else crosses. That keeps the tool layer testable without a real language
//! server and lets the daemon and the embedded mode share the same tools.
//!
//! 🔴 Changing this file changes the contract every crate codes against. Do
//! not edit it inside a feature branch — raise it with the project owner first.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lsp_types::Diagnostic;
use opencraylsp_proto::{Indexing, StatusReport};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// How a server counts the `character` field of a position.
///
/// LSP defaults to UTF-16 code units, which disagrees with what an editor (and
/// a model) calls a "column" as soon as a line holds CJK text or emoji. The
/// tool layer converts to and from 1-based Unicode scalar columns using the
/// line's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    /// Maps the `positionEncoding` a server returned in its capabilities.
    ///
    /// A missing or unknown value means UTF-16: that is the protocol default,
    /// and a server that did not negotiate is speaking it.
    pub fn from_negotiated(kind: Option<&str>) -> Self {
        match kind {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }
}

/// A successful response, with what the tool layer needs to interpret it.
#[derive(Debug, Clone, PartialEq)]
pub struct Served {
    /// The raw `result` of the LSP response (may be `null`).
    pub value: Value,
    /// The encoding every position in `value` is expressed in.
    pub encoding: PositionEncoding,
    /// The configured server name.
    pub server: String,
    /// The workspace root this server instance was started for.
    pub root: PathBuf,
    /// `Some` while the server reports work-done progress (indexing). A
    /// non-empty `value` may still be incomplete then; an empty one is
    /// reported as [`LspError::Indexing`] instead of being returned at all.
    pub indexing: Option<Indexing>,
}

/// The diagnostics currently known for one file .
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticsReport {
    pub items: Vec<Diagnostic>,
    pub encoding: PositionEncoding,
    /// True only when a `publishDiagnostics` for the document version that was
    /// just synced has arrived. When false, an empty `items` means "not known
    /// yet", never "no errors" — the tool must not report the file as clean.
    pub received_for_version: bool,
    /// True when the wait hit `diagnostics_timeout_ms` before the server went
    /// quiet; the server may still be analysing.
    pub timed_out: bool,
    pub server: String,
}

/// A language usable in one connection's workspace .
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageInfo {
    /// Canonical language: rust | go | php | typescript | javascript | python.
    pub name: String,
    /// Configured server name, e.g. `rust-analyzer`.
    pub server: String,
    /// File extensions without the dot.
    pub extensions: Vec<String>,
    pub root_markers: Vec<String>,
    /// The server command was found on `PATH`.
    pub installed: bool,
    /// A root marker exists at or below the workspace (depth <= 2).
    pub detected: bool,
    /// In this connection's enabled set. `LspBackend::languages` only returns
    /// enabled languages, so this is `true` for everything it yields.
    pub enabled: bool,
}

/// Why a request did not produce a result.
///
/// Each variant's message is shown to the model as-is, so it states what went
/// wrong and what to do next rather than a bare code.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LspError {
    #[error(
        "no LSP server is configured for .{extension} files; add a [[server]] entry whose `extensions` covers \"{extension}\" to the opencraylspd config"
    )]
    NoServerConfigured { extension: String },

    #[error(
        "no LSP server named `{server}` is configured; check the server name in the opencraylspd config (`lsp_status` lists the configured servers)"
    )]
    UnknownServer { server: String },

    #[error(
        "LSP server `{server}` could not be started: command `{command}` was not found; install it (run `opencraylspd doctor` for hints) or fix `command` in the opencraylspd config"
    )]
    ServerNotInstalled { server: String, command: String },

    #[error(
        "LSP server `{server}` failed and was restarted {restarts} time(s), which is the limit; it will not be started again until opencraylspd restarts. Last error: {last_error}"
    )]
    ServerFailed {
        server: String,
        restarts: u32,
        last_error: String,
    },

    #[error("LSP server `{server}` did not answer `{method}` within {ms} ms")]
    Timeout {
        server: String,
        method: String,
        ms: u64,
    },

    #[error("the LSP request was cancelled")]
    Cancelled,

    #[error(
        "`{path}` is outside the workspace boundary `{boundary}`; only files inside it (or inside configured allowed_roots) can be opened"
    )]
    OutsideWorkspace { path: String, boundary: String },

    #[error("LSP server `{server}` returned error {code}: {message}")]
    Rpc {
        server: String,
        code: i64,
        message: String,
    },

    /// The server is still indexing and the answer would be empty or
    /// untrustworthy. Retry in a few seconds; do not treat as "not found".
    #[error("LSP server `{server}` is still indexing: {message}")]
    Indexing {
        server: String,
        message: String,
        percent: Option<u32>,
    },

    /// Every instance slot is busy and none could be reclaimed in time.
    #[error("all {limit} language-server slots are busy; retry shortly")]
    Capacity { limit: u32 },

    /// The instance was just restarted because it exceeded its memory limit.
    #[error("LSP server `{server}` was just restarted after exceeding its memory limit; retry")]
    MemoryRestart { server: String },

    /// The file's language exists but this connection did not enable it.
    #[error("language `{language}` is not enabled for this connection")]
    LanguageDisabled {
        language: String,
        enabled: Vec<String>,
    },

    /// A request with no file to anchor to, in a workspace that is not itself a
    /// project, has no project to send it to.
    ///
    /// `candidates` is empty when the workspace holds no project of this
    /// language at all, and lists the project roots when it holds several and
    /// nothing in the request says which one is meant. Guessing either way
    /// would be wrong: starting a server at the workspace boundary indexes
    /// nothing while still costing gigabytes, and picking one of several
    /// projects answers about a project the caller never asked about.
    #[error(
        "no {language} project found under the workspace boundary `{boundary}`; pass a `path` \
         inside the project you mean{candidates}"
    )]
    NoProject {
        language: String,
        boundary: String,
        /// Rendered suffix: empty, or `\nthese projects were found: …` with a
        /// `path` retry line per root.
        candidates: String,
    },

    #[error("LSP I/O error: {0}")]
    Io(String),
}

/// Everything the tool layer needs from the server layer.
#[async_trait]
pub trait LspBackend: Send + Sync {
    /// Sends one request about `file`.
    ///
    /// Routes by extension, lazily starts the (server, root) instance,
    /// re-syncs open documents whose content changed on disk ,
    /// `didOpen`s `file` if needed, then sends `method` with `params`.
    /// Retries `-32801 ContentModified` with 500/1000/2000 ms back-off. Honors
    /// `cancel` by abandoning the wait without killing the server.
    ///
    /// `file` must already have passed [`Self::resolve_path`].
    async fn request(
        &self,
        file: &Path,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Served, LspError>;

    /// Sends a workspace-scoped request (e.g. `workspace/symbol`) to the
    /// configured server named `server`, at the instance for this backend's
    /// boundary, without needing a file.
    async fn request_workspace(
        &self,
        server: &str,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Served, LspError>;

    /// Syncs `file`, triggers analysis, and waits for its diagnostics with the
    /// semantics described in `docs/ARCHITECTURE.md`.
    async fn diagnostics(
        &self,
        file: &Path,
        cancel: &CancellationToken,
    ) -> Result<DiagnosticsReport, LspError>;

    /// Turns the model's `path` into a canonical absolute path inside the
    /// workspace boundary. Relative paths resolve against [`Self::boundary`].
    fn resolve_path(&self, file_path: &str) -> Result<PathBuf, LspError>;

    /// The workspace boundary, used to print paths relative to it.
    fn boundary(&self) -> PathBuf;

    /// Languages enabled for this connection .
    fn languages(&self) -> Vec<LanguageInfo>;

    /// Snapshot for the `lsp_status` tool.
    async fn status(&self) -> StatusReport;

    /// Shuts down what this backend owns. A backend with its own private pool
    /// stops every server it started; a backend bound to a *shared* pool
    /// leaves the pool running for the other connections. Safe to call twice.
    async fn shutdown(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiated_encoding_defaults_to_utf16() {
        assert_eq!(
            PositionEncoding::from_negotiated(None),
            PositionEncoding::Utf16
        );
        assert_eq!(
            PositionEncoding::from_negotiated(Some("weird")),
            PositionEncoding::Utf16
        );
        assert_eq!(
            PositionEncoding::from_negotiated(Some("utf-8")),
            PositionEncoding::Utf8
        );
        assert_eq!(
            PositionEncoding::from_negotiated(Some("utf-32")),
            PositionEncoding::Utf32
        );
    }

    #[test]
    fn error_messages_tell_the_model_what_to_do() {
        let e = LspError::ServerNotInstalled {
            server: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
        };
        assert!(e.to_string().contains("opencraylspd doctor"));
        let e = LspError::Indexing {
            server: "s".into(),
            message: "roots scanned".into(),
            percent: Some(40),
        };
        assert!(e.to_string().contains("still indexing"));
        assert!(
            LspError::Capacity { limit: 8 }
                .to_string()
                .contains("8 language-server slots")
        );
    }
}
