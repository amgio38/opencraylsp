//! The memory guard against the fake language server: over-limit instances are
//! drained and restarted, the restart budget is enforced, and nothing in
//! flight is killed by a graceful restart.

#![cfg(feature = "test-fake-lsp")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use opencraylsp_core::memory::MemorySampler;
use opencraylsp_core::{LanguageSelection, LspBackend, LspConfig, LspError, Pool, PoolOptions};
use opencraylsp_proto::InstanceState;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;

/// Reports whatever the test last set, for any pid.
#[derive(Debug, Default)]
struct Scripted {
    bytes: AtomicU64,
    absent: std::sync::atomic::AtomicBool,
}

impl Scripted {
    fn set_mib(&self, mib: u64) {
        self.bytes.store(mib * MIB, Ordering::SeqCst);
    }
}

impl MemorySampler for Scripted {
    fn tree_rss_bytes(&self, _pid: u32) -> Option<u64> {
        (!self.absent.load(Ordering::SeqCst)).then(|| self.bytes.load(Ordering::SeqCst))
    }

    fn self_rss_bytes(&self, _pid: u32) -> Option<u64> {
        (!self.absent.load(Ordering::SeqCst)).then(|| self.bytes.load(Ordering::SeqCst))
    }
}

struct Env {
    dir: tempfile::TempDir,
    sampler: Arc<Scripted>,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            sampler: Arc::new(Scripted::default()),
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

    fn pids(&self) -> Vec<u32> {
        std::fs::read_to_string(self.dir.path().join("pids.txt"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    fn config(&self, limits: &str, fake_args: &[&str]) -> Arc<LspConfig> {
        let mut all = vec![
            "-c".to_owned(),
            "echo $$ >> \"$0\"; f=\"$1\"; shift; exec \"$f\" \"$@\"".to_owned(),
            self.dir.path().join("pids.txt").display().to_string(),
            env!("CARGO_BIN_EXE_fake-lsp-server").to_owned(),
        ];
        all.extend(fake_args.iter().map(|s| (*s).to_owned()));
        let args_toml = all
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            "[limits]\nstartup_grace_ms = 0\nmax_rss_mb = 64\n{limits}\n\
             [[server]]\nname = \"fake\"\ncommand = \"sh\"\nargs = [ {args_toml} ]\n\
             extensions = {{ fl = \"fake\" }}\n"
        );
        Arc::new(LspConfig::from_toml_str_without_presets(&src).unwrap())
    }

    fn pool(
        &self,
        limits: &str,
        fake_args: &[&str],
        options: impl FnOnce(&mut PoolOptions),
    ) -> Arc<Pool> {
        let mut opts = PoolOptions {
            sampler: self.sampler.clone(),
            ..PoolOptions::default()
        };
        options(&mut opts);
        Pool::with_options(self.config(limits, fake_args), opts)
    }
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map(|s| !s.contains("State:\tZ"))
        .unwrap_or(false)
}

async fn hover(backend: &dyn LspBackend, file: &Path) -> Result<(), LspError> {
    backend
        .request(
            file,
            "textDocument/hover",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .map(|_| ())
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn an_instance_under_the_limit_is_left_alone_and_reported() {
    let env = Env::new();
    let file = env.file();
    env.sampler.set_mib(10);
    let pool = env.pool("", &[], |_| {});
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*backend, &file).await.unwrap();
    for _ in 0..3 {
        pool.guard_once().await;
    }
    assert_eq!(env.pids().len(), 1);
    let info = &backend.status().await.instances[0];
    assert_eq!(info.rss_bytes, Some(10 * MIB));
    assert_eq!(info.pid, Some(env.pids()[0]));
    assert_eq!(info.memory_restarts, 0);
    assert_eq!(info.state, InstanceState::Ready);
    pool.shutdown().await;
}

#[tokio::test]
async fn an_instance_over_the_limit_is_restarted_and_serves_again() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool("", &[], |_| {});
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    env.sampler.set_mib(10);
    hover(&*backend, &file).await.unwrap();
    let first = env.pids()[0];
    env.sampler.set_mib(100);
    pool.guard_once().await;
    wait_until("the old server to exit", || !alive(first)).await;
    env.sampler.set_mib(10);
    hover(&*backend, &file).await.unwrap();
    assert_eq!(env.pids().len(), 2, "a fresh process took over");
    let info = &backend.status().await.instances[0];
    assert_eq!(info.memory_restarts, 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn a_request_in_flight_finishes_before_the_restart_happens() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool(
        "",
        &["--delay-ms=1200", "--delay-method=textDocument/hover"],
        |_| {},
    );
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    env.sampler.set_mib(100);
    let slow = {
        let (backend, file) = (backend.clone(), file.clone());
        tokio::spawn(async move { hover(&*backend, &file).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let first = env.pids()[0];
    pool.guard_once().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        alive(first),
        "the server must survive while a request is in flight"
    );
    assert_eq!(
        backend.status().await.instances[0].state,
        InstanceState::Restarting
    );
    slow.await
        .unwrap()
        .expect("the in-flight request completes");
    wait_until("the drained server to exit", || !alive(first)).await;
    pool.shutdown().await;
}

#[tokio::test]
async fn a_new_request_during_the_drain_waits_and_gets_a_fresh_server() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool(
        "",
        &["--delay-ms=800", "--delay-method=textDocument/hover"],
        |_| {},
    );
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    env.sampler.set_mib(100);
    let slow = {
        let (backend, file) = (backend.clone(), file.clone());
        tokio::spawn(async move { hover(&*backend, &file).await })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    pool.guard_once().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    env.sampler.set_mib(10);
    // Arrives while the old server is draining: it must wait, not fail.
    hover(&*backend, &file)
        .await
        .expect("served after the restart");
    slow.await.unwrap().unwrap();
    assert_eq!(env.pids().len(), 2);
    pool.shutdown().await;
}

#[tokio::test]
async fn a_request_that_waits_too_long_for_the_drain_is_told_to_retry() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool(
        "",
        &["--delay-ms=1500", "--delay-method=textDocument/hover"],
        |o| o.drain_wait = Duration::from_millis(200),
    );
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    env.sampler.set_mib(100);
    let slow = {
        let (backend, file) = (backend.clone(), file.clone());
        tokio::spawn(async move { hover(&*backend, &file).await })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    pool.guard_once().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let err = hover(&*backend, &file).await.unwrap_err();
    assert_eq!(
        err,
        LspError::MemoryRestart {
            server: "fake".into()
        }
    );
    let _ = slow.await;
    pool.shutdown().await;
}

#[tokio::test]
async fn a_drain_that_never_ends_is_forced_after_the_timeout() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool(
        "",
        &["--delay-ms=5000", "--delay-method=textDocument/hover"],
        |o| o.drain_timeout = Duration::from_millis(300),
    );
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    env.sampler.set_mib(100);
    let stuck = {
        let (backend, file) = (backend.clone(), file.clone());
        tokio::spawn(async move { hover(&*backend, &file).await })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    let first = env.pids()[0];
    pool.guard_once().await;
    wait_until("the forced restart to kill the server", || !alive(first)).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), stuck)
        .await
        .expect("the stuck request is released, not hung");
    // Either outcome is honest: the request errors, or the instance's own
    // recovery re-runs it on the fresh server. What must not happen is a hang.
    let _ = outcome.unwrap();
    pool.shutdown().await;
}

/// Drives one over-limit cycle: sample high, wait for the old server to go.
async fn overflow_once(env: &Env, pool: &Arc<Pool>, backend: &dyn LspBackend, file: &Path) {
    env.sampler.set_mib(10);
    hover(backend, file).await.expect("server is up");
    let pid = *env.pids().last().unwrap();
    env.sampler.set_mib(100);
    pool.guard_once().await;
    wait_until("the over-limit server to exit", || !alive(pid)).await;
}

#[tokio::test]
async fn the_fourth_overflow_in_a_window_is_refused_with_an_explanation() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool("", &[], |_| {});
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    for _ in 0..3 {
        overflow_once(&env, &pool, &*backend, &file).await;
    }
    assert_eq!(backend.status().await.instances[0].memory_restarts, 3);
    // The fourth: stopped, and not started again.
    env.sampler.set_mib(10);
    hover(&*backend, &file).await.unwrap();
    let pid = *env.pids().last().unwrap();
    env.sampler.set_mib(100);
    pool.guard_once().await;
    wait_until("the fourth server to exit", || !alive(pid)).await;
    let count = env.pids().len();
    let err = hover(&*backend, &file).await.unwrap_err();
    match err {
        LspError::ServerFailed {
            server,
            restarts,
            last_error,
        } => {
            assert_eq!(server, "fake");
            assert_eq!(restarts, 3);
            assert!(
                last_error.contains("memory limit of 64 MiB"),
                "{last_error}"
            );
            assert!(last_error.contains("peak 100 MiB"), "{last_error}");
            assert!(last_error.contains("max_rss_mb"), "{last_error}");
        }
        other => panic!("expected ServerFailed, got {other:?}"),
    }
    assert_eq!(env.pids().len(), count, "no new process was started");
    assert_eq!(
        backend.status().await.instances[0].state,
        InstanceState::Failed
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn the_restart_budget_recovers_once_the_window_has_passed() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool("", &[], |o| o.memory_window = Duration::from_secs(3));
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    for _ in 0..3 {
        overflow_once(&env, &pool, &*backend, &file).await;
    }
    env.sampler.set_mib(10);
    hover(&*backend, &file).await.unwrap();
    let pid = *env.pids().last().unwrap();
    env.sampler.set_mib(100);
    pool.guard_once().await;
    wait_until("the fourth server to exit", || !alive(pid)).await;
    let refused = hover(&*backend, &file).await;
    assert!(
        matches!(refused, Err(LspError::ServerFailed { .. })),
        "expected a refusal, got {refused:?} (status: {:?})",
        backend.status().await.instances
    );
    tokio::time::sleep(Duration::from_millis(3300)).await;
    env.sampler.set_mib(10);
    hover(&*backend, &file)
        .await
        .expect("the window passed: a new server may run");
    pool.shutdown().await;
}

#[tokio::test]
async fn a_process_that_cannot_be_sampled_is_ignored() {
    let env = Env::new();
    let file = env.file();
    env.sampler.set_mib(1000);
    env.sampler.absent.store(true, Ordering::SeqCst);
    let pool = env.pool("", &[], |_| {});
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*backend, &file).await.unwrap();
    pool.guard_once().await;
    assert_eq!(env.pids().len(), 1);
    assert_eq!(backend.status().await.instances[0].rss_bytes, None);
    pool.shutdown().await;
}

/// A sample that *stops* arriving is not the same as a sample that never
/// arrived. Keeping the last good number after the sampler goes quiet would
/// report a stale figure as if it were current — and `lsp_status` is what an
/// operator reads to decide whether a server is still within its limit.
#[tokio::test]
async fn a_sample_that_stops_arriving_clears_the_reported_rss() {
    let env = Env::new();
    let file = env.file();
    env.sampler.set_mib(10);
    let pool = env.pool("", &[], |_| {});
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*backend, &file).await.unwrap();
    pool.guard_once().await;
    assert_eq!(
        backend.status().await.instances[0].rss_bytes,
        Some(10 * MIB),
        "the first sample is reported"
    );

    // The process is gone, or the platform cannot read it: no sample this pass.
    env.sampler.absent.store(true, Ordering::SeqCst);
    pool.guard_once().await;
    assert_eq!(
        backend.status().await.instances[0].rss_bytes,
        None,
        "an unsampled instance must say `unknown`, not repeat the last good number"
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn a_stopped_instance_reports_no_pid_and_no_rss() {
    let env = Env::new();
    let file = env.file();
    env.sampler.set_mib(10);
    let pool = env.pool("idle_shutdown_secs = 1", &[], |_| {});
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*backend, &file).await.unwrap();
    pool.guard_once().await;
    assert!(backend.status().await.instances[0].rss_bytes.is_some());
    pool.shutdown().await;
    pool.guard_once().await; // an empty pool is a no-op
    assert!(backend.status().await.instances.is_empty());
}

#[tokio::test]
async fn the_background_guard_restarts_a_real_memory_hog() {
    // No scripted sampler: the real /proc reading of a server that really
    // holds 150 MiB, against a 64 MiB limit.
    let dir = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(dir.path()).unwrap();
    let file = ws.join("a.fl");
    std::fs::write(&file, "x\n").unwrap();
    let src = format!(
        "[limits]\nstartup_grace_ms = 0\nmax_rss_mb = 64\nmemory_sample_ms = 200\n\
         [[server]]\nname = \"fake\"\ncommand = {:?}\nargs = [\"--alloc-mb=150\"]\n\
         extensions = {{ fl = \"fake\" }}\n",
        env!("CARGO_BIN_EXE_fake-lsp-server")
    );
    let pool = Pool::new(Arc::new(
        LspConfig::from_toml_str_without_presets(&src).unwrap(),
    ));
    pool.spawn_maintenance();
    let backend = pool.bind(&ws, LanguageSelection::All);
    hover(&*backend, &file).await.unwrap();
    let mut restarted = false;
    for _ in 0..80 {
        if backend
            .status()
            .await
            .instances
            .first()
            .is_some_and(|i| i.memory_restarts >= 1)
        {
            restarted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(restarted, "the guard never restarted the memory hog");
    pool.shutdown().await;
}

#[tokio::test]
async fn an_indexing_server_gets_twice_the_room_before_it_is_restarted() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool("", &["--progress-ms=3000"], |_| {});
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    // Indexing (the fake reports progress for 3 s): 100 MiB is over the
    // 64 MiB limit but under twice of it, so nothing happens...
    env.sampler.set_mib(100);
    let _ = hover(&*backend, &file).await;
    pool.guard_once().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.pids().len(), 1);
    assert_eq!(backend.status().await.instances[0].memory_restarts, 0);
    // ...but past twice the limit even indexing does not excuse it.
    env.sampler.set_mib(200);
    let first = env.pids()[0];
    pool.guard_once().await;
    wait_until("the runaway indexer to be restarted", || !alive(first)).await;
    pool.shutdown().await;
}

#[tokio::test]
async fn a_server_at_rest_is_held_to_the_plain_limit() {
    let env = Env::new();
    let file = env.file();
    let pool = env.pool("", &[], |_| {});
    let backend = pool.bind(env.ws(), LanguageSelection::All);
    env.sampler.set_mib(10);
    hover(&*backend, &file).await.unwrap();
    env.sampler.set_mib(100);
    let first = env.pids()[0];
    pool.guard_once().await;
    wait_until("the resting server to be restarted at 100 MiB", || {
        !alive(first)
    })
    .await;
    pool.shutdown().await;
}
