//! Indexing honesty against the fake language server: while a server reports
//! work-done progress, an empty answer is `Indexing`, never "nothing found".

#![cfg(feature = "test-fake-lsp")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opencraylsp_core::{BoundBackend, LspBackend, LspConfig, LspError};
use opencraylsp_proto::InstanceState;
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn ws(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }

    fn file(&self) -> PathBuf {
        let file = self.ws().join("a.fl");
        std::fs::write(&file, "content\n").unwrap();
        file
    }

    fn backend(&self, limits: &str, fake_args: &[&str]) -> Arc<BoundBackend> {
        let args_toml = fake_args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            "[limits]\n{limits}\n\
             [[server]]\nname = \"fake\"\ncommand = {:?}\nargs = [ {args_toml} ]\n\
             extensions = {{ fl = \"fake\" }}\n",
            env!("CARGO_BIN_EXE_fake-lsp-server"),
        );
        BoundBackend::standalone_in(
            Arc::new(LspConfig::from_toml_str_without_presets(&src).unwrap()),
            self.ws(),
        )
    }
}

async fn ask(
    backend: &BoundBackend,
    env: &Env,
    method: &str,
) -> Result<opencraylsp_core::Served, LspError> {
    backend
        .request(
            &env.file(),
            method,
            json!({"textDocument": {"uri": "file:///x"}, "position": {"line": 0, "character": 0}}),
            &CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn an_empty_answer_while_indexing_is_reported_as_indexing() {
    let env = Env::new();
    let backend = env.backend("startup_grace_ms = 0", &["--progress-ms=1500"]);
    let err = ask(&backend, &env, "textDocument/hover").await.unwrap_err();
    match err {
        LspError::Indexing {
            server, message, ..
        } => {
            assert_eq!(server, "fake");
            assert!(message.starts_with("Indexing"), "{message}");
        }
        other => panic!("expected Indexing, got {other:?}"),
    }
    backend.shutdown().await;
}

#[tokio::test]
async fn the_same_question_is_answered_once_indexing_ends() {
    let env = Env::new();
    let backend = env.backend("startup_grace_ms = 0", &["--progress-ms=600"]);
    assert!(matches!(
        ask(&backend, &env, "textDocument/hover").await,
        Err(LspError::Indexing { .. })
    ));
    // Progress ends after 600 ms and the end-of-work window adds 500 ms.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let served = ask(&backend, &env, "textDocument/hover").await.unwrap();
    assert!(
        served.value.is_null(),
        "a genuinely empty answer is passed through"
    );
    assert_eq!(served.indexing, None);
    backend.shutdown().await;
}

#[tokio::test]
async fn a_non_empty_answer_during_indexing_carries_the_marker() {
    let env = Env::new();
    let backend = env.backend("startup_grace_ms = 0", &["--progress-ms=1500"]);
    let served = ask(&backend, &env, "textDocument/definition")
        .await
        .unwrap();
    assert!(served.value.is_array());
    let indexing = served.indexing.expect("marked as possibly incomplete");
    assert!(indexing.message.starts_with("Indexing"), "{indexing:?}");
    backend.shutdown().await;
}

#[tokio::test]
async fn progress_reports_update_the_percentage() {
    let env = Env::new();
    let backend = env.backend("startup_grace_ms = 0", &["--progress-ms=1200"]);
    let _ = ask(&backend, &env, "textDocument/definition")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let err = ask(&backend, &env, "textDocument/hover").await.unwrap_err();
    match err {
        LspError::Indexing {
            percent, message, ..
        } => {
            assert_eq!(percent, Some(50), "{message}");
            assert!(message.contains("1/2"), "{message}");
        }
        other => panic!("expected Indexing, got {other:?}"),
    }
    backend.shutdown().await;
}

#[tokio::test]
async fn status_shows_the_indexing_state_then_ready() {
    let env = Env::new();
    let backend = env.backend("startup_grace_ms = 0", &["--progress-ms=700"]);
    let _ = ask(&backend, &env, "textDocument/definition")
        .await
        .unwrap();
    let during = backend.status().await;
    assert_eq!(during.instances[0].state, InstanceState::Indexing);
    assert!(during.instances[0].indexing.is_some());
    tokio::time::sleep(Duration::from_millis(1400)).await;
    let after = backend.status().await;
    assert_eq!(after.instances[0].state, InstanceState::Ready);
    assert_eq!(after.instances[0].indexing, None);
    backend.shutdown().await;
}

#[tokio::test]
async fn a_background_check_is_not_indexing() {
    let env = Env::new();
    let backend = env.backend(
        "startup_grace_ms = 0",
        &[
            "--progress-ms=1500",
            "--progress-token=rustAnalyzer/flycheck/0",
            "--progress-title=cargo check",
        ],
    );
    let served = ask(&backend, &env, "textDocument/hover").await.unwrap();
    assert!(served.value.is_null());
    assert_eq!(served.indexing, None);
    backend.shutdown().await;
}

#[tokio::test]
async fn the_startup_grace_covers_a_server_that_reports_no_progress() {
    let env = Env::new();
    let backend = env.backend("startup_grace_ms = 800", &[]);
    let err = ask(&backend, &env, "textDocument/hover").await.unwrap_err();
    match err {
        LspError::Indexing { message, .. } => assert!(message.contains("starting up"), "{message}"),
        other => panic!("expected Indexing, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(900)).await;
    let served = ask(&backend, &env, "textDocument/hover").await.unwrap();
    assert!(served.value.is_null(), "grace over: empty means empty");
    backend.shutdown().await;
}

#[tokio::test]
async fn a_restart_starts_a_fresh_grace_period() {
    let env = Env::new();
    let backend = env.backend("startup_grace_ms = 500\nidle_shutdown_secs = 1", &[]);
    tokio::time::sleep(Duration::from_millis(0)).await;
    let _ = ask(&backend, &env, "textDocument/hover").await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(ask(&backend, &env, "textDocument/hover").await.is_ok());
    tokio::time::sleep(Duration::from_millis(1300)).await;
    backend.pool().sweep_idle().await;
    // The reclaimed server starts again: its first empty answer is "starting".
    let again = ask(&backend, &env, "textDocument/hover").await;
    assert!(matches!(again, Err(LspError::Indexing { .. })), "{again:?}");
    backend.shutdown().await;
}

#[tokio::test]
async fn diagnostics_are_not_called_clean_while_indexing() {
    let env = Env::new();
    let backend = env.backend(
        "startup_grace_ms = 0\ndiagnostics_settle_ms = 50\ndiagnostics_timeout_ms = 400",
        &["--progress-ms=2000", "--push-diagnostics"],
    );
    // The file text contains CLEAN, so the fake publishes zero diagnostics.
    let file = env.ws().join("clean.fl");
    std::fs::write(&file, "CLEAN\n").unwrap();
    let err = backend
        .diagnostics(&file, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, LspError::Indexing { .. }), "{err:?}");
    backend.shutdown().await;
}

#[tokio::test]
async fn diagnostics_with_errors_are_reported_even_while_indexing() {
    let env = Env::new();
    let backend = env.backend(
        "startup_grace_ms = 0\ndiagnostics_settle_ms = 50\ndiagnostics_timeout_ms = 2000",
        &["--progress-ms=3000", "--push-diagnostics"],
    );
    let file = env.ws().join("broken.fl");
    std::fs::write(&file, "not clean\n").unwrap();
    let report = backend
        .diagnostics(&file, &CancellationToken::new())
        .await
        .unwrap();
    assert!(report.received_for_version && !report.items.is_empty());
    backend.shutdown().await;
}

#[tokio::test]
async fn workspace_requests_get_the_same_treatment() {
    let env = Env::new();
    let backend = env.backend("startup_grace_ms = 0", &["--progress-ms=1500"]);
    let err = backend
        .request_workspace(
            "fake",
            "workspace/symbol",
            json!({"query": "x"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, LspError::Indexing { .. }), "{err:?}");
    backend.shutdown().await;
}
