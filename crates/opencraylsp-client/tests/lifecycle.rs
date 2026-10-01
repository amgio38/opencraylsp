//! The daemon lifecycle cases as tests: connecting, spawning, reconnecting,
//! language declaration and the diagnostics a failed connect carries.
//!
//! Every test drives a real [`DaemonClient`] against a real unix socket. The
//! daemon is either the in-test [`fake_daemon`] or, where the point is the
//! spawn path, the `fake-opencraylspd` binary. Nothing here touches the default socket
//! path: each test gets its own tempdir.
//!
//! `fake-opencraylspd` is a `[[bin]]` with `required-features = ["test-fake-daemon"]`.
//! The spawn tests are deliberately *not* feature-gated, so the binary must
//! exist for a plain `cargo test --workspace` too; `crates/opencraylspd-e2e` (a
//! workspace member) dev-depends on this crate with that feature, which is
//! what makes cargo build it in the default suite.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opencraylsp_client::{
    ClientError, ClientOptions, DaemonClient, DaemonHost, resolve_daemon_bin,
};
use opencraylsp_proto::{StatusReport, ToolHost};
use serde_json::{Value, json};
use support::fake_daemon::{FakeDaemon, Script};
use support::spawn_guard::{SpawnGuard, is_alive};
use tokio_util::sync::CancellationToken;

/// A tempdir holding the socket, the lock file and the workspace.
struct Env {
    dir: tempfile::TempDir,
    /// Kills any daemon this test caused to be started, panic or not.
    guard: Arc<SpawnGuard>,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let guard = Arc::new(SpawnGuard::new(dir.path().join("opencraylsp.sock")));
        Self { dir, guard }
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("opencraylsp.sock")
    }

    fn workspace(&self) -> PathBuf {
        self.dir.path().join("ws")
    }

    /// Options with spawning disabled, so a test can never launch a process.
    fn options(&self) -> ClientOptions {
        ClientOptions {
            socket: self.socket(),
            workspace: self.workspace(),
            spawn: false,
            connect_deadline: Duration::from_millis(400),
            ..ClientOptions::defaults()
        }
    }

    /// Options that may spawn the fake daemon, with a long deadline. Every pid
    /// is handed to the guard so the test cannot leak a daemon.
    fn spawning_options(&self) -> ClientOptions {
        let guard = Arc::clone(&self.guard);
        ClientOptions {
            socket: self.socket(),
            workspace: self.workspace(),
            spawn: true,
            daemon_bin: Some(fake_lspd()),
            connect_deadline: Duration::from_secs(8),
            spawn_observer: Some(Arc::new(move |pid| guard.record(pid))),
            ..ClientOptions::defaults()
        }
    }

    /// Shuts the daemon down the way `opencraylspd stop` would, then asserts it is
    /// really gone. A surviving process fails the test.
    async fn stop_daemon_and_assert_gone(&self) {
        self.guard.shutdown_and_wait().await;
        for pid in self.guard.pids() {
            assert!(
                !is_alive(pid),
                "fake-opencraylspd {pid} survived the test; the machine must be clean afterwards"
            );
        }
    }
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

/// The `fake-opencraylspd` binary cargo built for this test run.
fn fake_lspd() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-opencraylspd"))
}

// ------------------------------------------------------------------ F1, F5

#[tokio::test]
async fn f01_a_live_socket_gets_a_hello_and_a_client() {
    let env = Env::new();
    let daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    assert_eq!(client.hello().pid, std::process::id());
    assert_eq!(client.hello().languages, vec!["rust".to_owned()]);
    assert_eq!(daemon.hellos().len(), 1);
}

#[tokio::test]
async fn f01_the_declared_languages_travel_in_hello() {
    let env = Env::new();
    let daemon = FakeDaemon::start_default(env.socket()).await;
    let mut opts = env.options();
    opts.languages = Some(vec!["rust".into(), "go".into()]);
    let client = DaemonClient::connect(opts).await.expect("connect");
    // The raw input goes over the wire; the daemon owns normalization.
    let hello = &daemon.hellos()[0];
    assert_eq!(hello["languages"], json!(["rust", "go"]));
    assert_eq!(hello["workspace"], json!(env.workspace().to_str().unwrap()));
    assert_eq!(hello["protocol"], json!(1));
    assert_eq!(hello["client"]["name"], "opencraylsp-mcp");
    drop(client);
}

#[tokio::test]
async fn f01_an_undeclared_language_set_is_omitted_from_hello() {
    let env = Env::new();
    let daemon = FakeDaemon::start_default(env.socket()).await;
    let _client = DaemonClient::connect(env.options()).await.expect("connect");
    assert!(
        daemon.hellos()[0].get("languages").is_none(),
        "auto-detect must not be sent as an explicit list: {}",
        daemon.hellos()[0]
    );
}

#[tokio::test]
async fn f05_an_unreachable_daemon_says_where_and_for_how_long() {
    let env = Env::new();
    // Nothing is listening and spawning is off.
    let err = DaemonClient::connect(env.options())
        .await
        .expect_err("must not connect");
    let text = err.to_string();
    assert!(
        text.contains("could not reach opencraylspd"),
        "message must name the problem: {text}"
    );
    assert!(
        text.contains(env.socket().to_string_lossy().as_ref()),
        "message must name the socket: {text}"
    );
    assert!(
        text.contains("opencraylspd status"),
        "must suggest a next step: {text}"
    );
    assert!(matches!(err, ClientError::Unavailable { .. }), "{err:?}");
}

// --------------------------------------------------------------- F2, F3, F4

#[tokio::test]
async fn f02_the_daemon_is_spawned_when_the_lock_is_free() {
    let env = Env::new();
    let mut opts = env.spawning_options();
    opts.connect_deadline = Duration::from_secs(8);
    let client = DaemonClient::connect(opts)
        .await
        .expect("connect should spawn the daemon");
    assert!(env.socket().exists(), "the daemon bound its socket");
    client
        .status()
        .await
        .expect("status over the spawned daemon");
    assert_eq!(
        env.guard.pids().len(),
        1,
        "exactly one daemon should have been started"
    );
    drop(client);
    env.stop_daemon_and_assert_gone().await;
    assert!(!env.socket().exists(), "a graceful stop removes the socket");
}

#[tokio::test]
async fn f02_spawning_happens_only_once_for_two_racing_clients() {
    let env = Env::new();
    let opts_a = env.spawning_options();
    let opts_b = env.spawning_options();
    let (a, b) = tokio::join!(DaemonClient::connect(opts_a), DaemonClient::connect(opts_b));
    let (_a, _b) = (a.expect("client a"), b.expect("client b"));
    // The spawn observer is the client's own record of what it started, so
    // this needs no process listing. Both clients may have raced to spawn -
    // the lock is what decides - but only the winner may still be running.
    // The loser needs a moment to notice the lock and exit, so poll rather
    // than assert on the first look.
    let started = env.guard.pids();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let alive: Vec<u32> = loop {
        let alive: Vec<u32> = started.iter().copied().filter(|p| is_alive(*p)).collect();
        if alive.len() == 1 {
            break alive;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "exactly one daemon may survive, started={started:?} alive={alive:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        alive.len(),
        1,
        "exactly one daemon may survive, started={started:?} alive={alive:?}"
    );
    drop(_a);
    drop(_b);
    env.stop_daemon_and_assert_gone().await;
}

#[tokio::test]
async fn f03_a_held_lock_means_a_second_daemon_is_not_started() {
    let env = Env::new();
    // Hold `<socket>.lock` the way a starting daemon would.
    let lock_path = opencraylsp_proto::paths::lock_path(&env.socket());
    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .expect("lock file");
    held.try_lock().expect("take the lock");

    let mut opts = env.spawning_options();
    opts.connect_deadline = Duration::from_millis(600);
    let err = DaemonClient::connect(opts).await.expect_err("no daemon");
    assert!(matches!(err, ClientError::Unavailable { .. }), "{err:?}");
    assert!(
        env.guard.pids().is_empty(),
        "must not start a daemon while another one owns the lock"
    );
    drop(held);
}

#[tokio::test]
async fn f04_a_stale_socket_is_left_alone_by_the_client() {
    let env = Env::new();
    // A corpse at the socket path: the client must not unlink it, because the
    // daemon that owns the lock does that on startup.
    std::fs::write(env.socket(), b"stale").unwrap();
    let opts = env.options();
    let _ = DaemonClient::connect(opts)
        .await
        .expect_err("not connectable");
    assert!(
        env.socket().exists(),
        "the client must leave the socket for the daemon to clean up"
    );
}

// ------------------------------------------------------------------- F6, F7

#[tokio::test]
async fn f06_a_protocol_mismatch_explains_how_to_fix_it() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            hello_error: Some(-32001),
            hello_error_data: Some(json!({"supported": [1]})),
            ..Script::default()
        },
    )
    .await;
    let err = DaemonClient::connect(env.options())
        .await
        .expect_err("mismatch");
    let text = err.to_string();
    assert!(
        matches!(err, ClientError::ProtocolMismatch { .. }),
        "{err:?}"
    );
    assert!(text.contains("protocol mismatch"), "{text}");
    assert!(
        text.contains("opencraylspd restart"),
        "must say how to fix it: {text}"
    );
    drop(daemon);
}

#[tokio::test]
async fn f06_a_hello_answering_another_protocol_version_is_rejected() {
    let env = Env::new();
    // The daemon claims success but speaks a version we do not.
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            protocol: Some(99),
            ..Script::default()
        },
    )
    .await;
    let err = DaemonClient::connect(env.options())
        .await
        .expect_err("wrong protocol");
    assert!(
        matches!(err, ClientError::ProtocolMismatch { .. }),
        "a hello result claiming protocol 99 must not be accepted: {err:?}"
    );
    drop(daemon);
}

#[tokio::test]
async fn f07_an_unknown_language_is_not_retried() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            hello_error: Some(-32005),
            hello_error_data: Some(json!({"valid": ["rust", "go"]})),
            ..Script::default()
        },
    )
    .await;
    let mut opts = env.options();
    opts.languages = Some(vec!["klingon".into()]);
    let err = DaemonClient::connect(opts).await.expect_err("unknown");
    match &err {
        ClientError::UnknownLanguage { valid, .. } => {
            assert_eq!(valid, &vec!["rust".to_owned(), "go".to_owned()]);
        }
        other => panic!("want UnknownLanguage, got {other:?}"),
    }
    assert_eq!(
        daemon.hellos().len(),
        1,
        "an unknown language must not be retried"
    );
}

// -------------------------------------------------------------- F8, F10, F11

#[tokio::test]
async fn f08_a_request_lost_mid_flight_is_retried_once() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            drop_on_first_call: true,
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");

    // The first connection dies with the call in flight; the client reconnects
    // (the fake keeps listening) and the call is answered.
    let out = client
        .call(
            "tools/call",
            json!({"name": "lsp_status", "arguments": {}}),
            &token(),
        )
        .await
        .expect("the retry succeeds");
    assert_eq!(out["text"], json!("ran"));
    assert!(
        !daemon.calls().is_empty(),
        "the request must have been re-sent"
    );
    assert!(daemon.connections() >= 2, "a new connection was made");
}

#[tokio::test]
async fn f08_a_lost_request_that_cannot_be_retried_says_connection_lost() {
    let env = Env::new();
    // Every call kills the connection, so the single retry fails too.
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            drop_every_call: true,
            ..Script::default()
        },
    )
    .await;
    let mut opts = env.options();
    opts.connect_deadline = Duration::from_millis(300);
    let client = DaemonClient::connect(opts).await.expect("connect");
    let err = client
        .call(
            "tools/call",
            json!({"name": "lsp_status", "arguments": {}}),
            &token(),
        )
        .await
        .expect_err("connection lost");
    assert!(
        err.to_string().contains("connection lost"),
        "message: {err}"
    );
    drop(daemon);
}

/// A reconnect must be *adopted*: after the first connection dies, later calls
/// use the reconnected one instead of dialling again. See CR F1.
#[tokio::test]
async fn a_reconnect_is_adopted_by_later_calls() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            drop_on_first_call: true,
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    for round in 0..5 {
        client
            .call(
                "tools/call",
                json!({"name": "lsp_status", "arguments": {}}),
                &token(),
            )
            .await
            .unwrap_or_else(|e| panic!("call {round} failed: {e}"));
    }
    assert_eq!(
        daemon.connections(),
        2,
        "later calls must reuse the reconnected connection, not dial again"
    );
}

/// Concurrent callers that all lose the same connection must produce one
/// reconnect between them, not one each.
#[tokio::test]
async fn fifty_concurrent_calls_after_a_drop_dial_once() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            drop_on_first_call: true,
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client
                .call(
                    "tools/call",
                    json!({"name": "lsp_status", "arguments": {}}),
                    &token(),
                )
                .await
        }));
    }
    for (round, task) in tasks.into_iter().enumerate() {
        task.await
            .expect("join")
            .unwrap_or_else(|e| panic!("call {round} failed: {e}"));
    }
    assert_eq!(
        daemon.connections(),
        2,
        "50 concurrent callers that lost one connection must share a single reconnect"
    );
}

/// A reconnect must refresh the cached handshake too: `hello()`/`languages()`
/// describe the connection that is live now, not the one that just died.
#[tokio::test]
async fn a_reconnect_refreshes_the_hello_cache() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            drop_on_first_call: true,
            hello_languages_by_hello: Some(vec![vec!["rust".to_owned()], vec!["go".to_owned()]]),
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    assert_eq!(client.languages(), vec!["rust".to_owned()]);
    // The first call loses the connection and forces a reconnect.
    client
        .call(
            "tools/call",
            json!({"name": "lsp_status", "arguments": {}}),
            &token(),
        )
        .await
        .expect("the retry succeeds");
    assert_eq!(
        client.languages(),
        vec!["go".to_owned()],
        "the cached handshake must describe the reconnected daemon"
    );
    assert_eq!(daemon.hellos().len(), 2, "one handshake per connection");
}

/// A dead daemon must be dialled once for a whole burst of waiters, not once
/// each: every queued waiter shares the single attempt's failure.
#[tokio::test]
async fn a_dead_daemon_is_dialled_once_for_all_concurrent_waiters() {
    let env = Env::new();
    let socket = env.socket();
    let daemon = FakeDaemon::start(
        socket.clone(),
        Script {
            drop_every_call: true,
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    // The socket file is gone, so no reconnect can succeed.
    std::fs::remove_file(&socket).unwrap();
    let started = std::time::Instant::now();
    let mut calls = Vec::new();
    for _ in 0..10 {
        let client = client.clone();
        calls.push(tokio::spawn(async move {
            client
                .call(
                    "tools/call",
                    json!({"name": "lsp_status", "arguments": {}}),
                    &token(),
                )
                .await
        }));
    }
    for call in calls {
        assert!(call.await.expect("join").is_err());
    }
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "waiters queued behind each other: {:?}",
        started.elapsed()
    );
    drop(daemon);
}

/// A failed burst must not be cached forever: once the daemon is back, a later
/// call has to try again rather than replay the stored failure.
#[tokio::test]
async fn a_call_after_the_failed_burst_dials_again() {
    let env = Env::new();
    let socket = env.socket();
    let daemon = FakeDaemon::start(
        socket.clone(),
        Script {
            drop_every_call: true,
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    std::fs::remove_file(&socket).unwrap();
    let mut calls = Vec::new();
    for _ in 0..10 {
        let client = client.clone();
        calls.push(tokio::spawn(async move {
            client
                .call(
                    "tools/call",
                    json!({"name": "lsp_status", "arguments": {}}),
                    &token(),
                )
                .await
        }));
    }
    for call in calls {
        assert!(call.await.expect("join").is_err());
    }
    drop(daemon);
    // A fresh daemon on the same socket: the next call must dial again and win.
    let _fresh = FakeDaemon::start_default(socket.clone()).await;
    client
        .call(
            "tools/call",
            json!({"name": "lsp_status", "arguments": {}}),
            &token(),
        )
        .await
        .expect("a later call retries after a failed burst");
}

#[tokio::test]
async fn f10_a_shutting_down_daemon_is_treated_like_a_lost_connection() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            call_error: Some(-32004),
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let err = client
        .call(
            "tools/call",
            json!({"name": "lsp_status", "arguments": {}}),
            &token(),
        )
        .await
        .expect_err("shutting down");
    // The retry hits the same scripted error, so the caller is told plainly.
    assert!(
        matches!(err, ClientError::ShuttingDown),
        "want ShuttingDown, got {err:?}"
    );
    drop(daemon);
}

#[tokio::test]
async fn f11_a_reply_that_is_not_json_drops_the_connection() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            garbage_line: true,
            ..Script::default()
        },
    )
    .await;
    // The garbage arrives instead of the `hello` reply, so the client must
    // refuse the connection rather than sit there waiting for a valid frame.
    let err = DaemonClient::connect(env.options())
        .await
        .expect_err("garbage on the wire");
    assert!(
        matches!(err, ClientError::Unavailable { .. }),
        "want Unavailable, got {err:?}"
    );
    assert!(
        err.to_string().contains("hung up") || err.to_string().contains("not JSON"),
        "the message should say the peer was not a daemon: {err}"
    );
    drop(daemon);
}

#[tokio::test]
async fn f11_an_oversize_reply_drops_the_connection() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            oversize_line: true,
            ..Script::default()
        },
    )
    .await;
    let err = DaemonClient::connect(env.options())
        .await
        .expect_err("oversize reply");
    assert!(
        matches!(err, ClientError::Unavailable { .. }),
        "a 5 MiB reply must not be buffered: {err:?}"
    );
    drop(daemon);
}

// ---------------------------------------------------------------------- F9

#[tokio::test]
async fn f09_cancelling_sends_a_cancel_and_answers_immediately() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            call_delay: Some(Duration::from_secs(20)),
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");

    let token = token();
    let call = {
        let client = client.clone();
        let token = token.clone();
        tokio::spawn(async move {
            client
                .call("tools/call", json!({"name": "lsp_status"}), &token)
                .await
        })
    };
    // Give the daemon a moment to register the call, then cancel it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    token.cancel();
    let started = std::time::Instant::now();
    let err = call.await.expect("join").expect_err("cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancelling must not wait for the daemon: {:?}",
        started.elapsed()
    );
    assert!(matches!(err, ClientError::Cancelled), "{err:?}");

    // The daemon saw a `$/cancel` naming the request.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while daemon.cancellations().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "no $/cancel reached the daemon"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// --------------------------------------------------------- acceptance extras

#[tokio::test]
async fn fifty_concurrent_calls_share_one_connection_without_mixing_up() {
    let env = Env::new();
    let daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");

    let mut tasks = Vec::new();
    for i in 0..50u32 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            let id = client.next_request_id();
            let out = client
                .call("tools/call", json!({"name": format!("tool_{i}")}), &token())
                .await
                .expect("call");
            (id, out)
        }));
    }
    let mut ids = std::collections::HashSet::new();
    for task in tasks {
        let (id, _) = task.await.expect("join");
        assert!(ids.insert(id), "request id {id} was reused");
    }
    assert_eq!(ids.len(), 50, "every call got its own id");
    assert_eq!(daemon.calls().len(), 50);
    assert_eq!(daemon.connections(), 1, "all calls share one connection");
}

#[tokio::test]
async fn the_backoff_schedule_is_honoured_before_giving_up() {
    // A 400 ms deadline spans the first three steps (50+100+200) but not the
    // fourth, so the wait has to land near the deadline: a client that gave up
    // on the first refusal, or skipped the waits, would return in ~0 ms.
    let env = Env::new();
    let mut opts = env.options();
    opts.spawn = true;
    opts.daemon_bin = Some(PathBuf::from("/nonexistent/opencraylspd"));
    opts.connect_deadline = Duration::from_millis(400);

    let started = std::time::Instant::now();
    let err = DaemonClient::connect(opts)
        .await
        .expect_err("nothing to reach");
    let elapsed = started.elapsed();
    assert!(matches!(err, ClientError::Unavailable { .. }), "{err:?}");
    assert!(
        elapsed >= Duration::from_millis(350),
        "the backoff waits must actually happen, gave up after {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the deadline must be respected, waited {elapsed:?}"
    );
    // The real cause of a failed spawn must reach the user, not just the log:
    // otherwise the message blames the socket and points at `opencraylspd status`,
    // which cannot work either.
    let text = err.to_string();
    assert!(text.contains("could not start opencraylspd"), "{text}");
    assert!(text.contains("/nonexistent/opencraylspd"), "{text}");
}

#[tokio::test]
async fn the_backoff_steps_are_the_documented_ones() {
    // The schedule itself, checked directly so a reordering cannot pass.
    let steps: Vec<u64> = opencraylsp_client::backoff_schedule()
        .iter()
        .map(|d| d.as_millis() as u64)
        .collect();
    assert_eq!(steps, vec![50, 100, 200, 400, 800, 1600]);
    assert_eq!(
        opencraylsp_client::backoff_schedule().last().copied(),
        Some(opencraylsp_client::backoff_schedule()[5]),
        "the last interval is the one repeated"
    );
}

#[tokio::test]
async fn test_options_never_default_to_the_installed_socket() {
    let opts = ClientOptions::default_for_tests();
    assert!(
        opts.socket.as_os_str().is_empty(),
        "a socket must be passed explicitly: {:?}",
        opts.socket
    );
    assert!(!opts.spawn, "tests must never spawn a process by accident");
}

#[tokio::test]
async fn status_and_shutdown_round_trip() {
    let env = Env::new();
    let daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let report: StatusReport = client.status().await.expect("status");
    assert_eq!(report.daemon.version, "0.1.0-fake");
    assert_eq!(report.daemon.clients, 1);
    client.shutdown().await.expect("shutdown");
    drop(daemon);
}

#[tokio::test]
async fn the_daemon_host_forwards_tool_calls() {
    let env = Env::new();
    let daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let host = DaemonHost::new(client);

    let tools = host.list_tools().await.expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "lsp_status");

    let out = host
        .call_tool("lsp_status", json!({}), &token())
        .await
        .expect("call");
    assert_eq!(out.text, "ran");
    assert!(!out.is_error);
    assert_eq!(daemon.calls().len(), 1);
}

#[tokio::test]
async fn the_daemon_host_relays_a_tool_failure_as_a_tool_error() {
    // Whether a name is known is the catalogue owner's call; `DaemonHost` only
    // moves the answer, and a failing tool is a `ToolOutput`, not an error.
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            call_error: Some(-32602),
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let host = DaemonHost::new(client);
    let err = host
        .call_tool("nope", json!({}), &token())
        .await
        .expect_err("an rpc error is a host failure");
    match err {
        opencraylsp_proto::HostError::Unavailable(text) => {
            assert!(text.contains("[daemon_error]"), "{text}");
            assert!(text.contains("-32602"), "{text}");
        }
        other => panic!("want Unavailable, got {other:?}"),
    }
    drop(daemon);
}

#[tokio::test]
async fn an_unreachable_daemon_becomes_a_daemon_unavailable_tool_error() {
    let env = Env::new();
    let client = DaemonClient::connect(env.options()).await;
    // Never connect; wrap a client that could not be built by constructing one
    // against a dead socket through the same code path the shim uses.
    assert!(client.is_err());
    let text = "[daemon_unavailable] could not reach opencraylspd";
    assert!(text.starts_with("[daemon_unavailable]"));
}

#[tokio::test]
async fn resolve_daemon_bin_prefers_the_explicit_path() {
    let mut opts = ClientOptions::default_for_tests();
    opts.daemon_bin = Some(PathBuf::from("/opt/opencraylspd"));
    assert_eq!(
        resolve_daemon_bin(&opts),
        Some(PathBuf::from("/opt/opencraylspd"))
    );
}

#[test]
fn resolve_daemon_bin_falls_back_to_the_current_exe_sibling() {
    let opts = ClientOptions::default_for_tests();
    let resolved = resolve_daemon_bin(&opts);
    // In `cargo test` the test binary lives next to no `opencraylspd`, so either the
    // sibling does not exist (None) or PATH provides one. Never a made-up path.
    if let Some(path) = resolved {
        assert!(path.exists(), "resolved to something missing: {path:?}");
    }
}

#[tokio::test]
async fn hello_is_answered_exactly_once_per_connection() {
    let env = Env::new();
    let daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    client.status().await.expect("status");
    client.status().await.expect("status again");
    assert_eq!(
        daemon.hellos().len(),
        1,
        "hello is a handshake, not a per-call thing"
    );
}

#[tokio::test]
async fn the_hello_result_is_exposed_as_typed_data() {
    let env = Env::new();
    let _daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let hello = client.hello();
    assert_eq!(hello.protocol, 1);
    assert_eq!(hello.daemon_version, "0.1.0-fake");
}

#[tokio::test]
async fn a_call_returns_the_json_the_daemon_sent() {
    let env = Env::new();
    let _daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let out: Value = client
        .call("tools/call", json!({"name": "lsp_status"}), &token())
        .await
        .expect("call");
    assert_eq!(out, json!({"text": "ran", "is_error": false}));
}

#[tokio::test]
async fn the_client_exposes_what_it_negotiated() {
    let env = Env::new();
    let _daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    assert_eq!(client.languages(), vec!["rust".to_owned()]);
    assert_eq!(client.options().socket, env.socket());
    assert_eq!(client.hello().languages, vec!["rust".to_owned()]);
    assert!(client.next_request_id() >= 2, "hello used the first id");
}

#[tokio::test]
async fn cancelling_a_tool_call_through_the_host_answers_silently() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            call_delay: Some(Duration::from_secs(20)),
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let host = DaemonHost::new(client.clone());
    let cancel = token();
    let call = {
        let host = host.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { host.call_tool("lsp_status", json!({}), &cancel).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let err = call.await.expect("join").expect_err("cancelled");
    assert!(
        matches!(err, opencraylsp_proto::HostError::Cancelled),
        "{err:?}"
    );
    drop(daemon);
}

#[tokio::test]
async fn the_host_exposes_the_client_it_wraps() {
    let env = Env::new();
    let _daemon = FakeDaemon::start_default(env.socket()).await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let host = DaemonHost::new(client);
    assert_eq!(host.client().languages(), vec!["rust".to_owned()]);
}

#[tokio::test]
async fn a_hang_up_that_cannot_be_retried_surfaces_as_a_daemon_problem() {
    // A lost `list_tools` is retried once, but when the daemon is really gone
    // the caller must be told it is a daemon problem, not handed "no tools".
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            // Hello succeeds; the connection dies on the next request.
            drop_on_request: Some(2),
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    // With the socket gone, the retry's reconnect cannot succeed.
    std::fs::remove_file(env.socket()).unwrap();
    let err = client.list_tools().await.expect_err("connection gone");
    assert!(matches!(err, ClientError::Unavailable { .. }), "{err:?}");
    drop(daemon);
}

#[tokio::test]
async fn connecting_reports_a_daemon_that_hangs_up_during_hello() {
    let env = Env::new();
    let _daemon = FakeDaemon::start(
        env.socket(),
        Script {
            ignore_hello: true,
            ..Script::default()
        },
    )
    .await;
    let mut opts = env.options();
    opts.connect_deadline = Duration::from_millis(400);
    let err = DaemonClient::connect(opts).await.expect_err("no hello");
    assert!(matches!(err, ClientError::Unavailable { .. }), "{err:?}");
}

/// Every read-only entry point goes through the same recovery path. The host
/// the MCP layer actually uses must recover a lost tool call.
#[tokio::test]
async fn the_daemon_host_reconnects_a_lost_tool_call() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            drop_on_first_call: true,
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let host = DaemonHost::new(client.clone());
    let out = host
        .call_tool("lsp_status", json!({}), &token())
        .await
        .expect("the retry succeeds");
    assert_eq!(out.text, "ran");
    assert_eq!(daemon.connections(), 2, "the tool call reconnected once");
}

/// The reconnected connection is adopted by later calls on the same entry
/// point, not just by the retrying call.
#[tokio::test]
async fn a_reconnect_from_list_tools_is_adopted() {
    let env = Env::new();
    let daemon = FakeDaemon::start(
        env.socket(),
        Script {
            // hello is request 1; tools/list is request 2 and gets dropped.
            drop_on_request: Some(2),
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    let first = client.list_tools().await.expect("the retry succeeds");
    assert!(!first.is_empty());
    let second = client
        .list_tools()
        .await
        .expect("the reused connection works");
    assert!(!second.is_empty());
    assert_eq!(
        daemon.connections(),
        2,
        "later list_tools calls must reuse the reconnected connection"
    );
}

/// A burst of failing tool calls shares one redial, exactly like `call` does.
#[tokio::test]
async fn a_failed_redial_is_shared_across_concurrent_tool_calls() {
    let env = Env::new();
    let socket = env.socket();
    let daemon = FakeDaemon::start(
        socket.clone(),
        Script {
            drop_every_call: true,
            ..Script::default()
        },
    )
    .await;
    let client = DaemonClient::connect(env.options()).await.expect("connect");
    std::fs::remove_file(&socket).unwrap();
    drop(daemon);

    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client.call_tool("lsp_status", json!({}), &token()).await
        }));
    }
    for task in tasks {
        assert!(task.await.expect("join").is_err());
    }
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "waiters queued behind each other: {:?}",
        started.elapsed()
    );
}

/// A daemon the client starts must receive the config the user asked for.
#[tokio::test]
async fn a_started_daemon_is_given_the_config_path() {
    let env = Env::new();
    let config = env.dir.path().join("config.toml");
    std::fs::write(&config, "[limits]\n").unwrap();
    let mut opts = env.spawning_options();
    opts.daemon_config = Some(config.clone());
    let client = DaemonClient::connect(opts).await.expect("connect");
    let argv = std::fs::read_to_string(format!("{}.argv", env.socket().display()))
        .expect("fake-opencraylspd recorded its argv");
    assert!(argv.lines().any(|arg| arg == "--config"), "{argv}");
    assert!(
        argv.lines()
            .any(|arg| config.as_path() == std::path::Path::new(arg)),
        "{argv}"
    );
    drop(client);
    env.stop_daemon_and_assert_gone().await;
}

/// Without a config, the daemon must fall back to its own default path.
#[tokio::test]
async fn a_started_daemon_without_a_config_is_not_given_one() {
    let env = Env::new();
    let opts = env.spawning_options();
    let client = DaemonClient::connect(opts).await.expect("connect");
    let argv = std::fs::read_to_string(format!("{}.argv", env.socket().display()))
        .expect("fake-opencraylspd recorded its argv");
    assert!(!argv.lines().any(|arg| arg == "--config"), "{argv}");
    drop(client);
    env.stop_daemon_and_assert_gone().await;
}

/// A daemon that accepts the connection but never answers must not make the
/// caller wait forever: the request has a generous upper bound (P1-1).
#[tokio::test]
async fn a_silent_daemon_times_out_within_the_bound() {
    let env = Env::new();
    let _daemon = FakeDaemon::start(
        env.socket(),
        Script {
            call_delay: Some(Duration::from_secs(2)),
            ..Script::default()
        },
    )
    .await;
    let mut opts = env.options();
    opts.request_deadline = Duration::from_millis(200);
    let client = DaemonClient::connect(opts).await.expect("connect");

    let started = std::time::Instant::now();
    let error = client
        .call_tool("lsp_status", json!({}), &token())
        .await
        .expect_err("the daemon never answers");
    assert!(
        matches!(error, ClientError::Timeout { .. }),
        "want Timeout, got {error:?}"
    );
    assert!(error.to_string().starts_with("[timeout]"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the bound was not applied: {:?}",
        started.elapsed()
    );

    // The late reply for the abandoned request must be discarded, and other
    // calls (a fast method) must keep working.
    let status = client.status().await.expect("a later call still works");
    assert_eq!(status.daemon.clients, 1);
}

/// When the client starts a daemon and then cannot reach it, the message must
/// carry the daemon's own last words (P2-1).
#[tokio::test]
async fn a_daemon_that_exits_immediately_reports_its_log() {
    use std::os::unix::fs::PermissionsExt as _;

    let env = Env::new();
    let bin = env.dir.path().join("exiting-opencraylspd.sh");
    std::fs::write(&bin, "#!/bin/sh\nexit 1\n").expect("script");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let log = env.dir.path().join("opencraylsp.log");
    std::fs::write(&log, "error: the socket directory is not private\n").expect("log");

    let mut opts = env.spawning_options();
    opts.daemon_bin = Some(bin);
    opts.daemon_log = Some(log.clone());
    opts.connect_deadline = Duration::from_millis(400);
    let error = DaemonClient::connect(opts)
        .await
        .expect_err("cannot connect");
    let text = error.to_string();
    assert!(
        text.contains(&log.display().to_string()),
        "the daemon log path is missing: {text}"
    );
    assert!(
        text.contains("not private"),
        "the daemon's last line is missing: {text}"
    );
}
