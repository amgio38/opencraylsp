//! The daemon's own runaway guard, against a real language server: a daemon
//! over its ceiling stops accepting work, lets what is in flight finish, shuts
//! the servers down and records the exit — and, once that has happened too often
//! in an hour, keeps serving instead of restarting in a loop.

#![cfg(feature = "test-fake-lsp")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use opencraylsp_core::daemon_guard;
use opencraylsp_core::memory::MemorySampler;
use opencraylsp_core::{
    DaemonGuard, LanguageSelection, LspBackend, LspConfig, LspError, Pool, PoolOptions,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;

/// A sampler whose answer the test sets. `absent` models a platform where the
/// read fails, which must not be read as "over the limit".
///
/// The two readings are scripted apart: the daemon's ceiling is about this
/// process, the servers' is about their trees, and a fake that answered both
/// with one number could not tell the two guards apart.
#[derive(Debug, Default)]
struct Scripted {
    self_bytes: AtomicU64,
    tree_bytes: AtomicU64,
    absent: AtomicBool,
}

impl Scripted {
    fn set_mib(&self, mib: u64) {
        self.self_bytes.store(mib * MIB, Ordering::SeqCst);
        self.tree_bytes.store(mib * MIB, Ordering::SeqCst);
    }

    /// The daemon process's own figure, leaving the servers' tree alone.
    fn set_self_mib(&self, mib: u64) {
        self.self_bytes.store(mib * MIB, Ordering::SeqCst);
    }

    /// What the supervised language servers weigh, without touching the
    /// daemon's own figure.
    fn set_tree_mib(&self, mib: u64) {
        self.tree_bytes.store(mib * MIB, Ordering::SeqCst);
    }

    fn make_absent(&self) {
        self.absent.store(true, Ordering::SeqCst);
    }
}

impl MemorySampler for Scripted {
    fn tree_rss_bytes(&self, _pid: u32) -> Option<u64> {
        (!self.absent.load(Ordering::SeqCst)).then(|| self.tree_bytes.load(Ordering::SeqCst))
    }

    fn self_rss_bytes(&self, _pid: u32) -> Option<u64> {
        (!self.absent.load(Ordering::SeqCst)).then(|| self.self_bytes.load(Ordering::SeqCst))
    }
}

struct Env {
    dir: tempfile::TempDir,
    sampler: Arc<Scripted>,
    /// How many times the guard asked to shut down.
    shutdowns: Arc<AtomicU64>,
    /// The RSS figures the guard reported, oldest last.
    seen: Arc<std::sync::Mutex<Vec<(u64, u64)>>>,
    socket: PathBuf,
    guard: Arc<DaemonGuard>,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let sampler = Arc::new(Scripted::default());
        let shutdowns = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let socket = dir.path().join("opencraylsp.sock");
        let guard = DaemonGuard::new(daemon_guard::stamp_path(&socket), {
            let shutdowns = shutdowns.clone();
            let seen = seen.clone();
            move |rss, limit| {
                shutdowns.fetch_add(1, Ordering::SeqCst);
                seen.lock().unwrap().push((rss, limit));
            }
        });
        Self {
            dir,
            sampler,
            shutdowns,
            seen,
            socket,
            guard: Arc::new(guard),
        }
    }

    fn ws(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).expect("canonical")
    }

    fn file(&self) -> PathBuf {
        let file = self.ws().join("a.fl");
        std::fs::write(&file, "content\n").expect("write");
        file
    }

    fn pids(&self) -> Vec<u32> {
        std::fs::read_to_string(self.dir.path().join("pids.txt"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    fn config(&self, daemon_limit_mb: u64) -> Arc<LspConfig> {
        let args = [
            "-c".to_owned(),
            "echo $$ >> \"$0\"; f=\"$1\"; shift; exec \"$f\" \"$@\"".to_owned(),
            self.dir.path().join("pids.txt").display().to_string(),
            env!("CARGO_BIN_EXE_fake-lsp-server").to_owned(),
        ];
        let args_toml = args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            "[limits]\nstartup_grace_ms = 0\nmax_rss_mb = 64\ndaemon_max_rss_mb = {daemon_limit_mb}\n\
             [[server]]\nname = \"fake\"\ncommand = \"sh\"\nargs = [ {args_toml} ]\n\
             extensions = {{ fl = \"fake\" }}\n"
        );
        Arc::new(LspConfig::from_toml_str_without_presets(&src).expect("config"))
    }

    /// A pool with the guard armed and the sampler scripted.
    fn pool(&self, daemon_limit_mb: u64) -> Arc<Pool> {
        Pool::with_options(
            self.config(daemon_limit_mb),
            PoolOptions {
                sampler: self.sampler.clone(),
                daemon_guard: Some((*self.guard).clone()),
                ..PoolOptions::default()
            },
        )
    }

    fn shutdowns(&self) -> u64 {
        self.shutdowns.load(Ordering::SeqCst)
    }

    /// The stamp file's recorded exits.
    fn history(&self) -> Vec<u64> {
        daemon_guard::read_stamp(&daemon_guard::stamp_path(&self.socket))
    }
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

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map(|s| !s.contains("State:\tZ"))
        .unwrap_or(false)
}

/// A daemon that is nowhere near its ceiling must not so much as ask to stop.
#[tokio::test]
async fn a_daemon_under_its_ceiling_is_left_alone() {
    let env = Env::new();
    // 64 MiB ceiling, a 4 MiB sample: comfortably under.
    let pool = env.pool(64);
    env.sampler.set_mib(4);
    assert_eq!(pool.guard_daemon_rss().await, Some(4 * MIB));
    assert_eq!(env.shutdowns(), 0, "nothing may be asked to stop");
    assert!(env.history().is_empty(), "no exit may be recorded");
    assert!(!pool.daemon_rss_over_limit());
}

/// A language server holding gigabytes is not a runaway daemon. This is the
/// shape a real session takes: two rust-analyzer instances indexing a large
/// workspace report several GiB each, and the daemon supervising them weighs
/// about ten MiB. If the daemon's ceiling were read over the whole tree, every
/// such workspace would trip it, and the guard would shut down a healthy daemon
/// while recording the exits that make it refuse to restart.
#[tokio::test]
async fn language_server_memory_is_not_charged_to_the_daemon() {
    let env = Env::new();
    // 64 MiB daemon ceiling; the servers alone weigh 4 GiB.
    let pool = env.pool(64);
    env.sampler.set_self_mib(10);
    env.sampler.set_tree_mib(4096);
    assert_eq!(
        pool.guard_daemon_rss().await,
        Some(10 * MIB),
        "the figure must be the daemon's own"
    );
    assert_eq!(env.shutdowns(), 0, "the servers' memory must not stop us");
    assert!(
        env.history().is_empty(),
        "and must not be recorded as an exit"
    );
    assert!(!pool.daemon_rss_over_limit());
}

/// The converse, so the test above cannot pass by ignoring the sample: a daemon
/// that really has grown past its ceiling still goes through the graceful exit.
#[tokio::test]
async fn a_daemon_that_really_is_over_its_ceiling_still_exits() {
    let env = Env::new();
    let pool = env.pool(8);
    env.sampler.set_self_mib(9);
    // The servers are small here, so only the daemon's own figure can trip this.
    env.sampler.set_tree_mib(2);
    pool.guard_daemon_rss().await.expect("a sample");
    assert_eq!(env.shutdowns(), 1, "our own leak is still a leak");
    let seen = env.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![(9 * MIB, 8 * MIB)], "the figure and limit both");
    assert_eq!(env.history().len(), 1, "and the exit is on disk");
}

/// Over the ceiling: the guard reports the figure and the limit, and records
/// the exit so the *next* daemon can see it.
#[tokio::test]
async fn a_daemon_over_its_ceiling_asks_to_stop_and_records_the_exit() {
    let env = Env::new();
    let pool = env.pool(8);
    env.sampler.set_mib(9);
    pool.guard_daemon_rss().await.expect("a sample");
    assert_eq!(env.shutdowns(), 1, "the guard must ask the daemon to stop");
    let seen = env.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![(9 * MIB, 8 * MIB)], "the figure and limit both");
    assert_eq!(
        env.history().len(),
        1,
        "the exit must be on disk for the next daemon to count"
    );
    assert!(
        !pool.daemon_rss_over_limit(),
        "the first exit is not a refusal"
    );
}

/// The exit is recorded *before* the shutdown is asked for: a daemon that died
/// without recording it would hand the next one a clean slate and restart-loop.
#[tokio::test]
async fn the_exit_is_recorded_before_the_shutdown_is_asked_for() {
    let env = Env::new();
    env.sampler.set_mib(9);
    // The callback reads the history the guard is supposed to have written.
    let seen_at_callback = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stamp = daemon_guard::stamp_path(&env.socket);
    let guard = DaemonGuard::new(stamp.clone(), {
        let seen_at_callback = seen_at_callback.clone();
        move |_, _| {
            let history = daemon_guard::read_stamp(&stamp);
            seen_at_callback.lock().unwrap().push(history.len());
        }
    });
    let pool = Pool::with_options(
        env.config(8),
        PoolOptions {
            sampler: env.sampler.clone(),
            daemon_guard: Some(guard),
            ..PoolOptions::default()
        },
    );
    pool.guard_daemon_rss().await.expect("a sample");
    assert_eq!(
        seen_at_callback.lock().unwrap().clone(),
        vec![1],
        "the history must already hold this exit when the shutdown is asked for"
    );
}

/// A platform that cannot measure is not a platform that is out of memory. If
/// "no sample" were read as "over the limit", the daemon would refuse to run at
/// all anywhere `/proc` is absent.
#[tokio::test]
async fn a_daemon_whose_memory_cannot_be_sampled_is_left_alone() {
    let env = Env::new();
    let pool = env.pool(1);
    env.sampler.make_absent();
    assert_eq!(pool.guard_daemon_rss().await, None);
    assert_eq!(env.shutdowns(), 0, "no sample must not become an exit");
    assert!(env.history().is_empty());
}

/// After the third over-limit exit inside the window the daemon keeps serving
/// and says so. This is the whole reason the history is on disk.
#[tokio::test]
async fn the_third_exit_in_an_hour_stops_the_restart_loop() {
    let env = Env::new();
    // Each `guard_daemon_rss` below plays one daemon lifetime, so the count
    // really does have to survive the process for the loop to stop.
    for expected in 1..=2 {
        let pool = env.pool(8);
        env.sampler.set_mib(9);
        pool.guard_daemon_rss().await.expect("a sample");
        assert_eq!(env.shutdowns(), expected, "exit {expected} is allowed");
    }
    // Two restarts have been paid for; the third must not happen.
    let third = env.pool(8);
    env.sampler.set_mib(9);
    third.guard_daemon_rss().await.expect("a sample");
    assert_eq!(
        env.shutdowns(),
        2,
        "the third exit must be refused, or the client restarts us forever"
    );
    assert!(
        third.daemon_rss_over_limit(),
        "and the state must be visible"
    );
    assert_eq!(env.history().len(), daemon_guard::RESTART_BUDGET + 1);
}

/// Coming back under the ceiling clears the refusal: the flag describes now, not
/// a past excursion, and a stale `true` would misreport a healthy daemon.
#[tokio::test]
async fn dropping_back_under_the_ceiling_clears_the_refusal() {
    let env = Env::new();
    for _ in 0..3 {
        let pool = env.pool(8);
        env.sampler.set_mib(9);
        pool.guard_daemon_rss().await.expect("a sample");
    }
    let pool = env.pool(8);
    assert!(pool.daemon_rss_over_limit());
    // A fresh history would also clear it, so the history is left alone and
    // only the sampler changes: the flag must follow the measurement.
    env.sampler.set_mib(1);
    pool.guard_daemon_rss().await.expect("a sample");
    assert!(
        !pool.daemon_rss_over_limit(),
        "a daemon back under its ceiling must stop claiming otherwise"
    );
    assert_eq!(env.shutdowns(), 2, "and it must not have exited again");
}

/// The server must not keep asking a client to stop once the history says the
/// loop is closed: the callback is the whole cost, and it must be quiet.
#[tokio::test]
async fn a_refused_daemon_keeps_serving_and_keeps_answering() {
    let env = Env::new();
    for _ in 0..3 {
        let pool = env.pool(8);
        env.sampler.set_mib(9);
        pool.guard_daemon_rss().await.expect("a sample");
    }
    let pool = env.pool(8);
    env.sampler.set_mib(9);
    for _ in 0..5 {
        pool.guard_daemon_rss().await.expect("a sample");
    }
    assert_eq!(env.shutdowns(), 2, "five more ticks must ask for nothing");
    // And it still serves: a real request against the fake server succeeds,
    // which is what "stays up and serving" has to mean.
    let backend = pool.bind(
        env.ws(),
        LanguageSelection::Explicit(["fake".into()].into()),
    );
    let file = env.file();
    env.sampler.set_mib(1);
    // The instance needs the sampler too; it is scripted, so it answers.
    hover(backend.as_ref(), &file)
        .await
        .expect("a refused daemon still answers");
    assert!(!env.pids().is_empty(), "the language server really started");
}

/// With no guard configured the guard is inert, so the per-instance tests that
/// run inside the test binary are not measured by a ceiling meant for a daemon.
#[tokio::test]
async fn a_pool_without_a_guard_never_asks_to_stop() {
    let env = Env::new();
    let pool = Pool::with_options(
        env.config(8),
        PoolOptions {
            sampler: env.sampler.clone(),
            ..PoolOptions::default()
        },
    );
    env.sampler.set_mib(9999);
    assert_eq!(pool.guard_daemon_rss().await, None);
    assert_eq!(env.shutdowns(), 0);
    assert!(!pool.daemon_rss_over_limit());
}

/// The language servers are shut down when the daemon goes over its ceiling.
/// This is the part that is about the *daemon* leaving: a restart that left
/// orphaned servers behind would leak the very memory the guard is watching.
#[tokio::test]
async fn an_over_limit_daemon_shuts_its_language_servers_down() {
    let env = Env::new();
    let pool = env.pool(8);
    let backend = pool.bind(
        env.ws(),
        LanguageSelection::Explicit(["fake".into()].into()),
    );
    let file = env.file();
    hover(backend.as_ref(), &file)
        .await
        .expect("the server starts");
    let pids = env.pids();
    assert!(!pids.is_empty(), "the fake server is running");
    assert!(pids.iter().all(|pid| alive(*pid)), "and alive");

    // Now the daemon goes over its own ceiling. The real daemon cancels its
    // shutdown token; here the callback stands in for that, and the pool is
    // shut down the same way `serve` does it.
    env.sampler.set_mib(9);
    pool.guard_daemon_rss().await.expect("a sample");
    assert_eq!(env.shutdowns(), 1);
    pool.shutdown().await;

    for pid in pids {
        let gone = wait_until_s(|| !alive(pid));
        assert!(gone, "language server {pid} outlived the daemon");
    }
}

fn wait_until_s(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}
