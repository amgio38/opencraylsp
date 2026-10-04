//! The binary as a harness actually runs it: stdin in, stdout out.
//!
//! Covers the process wiring that the in-process transcript tests cannot reach
//! (argument handling, stdio handles, tracing to stderr, exit codes). Only
//! available with the `test-fake-host` feature, since that is the only build
//! with a backend.

#![cfg(feature = "test-fake-host")]

use std::io::Write;
use std::process::{Command, Stdio};

fn opencraylsp_mcp() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Feeds a transcript in and returns `(exit code, stdout lines)`.
fn run(args: &[&str], transcript: &str) -> (i32, Vec<String>) {
    let mut child = opencraylsp_mcp()
        .args(args)
        .spawn()
        .expect("opencraylsp-mcp should start");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(transcript.as_bytes())
        .expect("write transcript");
    let out = child
        .wait_with_output()
        .expect("opencraylsp-mcp should exit");
    let stdout = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    (out.status.code().unwrap_or(-1), stdout)
}

const SESSION: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    "\n",
);

#[test]
fn a_full_session_is_answered_on_stdout_and_exits_zero() {
    let (code, lines) = run(&["--fake-host"], SESSION);
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 2, "two requests, two replies: {lines:?}");
    // Requests are served on spawned tasks (see mcp.rs), so reply order on
    // stdout is not the request order. Match by id, not by line index — the
    // CI flake on main after #5 was exactly id 2 landing first.
    let mut by_id = std::collections::BTreeMap::new();
    for line in &lines {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON-RPC: {line} ({e})"));
        assert_eq!(v["jsonrpc"], "2.0");
        let id = v["id"]
            .as_i64()
            .unwrap_or_else(|| panic!("reply without numeric id: {line}"));
        assert!(
            by_id.insert(id, v).is_none(),
            "duplicate reply id {id}: {lines:?}"
        );
    }
    let init = by_id
        .get(&1)
        .unwrap_or_else(|| panic!("missing id 1: {lines:?}"));
    assert_eq!(init["result"]["serverInfo"]["name"], "opencraylsp-mcp");
    let listed = by_id
        .get(&2)
        .unwrap_or_else(|| panic!("missing id 2: {lines:?}"));
    assert_eq!(listed["result"]["tools"][0]["name"], "lsp_status");
}

#[test]
fn a_single_initialize_line_gets_a_legal_reply() {
    let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    let (code, lines) = run(&["--fake-host"], &format!("{line}\n"));
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
}

#[test]
fn tracing_output_never_reaches_stdout() {
    // RUST_LOG turns on debug logging; stdout must stay pure protocol.
    let mut child = opencraylsp_mcp()
        .args(["--fake-host"])
        .env("RUST_LOG", "trace")
        .spawn()
        .expect("opencraylsp-mcp should start");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(SESSION.as_bytes())
        .expect("write transcript");
    let out = child
        .wait_with_output()
        .expect("opencraylsp-mcp should exit");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut ids = std::collections::BTreeSet::new();
    for line in stdout.lines() {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout polluted: {line:?} ({e})"));
        ids.insert(v["id"].as_i64().expect("numeric id"));
    }
    assert_eq!(
        ids,
        [1, 2].into_iter().collect(),
        "expected replies for ids 1 and 2: {stdout}"
    );
}

#[test]
fn help_and_version_go_to_stdout_and_exit_zero() {
    let (code, lines) = run(&["--help"], "");
    assert_eq!(code, 0);
    let help = lines.join("\n");
    assert!(help.contains("--languages"), "{help}");
    assert!(help.contains("LANGUAGES"), "{help}");
    assert!(help.contains("claude mcp add opencraylsp"), "{help}");

    let (code, lines) = run(&["--version"], "");
    assert_eq!(code, 0);
    assert!(lines[0].starts_with("opencraylsp-mcp "), "{lines:?}");
}

#[test]
fn an_unknown_flag_exits_2_without_touching_the_protocol() {
    let (code, lines) = run(&["--not-a-real-flag"], "");
    assert_eq!(code, 2, "clap rejects it before anything else");
    assert!(lines.is_empty(), "nothing may be written: {lines:?}");
}

#[test]
fn a_bad_workspace_exits_2_and_says_why_on_stderr() {
    let child = opencraylsp_mcp()
        .args(["--workspace", "/definitely/not/here"])
        .spawn()
        .expect("opencraylsp-mcp should start");
    let out = child
        .wait_with_output()
        .expect("opencraylsp-mcp should exit");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout stays clean");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("workspace"), "{stderr}");
}
