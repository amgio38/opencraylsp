//! Wire types and host contracts shared by `opencraylspd`, `opencraylsp-client`, `opencraylsp-mcp`
//! and the tool layer. Nothing here performs I/O or holds policy: these are the
//! shapes the processes agree on.
//!
//! 🔴 This crate is a contract. Do not edit it inside a feature branch: raise
//! the change with the project owner first.

pub mod paths;
pub mod rpc;
pub mod trust;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Version of the daemon protocol spoken over the unix socket.
pub const PROTOCOL_VERSION: u32 = 1;

/// MCP-style annotations attached to a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    /// Every tool this project ships is read-only: none of them may write to
    /// the workspace.
    #[serde(rename = "readOnlyHint")]
    pub read_only_hint: bool,
}

impl Default for ToolAnnotations {
    fn default() -> Self {
        Self {
            read_only_hint: true,
        }
    }
}

/// A tool as advertised to a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema (`type: object`) of the tool's arguments.
    pub input_schema: Value,
    #[serde(default)]
    pub annotations: ToolAnnotations,
}

/// The result of running a tool. A failing tool is *not* a protocol error:
/// it is a `ToolOutput` with `is_error = true` whose first line is
/// `[code] message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful output.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// A failed output. `text` should already start with `[code] `.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// How the connection's language set was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguageMode {
    /// Not declared: detected from project markers in the workspace.
    Auto,
    /// Declared explicitly, e.g. `--languages rust,go`.
    Declared,
    /// `--languages all`.
    All,
}

/// Lifecycle state of one language-server instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    Starting,
    Indexing,
    Ready,
    Restarting,
    Failed,
    Stopped,
}

/// Indexing progress reported by a server through `$/progress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Indexing {
    pub message: String,
    pub percent: Option<u32>,
}

/// One running (or recently stopped) language-server instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub server: String,
    pub root: String,
    pub state: InstanceState,
    pub pid: Option<u32>,
    /// Resident memory of the whole process tree; `None` where unsupported.
    pub rss_bytes: Option<u64>,
    pub idle_secs: u64,
    pub restarts: u32,
    pub memory_restarts: u32,
    pub open_docs: u32,
    pub indexing: Option<Indexing>,
}

/// Daemon-level facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    /// Resident memory of the daemon process alone; language servers are not
    /// included, since they are reported on their own rows and have their own
    /// ceiling. `None` where unsupported.
    pub rss_bytes: Option<u64>,
    /// Number of currently connected clients.
    pub clients: u32,
    /// The daemon's own resident-memory ceiling, in MiB.
    ///
    /// `None` on a daemon too old to report it, which is what keeps a newer
    /// client able to read an older daemon's answer — and, with `rss_bytes`,
    /// what lets `rss` be printed as `12/512 MB` rather than a bare figure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rss_mb: Option<u64>,
    /// The daemon is over its own ceiling and has refused to restart, because it
    /// has already exited over that ceiling too often in the last hour.
    ///
    /// A daemon in this state is serving normally; the flag says that the
    /// ceiling is not being enforced, which an operator needs to see rather than
    /// infer from a memory figure that keeps climbing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub rss_over_limit: bool,
}

/// Whether a `rss_over_limit` of `false` is worth writing out.
///
/// Omitting it keeps the common answer byte-for-byte what it was before the
/// field existed, so a client that diffs two status answers does not see a change
/// on every poll. `false` carries no information; `true` is the whole message.
fn is_false(value: &bool) -> bool {
    !*value
}

/// The resource limits the daemon is enforcing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_instances: u32,
    pub max_rss_mb: u64,
    pub idle_shutdown_secs: u64,
    pub max_open_docs: u32,
}

/// Snapshot returned by the `status` method and rendered by `lsp_status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    pub daemon: DaemonInfo,
    pub limits: Limits,
    /// Canonical names of the languages enabled for the asking connection.
    pub enabled_languages: Vec<String>,
    pub language_mode: LanguageMode,
    /// Languages whose server command was not found on `PATH`.
    pub not_installed: Vec<String>,
    pub instances: Vec<InstanceInfo>,
}

/// `hello` request parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloParams {
    pub protocol: u32,
    pub client: ClientInfo,
    /// Absolute path of the workspace boundary for this connection.
    pub workspace: String,
    /// Raw user input (`rust`, `ts`, `all`, ...); the daemon normalizes it.
    /// `None` means auto-detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<String>>,
}

/// Who is connecting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// `hello` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub daemon_version: String,
    pub pid: u32,
    /// The languages actually enabled for this connection.
    pub languages: Vec<String>,
    pub language_mode: LanguageMode,
}

/// `tools/list` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListResult {
    pub tools: Vec<ToolDef>,
}

/// `tools/call` request parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// The backend of an MCP server could not run the tool at all (as opposed to
/// the tool running and failing, which is a [`ToolOutput`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The daemon cannot be reached or spoke nonsense; the message is shown to
    /// the model and starts with `[daemon_unavailable]`.
    #[error("{0}")]
    Unavailable(String),
    /// The caller cancelled the request.
    #[error("the request was cancelled")]
    Cancelled,
    /// The tool name is not one this host advertises.
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
}

/// What an MCP server needs from whatever actually runs the tools.
///
/// Implemented by `DaemonHost` (`opencraylsp-client`) and `EmbeddedHost` (`opencraylsp-mcp`).
#[async_trait]
pub trait ToolHost: Send + Sync {
    /// The tools this host offers.
    async fn list_tools(&self) -> Result<Vec<ToolDef>, HostError>;

    /// Runs one tool. Honors `cancel` by returning [`HostError::Cancelled`].
    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, HostError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_output_constructors_set_the_flag() {
        assert!(!ToolOutput::ok("x").is_error);
        assert!(ToolOutput::error("[timeout] x").is_error);
    }

    #[test]
    fn tool_def_serializes_read_only_hint_camel_case() {
        let def = ToolDef {
            name: "lsp_status".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
            annotations: ToolAnnotations::default(),
        };
        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(v["annotations"]["readOnlyHint"], json!(true));
        assert_eq!(serde_json::from_value::<ToolDef>(v).unwrap(), def);
    }

    #[test]
    fn tool_def_without_annotations_defaults_to_read_only() {
        let def: ToolDef = serde_json::from_value(
            json!({"name": "n", "description": "d", "input_schema": {"type": "object"}}),
        )
        .unwrap();
        assert!(def.annotations.read_only_hint);
    }

    /// A client that predates the ceiling must still be able to read a newer
    /// daemon's answer. This is the whole reason the two new fields are
    /// optional and defaulted: a `DaemonInfo` written by an older build simply
    /// has no `max_rss_mb` and no `rss_over_limit`, and refusing that would mean
    /// a client could not talk to a daemon across a version boundary — the one
    /// thing a shared daemon has to survive.
    #[test]
    fn a_daemon_answer_from_an_older_build_still_reads() {
        let older = json!({
            "version": "0.1.0",
            "pid": 7,
            "uptime_secs": 3,
            "rss_bytes": 12582912,
            "clients": 2
        });
        let info: DaemonInfo =
            serde_json::from_value(older).expect("an older answer must still decode");
        assert_eq!(
            info.max_rss_mb, None,
            "absent means 'this daemon has no ceiling'"
        );
        assert!(
            !info.rss_over_limit,
            "absent must not read as over the limit"
        );
        assert_eq!(info.pid, 7, "and the fields that were there still are");
    }

    /// `rss_over_limit: false` is not written: it is the absence of news, and a
    /// client polling every few seconds should not see a changed answer because
    /// a field appeared.
    #[test]
    fn a_healthy_daemon_serializes_exactly_as_before() {
        let info = DaemonInfo {
            version: "0.1.0".into(),
            pid: 1,
            uptime_secs: 2,
            rss_bytes: Some(1024),
            clients: 1,
            max_rss_mb: Some(512),
            rss_over_limit: false,
        };
        let v = serde_json::to_value(&info).unwrap();
        assert!(v.get("rss_over_limit").is_none(), "false is not news: {v}");
        assert_eq!(v["max_rss_mb"], json!(512));
    }

    /// And when it *is* over the limit, the flag is present and survives a round
    /// trip. Omitting it would lose the one thing an operator needs to see.
    #[test]
    fn the_over_limit_flag_is_written_when_it_is_set() {
        let info = DaemonInfo {
            version: "0.1.0".into(),
            pid: 1,
            uptime_secs: 2,
            rss_bytes: Some(900 * 1024 * 1024),
            clients: 1,
            max_rss_mb: Some(512),
            rss_over_limit: true,
        };
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["rss_over_limit"], json!(true));
        let back: DaemonInfo = serde_json::from_value(v).unwrap();
        assert!(
            back.rss_over_limit,
            "the refusal must survive the round trip"
        );
    }

    #[test]
    fn hello_params_omit_absent_languages() {
        let p = HelloParams {
            protocol: PROTOCOL_VERSION,
            client: ClientInfo {
                name: "t".into(),
                version: "0".into(),
            },
            workspace: "/ws".into(),
            languages: None,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("languages").is_none());
        let back: HelloParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn language_mode_and_state_use_lowercase_names() {
        assert_eq!(
            serde_json::to_value(LanguageMode::Auto).unwrap(),
            json!("auto")
        );
        assert_eq!(
            serde_json::to_value(InstanceState::Indexing).unwrap(),
            json!("indexing")
        );
    }

    #[test]
    fn status_report_round_trips() {
        let report = StatusReport {
            daemon: DaemonInfo {
                version: "0.1.0".into(),
                pid: 1,
                uptime_secs: 2,
                rss_bytes: None,
                clients: 3,
                max_rss_mb: Some(512),
                rss_over_limit: false,
            },
            limits: Limits {
                max_instances: 8,
                max_rss_mb: 6144,
                idle_shutdown_secs: 900,
                max_open_docs: 256,
            },
            enabled_languages: vec!["rust".into()],
            language_mode: LanguageMode::Auto,
            not_installed: vec![],
            instances: vec![InstanceInfo {
                server: "rust-analyzer".into(),
                root: "/ws".into(),
                state: InstanceState::Ready,
                pid: Some(9),
                rss_bytes: Some(10),
                idle_secs: 0,
                restarts: 0,
                memory_restarts: 0,
                open_docs: 1,
                indexing: Some(Indexing {
                    message: "x".into(),
                    percent: Some(40),
                }),
            }],
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(serde_json::from_value::<StatusReport>(v).unwrap(), report);
    }

    #[test]
    fn host_error_messages_are_shown_verbatim() {
        assert_eq!(
            HostError::Unavailable("[daemon_unavailable] x".into()).to_string(),
            "[daemon_unavailable] x"
        );
        assert!(
            HostError::UnknownTool("t".into())
                .to_string()
                .contains("`t`")
        );
    }
}
