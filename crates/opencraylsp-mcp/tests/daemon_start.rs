//! The `opencraylsp-mcp` binary against a scripted daemon, for the startup rules that
//! need a real process.
//!
//! The fake daemon is a tempdir socket; `--workspace` points into the same
//! tempdir, so neither the real socket nor the real home is touched.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// An unknown language is rejected from the flags alone: the process exits 2
/// without ever contacting a daemon. The socket here has no listener and
/// `OPENCRAYLSP_BIN` points nowhere, so a connect could only ever time out; the fast
/// exit is what proves the check is local.
#[test]
fn an_unknown_language_exits_2_with_the_full_message_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    command
        .arg("--socket")
        .arg(dir.path().join("opencraylsp.sock"))
        .arg("--languages")
        .arg("klingon")
        .arg("--workspace")
        .arg(&workspace)
        .env("OPENCRAYLSP_BIN", dir.path().join("no-such-opencraylspd"));
    isolated(&mut command, dir.path());

    let started = Instant::now();
    let out = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run opencraylsp-mcp");
    let elapsed = started.elapsed();

    assert_eq!(out.status.code(), Some(2), "unknown language is fatal");
    assert!(
        elapsed < Duration::from_secs(1),
        "the rejection must not wait on a daemon: {elapsed:?}"
    );
    assert!(out.stdout.is_empty(), "stdout is protocol-only");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "error: unknown language \"klingon\"; valid: rust, go, php, typescript, javascript, python, all, auto (aliases: ts, js, rs, golang, py)"
        ),
        "stderr: {stderr}"
    );
}

mod support;
use support::FakeDaemon;

const SESSION: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lsp_status","arguments":{}}}"#,
    "\n",
);

fn isolated(command: &mut Command, dir: &std::path::Path) {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    command
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("XDG_STATE_HOME");
}

fn run_session(mut command: Command) -> std::process::Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start opencraylsp-mcp");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(SESSION.as_bytes())
        .expect("write the session");
    child.wait_with_output().expect("opencraylsp-mcp exits")
}

fn replies(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}")))
        .collect()
}

/// Sends one request and reads one reply, timing the round trip.
fn time_request(
    stdin: &mut impl Write,
    reader: &mut impl BufRead,
    request: &str,
) -> (Duration, serde_json::Value) {
    let started = Instant::now();
    writeln!(stdin, "{request}").expect("write the request");
    stdin.flush().expect("flush the request");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read the reply");
    let elapsed = started.elapsed();
    (
        elapsed,
        serde_json::from_str(&line).expect("the reply is JSON"),
    )
}

/// The harness must not be made to wait for a daemon: `initialize` and
/// `tools/list` answer immediately, and only a real `tools/call` pays the
/// connect deadline before reporting `[daemon_unavailable]`.
#[test]
fn initialize_and_tools_list_do_not_wait_for_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    command
        .arg("--socket")
        .arg(dir.path().join("opencraylsp.sock"))
        .arg("--workspace")
        .arg(&workspace)
        // No daemon can be started: the spawn target does not exist, so the
        // dial can only end in `[daemon_unavailable]`.
        .env("OPENCRAYLSP_BIN", dir.path().join("no-such-opencraylspd"));
    isolated(&mut command, dir.path());

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start opencraylsp-mcp");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout is piped"));

    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    let (elapsed, reply) = time_request(&mut stdin, &mut reader, initialize);
    assert_eq!(reply["id"], 1);
    assert!(reply["result"]["protocolVersion"].is_string(), "{reply}");
    assert!(
        elapsed < Duration::from_secs(1),
        "initialize must not wait for a daemon: {elapsed:?}"
    );

    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    writeln!(stdin, "{initialized}").expect("write initialized");
    stdin.flush().expect("flush initialized");

    let (elapsed, reply) = time_request(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    );
    assert_eq!(reply["id"], 2);
    let tools = reply["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 11, "the built-in catalogue, with no daemon");
    assert!(
        elapsed < Duration::from_secs(1),
        "tools/list must not wait for a daemon: {elapsed:?}"
    );

    let call = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lsp_status","arguments":{}}}"#;
    let (elapsed, reply) = time_request(&mut stdin, &mut reader, call);
    assert_eq!(reply["id"], 3);
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(text.starts_with("[daemon_unavailable]"), "{text}");
    assert!(
        elapsed >= Duration::from_millis(1),
        "the call must have tried to connect first: {elapsed:?}"
    );

    drop(stdin);
    let status = child.wait().expect("opencraylsp-mcp exits");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn a_tools_call_still_answers_when_stdin_closes_before_the_daemon_does() {
    // A transcript piped in and closed immediately must still get the
    // `[daemon_unavailable]` answer: it takes the daemon's connect deadline,
    // so the EOF grace must not cancel the call sooner.
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    command
        .arg("--socket")
        .arg(dir.path().join("opencraylsp.sock"))
        .arg("--workspace")
        .arg(&workspace)
        .env("OPENCRAYLSP_BIN", dir.path().join("no-such-opencraylspd"));
    isolated(&mut command, dir.path());

    // `run_session` writes the whole session and closes stdin at once.
    let out = run_session(command);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let replies = replies(&out.stdout);
    let call = replies
        .iter()
        .find(|reply| reply["id"] == 3)
        .unwrap_or_else(|| panic!("tools/call got no reply: {replies:?}"));
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.starts_with("[daemon_unavailable]"), "{text}");
}

#[test]
fn a_daemon_session_answers_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("opencraylsp.sock");
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _daemon = runtime.block_on(FakeDaemon::start(socket.clone()));

    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    command
        .arg("--socket")
        .arg(&socket)
        .arg("--workspace")
        .arg(&workspace);
    isolated(&mut command, dir.path());
    let out = run_session(command);

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let replies = replies(&out.stdout);
    assert_eq!(replies.len(), 3, "one reply per request");
    // Requests are handled concurrently, so match by id rather than position.
    let called = replies
        .iter()
        .find(|reply| reply["id"] == 3)
        .expect("a reply to tools/call");
    assert_eq!(
        called["result"]["content"][0]["text"],
        "ran",
        "replies: {replies:?} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_embedded_session_answers_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    command.arg("--embedded").arg("--workspace").arg(&workspace);
    isolated(&mut command, dir.path());
    let out = run_session(command);

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let replies = replies(&out.stdout);
    assert_eq!(replies.len(), 3, "one reply per request");
    assert!(replies[2].get("result").is_some(), "{:?}", replies[2]);
}

#[test]
fn an_embedded_unknown_language_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    command
        .arg("--embedded")
        .arg("--languages")
        .arg("klingon")
        .arg("--workspace")
        .arg(&workspace);
    isolated(&mut command, dir.path());
    let out = command.output().expect("run opencraylsp-mcp");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown language \"klingon\""), "{stderr}");
}

#[test]
fn embedded_rejects_a_broken_config() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "not = toml [").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    command
        .arg("--embedded")
        .arg("--config")
        .arg(&config)
        .arg("--workspace")
        .arg(&workspace);
    isolated(&mut command, dir.path());
    let out = command.output().expect("run opencraylsp-mcp");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot load the config"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
