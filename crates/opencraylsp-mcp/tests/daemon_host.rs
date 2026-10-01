//! The daemon-backed host, without a daemon: how `opencraylsp-mcp` behaves when
//! nothing is listening.
//!
//! All of these point `ClientOptions` at a tempdir socket and disable
//! spawning, so the real default socket is never touched.

use std::sync::Arc;
use std::time::{Duration, Instant};

use opencraylsp_client::ClientOptions;
use opencraylsp_mcp::daemon_host::LazyDaemonHost;
use opencraylsp_proto::{HostError, ToolHost};
use serde_json::json;
use tokio_util::sync::CancellationToken;

mod support;
use support::FakeDaemon;

fn dead_options(dir: &tempfile::TempDir, deadline: Duration) -> ClientOptions {
    let mut options = ClientOptions::default_for_tests();
    options.socket = dir.path().join("absent.sock");
    options.workspace = dir.path().to_owned();
    options.spawn = false;
    options.connect_deadline = deadline;
    options
}

fn live_options(dir: &tempfile::TempDir, socket: &std::path::Path) -> ClientOptions {
    let mut options = ClientOptions::default_for_tests();
    options.socket = socket.to_owned();
    options.workspace = dir.path().to_owned();
    options.spawn = false;
    options.connect_deadline = Duration::from_millis(500);
    options
}

#[tokio::test]
async fn list_tools_without_a_daemon_is_the_builtin_catalogue() {
    let dir = tempfile::tempdir().unwrap();
    let host = LazyDaemonHost::new(dead_options(&dir, Duration::from_millis(100)));
    let tools = host.list_tools().await.expect("static fallback");
    assert_eq!(tools.len(), opencraylsp_tools::tool_defs().len());
    assert!(tools.iter().any(|t| t.name == "lsp_status"));
}

#[tokio::test]
async fn call_tool_without_a_daemon_is_daemon_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let host = LazyDaemonHost::new(dead_options(&dir, Duration::from_millis(100)));
    let error = host
        .call_tool("lsp_status", json!({}), &CancellationToken::new())
        .await
        .expect_err("no daemon");
    match error {
        HostError::Unavailable(text) => assert!(text.starts_with("[daemon_unavailable]"), "{text}"),
        other => panic!("want Unavailable, got {other:?}"),
    }
}

/// A burst of callers who all find the daemon missing shares one dial; without
/// single-flight they would queue one `connect_deadline` each.
#[tokio::test]
async fn a_burst_against_a_missing_daemon_dials_once() {
    let dir = tempfile::tempdir().unwrap();
    let host = LazyDaemonHost::new(dead_options(&dir, Duration::from_millis(200)));
    let host: Arc<LazyDaemonHost> = host;
    let started = Instant::now();
    let mut calls = Vec::new();
    for _ in 0..10 {
        let host = Arc::clone(&host);
        calls.push(tokio::spawn(async move {
            host.call_tool("lsp_status", json!({}), &CancellationToken::new())
                .await
        }));
    }
    for call in calls {
        assert!(call.await.expect("join").is_err());
    }
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "waiters queued behind each other: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn list_tools_while_connected_comes_from_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("opencraylsp.sock");
    let _daemon = FakeDaemon::start(socket.clone()).await;
    let host = LazyDaemonHost::new(live_options(&dir, &socket));
    // `list_tools` never dials on its own; once a connection exists (here made
    // explicitly) the daemon's catalogue is the one used.
    host.connect_now().await.expect("connect");
    let tools = host.list_tools().await.expect("the daemon answers");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "lsp_status");
}

#[tokio::test]
async fn call_tool_while_connected_runs_on_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("opencraylsp.sock");
    let _daemon = FakeDaemon::start(socket.clone()).await;
    let host = LazyDaemonHost::new(live_options(&dir, &socket));
    let out = host
        .call_tool("lsp_status", json!({}), &CancellationToken::new())
        .await
        .expect("the daemon answers");
    assert_eq!(out.text, "ran");
    assert!(!out.is_error);
}

#[tokio::test]
async fn the_host_connects_once_for_many_calls() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("opencraylsp.sock");
    let daemon = FakeDaemon::start(socket.clone()).await;
    let host = LazyDaemonHost::new(live_options(&dir, &socket));
    host.list_tools().await.expect("first");
    host.call_tool("lsp_status", json!({}), &CancellationToken::new())
        .await
        .expect("second");
    host.list_tools().await.expect("third");
    assert_eq!(daemon.connections(), 1, "the connection is reused");
}

#[tokio::test]
async fn the_connect_options_are_exposed() {
    let dir = tempfile::tempdir().unwrap();
    let host = LazyDaemonHost::new(dead_options(&dir, Duration::from_millis(100)));
    assert!(host.options().socket.ends_with("absent.sock"));
}

#[tokio::test]
async fn concurrent_calls_share_one_connection() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("opencraylsp.sock");
    let daemon = FakeDaemon::start(socket.clone()).await;
    let host = LazyDaemonHost::new(live_options(&dir, &socket));
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let host = Arc::clone(&host);
        // `call_tool` is what dials; a burst of first calls must share one
        // connection, not open one each.
        tasks.push(tokio::spawn(async move {
            host.call_tool("lsp_status", json!({}), &CancellationToken::new())
                .await
        }));
    }
    for task in tasks {
        assert!(task.await.expect("join").is_ok());
    }
    assert_eq!(daemon.connections(), 1, "one connection serves them all");
}
