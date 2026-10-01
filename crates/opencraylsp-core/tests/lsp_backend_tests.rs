//! Integration tests: the production `LspBackend` against the
//! fake language server over real stdio.
//!
//! These tests need the `fake-lsp-server` binary, which only exists with the
//! `test-fake-lsp` feature: run them as
//! `cargo test -p opencraylsp-core --features test-fake-lsp`. Without the
//! feature this file compiles to nothing, so the default `cargo test` (and
//! the release graphs) stay untouched.
//!
//! Every row of the contract's failure table has a test here:
//! missing command, startup timeout, restart cap, exhausted `-32801`,
//! cancellation that keeps the server, and shutdown that leaves no child.
//! The three `real_rust_analyzer_*` tests are `#[ignore]`d live checks
//! against the checkout itself.

// Test diagnostics (a hung test's name, a skipped live check) go to stderr.
#![allow(clippy::print_stderr)]
#![cfg(feature = "test-fake-lsp")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use opencraylsp_core::{BoundBackend, LspBackend, LspConfig, LspError, PositionEncoding};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn fake_bin() -> String {
    env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()
}

/// Aborts the whole test process, naming the test, when one test runs longer
/// than `TEST_DEADLINE`. Without it a single hung test stalls `cargo test`
/// silently until an outer timeout kills everything with no name attached.
struct Watchdog(Arc<std::sync::atomic::AtomicBool>);

const TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

impl Watchdog {
    fn arm() -> Self {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        let name = std::thread::current().name().unwrap_or("?").to_owned();
        std::thread::spawn(move || {
            std::thread::sleep(TEST_DEADLINE);
            if !flag.load(std::sync::atomic::Ordering::SeqCst) {
                eprintln!("TEST HUNG (> {TEST_DEADLINE:?}): {name}");
                std::process::abort();
            }
        });
        Self(done)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A scratch workspace: `root` is the server workspace override, `file` is a
/// source file inside it. Files live under the OS temp dir; `allowed_roots`
/// admits them past the boundary check.
struct Scratch {
    _dir: tempfile::TempDir,
    root: PathBuf,
    _watchdog: Watchdog,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            root: dir.path().to_owned(),
            _dir: dir,
            _watchdog: Watchdog::arm(),
        }
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }
}

/// Builds a manager with one `fake` server. `extra` holds per-test fake flags
/// plus optional `[lsp]` overrides (`startup_timeout_ms`, `max_restarts`,
/// `idle_shutdown_secs`, `diagnostics_settle_ms`, `diagnostics_timeout_ms`,
/// `request_timeout_ms`, `encoding`, `settings_json`).
struct Harness {
    tag: String,
    fake_args: Vec<String>,
    lsp_overrides: Vec<String>,
    settings_json: Option<String>,
    root_markers: Vec<String>,
}

static HARNESS_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Default for Harness {
    fn default() -> Self {
        let seq = HARNESS_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            // Unique per test: tests run in parallel, and the no-zombie
            // `pgrep` check must only see this test's own child.
            tag: format!("lsp-b-{}-{seq}", std::process::id()),
            fake_args: Vec::new(),
            lsp_overrides: Vec::new(),
            settings_json: None,
            root_markers: Vec::new(),
        }
    }
}

impl Harness {
    fn manager(&self, scratch: &Scratch) -> Arc<BoundBackend> {
        let mut args = vec![
            format!("--tag={}", self.tag),
            format!(
                "--record-events={}",
                scratch.root.join("events.log").display()
            ),
        ];
        args.extend(self.fake_args.clone());
        let args_toml = args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let markers = self
            .root_markers
            .iter()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let settings = self
            .settings_json
            .clone()
            .map(|s| format!("settings = {s}"))
            .unwrap_or_default();
        // Overrides are written in the old flat style; route each key to the
        // table it lives in now (`warmup*`/`watch_*` are top-level keys, the
        // rest belong under `[limits]`).
        let (top, limits): (Vec<&String>, Vec<&String>) = self
            .lsp_overrides
            .iter()
            .partition(|o| o.starts_with("warmup") || o.starts_with("watch_interval_ms"));
        let src = format!(
            "{}\nallowed_roots = [ {:?} ]\n[limits]\nstartup_grace_ms = 0\n{}\n\
             [[server]]\nname = \"fake\"\ncommand = {:?}\nargs = [ {args_toml} ]\n\
             extensions = {{ fl = \"fake\" }}\nroot_markers = [ {markers} ]\n\
             workspace = {:?}\n{settings}\n",
            top.iter()
                .map(|o| o.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            scratch.root.display().to_string(),
            limits
                .iter()
                .map(|o| o.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            fake_bin(),
            scratch.root.display().to_string(),
        );
        let config =
            Arc::new(LspConfig::from_toml_str_without_presets(&src).expect("test config parses"));
        assert!(
            config.servers.contains_key("fake"),
            "test config must parse"
        );
        BoundBackend::standalone(config)
    }
}

fn events(scratch: &Scratch) -> String {
    std::fs::read_to_string(scratch.root.join("events.log")).unwrap_or_default()
}

fn definition_params(uri: &str) -> Value {
    json!({
        "textDocument": {"uri": uri},
        "position": {"line": 0, "character": 1},
    })
}

fn file_uri(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

/// No child with `tag` may exist. `pgrep -f` matches the fake's command
/// line, which carries the test-unique `--tag=`.
///
/// The pattern is anchored on both sides on purpose: tags are
/// `lsp-b-<pid>-<seq>`, so a bare `lsp-b-123-1` also matches the *different*
/// tag `lsp-b-123-10` (and `-2` matches `-20`…), which made this check fail
/// whenever a sibling test's child was still alive — the suite went red in
/// parallel runs and green with `--test-threads=1`.
fn assert_no_fake_child(tag: &str) {
    let out = std::process::Command::new("pgrep")
        .args(["-f", &format!("tag={tag}( |$)")])
        .output()
        .expect("pgrep must exist for the no-zombie check");
    assert!(
        !out.status.success(),
        "a fake-lsp-server child is still alive after shutdown"
    );
}

#[tokio::test]
async fn request_roundtrip_returns_fake_definition() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "let x = 1\n");
    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    let served = backend
        .request(
            &file,
            "textDocument/definition",
            definition_params(&file_uri(&file)),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(served.server, "fake");
    assert_eq!(served.root, scratch.root);
    assert_eq!(served.encoding, PositionEncoding::Utf32);
    assert_eq!(served.value[0]["uri"], json!(file_uri(&file)));

    let log = events(&scratch);
    assert!(
        log.contains("request:initialize"),
        "expected initialize, got:\n{log}"
    );
    assert!(log.contains("didOpen"), "expected didOpen, got:\n{log}");
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn missing_command_reports_server_not_installed() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let src = format!(
        "allowed_roots = [ {:?} ]\n\
         [[server]]\nname = \"fake\"\ncommand = \"definitely-not-installed-ls-xyz\"\n\
         extensions = {{ fl = \"fake\" }}\nworkspace = {:?}\n",
        scratch.root.display().to_string(),
        scratch.root.display().to_string(),
    );
    let backend = BoundBackend::standalone(Arc::new(
        LspConfig::from_toml_str_without_presets(&src).unwrap(),
    ));
    let err = backend
        .request(
            &file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .expect_err("missing command must fail");
    assert!(
        matches!(err, LspError::ServerNotInstalled { .. }),
        "unexpected: {err:?}"
    );
    assert!(err.to_string().contains("definitely-not-installed-ls-xyz"));
    backend.shutdown().await;
}

#[tokio::test]
async fn startup_timeout_then_restart_cap() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--delay-ms=1500".to_owned()],
        lsp_overrides: vec![
            "startup_timeout_ms = 200".to_owned(),
            "max_restarts = 0".to_owned(),
        ],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    let first = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .expect_err("slow initialize must time out");
    assert!(
        matches!(first, LspError::Timeout { .. }),
        "unexpected: {first:?}"
    );
    assert!(first.to_string().contains("initialize"));

    // One failure already spent the (zero) restart budget: no more spawns.
    let second = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .expect_err("restart cap must refuse");
    assert!(
        matches!(second, LspError::ServerFailed { .. }),
        "unexpected: {second:?}"
    );
    let third = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .expect_err("refusal must stick");
    assert_eq!(
        second.to_string(),
        third.to_string(),
        "no new spawn may happen"
    );
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn crashing_server_hits_restart_cap() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--crash-after=0".to_owned()],
        lsp_overrides: vec!["max_restarts = 1".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    let first = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .expect_err("immediate crash must fail");
    assert!(matches!(first, LspError::Io(_)), "unexpected: {first:?}");
    let second = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .expect_err("second crash spends the budget");
    assert!(matches!(second, LspError::Io(_)), "unexpected: {second:?}");
    let third = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .expect_err("cap exceeded");
    assert!(
        matches!(third, LspError::ServerFailed { .. }),
        "unexpected: {third:?}"
    );
    assert!(third.to_string().contains("restarted 2 time(s)"));
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn mid_request_crash_errors_then_restarts() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--die-during=textDocument/hover".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    let err = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .expect_err("dying mid-request must fail");
    assert!(matches!(err, LspError::Io(_)), "unexpected: {err:?}");
    assert!(err.to_string().contains("exited"), "unexpected: {err}");

    // The next request restarts transparently and succeeds.
    let served = backend
        .request(
            &file,
            "textDocument/definition",
            definition_params(&file_uri(&file)),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(served.value[0]["uri"], json!(file_uri(&file)));
    assert_eq!(events(&scratch).matches("request:initialize").count(), 2);
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn content_modified_retries_then_succeeds() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--fail-32801=2".to_owned()],
        lsp_overrides: vec!["request_timeout_ms = 10000".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);

    let started = Instant::now();
    let served = backend
        .request(
            &file,
            "textDocument/definition",
            definition_params(&file_uri(&file)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(served.value[0]["uri"], json!(file_uri(&file)));
    // Two backoff sleeps (500 + 1000 ms) must have happened.
    assert!(
        started.elapsed().as_millis() >= 1400,
        "retries did not back off: {:?}",
        started.elapsed()
    );
    let log = events(&scratch);
    assert_eq!(log.matches("-> -32801").count(), 2, "log:\n{log}");
    backend.shutdown().await;
}

#[tokio::test]
async fn content_modified_exhausted_reports_indexing() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--fail-32801=99".to_owned()],
        lsp_overrides: vec!["request_timeout_ms = 10000".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);

    let err = backend
        .request(
            &file,
            "textDocument/definition",
            definition_params(&file_uri(&file)),
            &CancellationToken::new(),
        )
        .await
        .expect_err("endless -32801 must surface");
    match err {
        LspError::Rpc { code, message, .. } => {
            assert_eq!(code, -32801);
            assert!(message.contains("indexing"), "unexpected: {message}");
        }
        other => panic!("unexpected: {other:?}"),
    }
    backend.shutdown().await;
}

#[tokio::test]
async fn initialize_retries_content_modified() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--fail-init-32801=1".to_owned()],
        lsp_overrides: vec!["request_timeout_ms = 10000".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);

    let served = backend
        .request(
            &file,
            "textDocument/definition",
            definition_params(&file_uri(&file)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(served.value[0]["uri"], json!(file_uri(&file)));
    assert!(events(&scratch).contains("request:initialize -> -32801"));
    backend.shutdown().await;
}

#[tokio::test]
async fn cancel_abandons_wait_and_keeps_server() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec![
            "--delay-ms=3000".to_owned(),
            "--delay-method=textDocument/hover".to_owned(),
        ],
        lsp_overrides: vec!["request_timeout_ms = 10000".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);

    let cancel = CancellationToken::new();
    let backend_clone = backend.clone();
    let file_clone = file.clone();
    let waiter_cancel = cancel.clone();
    let waiting = tokio::spawn(async move {
        backend_clone
            .request(&file_clone, "textDocument/hover", json!({}), &waiter_cancel)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel.cancel();
    let outcome = waiting.await.unwrap();
    assert_eq!(outcome, Err(LspError::Cancelled));

    // The server survived the cancellation: only one initialize ever happened,
    // and the next request succeeds on the same child.
    let served = backend
        .request(
            &file,
            "textDocument/definition",
            definition_params(&file_uri(&file)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(served.value[0]["uri"], json!(file_uri(&file)));
    assert_eq!(events(&scratch).matches("request:initialize").count(), 1);
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn workspace_configuration_answers_settings() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let record = scratch.root.join("config_reply.json");
    let harness = Harness {
        fake_args: vec![
            "--ask-config".to_owned(),
            format!("--record-config={}", record.display()),
        ],
        settings_json: Some(r#"{ fake = { mode = "fast" } }"#.to_owned()),
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);

    backend
        .request(
            &file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let reply = std::fs::read_to_string(&record).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap(),
        json!([{"fake": {"mode": "fast"}}])
    );
    backend.shutdown().await;
}

#[tokio::test]
async fn diagnostics_error_then_clean() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "let broken token\n");
    let harness = Harness {
        fake_args: vec!["--push-diagnostics".to_owned()],
        lsp_overrides: vec![
            "diagnostics_settle_ms = 100".to_owned(),
            "diagnostics_timeout_ms = 8000".to_owned(),
        ],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    let report = backend.diagnostics(&file, &cancel).await.unwrap();
    assert!(report.received_for_version, "must have seen this version");
    assert!(!report.timed_out);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].message, "fake error: unexpected token");

    std::fs::write(&file, "// CLEAN\n").unwrap();
    let report = backend.diagnostics(&file, &cancel).await.unwrap();
    assert!(report.received_for_version);
    assert!(!report.timed_out);
    assert!(report.items.is_empty(), "fixed file must read clean");

    let log = events(&scratch);
    assert!(log.contains("didOpen"), "log:\n{log}");
    assert!(log.contains("didChange"), "log:\n{log}");
    assert!(log.contains("didSave"), "log:\n{log}");
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn diagnostics_timeout_returns_cache_marked() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        // No --push-diagnostics: the server stays silent.
        lsp_overrides: vec![
            "diagnostics_settle_ms = 50".to_owned(),
            "diagnostics_timeout_ms = 400".to_owned(),
        ],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);

    let report = backend
        .diagnostics(&file, &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !report.received_for_version,
        "silence is unknown, not clean"
    );
    assert!(report.timed_out);
    assert!(report.items.is_empty());
    backend.shutdown().await;
}

#[tokio::test]
async fn edit_resync_sends_didchange_with_new_version() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "version one\n");
    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();
    let params = || definition_params(&file_uri(&file));

    backend
        .request(&file, "textDocument/definition", params(), &cancel)
        .await
        .unwrap();
    std::fs::write(&file, "version two\n").unwrap();
    backend
        .request(&file, "textDocument/definition", params(), &cancel)
        .await
        .unwrap();

    let log = events(&scratch);
    assert!(log.contains("didOpen"), "log:\n{log}");
    assert!(
        log.lines()
            .any(|l| l.starts_with("didChange") && l.ends_with(" 2")),
        "expected didChange version 2, log:\n{log}"
    );
    backend.shutdown().await;
}

#[tokio::test]
async fn deleted_file_sends_didclose() {
    let scratch = Scratch::new();
    let target = scratch.write("a.fl", "x\n");
    let doomed = scratch.write("b.fl", "y\n");
    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    backend
        .request(&doomed, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    std::fs::remove_file(&doomed).unwrap();
    backend
        .request(&target, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();

    let uri = file_uri(&doomed);
    let log = events(&scratch);
    assert!(
        log.lines().any(|l| l == format!("didClose {uri}")),
        "expected didClose, log:\n{log}"
    );
    backend.shutdown().await;
}

#[tokio::test]
async fn shutdown_reaps_child_and_restarts_cleanly() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    backend.shutdown().await;
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);

    // A query after shutdown restarts the server instead of using a corpse.
    backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(events(&scratch).matches("request:initialize").count(), 2);
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn idle_shutdown_reaps_quiet_server() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        lsp_overrides: vec!["idle_shutdown_secs = 1".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    // This request sweeps the idle instance first, then restarts it.
    backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(
        events(&scratch).matches("request:initialize").count(),
        2,
        "the idle server must have been shut down and restarted"
    );
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn root_markers_select_topmost_root() {
    let scratch = Scratch::new();
    scratch.write("proj/Fake.toml", "[workspace]");
    scratch.write("proj/member/Fake.toml", "[package]");
    let nested = scratch.write("proj/member/src/a.fl", "x\n");
    let other = scratch.write("other/b.fl", "y\n");
    let harness = Harness {
        root_markers: vec!["Fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();

    let served = backend
        .request(&nested, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(served.root, scratch.root.join("proj"));
    let served = backend
        .request(&other, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(served.root, scratch.root);
    backend.shutdown().await;
}

#[tokio::test]
async fn encoding_follows_server_capabilities() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let cancel = CancellationToken::new();

    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let served = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(served.encoding, PositionEncoding::Utf32);
    backend.shutdown().await;

    let harness = Harness {
        fake_args: vec!["--encoding=utf-16".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let served = backend
        .request(&file, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(served.encoding, PositionEncoding::Utf16);
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn unknown_extension_is_an_honest_error() {
    let scratch = Scratch::new();
    let file = scratch.write("a.zzz9", "x\n");
    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let err = backend
        .request(
            &file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .expect_err("no server handles .zzz9");
    assert!(matches!(err, LspError::NoServerConfigured { .. }));
    backend.shutdown().await;
}

#[tokio::test]
async fn concurrent_first_queries_share_one_spawn() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let mut handles = Vec::new();
    for _ in 0..5 {
        let backend = backend.clone();
        let file = file.clone();
        handles.push(tokio::spawn(async move {
            backend
                .request(
                    &file,
                    "textDocument/hover",
                    json!({}),
                    &CancellationToken::new(),
                )
                .await
        }));
    }
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    assert_eq!(
        events(&scratch).matches("request:initialize").count(),
        1,
        "concurrent first queries must share one child"
    );
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn non_retryable_error_maps_to_rpc() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--fail-method=textDocument/hover".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let err = backend
        .request(
            &file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .expect_err("a -32602 must surface, never retried");
    match err {
        LspError::Rpc { code, message, .. } => {
            assert_eq!(code, -32602);
            assert!(message.contains("fake failure"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    // One attempt only: no -32801-style retry for non-transient errors.
    assert_eq!(
        events(&scratch)
            .matches("request:textDocument/hover")
            .count(),
        1
    );
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn initialize_error_maps_to_rpc() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--fail-method=initialize".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let err = backend
        .request(
            &file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .expect_err("initialize failure must surface");
    assert!(matches!(err, LspError::Rpc { .. }), "unexpected: {err:?}");
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn crash_during_didopen_errors_honestly() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--die-during=textDocument/didOpen".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let err = backend
        .request(
            &file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .expect_err("dying during didOpen must fail");
    assert!(matches!(err, LspError::Io(_)), "unexpected: {err:?}");
    // `didOpen` is a notification: nothing answers it, so the death surfaces on
    // the request that follows. What matters is an honest "exited" error, not
    // a silent empty result.
    assert!(err.to_string().contains("exited"), "unexpected: {err}");
    assert!(events(&scratch).contains("dying during textDocument/didOpen"));
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn cancel_during_startup_abandons_start() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--delay-ms=2000".to_owned()],
        lsp_overrides: vec!["startup_timeout_ms = 30000".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();
    let backend_clone = backend.clone();
    let file_clone = file.clone();
    let cancel_clone = cancel.clone();
    let waiting = tokio::spawn(async move {
        backend_clone
            .request(&file_clone, "textDocument/hover", json!({}), &cancel_clone)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel.cancel();
    assert_eq!(waiting.await.unwrap(), Err(LspError::Cancelled));
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn extra_server_requests_answered_with_null() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--ask-extra".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    backend
        .request(
            &file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let log = events(&scratch);
    assert!(
        log.contains("client/registerCapability -> null"),
        "log:\n{log}"
    );
    assert!(
        log.contains("window/workDoneProgress/create -> null"),
        "log:\n{log}"
    );
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn missing_file_is_an_honest_io_error() {
    let scratch = Scratch::new();
    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let ghost = scratch.root.join("ghost.fl");
    let err = backend
        .request(
            &ghost,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .expect_err("a file nobody can read must fail");
    assert!(matches!(err, LspError::Io(_)), "unexpected: {err:?}");
    assert!(err.to_string().contains("cannot read"), "unexpected: {err}");
    backend.shutdown().await;
    assert_no_fake_child(&harness.tag);
}

#[tokio::test]
async fn pre_cancelled_request_short_circuits() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness::default();
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        backend
            .request(&file, "textDocument/hover", json!({}), &cancel)
            .await,
        Err(LspError::Cancelled)
    );
    // Nothing was spawned for a call that never started.
    assert!(!scratch.root.join("events.log").exists());
    backend.shutdown().await;
}

#[tokio::test]
async fn shutdown_kills_exit_ignoring_server() {
    let scratch = Scratch::new();
    let file = scratch.write("a.fl", "x\n");
    let harness = Harness {
        fake_args: vec!["--ignore-exit".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    backend
        .request(
            &file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    backend.shutdown().await;
    assert!(events(&scratch).contains("ignored exit"));
    assert_no_fake_child(&harness.tag);
}

// --- Live checks against a real rust-analyzer (ignored by default) ---//
// These run the production backend against this checkout itself. They need
// `rust-analyzer` on PATH and take a while (first index pass), so they stay
// `#[ignore]`d: run explicitly with
// `cargo test -p opencraylsp-core --features test-fake-lsp -- --ignored`.

/// Locates a working rust-analyzer: `$RUST_ANALYZER` wins, then plain PATH
/// lookup. Skips the test when none answers —
/// these are live environment checks, not hermetic tests.
fn rust_analyzer_cmd() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(cmd) = std::env::var("RUST_ANALYZER") {
        candidates.push(cmd);
    }
    candidates.push("rust-analyzer".to_owned());
    candidates.into_iter().find(|cmd| {
        std::process::Command::new(cmd)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    })
}

/// Points the production backend at `root` with the real `rust-analyzer`.
fn real_rust_backend(root: &Path) -> Option<Arc<BoundBackend>> {
    let command = rust_analyzer_cmd()?;
    let src = format!(
        "allowed_roots = [ {:?} ]\n[limits]\nstartup_grace_ms = 0\nstartup_timeout_ms = 180000\n\
         request_timeout_ms = 60000\ndiagnostics_settle_ms = 1000\n\
         diagnostics_timeout_ms = 60000\n\
         [[server]]\nname = \"rust\"\ncommand = {:?}\n\
         extensions = {{ rs = \"rust\" }}\nroot_markers = [ \"Cargo.toml\" ]\n\
         workspace = {:?}\n\
         initialization_options = {{ files = {{ watcher = \"server\" }} }}\n",
        root.display().to_string(),
        command,
        root.display().to_string(),
    );
    Some(BoundBackend::standalone(Arc::new(
        LspConfig::from_toml_str_without_presets(&src).unwrap(),
    )))
}

/// The workspace root of this checkout: three levels up from this file's
/// package (`crates/opencraylsp-core`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .to_owned()
}

/// Polls one request until it returns a non-empty array or the budget runs
/// out. A real server answers `null` (not `-32801`) while its first index
/// pass is still running; polling here is what an agent would do, and keeps
/// these live checks from flaking on a cold cache.
async fn poll_non_empty(
    backend: &Arc<BoundBackend>,
    file: &Path,
    method: &str,
    params: Value,
    attempts: u32,
) -> Vec<Value> {
    for attempt in 1..=attempts {
        match backend
            .request(file, method, params.clone(), &CancellationToken::new())
            .await
        {
            Ok(served) => {
                let items = served.value.as_array().cloned().unwrap_or_default();
                if !items.is_empty() {
                    return items;
                }
            }
            // The backend reports an unfinished first index pass honestly.
            Err(LspError::Indexing { .. }) => {}
            Err(other) => panic!("{method}: {other}"),
        }
        eprintln!("{method} attempt {attempt}/{attempts}: empty, server still indexing");
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
    Vec::new()
}

/// Finds the definition line of `needle` (a `fn name(` declaration) in `file`.
fn find_def_line(file: &Path, needle: &str) -> (usize, usize) {
    let text = std::fs::read_to_string(file).unwrap();
    for (index, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (index + 1, col + 4);
        }
    }
    panic!("{needle} not found in {}", file.display());
}

#[tokio::test]
#[ignore]
async fn real_rust_analyzer_definition() {
    let root = workspace_root();
    let Some(backend) = real_rust_backend(&root) else {
        eprintln!("SKIP: no working rust-analyzer found");
        return;
    };
    let file = root.join("crates/opencraylsp-core/src/config.rs");
    let text = std::fs::read_to_string(&file).unwrap();
    // A *use* of the symbol, not its declaration: the second occurrence.
    let mut uses = text.match_indices("server_for_extension(");
    let _ = uses.next();
    let (offset, _) = uses.next().expect("need a call site");
    let line = text[..offset].lines().count();
    let col = offset - text[..offset].rfind('\n').unwrap_or(0);
    let uri = file_uri(&file);
    let targets = poll_non_empty(
        &backend,
        &file,
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": line - 1, "character": col},
        }),
        18,
    )
    .await;
    assert!(!targets.is_empty(), "rust-analyzer returned no definition");
    let (def_line, _) = find_def_line(&file, "fn server_for_extension(");
    // rust-analyzer answers with LocationLink (`targetUri` /
    // `targetSelectionRange`); simpler servers use Location (`uri` / `range`).
    let points_at_def = targets.iter().any(|t| {
        let uri_hit = t["targetUri"] == json!(uri) || t["uri"] == json!(uri);
        let line_hit = t["targetSelectionRange"]["start"]["line"].as_u64()
            == Some(def_line as u64 - 1)
            || t["range"]["start"]["line"].as_u64() == Some(def_line as u64 - 1);
        uri_hit && line_hit
    });
    assert!(points_at_def, "definition misses: {targets:?}");
    backend.shutdown().await;
}

#[tokio::test]
#[ignore]
async fn real_rust_analyzer_references() {
    let root = workspace_root();
    let Some(backend) = real_rust_backend(&root) else {
        eprintln!("SKIP: no working rust-analyzer found");
        return;
    };
    let file = root.join("crates/opencraylsp-core/src/config.rs");
    let (line, col) = find_def_line(&file, "fn server_for_extension(");
    let uri = file_uri(&file);
    let refs = poll_non_empty(
        &backend,
        &file,
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": line - 1, "character": col},
            "context": {"includeDeclaration": false},
        }),
        18,
    )
    .await;
    assert!(!refs.is_empty(), "rust-analyzer returned no references");
    backend.shutdown().await;
}

#[tokio::test]
#[ignore]
async fn real_rust_analyzer_diagnostics() {
    let root = workspace_root();
    let Some(backend) = real_rust_backend(&root) else {
        eprintln!("SKIP: no working rust-analyzer found");
        return;
    };
    let file = root.join("crates/opencraylsp-core/src/config.rs");
    let mut report = None;
    for attempt in 1..=30 {
        match backend.diagnostics(&file, &CancellationToken::new()).await {
            Ok(done) => {
                report = Some(done);
                break;
            }
            Err(LspError::Indexing { .. }) => {
                eprintln!("diagnostics attempt {attempt}/30: still indexing");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
            Err(other) => panic!("diagnostics: {other}"),
        }
    }
    let report = report.expect("rust-analyzer never finished indexing");
    // The file compiles, so once the server has actually analyzed this
    // version there must be no error-severity items left.
    assert!(
        report.received_for_version || report.timed_out,
        "diagnostics returned neither a versioned report nor a marked timeout"
    );
    backend.shutdown().await;
}

#[tokio::test]
async fn a_workspace_request_opens_a_probe_file_when_nothing_is_open() {
    let scratch = Scratch::new();
    scratch.write("src/probe.fl", "fn probe() {}\n");
    let backend = Harness::default().manager(&scratch);
    // tsserver answers workspace/symbol with "No Project" until some file is
    // open, so opencraylspd opens one of the server's own files first.
    let _ = backend
        .request_workspace(
            "fake",
            "workspace/symbol",
            json!({"query": "probe"}),
            &CancellationToken::new(),
        )
        .await;
    assert!(
        wait_for_event(&scratch, "didOpen", 3000).await,
        "no probe file was opened: {}",
        events(&scratch)
    );
    backend.shutdown().await;
}

/// Polls the fake's event log until `needle` shows up or `ms` elapse.
async fn wait_for_event(scratch: &Scratch, needle: &str, ms: u64) -> bool {
    let deadline = Instant::now() + std::time::Duration::from_millis(ms);
    while Instant::now() < deadline {
        if events(scratch).contains(needle) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// With `warmup = true` the server starts and opens a file
/// before anyone asks anything, and the watcher reports later file changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warmup_starts_without_a_request_and_watcher_reports_changes() {
    let scratch = Scratch::new();
    scratch.write("proj/fake.toml", "");
    scratch.write("proj/src/a.fl", "let x = 1\n");
    let harness = Harness {
        lsp_overrides: vec![
            "warmup = true".to_owned(),
            "watch_interval_ms = 100".to_owned(),
        ],
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    backend.spawn_background();

    assert!(
        wait_for_event(&scratch, "request:initialize", 5_000).await,
        "warm-up must start the server with no request; log:\n{}",
        events(&scratch)
    );
    assert!(
        wait_for_event(&scratch, "didOpen", 5_000).await,
        "warm-up must open a sample file"
    );

    // Let the watcher record its baseline, then change the tree.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    scratch.write("proj/src/b.fl", "let y = 2\n");
    assert!(
        wait_for_event(&scratch, "didChangeWatchedFiles", 5_000).await,
        "watcher must report the new file; log:\n{}",
        events(&scratch)
    );
    backend.shutdown().await;
    drop(backend);
    assert_no_fake_child(&harness.tag);
}

/// Warm-up is opt-in: without it nothing spawns until the first request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_warmup_means_no_spawn() {
    let scratch = Scratch::new();
    scratch.write("proj/fake.toml", "");
    scratch.write("proj/src/a.fl", "let x = 1\n");
    let harness = Harness {
        lsp_overrides: vec!["watch_interval_ms = 100".to_owned()],
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    backend.spawn_background();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!events(&scratch).contains("request:initialize"));
    assert_no_fake_child(&harness.tag);
}

// ---- a workspace-level request and the project root it picks ----------------
//
// A request with no file to anchor to has to pick a project root some other way.
// Picking the boundary is only right when the boundary *is* a project: with the
// marker one directory down, the boundary is not a rust-analyzer project, and a
// server started there indexes nothing while costing gigabytes.

/// The roots of every instance the manager currently holds.
async fn roots_of(backend: &BoundBackend) -> Vec<String> {
    let mut roots: Vec<String> = backend
        .status()
        .await
        .instances
        .iter()
        .map(|i| i.root.clone())
        .collect();
    roots.sort();
    roots
}

#[tokio::test]
async fn a_workspace_request_whose_boundary_is_not_a_project_uses_the_project_below() {
    let scratch = Scratch::new();
    // The boundary holds no marker; the only project is one directory down.
    scratch.write("proj/fake.toml", "[workspace]");
    let probe = scratch.write("proj/src/a.fl", "fn probe() {}\n");
    let harness = Harness {
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let served = backend
        .request_workspace(
            "fake",
            "workspace/symbol",
            json!({"query": "probe"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        served.root,
        scratch.root.join("proj"),
        "a workspace request must land on the project, not on the boundary"
    );
    assert_eq!(
        roots_of(&backend).await,
        vec![scratch.root.join("proj").display().to_string()],
        "and must not leave a second, useless server running on the boundary"
    );
    let log = events(&scratch);
    assert!(
        log.lines()
            .any(|line| line.starts_with("didOpen ") && line.contains(&probe.display().to_string())),
        "the probe file of the chosen project must be opened: {log}"
    );
    backend.shutdown().await;
}

/// Two projects under the boundary: a workspace request has no file to tell
/// them apart, so it must refuse rather than pick one at random.
#[tokio::test]
async fn a_workspace_request_with_several_projects_below_says_which_to_choose() {
    let scratch = Scratch::new();
    scratch.write("one/fake.toml", "[workspace]");
    scratch.write("one/src/a.fl", "fn a() {}\n");
    scratch.write("two/fake.toml", "[workspace]");
    scratch.write("two/src/b.fl", "fn b() {}\n");
    let harness = Harness {
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let err = backend
        .request_workspace(
            "fake",
            "workspace/symbol",
            json!({"query": "x"}),
            &CancellationToken::new(),
        )
        .await
        .expect_err("two projects cannot be told apart without a path");
    let text = err.to_string();
    assert!(text.contains("one"), "{text}");
    assert!(text.contains("two"), "{text}");
    assert!(
        roots_of(&backend).await.is_empty(),
        "and no server may be started for a request that cannot be routed"
    );
    backend.shutdown().await;
}

/// No project at all under the boundary: there is nothing to start, and saying
/// so is more use than a server that answers nothing.
#[tokio::test]
async fn a_workspace_request_with_no_project_below_says_so() {
    let scratch = Scratch::new();
    scratch.write("notes/a.fl", "fn a() {}\n");
    let harness = Harness {
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let err = backend
        .request_workspace(
            "fake",
            "workspace/symbol",
            json!({"query": "x"}),
            &CancellationToken::new(),
        )
        .await
        .expect_err("there is no project to route to");
    assert!(
        err.to_string().contains("no "),
        "the message must say there is none: {err}"
    );
    assert!(
        roots_of(&backend).await.is_empty(),
        "and no server may be started"
    );
    backend.shutdown().await;
}

/// An instance is already running for one of the projects: a later workspace
/// request reuses the most recently used one rather than starting a second.
#[tokio::test]
async fn a_workspace_request_reuses_the_running_instance() {
    let scratch = Scratch::new();
    scratch.write("one/fake.toml", "[workspace]");
    let in_one = scratch.write("one/src/a.fl", "fn a() {}\n");
    scratch.write("two/fake.toml", "[workspace]");
    scratch.write("two/src/b.fl", "fn b() {}\n");
    let harness = Harness {
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();
    let served = backend
        .request(&in_one, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(served.root, scratch.root.join("one"));
    assert_eq!(roots_of(&backend).await.len(), 1);

    // The boundary cannot be told apart from `one`, but `one` is already up, so
    // the request goes there and no second server appears.
    backend
        .request_workspace("fake", "workspace/symbol", json!({"query": "a"}), &cancel)
        .await
        .unwrap();
    assert_eq!(
        roots_of(&backend).await,
        vec![scratch.root.join("one").display().to_string()],
        "the running instance must be reused, not joined by a second"
    );
    backend.shutdown().await;
}

/// A boundary that *is* a project keeps working: the loose-file case (a pile of
/// .py files with no pyproject.toml) has no project below it either, and there
/// the boundary is the only sensible root.
#[tokio::test]
async fn a_boundary_that_is_itself_a_project_is_still_the_root() {
    let scratch = Scratch::new();
    scratch.write("fake.toml", "[workspace]");
    scratch.write("src/a.fl", "fn probe() {}\n");
    let harness = Harness {
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let served = backend
        .request_workspace(
            "fake",
            "workspace/symbol",
            json!({"query": "probe"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(served.root, scratch.root);
    assert_eq!(roots_of(&backend).await.len(), 1);
    backend.shutdown().await;
}

/// No markers configured at all (a server that works file by file): the boundary
/// is the root, because that is the only thing that can be said.
#[tokio::test]
async fn without_root_markers_the_boundary_is_the_root() {
    let scratch = Scratch::new();
    scratch.write("src/a.fl", "fn probe() {}\n");
    let backend = Harness::default().manager(&scratch);
    let served = backend
        .request_workspace(
            "fake",
            "workspace/symbol",
            json!({"query": "probe"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(served.root, scratch.root);
    backend.shutdown().await;
}

/// Two instances are up and the boundary cannot be told apart from either: the
/// most *recently* used one wins. The direction of that comparison is the whole
/// point, so the test uses two instances and asks for the second one — a
/// one-instance workspace cannot tell "most recent" from "any".
#[tokio::test]
async fn a_workspace_request_prefers_the_most_recently_used_instance() {
    let scratch = Scratch::new();
    scratch.write("one/fake.toml", "[workspace]");
    let in_one = scratch.write("one/src/a.fl", "fn a() {}\n");
    scratch.write("two/fake.toml", "[workspace]");
    let in_two = scratch.write("two/src/b.fl", "fn b() {}\n");
    let harness = Harness {
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();
    backend
        .request(&in_one, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    // A real gap: `Instant` resolution must not hide which instance is newer.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    backend
        .request(&in_two, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(roots_of(&backend).await.len(), 2, "two instances are up");

    let served = backend
        .request_workspace("fake", "workspace/symbol", json!({"query": "b"}), &cancel)
        .await
        .unwrap();
    assert_eq!(
        served.root,
        scratch.root.join("two"),
        "the instance used most recently must be the one reused"
    );
    assert_eq!(roots_of(&backend).await.len(), 2, "and no third may appear");
    backend.shutdown().await;
}

/// The candidate retry a `no_project` answer prints has to be usable as it
/// stands. It points at a *file*, not at the project directory: the server is
/// chosen by file extension, so a directory has none and the copy-pasted call
/// would fail with "no LSP server is configured for . files".
#[tokio::test]
async fn a_no_project_candidate_is_a_file_the_call_can_actually_use() {
    let scratch = Scratch::new();
    scratch.write("one/fake.toml", "[workspace]");
    scratch.write("one/src/a.fl", "fn helper() {}\n");
    scratch.write("two/fake.toml", "[workspace]");
    scratch.write("two/src/b.fl", "fn helper() {}\n");
    let harness = Harness {
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();
    let err = backend
        .request_workspace(
            "fake",
            "workspace/symbol",
            json!({"query": "helper"}),
            &cancel,
        )
        .await
        .expect_err("two projects, no path");

    // Pull one retry out of the message and make that exact call.
    let retry = err
        .to_string()
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .expect("the answer must print a ready-to-use call")
        .to_owned();
    let args: Value = serde_json::from_str(&retry).expect("the retry parses as JSON");
    let path = args["path"].as_str().expect("a path").to_owned();
    assert!(
        scratch.root.join(&path).is_file(),
        "`{path}` must be a file inside the workspace, since the server is          chosen by extension and a directory has none"
    );

    // The retry is a *position* target, so it is fed to the file-anchored tools
    // a model would reach for next (`lsp_definition`, `lsp_hover`, …). A
    // `path` handed to `lsp_find_symbol` is only a filter, so it cannot stand in
    // for the check.
    let file = scratch.root.join(&path);
    let served = backend
        .request(&file, "textDocument/hover", args, &cancel)
        .await
        .expect("the printed retry must work as it stands");
    assert_eq!(
        served.root,
        scratch.root.join("one"),
        "and it must land on that project, not on the boundary"
    );
    assert_eq!(
        roots_of(&backend).await,
        vec![scratch.root.join("one").display().to_string()],
        "and only that project's server may be running"
    );
    backend.shutdown().await;
}

/// Nested projects stay separate: one instance per project root, which is what
/// a language server can actually serve.
#[tokio::test]
async fn nested_projects_still_get_one_instance_each() {
    let scratch = Scratch::new();
    scratch.write("one/fake.toml", "[workspace]");
    let in_one = scratch.write("one/src/a.fl", "fn a() {}\n");
    scratch.write("two/fake.toml", "[workspace]");
    let in_two = scratch.write("two/src/b.fl", "fn b() {}\n");
    let harness = Harness {
        root_markers: vec!["fake.toml".to_owned()],
        ..Harness::default()
    };
    let backend = harness.manager(&scratch);
    let cancel = CancellationToken::new();
    backend
        .request(&in_one, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    backend
        .request(&in_two, "textDocument/hover", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(
        roots_of(&backend).await,
        vec![
            scratch.root.join("one").display().to_string(),
            scratch.root.join("two").display().to_string(),
        ]
    );
    backend.shutdown().await;
}
