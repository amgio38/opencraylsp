//! Shared-pool behavior against the fake language server over real stdio:
//! sharing across connections, startup storms, idle reclaim, eviction,
//! capacity, document limits and the (mtime, size) fast path.
//!
//! Every server is launched through a tiny `sh` wrapper that appends its pid
//! to a file before `exec`ing the fake, so a test can count how many
//! processes the pool really started and prove none survives shutdown.

#![cfg(feature = "test-fake-lsp")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use opencraylsp_core::{BoundBackend, LanguageSelection, LspBackend, LspConfig, LspError, Pool};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn fake_bin() -> String {
    env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()
}

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
        let ws = self.dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::canonicalize(ws).unwrap()
    }

    fn pids_file(&self) -> PathBuf {
        self.dir.path().join("pids.txt")
    }

    /// A config with one server `fake` for `.fl` files rooted by `proj.toml`.
    fn config(&self, limits: &str, fake_args: &[&str]) -> Arc<LspConfig> {
        self.config_with(limits, fake_args, "fake", "fl", "fake")
    }

    fn config_with(
        &self,
        limits: &str,
        fake_args: &[&str],
        name: &str,
        ext: &str,
        language: &str,
    ) -> Arc<LspConfig> {
        // sh -c '<script>' <pids-file> <fake-bin> [fake args...]: `$0` is the
        // pids file, `$1` the fake, and the rest are the fake's own arguments.
        let mut all = vec![
            "-c".to_owned(),
            "echo $$ >> \"$0\"; f=\"$1\"; shift; exec \"$f\" \"$@\"".to_owned(),
            self.pids_file().display().to_string(),
            fake_bin(),
        ];
        all.extend(fake_args.iter().map(|s| (*s).to_owned()));
        let args_toml = all
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            "[limits]\nstartup_grace_ms = 0\n{limits}\n\
             [[server]]\nname = {name:?}\ncommand = \"sh\"\nargs = [ {args_toml} ]\n\
             extensions = {{ {ext} = {language:?} }}\nroot_markers = [ \"proj.toml\" ]\n"
        );
        Arc::new(LspConfig::from_toml_str_without_presets(&src).expect("test config parses"))
    }

    fn pids(&self) -> Vec<u32> {
        std::fs::read_to_string(self.pids_file())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    /// Writes `<project>/proj.toml` and `<project>/<name>` and returns the file.
    fn file_in(&self, project: &str, name: &str) -> PathBuf {
        let dir = self.ws().join(project);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("proj.toml"), "").unwrap();
        let file = dir.join(name);
        std::fs::write(&file, "content\n").unwrap();
        std::fs::canonicalize(file).unwrap()
    }
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && std::fs::read_to_string(format!("/proc/{pid}/status"))
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
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn two_connections_share_one_server_process() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let pool = Pool::new(env.config("", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    let b = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &file).await.unwrap();
    hover(&*b, &file).await.unwrap();
    assert_eq!(env.pids().len(), 1, "one process for two connections");
    assert_eq!(pool.instance_count().await, 1);
    let status = a.status().await;
    assert_eq!(status.daemon.clients, 2);
    assert_eq!(status.instances.len(), 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn different_project_roots_get_their_own_process() {
    let env = Env::new();
    let one = env.file_in("one", "a.fl");
    let two = env.file_in("two", "a.fl");
    let pool = Pool::new(env.config("", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &one).await.unwrap();
    hover(&*a, &two).await.unwrap();
    hover(&*a, &one).await.unwrap();
    assert_eq!(env.pids().len(), 2);
    assert_eq!(pool.instance_count().await, 2);
    pool.shutdown().await;
}

#[tokio::test]
async fn a_startup_storm_starts_exactly_one_process() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let pool = Pool::new(env.config("", &[]));
    let mut handles = Vec::new();
    for _ in 0..20 {
        let backend = pool.bind(env.ws(), LanguageSelection::All);
        let file = file.clone();
        handles.push(tokio::spawn(async move { hover(&*backend, &file).await }));
    }
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    assert_eq!(env.pids().len(), 1, "20 first requests, one spawn");
    pool.shutdown().await;
}

#[tokio::test]
async fn a_connection_without_the_language_cannot_use_a_shared_server() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let pool = Pool::new(env.config("", &[]));
    let with = pool.bind(env.ws(), LanguageSelection::All);
    let without = pool.bind(
        env.ws(),
        LanguageSelection::Explicit(["other".to_owned()].into()),
    );
    hover(&*with, &file).await.unwrap();
    let err = hover(&*without, &file).await.unwrap_err();
    assert!(
        matches!(err, LspError::LanguageDisabled { ref language, .. } if language == "fake"),
        "{err:?}"
    );
    assert_eq!(env.pids().len(), 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn idle_instances_are_reclaimed_and_restart_on_demand() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let pool = Pool::new(env.config("idle_shutdown_secs = 1", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &file).await.unwrap();
    let first = env.pids()[0];
    tokio::time::sleep(Duration::from_millis(1300)).await;
    pool.sweep_idle().await;
    assert_eq!(pool.instance_count().await, 0);
    wait_until("idle server to exit", || !alive(first)).await;
    hover(&*a, &file).await.unwrap();
    assert_eq!(env.pids().len(), 2, "a fresh process served the next call");
    pool.shutdown().await;
}

#[tokio::test]
async fn a_request_in_flight_keeps_its_server_alive_past_the_idle_limit() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let pool = Pool::new(env.config(
        "idle_shutdown_secs = 1",
        &["--delay-ms=1800", "--delay-method=textDocument/hover"],
    ));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    let slow = {
        let a = a.clone();
        let file = file.clone();
        tokio::spawn(async move { hover(&*a, &file).await })
    };
    tokio::time::sleep(Duration::from_millis(1400)).await;
    pool.sweep_idle().await;
    assert_eq!(
        pool.instance_count().await,
        1,
        "leased entries are not swept"
    );
    slow.await.unwrap().expect("the slow request must complete");
    pool.shutdown().await;
}

#[tokio::test]
async fn the_cap_evicts_the_least_recently_used_idle_instance() {
    let env = Env::new();
    let (one, two, three) = (
        env.file_in("one", "a.fl"),
        env.file_in("two", "a.fl"),
        env.file_in("three", "a.fl"),
    );
    let pool = Pool::new(env.config("max_instances = 2", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &one).await.unwrap();
    hover(&*a, &two).await.unwrap();
    hover(&*a, &one).await.unwrap(); // `two` is now the oldest
    hover(&*a, &three).await.unwrap();
    let mut roots: Vec<String> = a
        .status()
        .await
        .instances
        .into_iter()
        .map(|i| i.root)
        .collect();
    roots.sort();
    assert_eq!(roots.len(), 2);
    assert!(
        roots[0].ends_with("one") && roots[1].ends_with("three"),
        "{roots:?}"
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn when_every_slot_is_busy_the_next_request_gets_capacity() {
    let env = Env::new();
    let (one, two) = (env.file_in("one", "a.fl"), env.file_in("two", "a.fl"));
    let pool = Pool::with_capacity_wait(
        env.config(
            "max_instances = 1",
            &["--delay-ms=1500", "--delay-method=textDocument/hover"],
        ),
        Duration::from_millis(200),
    );
    let a = pool.bind(env.ws(), LanguageSelection::All);
    let busy = {
        let a = a.clone();
        tokio::spawn(async move { hover(&*a, &one).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    let err = hover(&*a, &two).await.unwrap_err();
    assert_eq!(err, LspError::Capacity { limit: 1 });
    busy.await.unwrap().unwrap();
    pool.shutdown().await;
}

#[tokio::test]
async fn open_documents_are_capped_per_instance() {
    let env = Env::new();
    let pool = Pool::new(env.config("max_open_docs = 2", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    for name in ["a.fl", "b.fl", "c.fl", "d.fl"] {
        hover(&*a, &env.file_in("p", name)).await.unwrap();
    }
    let status = a.status().await;
    assert_eq!(status.instances.len(), 1);
    assert_eq!(status.instances[0].open_docs, 2);
    pool.shutdown().await;
}

fn backdate(path: &Path) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
}

#[tokio::test]
async fn an_unchanged_old_file_is_not_read_again() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    backdate(&file);
    let pool = Pool::new(env.config("", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &file).await.unwrap();
    let after_open = pool.doc_reads();
    hover(&*a, &file).await.unwrap();
    hover(&*a, &file).await.unwrap();
    assert_eq!(
        pool.doc_reads(),
        after_open,
        "mtime+size unchanged => no re-read"
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn a_freshly_written_file_is_always_re_hashed() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let pool = Pool::new(env.config("", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &file).await.unwrap();
    let after_open = pool.doc_reads();
    // Same size, edited inside the timestamp-trust window: must be noticed.
    std::fs::write(&file, "CONTENT\n").unwrap();
    hover(&*a, &file).await.unwrap();
    assert!(pool.doc_reads() > after_open, "recent mtime is not trusted");
    pool.shutdown().await;
}

#[tokio::test]
async fn a_changed_old_file_is_noticed_through_its_metadata() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    backdate(&file);
    let pool = Pool::new(env.config("", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &file).await.unwrap();
    let after_open = pool.doc_reads();
    std::fs::write(&file, "a much longer replacement body\n").unwrap();
    hover(&*a, &file).await.unwrap();
    assert!(pool.doc_reads() > after_open);
    pool.shutdown().await;
}

#[tokio::test]
async fn a_deleted_file_is_dropped_from_the_open_set() {
    let env = Env::new();
    let (keep, gone) = (env.file_in("p", "a.fl"), env.file_in("p", "b.fl"));
    let pool = Pool::new(env.config("", &[]));
    let a = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &keep).await.unwrap();
    hover(&*a, &gone).await.unwrap();
    assert_eq!(a.status().await.instances[0].open_docs, 2);
    std::fs::remove_file(&gone).unwrap();
    hover(&*a, &keep).await.unwrap();
    assert_eq!(a.status().await.instances[0].open_docs, 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn constant_eviction_under_concurrency_leaks_no_processes() {
    let env = Env::new();
    let one = env.file_in("one", "a.fl");
    let two = env.file_in("two", "a.fl");
    let pool = Pool::new(env.config("max_instances = 1", &[]));
    let mut handles = Vec::new();
    for task in 0..4 {
        let backend = pool.bind(env.ws(), LanguageSelection::All);
        let file = if task % 2 == 0 {
            one.clone()
        } else {
            two.clone()
        };
        handles.push(tokio::spawn(async move {
            for _ in 0..12 {
                hover(&*backend, &file).await.expect("request succeeds");
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    pool.shutdown().await;
    assert_eq!(pool.instance_count().await, 0);
    let pids = env.pids();
    assert!(pids.len() >= 2, "eviction really happened: {pids:?}");
    wait_until("every spawned server to exit", || {
        pids.iter().all(|p| !alive(*p))
    })
    .await;
}

#[tokio::test]
async fn shutting_down_a_standalone_backend_stops_its_servers() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let backend = BoundBackend::standalone_in(env.config("", &[]), env.ws());
    hover(&*backend, &file).await.unwrap();
    let pid = env.pids()[0];
    backend.shutdown().await;
    wait_until("server to exit", || !alive(pid)).await;
}

#[tokio::test]
async fn a_graceful_shutdown_is_prompt() {
    // Regression: `shutdown()` once waited out its whole 5 s grace before
    // sending `exit`, so every idle reclaim and daemon stop cost 5 s.
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let backend = BoundBackend::standalone_in(env.config("", &[]), env.ws());
    hover(&*backend, &file).await.unwrap();
    let pid = env.pids()[0];
    let started = std::time::Instant::now();
    backend.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "shutdown took {:?}",
        started.elapsed()
    );
    wait_until("server to exit", || !alive(pid)).await;
}

#[tokio::test]
async fn a_server_that_answers_shutdown_late_is_still_stopped() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let backend = BoundBackend::standalone_in(
        env.config("", &["--delay-ms=4500", "--delay-method=shutdown"]),
        env.ws(),
    );
    hover(&*backend, &file).await.unwrap();
    let pid = env.pids()[0];
    backend.shutdown().await;
    wait_until("server to exit", || !alive(pid)).await;
}

#[tokio::test]
async fn a_server_that_dies_at_startup_says_why() {
    // Regression: the error was "exited during initialize (stdout closed)"
    // with the server's own explanation thrown away, which left nothing to act
    // on (a rustup proxy with no toolchain, a missing runtime, a bad flag...).
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let backend = BoundBackend::standalone_in(
        env.config(
            "max_restarts = 0",
            &[
                "--crash-after=0",
                "--stderr=error: toolchain 'nope' is not installed",
            ],
        ),
        env.ws(),
    );
    let err = hover(&*backend, &file).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("exited during initialize"), "{text}");
    assert!(text.contains("toolchain 'nope' is not installed"), "{text}");
    backend.shutdown().await;
}

#[tokio::test]
async fn the_restart_limit_message_carries_the_last_real_error() {
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let backend = BoundBackend::standalone_in(
        env.config(
            "max_restarts = 1",
            &["--crash-after=0", "--stderr=fatal: config file is corrupt"],
        ),
        env.ws(),
    );
    let mut last = None;
    for _ in 0..5 {
        last = Some(hover(&*backend, &file).await.unwrap_err());
        if matches!(last, Some(LspError::ServerFailed { .. })) {
            break;
        }
    }
    match last.expect("at least one attempt") {
        LspError::ServerFailed { last_error, .. } => {
            assert!(
                last_error.contains("config file is corrupt"),
                "{last_error}"
            );
            assert!(!last_error.contains("no further detail"), "{last_error}");
        }
        other => panic!("expected the restart limit to be reached, got {other:?}"),
    }
    backend.shutdown().await;
}

#[tokio::test]
async fn an_unused_server_is_reclaimed_by_the_sweeper_without_any_new_request() {
    // T3: call once, then do nothing at all; the server must go away by itself.
    let env = Env::new();
    let file = env.file_in("p", "a.fl");
    let pool = Pool::new(env.config("idle_shutdown_secs = 2", &[]));
    pool.spawn_maintenance();
    let a = pool.bind(env.ws(), LanguageSelection::All);
    hover(&*a, &file).await.unwrap();
    let pid = env.pids()[0];
    assert!(alive(pid));
    wait_until("the idle server to be reclaimed on its own", || !alive(pid)).await;
    assert_eq!(pool.instance_count().await, 0);
    // A later call cold-starts a fresh one.
    hover(&*a, &file).await.unwrap();
    assert_eq!(env.pids().len(), 2);
    pool.shutdown().await;
}
