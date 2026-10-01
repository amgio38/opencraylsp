//! Rendering [`LspError`] as the machine-readable marker the model reads.
//!
//! The first line of a failed [`ToolOutput`] is fixed: `[code] one English
//! sentence`. The code is what a harness or an agent branches on; the sentence
//! says what happened and what to do next. Every variant maps to exactly one
//! code and the mapping is a table, so adding a variant without a code is a
//! compile error rather than an unmarked error shipped to a model.

use opencraylsp_core::backend::LspError;
use opencraylsp_proto::ToolOutput;

/// JSON-RPC `MethodNotFound`: the server does not implement the method.
const METHOD_NOT_FOUND: i64 = -32601;

/// The `[code]` an [`LspError`] renders as.
///
/// Exhaustive over [`LspError`] on purpose: a new variant must be given a code
/// here before the crate compiles.
pub fn error_code(error: &LspError) -> &'static str {
    match error {
        LspError::NoServerConfigured { .. } | LspError::UnknownServer { .. } => "no_server",
        LspError::ServerNotInstalled { .. } => "server_not_installed",
        LspError::ServerFailed { .. } => "server_failed",
        LspError::Timeout { .. } => "timeout",
        LspError::Cancelled => "cancelled",
        LspError::OutsideWorkspace { .. } => "outside_workspace",
        // A method the server does not implement is its own sentence to the
        // model: another server may answer, retrying with the same one cannot.
        LspError::Rpc {
            code: METHOD_NOT_FOUND,
            ..
        } => "unsupported",
        LspError::Rpc { .. } => "rpc_error",
        LspError::Indexing { .. } => "indexing",
        LspError::Capacity { .. } => "capacity",
        LspError::MemoryRestart { .. } => "memory_restart",
        LspError::LanguageDisabled { .. } => "language_disabled",
        // Not `not_found` and not `ambiguous`, though it is about both: those
        // two are the *lenient* codes, meaning a lookup ran and came back
        // empty or unresolved (`is_error: false`), and the tools crate builds
        // them with a fixed shape. Here no lookup happened at all — the request
        // could not be routed to a project — so it is an error with its own
        // code, and the candidate roots it lists are what turns it back into a
        // question the model can answer with a `path`.
        LspError::NoProject { .. } => "no_project",
        LspError::Io(_) => "io_error",
    }
}

/// The sentence shown after the code.
///
/// Most variants already carry a message written for the model, so they are
/// passed through verbatim; the two that need the connection's own state (the
/// enabled language set, the missing method name) are spelled out here.
fn error_message(error: &LspError) -> String {
    match error {
        LspError::LanguageDisabled { language, enabled } => {
            if enabled.is_empty() {
                format!(
                    "`{language}` is not enabled for this connection (none; no project markers \
                     found — pass --languages including `{language}`, or use --languages all)."
                )
            } else {
                format!(
                    "`{language}` is not enabled for this connection (enabled: {}). Restart the \
                     harness's opencraylsp-mcp with --languages including `{language}`, or use \
                     --languages all.",
                    enabled.join(", ")
                )
            }
        }
        LspError::Rpc {
            server,
            code: METHOD_NOT_FOUND,
            message,
        } => {
            format!("LSP server `{server}` does not support this request: {message}")
        }
        _ => error.to_string(),
    }
}

/// A failed [`ToolOutput`] whose first line is `[code] message`.
pub fn render_error(error: &LspError) -> ToolOutput {
    ToolOutput::error(format!("[{}] {}", error_code(error), error_message(error)))
}

/// The `[internal_error]` marker: a bug inside opencraylspd or this crate.
///
/// No [`LspError`] carries this — it is not a language server or workspace
/// problem but an invariant of our own code — so it is built directly. The
/// daemon calls this when a tool handler panics, which is the one place a
/// request would otherwise never be answered at all: an `internal_error`
/// result at least tells the model (and the operator) that the failure is our
/// bug and not something about the code they asked about.
pub fn internal_error_output(message: &str) -> ToolOutput {
    ToolOutput::error(format!("[internal_error] {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use opencraylsp_core::backend::PositionEncoding;
    use serde_json::json;

    use crate::format::LineIndex;
    use crate::operations::Site;
    use crate::resolve::Candidate;
    use crate::resolve_render::{self, candidate_view};
    use crate::{rename, tools};

    /// One row per [`LspError`] variant (two rows for `Rpc`, one per branch of
    /// the `-32601` split), with the code and a phrase the message must contain.
    ///
    /// Keep this list exhaustive by hand: `error_code` is an exhaustive match,
    /// so a *new variant* fails to compile there — but nothing forces it to
    /// appear here too.
    ///
    /// `not_renamable` is deliberately absent: it is produced by the rename
    /// preview (`ToolOutput` built directly), never through an [`LspError`].
    fn table() -> Vec<(LspError, &'static str, &'static str)> {
        vec![
            (
                LspError::NoServerConfigured {
                    extension: "rs".into(),
                },
                "no_server",
                "no LSP server is configured",
            ),
            // A second path to `no_server`: the request named a server that is
            // not in the config, which is a lookup mistake and not a missing
            // language (P2-12 of the CR report).
            (
                LspError::UnknownServer {
                    server: "rust-analyzer".into(),
                },
                "no_server",
                "no LSP server named",
            ),
            (
                LspError::ServerNotInstalled {
                    server: "rust-analyzer".into(),
                    command: "rust-analyzer".into(),
                },
                "server_not_installed",
                "could not be started",
            ),
            (
                LspError::ServerFailed {
                    server: "rust-analyzer".into(),
                    restarts: 3,
                    last_error: "segfault".into(),
                },
                "server_failed",
                "segfault",
            ),
            (
                LspError::Timeout {
                    server: "rust-analyzer".into(),
                    method: "textDocument/hover".into(),
                    ms: 30000,
                },
                "timeout",
                "did not answer",
            ),
            (LspError::Cancelled, "cancelled", "cancelled"),
            (
                LspError::OutsideWorkspace {
                    path: "../x".into(),
                    boundary: "/ws".into(),
                },
                "outside_workspace",
                "outside the workspace boundary",
            ),
            (
                LspError::Rpc {
                    server: "rust-analyzer".into(),
                    code: -32601,
                    message: "method not found".into(),
                },
                "unsupported",
                "does not support",
            ),
            (
                LspError::Rpc {
                    server: "rust-analyzer".into(),
                    code: -32602,
                    message: "invalid params".into(),
                },
                "rpc_error",
                "invalid params",
            ),
            (
                LspError::Indexing {
                    server: "rust-analyzer".into(),
                    message: "roots scanned".into(),
                    percent: Some(40),
                },
                "indexing",
                "still indexing",
            ),
            (
                LspError::Capacity { limit: 8 },
                "capacity",
                "8 language-server slots",
            ),
            (
                LspError::MemoryRestart {
                    server: "rust-analyzer".into(),
                },
                "memory_restart",
                "memory limit",
            ),
            (
                LspError::LanguageDisabled {
                    language: "go".into(),
                    enabled: vec!["rust".into(), "php".into()],
                },
                "language_disabled",
                "enabled: rust, php",
            ),
            (
                LspError::LanguageDisabled {
                    language: "go".into(),
                    enabled: Vec::new(),
                },
                "language_disabled",
                "no project markers found",
            ),
            (
                LspError::NoProject {
                    language: "rust".into(),
                    boundary: "/ws".into(),
                    candidates: String::new(),
                },
                "no_project",
                "no rust project found",
            ),
            (
                LspError::NoProject {
                    language: "rust".into(),
                    boundary: "/ws".into(),
                    candidates: "\nthese projects were found (pass `path`):\n  {\"path\": \"one\"}"
                        .to_owned(),
                },
                "no_project",
                "these projects were found",
            ),
            (
                LspError::Io("broken pipe".into()),
                "io_error",
                "broken pipe",
            ),
        ]
    }

    #[test]
    fn every_variant_renders_a_marked_first_line() {
        for (error, code, needle) in table() {
            let out = render_error(&error);
            assert!(out.is_error, "{error:?} must set is_error");
            let first = out.text.lines().next().unwrap_or_default();
            assert_eq!(
                first.split(' ').next(),
                Some(format!("[{code}]").as_str()),
                "{error:?} -> {first}"
            );
            assert!(
                first.starts_with(&format!("[{code}] ")),
                "{error:?} -> {first}"
            );
            // The code alone is not an answer: the sentence after it must say
            // something.
            assert!(
                first.trim_end().len() > code.len() + 3,
                "{error:?} -> {first}"
            );
            assert!(out.text.contains(needle), "{error:?} -> {}", out.text);
        }
    }

    #[test]
    fn the_lsp_error_surface_renders_exactly_these_codes() {
        let codes: std::collections::BTreeSet<&str> =
            table().iter().map(|(_, code, _)| *code).collect();
        let expected: std::collections::BTreeSet<&str> = [
            "no_server",
            "server_not_installed",
            "server_failed",
            "timeout",
            "cancelled",
            "outside_workspace",
            "unsupported",
            "rpc_error",
            "indexing",
            "capacity",
            "memory_restart",
            "language_disabled",
            "no_project",
            "io_error",
        ]
        .into_iter()
        .collect();
        // One row per distinct code, and `UnknownServer` deliberately shares
        // `no_server` with `NoServerConfigured`.
        assert_eq!(table().len(), 17);
        assert_eq!(codes, expected);
    }

    // ---- every marker, produced or explicitly exempt --------------------- //

    /// Every code the tool layer can emit, with the `is_error` value it carries.
    ///
    /// Kept here, spelled out, so the check below is against the published
    /// contract and not against whatever this crate happens to do.
    const MARKERS: [(&str, bool); 22] = [
        ("indexing", true),
        ("ambiguous", false),
        ("not_found", false),
        ("no_server", true),
        ("server_not_installed", true),
        ("server_failed", true),
        ("timeout", true),
        ("outside_workspace", true),
        ("capacity", true),
        ("memory_restart", true),
        ("daemon_unavailable", true),
        ("invalid_args", true),
        ("language_disabled", true),
        ("no_project", true),
        ("cancelled", true),
        ("unsupported", true),
        ("not_renamable", true),
        ("not_implemented", true),
        ("rpc_error", true),
        ("io_error", true),
        ("invalid_response", true),
        ("internal_error", true),
    ];

    /// Codes the contract defines that this crate cannot produce, and why. Listing
    /// them is the point: an unproduced code must be a decision, not an
    /// oversight, and the check below refuses a code that is neither produced
    /// here nor exempted.
    const EXEMPT: [(&str, &str); 2] = [
        (
            "daemon_unavailable",
            "only opencraylsp-client knows the daemon is unreachable; it builds the message ",
        ),
        (
            "not_implemented",
            "development-only by design: no shipping code path may return it",
        ),
    ];

    /// One sample of every code this crate produces, built by the same function
    /// production uses — a table of literals would prove nothing about the
    /// code that actually assembles the answer.
    fn samples() -> std::collections::BTreeMap<&'static str, ToolOutput> {
        let mut samples = std::collections::BTreeMap::new();
        for (error, _, _) in table() {
            samples.insert(error_code(&error), render_error(&error));
        }
        let boundary = Path::new("/ws");
        let lines = LineIndex::lazy(boundary);
        lines.insert("/ws/a.rs", "fn f() {}\n");
        let view = candidate_view(boundary, &lines);
        let candidates = vec![
            candidate("new", 6, Some("Foo")),
            candidate("new", 6, Some("Bar")),
        ];
        samples.insert(
            "ambiguous",
            resolve_render::output(
                resolve_render::render_ambiguous("new", &candidates, &view),
                &[],
            ),
        );
        samples.insert("not_found", tools::not_found("no symbol named `handle`"));
        samples.insert("invalid_args", tools::invalid("`line` is required"));
        samples.insert(
            "invalid_response",
            tools::shape_error("textDocument/hover", "the answer is not an object"),
        );
        samples.insert(
            "not_renamable",
            rename::not_renamable(
                boundary,
                &candidate("new", 6, None),
                PositionEncoding::Utf16,
                &json!({"message": "this position cannot be renamed"}),
            ),
        );
        samples.insert(
            "internal_error",
            internal_error_output("a tool handler panicked; please report it"),
        );
        samples
    }

    fn candidate(name: &str, kind: u32, container: Option<&str>) -> Candidate {
        Candidate {
            site: Site {
                path: Some(PathBuf::from("/ws/a.rs")),
                uri: "file:///ws/a.rs".to_owned(),
                line: Some(0),
                character: Some(3),
            },
            name: name.to_owned(),
            kind,
            container: container.map(str::to_owned),
            server: "rust-analyzer".to_owned(),
            outside_workspace: false,
        }
    }

    #[test]
    fn every_error_marker_is_produced_here_or_explicitly_exempt() {
        let mut accounted: std::collections::BTreeSet<&str> = samples().keys().copied().collect();
        for (code, why) in EXEMPT {
            assert!(
                !why.is_empty(),
                "`{code}` is exempt but says nothing about why"
            );
            assert!(
                accounted.insert(code),
                "`{code}` is both produced here and listed as exempt"
            );
        }
        let markers: std::collections::BTreeSet<&str> =
            MARKERS.iter().map(|(code, _)| *code).collect();
        let missing: Vec<&&str> = markers.difference(&accounted).collect();
        let extra: Vec<&&str> = accounted.difference(&markers).collect();
        assert!(
            missing.is_empty(),
            "codes in the contract with no producer and no exemption: {missing:?}"
        );
        assert!(
            extra.is_empty(),
            "codes produced that the contract does not define: {extra:?}"
        );
        assert_eq!(markers.len(), 22, "the marker table itself changed");
        // The two non-error codes are the only `is_error = false` answers in the
        // contract; a typo here would flip a failure into a success.
        let lenient: Vec<&str> = MARKERS
            .iter()
            .filter(|(_, is_error)| !*is_error)
            .map(|(code, _)| *code)
            .collect();
        assert_eq!(lenient, ["ambiguous", "not_found"]);
    }

    #[test]
    fn every_produced_code_carries_its_marker_and_is_error() {
        let expected: std::collections::BTreeMap<&str, bool> = MARKERS.into_iter().collect();
        let samples = samples();
        assert!(samples.len() >= 19, "only {} codes sampled", samples.len());
        for (code, output) in samples {
            let want = expected[code];
            assert_eq!(output.is_error, want, "`[{code}]` has the wrong is_error");
            let first = output.text.lines().next().unwrap_or_default();
            assert!(
                first.starts_with(&format!("[{code}] ")),
                "`{code}` -> {first}"
            );
            // The code alone is not an answer: the sentence after it must say
            // something the model can act on.
            assert!(
                first.trim_end().len() > code.len() + 3,
                "`{code}` has no sentence: {first}"
            );
        }
    }

    #[test]
    fn a_language_disabled_message_names_the_way_out() {
        let with_enabled = render_error(&LspError::LanguageDisabled {
            language: "go".into(),
            enabled: vec!["rust".into()],
        });
        assert!(
            with_enabled.text.contains("--languages"),
            "{}",
            with_enabled.text
        );
        assert!(with_enabled.text.contains("`go`"), "{}", with_enabled.text);

        let bare = render_error(&LspError::LanguageDisabled {
            language: "go".into(),
            enabled: Vec::new(),
        });
        assert!(
            bare.text.contains("no project markers found"),
            "{}",
            bare.text
        );
    }

    #[test]
    fn only_method_not_found_becomes_unsupported() {
        let not_found = LspError::Rpc {
            server: "s".into(),
            code: -32601,
            message: "x".into(),
        };
        assert_eq!(error_code(&not_found), "unsupported");
        let other = LspError::Rpc {
            server: "s".into(),
            code: -32000,
            message: "x".into(),
        };
        assert_eq!(error_code(&other), "rpc_error");
    }
}
