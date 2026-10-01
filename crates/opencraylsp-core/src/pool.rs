//! The shared instance pool .
//!
//! One [`Pool`] serves every client connection. It owns the language-server
//! instances — one per `(server, root)`, however many connections ask for it —
//! keeps their open documents in sync with the bytes on disk, and enforces the
//! resource policy: idle reclaim, an instance cap with least-recently-used
//! eviction, and a per-instance cap on open documents.
//!
//! Per-connection concerns (which directory is the boundary, which languages
//! are enabled) live in [`crate::manager::BoundBackend`]; the pool never sees a
//! connection.
//!
//! Lifecycle of an instance table entry — why leases exist: an entry may only
//! be retired (stopped and removed) while nobody is using it, and nobody may
//! start using it once retirement began. [`Pool::acquire`] hands out a
//! [`Lease`] under the table lock; retirement flips `retiring` under the same
//! lock only when no lease is out. Without that a request could be talking to
//! an instance the sweeper is shutting down, or a second instance for the same
//! key could appear while the first was still exiting.

use std::collections::{HashMap, VecDeque, hash_map::DefaultHasher};
use std::hash::Hasher;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use lsp_types::Diagnostic;
use opencraylsp_proto::{InstanceInfo, InstanceState as ProtoState};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::backend::{LspError, PositionEncoding};
use crate::config::{LspConfig, ServerConfig};
use crate::instance::{DiagnosticsSink, InstanceLimits, LspServerInstance};
use crate::memory::{MemorySampler, ProcSampler};
use crate::warmup;

/// How long a request waits for a free instance slot before `Capacity`.
const CAPACITY_WAIT: Duration = Duration::from_secs(10);

/// Memory restarts allowed per [`MEMORY_WINDOW`]; the next over-limit event
/// after that is a refusal, not another restart.
const MEMORY_RESTARTS_PER_WINDOW: usize = 3;

/// Sliding window for counting memory restarts.
const MEMORY_WINDOW: Duration = Duration::from_secs(3600);

/// How long a new request waits for an instance that is restarting because of
/// memory before it is told to retry.
const DRAIN_WAIT: Duration = Duration::from_secs(30);

/// How long in-flight requests get to finish before a memory restart proceeds.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval while waiting for a slot or for a retiring entry to vanish.
const WAIT_POLL: Duration = Duration::from_millis(25);

/// A file modified less than this long before we hashed it cannot be trusted
/// by its (mtime, size) alone: a same-size edit inside the filesystem's
/// timestamp granularity would go unnoticed.
const MTIME_TRUST_AGE: Duration = Duration::from_secs(2);

/// Tunables that are fixed in production but need shortening in tests.
/// Most file changes carried by one `workspace/didChangeWatchedFiles`.
const WATCH_BATCH: usize = 1000;

#[derive(Debug, Clone)]
pub struct PoolOptions {
    /// How long a request waits for a free instance slot before `Capacity`.
    pub capacity_wait: Duration,
    /// How long a request waits for an instance restarting over memory.
    pub drain_wait: Duration,
    /// How long in-flight requests get to finish before a memory restart.
    pub drain_timeout: Duration,
    /// The sliding window over which memory restarts are counted.
    pub memory_window: Duration,
    /// How process-tree memory is measured.
    pub sampler: Arc<dyn MemorySampler>,
    /// The daemon's own runaway guard: where its exit history is kept and what
    /// to do when the ceiling is hit.
    ///
    /// `None` disables the guard, which is what a test that only exercises the
    /// per-instance limits wants: those tests run inside the test binary, whose
    /// own memory is nobody's business.
    pub daemon_guard: Option<DaemonGuard>,
}

/// The daemon's own resident-memory ceiling, and the bookkeeping that keeps it
/// from becoming a restart loop.
///
/// Cloneable because [`PoolOptions`] is: a pool that handed out a copy of its
/// options would otherwise need to hand out its guard's state too, and the
/// guard's state is exactly what must be shared.
#[derive(Clone)]
pub struct DaemonGuard {
    /// Where the over-limit exit history is kept; see
    /// [`crate::daemon_guard::stamp_path`].
    pub stamp: PathBuf,
    /// What to do when the ceiling is hit and the history still allows an exit.
    ///
    /// A closure rather than an exit code so this crate, which is also a
    /// library, does not have to know how the daemon process ends: the
    /// embedding decides, and the tests decide differently from `serve`.
    pub on_over_limit: Arc<dyn Fn(u64, u64) + Send + Sync>,
    /// When the refusal was last logged, so it is said at most once a minute.
    /// Behind an `Arc` so a cloned pool shares it rather than re-announcing
    /// every minute of its own.
    last_refusal_logged: Arc<std::sync::Mutex<Option<std::time::SystemTime>>>,
    /// Whether the guard is currently refusing to exit.
    over_limit: Arc<AtomicBool>,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            capacity_wait: CAPACITY_WAIT,
            drain_wait: DRAIN_WAIT,
            drain_timeout: DRAIN_TIMEOUT,
            memory_window: MEMORY_WINDOW,
            sampler: Arc::new(ProcSampler::default()),
            // Off unless the embedding asks for it: this crate is a library,
            // and the test binary's own memory is not a runaway.
            daemon_guard: None,
        }
    }
}

/// Key for the instance table: one live child per `(server name, root)` so
/// two projects using the same language never share an index, while repeated
/// queries in one project reuse the warmed-up server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct InstanceKey {
    pub(crate) server: String,
    pub(crate) root: PathBuf,
}

/// One document the server has opened: the version it last saw plus a hash of
/// those bytes, so the next request can detect out-of-band edits.
#[derive(Debug, Clone)]
pub(crate) struct OpenDoc {
    pub(crate) path: PathBuf,
    pub(crate) version: i32,
    pub(crate) hash: u64,
    /// Metadata seen when `hash` was taken; lets an unchanged file skip the
    /// read-and-hash entirely.
    mtime: Option<SystemTime>,
    size: u64,
    /// `mtime` was old enough at hash time to be trusted (see [`MTIME_TRUST_AGE`]).
    trusted: bool,
    /// Monotonic use counter for least-recently-used eviction.
    tick: u64,
}

/// The newest diagnostics known for one URI.
#[derive(Debug, Clone)]
pub(crate) struct CachedDiagnostics {
    pub(crate) items: Vec<Diagnostic>,
    pub(crate) version: Option<i32>,
    pub(crate) encoding: PositionEncoding,
    /// When this entry arrived: backs both the settle quiet-period and the
    /// "did it arrive after we synced?" check for version-less servers.
    pub(crate) arrived: Instant,
}

/// How many documents the shared diagnostics cache keeps at once.
///
/// A language server publishes diagnostics for every file it has ever seen,
/// and a large workspace has tens of thousands of them. Each entry holds the
/// full `Vec<Diagnostic>`, so without a ceiling the cache grows with the union
/// of every document any instance ever touched — the daemon never restarts, so
/// that is monotonic. Past the cap the least recently arrived entries are
/// dropped, which is safe: a cached entry is only ever read by a request for
/// that exact document, and a document nobody asks about again does not need
/// its stale diagnostics.
const DIAGNOSTICS_CACHE_MAX_ENTRIES: usize = 4_096;

/// The diagnostics cache shared with every instance's `publishDiagnostics`
/// sink. Kept separate from the pool so sinks can own a clone without
/// borrowing the pool itself.
#[derive(Debug, Default)]
pub(crate) struct DiagnosticsCache {
    map: std::sync::Mutex<CacheState>,
    /// Fired on every insert; the diagnostics wait loop sleeps on this
    /// instead of polling the map blindly.
    pub(crate) notify: tokio::sync::Notify,
}

/// The cache's contents plus the arrival order needed to evict oldest-first.
#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<String, CachedDiagnostics>,
    /// URIs in arrival order, oldest first. Kept alongside the map so eviction
    /// is O(evicted) instead of a sort of the whole map on every insert past
    /// the cap.
    order: VecDeque<String>,
}

impl DiagnosticsCache {
    /// Records one event. The encoding is read from the owning instance's
    /// cell at arrival time, so a restart that renegotiates never leaves a
    /// stale encoding behind.
    fn store(
        &self,
        encoding: PositionEncoding,
        uri: String,
        version: Option<i32>,
        items: Vec<Diagnostic>,
    ) {
        if let Ok(mut state) = self.map.lock() {
            let arrived = Instant::now();
            // A re-publish for a document already tracked must not leave a
            // stale key in the arrival order, or the same URI would be evicted
            // twice and the order would grow forever.
            if state
                .entries
                .insert(
                    uri.clone(),
                    CachedDiagnostics {
                        items,
                        version,
                        encoding,
                        arrived,
                    },
                )
                .is_some()
            {
                // The document is already tracked, so its old position in the
                // arrival order is stale: drop every copy and re-push at the
                // back. The deque is rebuilt rather than `retain`ed in place
                // because `retain` borrows it mutably while its closure runs,
                // which conflicts with the map lookup the closure needs.
                let taken = std::mem::take(&mut state.order);
                let seen = &uri;
                state.order = taken.into_iter().filter(|t| t != seen).collect();
            }
            state.order.push_back(uri);
            while state.order.len() > DIAGNOSTICS_CACHE_MAX_ENTRIES
                && let Some(oldest) = state.order.pop_front()
            {
                state.entries.remove(&oldest);
            }
        }
        self.notify.notify_waiters();
    }

    pub(crate) fn get(&self, uri: &str) -> Option<CachedDiagnostics> {
        self.map.lock().ok()?.entries.get(uri).cloned()
    }

    /// How many documents are currently cached (a test hook, and the cheap
    /// way for an operator-facing test to prove eviction works).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map
            .lock()
            .map(|state| state.entries.len())
            .unwrap_or(0)
    }

    /// Forgets every entry belonging to `root`.
    ///
    /// Called when an instance is retired: the cache is shared by every
    /// instance in the pool, so an entry for a document under a retired root
    /// is unreachable forever — nothing will ever wait on that URI again,
    /// because the requests that could have waited have all completed or been
    /// cancelled. Keeping it would pin its `Vec<Diagnostic>` for the lifetime
    /// of the daemon.
    ///
    /// `root` is matched as a `file://` URI path prefix, so it removes exactly
    /// the documents at or below that root and nothing outside it.
    pub(crate) fn forget_root(&self, root: &Path) -> usize {
        let prefix = match root_to_uri_prefix(root) {
            Some(prefix) => prefix,
            None => return 0,
        };
        let Ok(mut state) = self.map.lock() else {
            return 0;
        };
        let doomed: Vec<String> = state
            .entries
            .keys()
            .filter(|uri| uri_is_under(uri, &prefix))
            .cloned()
            .collect();
        let removed = doomed.len();
        for uri in &doomed {
            state.entries.remove(uri);
        }
        // Keep the arrival order in step with the map, or a later eviction
        // would try to remove URIs that are already gone and the deque would
        // drift out of sync with its size. Collected into a new deque rather
        // than `retain`ed in place: `retain` holds the deque mutably while its
        // closure runs, which conflicts with reading the map at the same time.
        let kept: VecDeque<String> = {
            let old = std::mem::take(&mut state.order);
            old.into_iter()
                .filter(|uri| state.entries.contains_key(uri))
                .collect()
        };
        state.order = kept;
        removed
    }
}

/// The `file://` prefix that every URI under `root` starts with, with a
/// trailing separator so `/ws/a` never matches `/ws/ab`.
fn root_to_uri_prefix(root: &Path) -> Option<String> {
    let uri = url::Url::from_file_path(root).ok()?.to_string();
    Some(if uri.ends_with('/') {
        uri
    } else {
        format!("{uri}/")
    })
}

/// Whether a document URI sits at or below `prefix`.
fn uri_is_under(uri: &str, prefix: &str) -> bool {
    uri.starts_with(prefix)
}

/// Usage bookkeeping of one entry, guarded by one small mutex.
#[derive(Debug)]
struct Life {
    /// Leases currently out.
    inflight: usize,
    /// Set once retirement began; no new lease may be granted.
    retiring: bool,
    /// A memory restart is in progress: no new lease until it finishes.
    draining: bool,
    last_used: Instant,
}

/// Memory bookkeeping of one entry.
#[derive(Debug, Default)]
struct MemState {
    last_rss: Option<u64>,
    peak_rss: u64,
    /// Memory restarts performed, ever.
    total_restarts: u32,
    /// When the restarts inside the sliding window happened.
    events: VecDeque<Instant>,
    /// Set when the restart budget ran out: requests are refused until then.
    failed_until: Option<Instant>,
    failure: Option<String>,
}

impl MemState {
    fn failure_active(&self) -> bool {
        self.failed_until
            .is_some_and(|until| Instant::now() < until)
    }
}

/// The documents one instance has open.
#[derive(Debug, Default)]
pub(crate) struct DocTable {
    docs: HashMap<String, OpenDoc>,
    tick: u64,
}

impl DocTable {
    pub(crate) fn len(&self) -> usize {
        self.docs.len()
    }

    pub(crate) fn contains(&self, uri: &str) -> bool {
        self.docs.contains_key(uri)
    }

    fn touch(&mut self, uri: &str) {
        self.tick += 1;
        let tick = self.tick;
        if let Some(doc) = self.docs.get_mut(uri) {
            doc.tick = tick;
        }
    }
}

/// One instance table entry.
pub(crate) struct Entry {
    pub(crate) key: InstanceKey,
    /// The workspace boundary this entry reads within. Stored per entry
    /// because the pool serves many connections with different boundaries.
    pub(crate) boundary: PathBuf,
    pub(crate) instance: Arc<LspServerInstance>,
    pub(crate) docs: tokio::sync::Mutex<DocTable>,
    life: std::sync::Mutex<Life>,
    mem: std::sync::Mutex<MemState>,
}

impl Entry {
    fn life(&self) -> std::sync::MutexGuard<'_, Life> {
        // Poisoning only happens if a holder panicked; the counters inside are
        // still meaningful, so keep going rather than cascade the panic.
        self.life.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, MemState> {
        self.mem.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn idle_secs(&self) -> u64 {
        self.life().last_used.elapsed().as_secs()
    }

    /// The refusal to serve while the memory restart budget is exhausted, or
    /// `None`. An expired refusal is cleared here, together with the old
    /// restart history, so the instance starts over.
    fn memory_failure(&self) -> Option<LspError> {
        let mut mem = self.mem();
        let until = mem.failed_until?;
        if Instant::now() >= until {
            mem.failed_until = None;
            mem.failure = None;
            mem.events.clear();
            return None;
        }
        Some(LspError::ServerFailed {
            server: self.key.server.clone(),
            restarts: mem.total_restarts,
            last_error: mem.failure.clone().unwrap_or_default(),
        })
    }
}

/// Proof that an entry is in use: it cannot be retired while a lease exists.
pub(crate) struct Lease {
    pub(crate) entry: Arc<Entry>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut life = self.entry.life();
        life.inflight = life.inflight.saturating_sub(1);
        life.last_used = Instant::now();
    }
}

/// See the module docs.
pub struct Pool {
    pub(crate) config: Arc<LspConfig>,
    pub(crate) allowed_roots: Vec<PathBuf>,
    entries: tokio::sync::Mutex<HashMap<InstanceKey, Arc<Entry>>>,
    pub(crate) diagnostics: Arc<DiagnosticsCache>,
    started_at: Instant,
    options: PoolOptions,
    memory_guard_started: AtomicBool,
    idle_sweeper_started: AtomicBool,
    /// Bound connections currently alive (for `status`).
    pub(crate) connections: AtomicU32,
    watcher_started: AtomicBool,
    /// Documents actually read and hashed during sync; a test hook that proves
    /// the (mtime, size) fast path skips work.
    doc_reads: AtomicU64,
    /// The boundary new entries are created with, set by the connection that
    /// first asks for an instance. `None` until then.
    boundary_hint: std::sync::Mutex<Option<PathBuf>>,
}

impl std::fmt::Debug for DaemonGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonGuard")
            .field("stamp", &self.stamp)
            .field("over_limit", &self.over_limit.load(Ordering::SeqCst))
            .field("on_over_limit", &"set")
            .finish()
    }
}

impl DaemonGuard {
    /// A guard that shuts the daemon down through `on_over_limit`, keeping its
    /// history at `stamp`.
    pub fn new(
        stamp: impl Into<PathBuf>,
        on_over_limit: impl Fn(u64, u64) + Send + Sync + 'static,
    ) -> Self {
        Self {
            stamp: stamp.into(),
            on_over_limit: Arc::new(on_over_limit),
            last_refusal_logged: Arc::new(std::sync::Mutex::new(None)),
            over_limit: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the daemon is over its ceiling and refusing to exit.
    pub fn is_over_limit(&self) -> bool {
        self.over_limit.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("servers", &self.config.servers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Pool {
    /// Builds an empty pool. Spawns nothing: servers start lazily on first use.
    pub fn new(config: Arc<LspConfig>) -> Arc<Self> {
        Self::with_options(config, PoolOptions::default())
    }

    /// Like [`Self::new`] with a custom wait for a free instance slot (tests
    /// shorten it; the default is 10 s).
    pub fn with_capacity_wait(config: Arc<LspConfig>, capacity_wait: Duration) -> Arc<Self> {
        Self::with_options(
            config,
            PoolOptions {
                capacity_wait,
                ..PoolOptions::default()
            },
        )
    }

    /// Like [`Self::new`] with explicit [`PoolOptions`].
    pub fn with_options(config: Arc<LspConfig>, options: PoolOptions) -> Arc<Self> {
        let allowed_roots = config
            .allowed_roots
            .iter()
            .map(|p| canonicalize_best_effort(p))
            .collect();
        Arc::new(Self {
            config,
            allowed_roots,
            entries: tokio::sync::Mutex::new(HashMap::new()),
            diagnostics: Arc::new(DiagnosticsCache::default()),
            started_at: Instant::now(),
            options,
            memory_guard_started: AtomicBool::new(false),
            idle_sweeper_started: AtomicBool::new(false),
            connections: AtomicU32::new(0),
            watcher_started: AtomicBool::new(false),
            doc_reads: AtomicU64::new(0),
            boundary_hint: std::sync::Mutex::new(None),
        })
    }

    /// The configuration this pool runs under.
    pub fn config(&self) -> &Arc<LspConfig> {
        &self.config
    }

    pub(crate) fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Records the boundary new instances are created within.
    pub(crate) fn note_boundary(&self, boundary: &Path) {
        if let Ok(mut hint) = self.boundary_hint.lock() {
            hint.get_or_insert_with(|| boundary.to_owned());
        }
    }

    fn boundary_hint(&self) -> PathBuf {
        match self.boundary_hint.lock() {
            // No boundary recorded (tests that build entries directly): an
            // empty path disables the boundary re-check rather than failing every
            // read.
            Ok(hint) => hint.clone().unwrap_or_default(),
            Err(_) => PathBuf::new(),
        }
    }

    /// Documents read and hashed so far (test hook).
    pub fn doc_reads(&self) -> u64 {
        self.doc_reads.load(Ordering::Relaxed)
    }

    /// Number of instances in the table (running, starting or failed).
    pub async fn instance_count(&self) -> usize {
        self.entries.lock().await.len()
    }

    /// The root of `server`'s most recently used live instance, if it has one.
    ///
    /// A request that names no file cannot be routed by walking up from a
    /// directory, so when several projects share a boundary the only honest
    /// answers are "reuse the one that is already up" or "ask which project".
    /// Reusing is right far more often than not: the caller is usually in the
    /// same project as its previous question, and the alternative is a second
    /// multi-gigabyte server beside the first.
    ///
    /// Retiring and draining instances are skipped — they are on their way out
    /// and must not be adopted, and a `None` here falls through to project
    /// discovery, which is the better answer anyway.
    pub(crate) async fn most_recent_root_of(&self, server: &str) -> Option<PathBuf> {
        let entries: Vec<Arc<Entry>> = { self.entries.lock().await.values().cloned().collect() };
        let mut best: Option<(Instant, PathBuf)> = None;
        for entry in entries {
            if entry.key.server != server {
                continue;
            }
            let life = entry.life();
            if life.retiring || life.draining {
                continue;
            }
            let last_used = life.last_used;
            let root = entry.key.root.clone();
            drop(life);
            // The *newest* `last_used` wins, so the comparison keeps the larger
            // one. The same expression with the comparison flipped is
            // `pick_victim`, which wants the oldest — the two are opposite on
            // purpose, and getting this one backwards silently routes every
            // workspace request to whichever project the session touched first.
            if best.as_ref().is_none_or(|(at, _)| last_used > *at) {
                best = Some((last_used, root));
            }
        }
        best.map(|(_, root)| root)
    }

    fn new_entry(&self, key: &InstanceKey, server: &ServerConfig, boundary: &Path) -> Arc<Entry> {
        let limits = InstanceLimits {
            startup_timeout_ms: self.config.startup_timeout_ms,
            startup_grace_ms: self.config.startup_grace_ms,
            request_timeout_ms: self.config.request_timeout_ms,
            max_restarts: self.config.max_restarts,
            write_timeout_ms: self.config.write_timeout_ms,
        };
        let encoding = Arc::new(std::sync::Mutex::new(PositionEncoding::Utf16));
        let cache = self.diagnostics.clone();
        let encoding_for_sink = encoding.clone();
        let sink: DiagnosticsSink = Arc::new(move |uri, version, items| {
            let current = encoding_for_sink
                .lock()
                .ok()
                .map(|e| *e)
                .unwrap_or(PositionEncoding::Utf16);
            cache.store(current, uri, version, items);
        });
        Arc::new(Entry {
            key: key.clone(),
            boundary: boundary.to_owned(),
            instance: Arc::new(LspServerInstance::new(
                key.server.clone(),
                server.clone(),
                key.root.clone(),
                limits,
                encoding,
                sink,
            )),
            docs: tokio::sync::Mutex::new(DocTable::default()),
            mem: std::sync::Mutex::new(MemState::default()),
            life: std::sync::Mutex::new(Life {
                inflight: 1,
                retiring: false,
                draining: false,
                last_used: Instant::now(),
            }),
        })
    }

    /// Returns a lease on the entry for `key`, creating it (and making room
    /// under `max_instances`) when needed. Never spawns a process: that is
    /// `ensure_running`'s job, after the table lock is released.
    pub(crate) async fn acquire(
        &self,
        key: &InstanceKey,
        server: &ServerConfig,
        cancel: &CancellationToken,
    ) -> Result<Lease, LspError> {
        let started = Instant::now();
        let deadline = started + self.options.capacity_wait;
        let drain_deadline = started + self.options.drain_wait;
        loop {
            if cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            let mut victim: Option<Arc<Entry>> = None;
            {
                let mut entries = self.entries.lock().await;
                if let Some(entry) = entries.get(key) {
                    if let Some(refusal) = entry.memory_failure() {
                        return Err(refusal);
                    }
                    let mut life = entry.life();
                    if !life.retiring && !life.draining {
                        life.inflight += 1;
                        life.last_used = Instant::now();
                        drop(life);
                        return Ok(Lease {
                            entry: entry.clone(),
                        });
                    }
                    if life.draining && Instant::now() >= drain_deadline {
                        return Err(LspError::MemoryRestart {
                            server: key.server.clone(),
                        });
                    }
                    // Being retired (or restarted for memory): wait until it
                    // is gone or ready again.
                } else if entries.len() < self.config.max_instances {
                    let entry = self.new_entry(key, server, &self.boundary_hint());
                    entries.insert(key.clone(), entry.clone());
                    return Ok(Lease { entry });
                } else {
                    victim = Self::pick_victim(&entries);
                }
            }
            if let Some(victim) = victim {
                self.finish_retire(&victim).await;
                continue;
            }
            if Instant::now() >= deadline {
                return Err(LspError::Capacity {
                    limit: self.config.max_instances as u32,
                });
            }
            tokio::select! {
                () = tokio::time::sleep(WAIT_POLL) => {}
                () = cancel.cancelled() => return Err(LspError::Cancelled),
            }
        }
    }

    /// The least-recently-used entry nobody is using, marked as retiring. The
    /// caller must follow up with [`Self::finish_retire`].
    fn pick_victim(entries: &HashMap<InstanceKey, Arc<Entry>>) -> Option<Arc<Entry>> {
        let mut best: Option<(Instant, Arc<Entry>)> = None;
        for entry in entries.values() {
            let life = entry.life();
            if life.retiring || life.draining || life.inflight > 0 {
                continue;
            }
            if best.as_ref().is_none_or(|(at, _)| life.last_used < *at) {
                best = Some((life.last_used, entry.clone()));
            }
        }
        let (_, entry) = best?;
        // Re-check under the entry's own lock before claiming it: the scan
        // above released each lock, and a lease may have been granted since.
        {
            let mut life = entry.life();
            if life.retiring || life.draining || life.inflight > 0 {
                return None;
            }
            life.retiring = true;
        }
        Some(entry)
    }

    /// Stops a retiring entry's instance (outside every lock) and removes it.
    async fn finish_retire(&self, entry: &Arc<Entry>) {
        entry.instance.stop().await;
        entry.docs.lock().await.docs.clear();
        // The diagnostics cache is shared by every instance, so the entries
        // this root published have to be dropped explicitly or they outlive
        // the instance forever.
        self.diagnostics.forget_root(&entry.key.root);
        let mut entries = self.entries.lock().await;
        if entries
            .get(&entry.key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(&entry.key);
        }
    }

    /// Retires every idle instance whose last use is older than
    /// `idle_shutdown_secs` (`0` disables). Only entries with no lease out are
    /// touched, so a long-running request keeps its server.
    pub async fn sweep_idle(&self) {
        let limit = self.config.idle_shutdown_secs;
        if limit == 0 {
            return;
        }
        let candidates: Vec<Arc<Entry>> = {
            let entries = self.entries.lock().await;
            entries
                .values()
                .filter(|entry| {
                    let mut life = entry.life();
                    let idle = !life.retiring
                        && !life.draining
                        && life.inflight == 0
                        && life.last_used.elapsed().as_secs() >= limit;
                    if idle {
                        life.retiring = true;
                    }
                    idle
                })
                .cloned()
                .collect()
        };
        for entry in candidates {
            self.finish_retire(&entry).await;
        }
    }

    /// Leases an entry and makes sure its server is running.
    pub(crate) async fn prepare(
        &self,
        key: &InstanceKey,
        server: &ServerConfig,
        cancel: &CancellationToken,
    ) -> Result<Lease, LspError> {
        // Sweep before acquiring: acquiring touches the entry, so a sweep
        // after it would never see anything idle.
        self.sweep_idle().await;
        let lease = self.acquire(key, server, cancel).await?;
        lease.entry.instance.ensure_running(cancel).await?;
        Ok(lease)
    }

    /// Re-syncs every open document of `entry` whose bytes changed on disk,
    /// then `didOpen`s `file` if needed and returns its URI and version. A file
    /// that cannot be read is an honest `Io` error — the server must never be
    /// asked about content nobody sent it.
    pub(crate) async fn sync_for_request(
        &self,
        entry: &Entry,
        file: &Path,
        cancel: &CancellationToken,
    ) -> Result<(String, i32), LspError> {
        if cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        let instance = &entry.instance;
        let mut table = entry.docs.lock().await;
        // Re-sync previously opened documents first: the target file's own
        // `didOpen` below must see versions consistent with everything else
        // the server holds.
        let known: Vec<String> = table.docs.keys().cloned().collect();
        for uri in known {
            let Some(doc) = table.docs.get(&uri).cloned() else {
                continue;
            };
            let Ok(meta) = tokio::fs::metadata(&doc.path).await else {
                // Deleted out from under the server: close it so later
                // queries stop resolving into a ghost.
                let _ = instance
                    .notify(
                        "textDocument/didClose",
                        serde_json::json!({"textDocument": {"uri": uri}}),
                        cancel,
                    )
                    .await;
                table.docs.remove(&uri);
                continue;
            };
            let (mtime, size) = (meta.modified().ok(), meta.len());
            if doc.trusted && doc.mtime == mtime && doc.size == size {
                continue;
            }
            self.doc_reads.fetch_add(1, Ordering::Relaxed);
            let Ok(bytes) = tokio::fs::read(&doc.path).await else {
                continue;
            };
            let hash = hash_bytes(&bytes);
            let refreshed = |version: i32| OpenDoc {
                path: doc.path.clone(),
                version,
                hash,
                mtime,
                size,
                trusted: trusted(mtime),
                tick: doc.tick,
            };
            if hash == doc.hash {
                table.docs.insert(uri, refreshed(doc.version));
                continue;
            }
            let version = doc.version + 1;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            instance
                .notify(
                    "textDocument/didChange",
                    serde_json::json!({
                        "textDocument": {"uri": uri, "version": version},
                        "contentChanges": [{"text": text}],
                    }),
                    cancel,
                )
                .await?;
            table.docs.insert(uri, refreshed(version));
        }
        let uri = uri_for_path(file);
        if let Some(doc) = table.docs.get(&uri) {
            let version = doc.version;
            table.touch(&uri);
            return Ok((uri, version));
        }
        self.doc_reads.fetch_add(1, Ordering::Relaxed);
        // The boundary was checked when this path was resolved, but the
        // read happens a moment later; in between, the file (or a component of
        // it) could have been swapped for a symlink pointing outside the
        // boundary. `read_within` reads through a file handle and re-checks
        // that handle's own path *after* the bytes are in hand, so bytes that
        // came from outside are never handed to the server.
        let bytes = read_within(file, &entry.boundary, &self.allowed_roots)
            .await
            .map_err(|err| LspError::Io(format!("cannot read `{}`: {err}", file.display())))?;
        let meta = tokio::fs::metadata(file).await.ok();
        let mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let extension = file.extension().and_then(|e| e.to_str());
        let language = extension
            .and_then(|ext| {
                self.config
                    .servers
                    .get(&entry.key.server)
                    .and_then(|s| s.extensions.get(&ext.to_lowercase()))
            })
            .cloned()
            .unwrap_or_else(|| "plaintext".to_owned());
        let text = String::from_utf8_lossy(&bytes).into_owned();
        instance
            .notify(
                "textDocument/didOpen",
                serde_json::json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language,
                        "version": 1,
                        "text": text,
                    },
                }),
                cancel,
            )
            .await?;
        table.tick += 1;
        let tick = table.tick;
        table.docs.insert(
            uri.clone(),
            OpenDoc {
                path: file.to_owned(),
                version: 1,
                hash: hash_bytes(&bytes),
                mtime,
                size: bytes.len() as u64,
                trusted: trusted(mtime),
                tick,
            },
        );
        self.enforce_doc_cap(&mut table, instance, &uri, cancel)
            .await;
        Ok((uri, 1))
    }

    /// Closes the least-recently-used documents beyond `max_open_docs`, never
    /// the one just opened (`keep`).
    async fn enforce_doc_cap(
        &self,
        table: &mut DocTable,
        instance: &LspServerInstance,
        keep: &str,
        cancel: &CancellationToken,
    ) {
        let cap = self.config.max_open_docs;
        while table.docs.len() > cap {
            let Some(oldest) = table
                .docs
                .iter()
                .filter(|(uri, _)| uri.as_str() != keep)
                .min_by_key(|(_, doc)| doc.tick)
                .map(|(uri, _)| uri.clone())
            else {
                return;
            };
            let _ = instance
                .notify(
                    "textDocument/didClose",
                    serde_json::json!({"textDocument": {"uri": oldest}}),
                    cancel,
                )
                .await;
            table.docs.remove(&oldest);
        }
    }

    /// Per-instance rows for `status`.
    pub(crate) async fn instance_infos(&self) -> Vec<InstanceInfo> {
        let entries: Vec<Arc<Entry>> = { self.entries.lock().await.values().cloned().collect() };
        let mut infos = Vec::with_capacity(entries.len());
        for entry in entries {
            let indexing = entry.instance.indexing();
            let failed = entry.mem().failure_active();
            let state = if entry.life().retiring {
                ProtoState::Stopped
            } else if failed {
                ProtoState::Failed
            } else if entry.life().draining {
                ProtoState::Restarting
            } else {
                match entry.instance.state().await {
                    crate::instance::InstanceState::Stopped => ProtoState::Stopped,
                    crate::instance::InstanceState::Starting => ProtoState::Starting,
                    crate::instance::InstanceState::Running { .. } if indexing.is_some() => {
                        ProtoState::Indexing
                    }
                    crate::instance::InstanceState::Running { .. } => ProtoState::Ready,
                    crate::instance::InstanceState::Error { .. } => ProtoState::Failed,
                }
            };
            let pid = entry.instance.pid().await;
            let (rss_bytes, memory_restarts) = {
                let mem = entry.mem();
                (mem.last_rss, mem.total_restarts)
            };
            infos.push(InstanceInfo {
                server: entry.key.server.clone(),
                root: entry.key.root.display().to_string(),
                state,
                pid,
                rss_bytes,
                idle_secs: entry.idle_secs(),
                restarts: entry.instance.restarts(),
                memory_restarts,
                open_docs: entry.docs.lock().await.len() as u32,
                indexing,
            });
        }
        infos.sort_by(|a, b| (&a.server, &a.root).cmp(&(&b.server, &b.root)));
        infos
    }

    /// Shuts every instance down and empties the table. Safe to call twice.
    pub async fn shutdown(&self) {
        // Collect first, stop outside the lock.
        let entries: Vec<Arc<Entry>> = { self.entries.lock().await.values().cloned().collect() };
        let roots: Vec<PathBuf> = entries.iter().map(|entry| entry.key.root.clone()).collect();
        let mut handles = Vec::with_capacity(entries.len());
        for entry in entries {
            entry.life().retiring = true;
            handles.push(tokio::spawn(async move {
                entry.instance.stop().await;
            }));
        }
        for handle in handles {
            let _ = handle.await;
        }
        self.entries.lock().await.clear();
        // Everything is gone, so every cached diagnostic is unreachable. A
        // private pool that is then reused (a restart of the embedded backend)
        // must not inherit a cache full of entries for servers that no longer
        // exist.
        for entry in roots {
            self.diagnostics.forget_root(&entry);
        }
    }

    /// Starts the file watcher, the memory guard and the idle sweeper (all
    /// idempotent).
    pub fn spawn_maintenance(self: &Arc<Self>) {
        self.spawn_watcher();
        self.spawn_memory_guard();
        self.spawn_idle_sweeper();
    }

    /// Reclaims idle instances on a timer. Idle reclaim exists to give memory
    /// back; if it only ran when the next request arrived, a server left alone
    /// would hold gigabytes forever, which is exactly when nobody is asking.
    ///
    /// The period is half the idle limit, at least 1 s and at most 30 s, so an
    /// instance is gone within about one and a half idle periods. Does nothing
    /// when idle shutdown is disabled (`idle_shutdown_secs = 0`). Holds only a
    /// `Weak` reference, so dropping the pool ends the loop.
    pub fn spawn_idle_sweeper(self: &Arc<Self>) {
        let idle = self.config.idle_shutdown_secs;
        if idle == 0 {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::info!("lsp: no async runtime; idle sweeper not started");
            return;
        };
        if self.idle_sweeper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let period = Duration::from_secs((idle / 2).clamp(1, 30));
        let weak = Arc::downgrade(self);
        handle.spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let Some(pool) = weak.upgrade() else {
                    break;
                };
                pool.sweep_idle().await;
            }
        });
    }

    /// Starts sampling instance memory every `memory_sample_ms`. Holds only a
    /// `Weak` reference, so dropping the pool ends the loop.
    pub fn spawn_memory_guard(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::info!("lsp: no async runtime; memory guard not started");
            return;
        };
        if self.memory_guard_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let interval = Duration::from_millis(self.config.memory_sample_ms);
        let weak = Arc::downgrade(self);
        handle.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(pool) = weak.upgrade() else {
                    break;
                };
                pool.guard_once().await;
            }
        });
    }

    /// One sampling pass for the *daemon's own* memory.
    ///
    /// The figure is the daemon process alone: the language servers it
    /// supervises are its children, they have their own ceiling in
    /// `limits.max_rss_mb`, and adding them here would both charge the same
    /// memory twice and make this guard read a multi-gigabyte rust-analyzer as
    /// a runaway daemon.
    ///
    /// Runs on the same tick as the per-instance guard: the ceiling is a
    /// runaway guard, not a budget, so a sample a few seconds late costs
    /// nothing, and a second timer would be one more thing to reason about.
    ///
    /// The three outcomes, in order:
    ///
    /// * no sample (no `/proc`, or the read failed) — do nothing. A platform
    ///   that cannot measure is not a platform that is out of memory, and
    ///   treating the absence as "over the limit" would shut the daemon down
    ///   permanently on macOS.
    /// * under the ceiling — clear the refusal flag, because it describes a
    ///   past excursion and `lsp_status` must not keep reporting it.
    /// * over the ceiling — record the exit and ask the embedding to shut down,
    ///   unless the history says this has happened too often already, in which
    ///   case keep serving and say so instead.
    pub async fn guard_daemon_rss(self: &Arc<Self>) -> Option<u64> {
        let guard = self.options.daemon_guard.as_ref()?;
        let limit_mib = self.config.daemon_max_rss_mb;
        let Some(rss) = self.measured_self_rss().await else {
            tracing::debug!("daemon memory could not be sampled; the guard does not act");
            return None;
        };
        if rss <= limit_mib.saturating_mul(1024 * 1024) {
            if guard.over_limit.swap(false, Ordering::SeqCst) {
                tracing::info!("daemon memory is back under its ceiling");
            }
            return Some(rss);
        }
        let now = std::time::SystemTime::now();
        // The history is read *after* this exit is folded in, so the count
        // includes the one being decided. Reading it first would refuse one
        // daemon too late: exit 1 would see 0 and record, exit 2 would see 1
        // and record, exit 3 would see 2 and record — and only a fourth tick
        // would ever be refused, which is one restart more than the budget.
        let mut history = crate::daemon_guard::read_stamp(&guard.stamp);
        if let Err(error) = crate::daemon_guard::record_exit(&guard.stamp, now) {
            // The history is what stops a restart loop, so failing to write
            // it is worth saying, but not worth refusing to exit: the
            // alternative is a daemon that stays up and leaks.
            tracing::warn!(
                error = %error,
                path = %guard.stamp.display(),
                "could not record the over-limit exit; the next daemon may restart-loop"
            );
        }
        if let Ok(epoch) = now.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            history.push(epoch.as_secs());
        }
        match crate::daemon_guard::verdict(&history, now) {
            crate::daemon_guard::Verdict::Record => {
                tracing::warn!(
                    rss_mib = rss / (1024 * 1024),
                    limit_mib,
                    "daemon exceeds its own memory ceiling; shutting down so the client can \
                     start a fresh one (raise limits.daemon_max_rss_mb, or investigate the leak)"
                );
                (guard.on_over_limit)(rss, limit_mib.saturating_mul(1024 * 1024));
            }
            crate::daemon_guard::Verdict::Refuse { exits } => {
                guard.over_limit.store(true, Ordering::SeqCst);
                let mut last = guard
                    .last_refusal_logged
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if crate::daemon_guard::may_log_refusal(*last, now) {
                    tracing::error!(
                        rss_mib = rss / (1024 * 1024),
                        limit_mib,
                        exits,
                        "daemon is over its memory ceiling and has been restarted too often in the \
                         last hour; staying up and serving rather than restarting in a loop \
                         (raise limits.daemon_max_rss_mb, or investigate the leak)"
                    );
                    *last = Some(now);
                }
            }
        }
        Some(rss)
    }

    /// Whether the daemon is currently over its ceiling and refusing to exit.
    pub fn daemon_rss_over_limit(&self) -> bool {
        self.options
            .daemon_guard
            .as_ref()
            .is_some_and(|guard| guard.over_limit.load(Ordering::SeqCst))
    }

    /// The daemon's own resident memory, sampled the way the tick needs it:
    /// `/proc` reads are blocking file I/O, so they go to a blocking thread
    /// rather than parking an async worker. Only this process: descendants are
    /// the language servers, which are accounted against `max_rss_mb`.
    async fn measured_self_rss(&self) -> Option<u64> {
        let sampler = self.options.sampler.clone();
        tokio::task::spawn_blocking(move || sampler.self_rss_bytes(std::process::id()))
            .await
            .ok()
            .flatten()
    }

    pub async fn guard_once(self: &Arc<Self>) {
        self.guard_daemon_rss().await;
        let limit = self.config.max_rss_mb.saturating_mul(1024 * 1024);
        let entries: Vec<Arc<Entry>> = { self.entries.lock().await.values().cloned().collect() };
        for entry in entries {
            let Some(pid) = entry.instance.pid().await else {
                entry.mem().last_rss = None;
                continue;
            };
            // `/proc` reads are blocking file I/O; keep them off the async workers.
            let sampler = self.options.sampler.clone();
            let sampled = tokio::task::spawn_blocking(move || sampler.tree_rss_bytes(pid))
                .await
                .ok()
                .flatten();
            let Some(rss) = sampled else {
                // No sample this pass: the figure from the last one is no
                // longer a measurement of anything, so it is cleared rather
                // than kept. `lsp_status` prints `n/a` for `None`, which is
                // the truth; a repeated stale number is not.
                entry.mem().last_rss = None;
                continue;
            };
            {
                let mut mem = entry.mem();
                mem.last_rss = Some(rss);
                mem.peak_rss = mem.peak_rss.max(rss);
            }
            // Indexing peaks are transient (and restarting would restart the
            // peak), so a server that is busy indexing gets twice the room.
            let ceiling = if entry.instance.indexing().is_some() {
                limit.saturating_mul(2)
            } else {
                limit
            };
            if rss > ceiling {
                let pool = self.clone();
                tokio::spawn(async move { pool.enforce_memory_limit(&entry, rss).await });
            }
        }
    }

    /// Restarts `entry` because it holds `rss` bytes, or — when it already
    /// used up the restart budget for the window — refuses to run it again for
    /// a while, with a message saying why and what to change.
    ///
    /// Sequence: mark draining (no new leases) → wait for in-flight requests
    /// (bounded) → stop the server → clear its documents → reopen for leases,
    /// which start a fresh server on demand.
    pub(crate) async fn enforce_memory_limit(&self, entry: &Arc<Entry>, rss: u64) {
        {
            let mut life = entry.life();
            if life.retiring || life.draining {
                return;
            }
            life.draining = true;
        }
        let limit_mib = self.config.max_rss_mb;
        tracing::warn!(
            server = %entry.key.server,
            root = %entry.key.root.display(),
            rss_mib = rss / (1024 * 1024),
            limit_mib,
            "instance exceeds its memory limit; draining for restart"
        );
        let drain_until = Instant::now() + self.options.drain_timeout;
        while entry.life().inflight > 0 && Instant::now() < drain_until {
            tokio::time::sleep(WAIT_POLL).await;
        }
        let refused = {
            let mut mem = entry.mem();
            let now = Instant::now();
            let window = self.options.memory_window;
            mem.events.retain(|at| now.duration_since(*at) < window);
            if mem.events.len() >= MEMORY_RESTARTS_PER_WINDOW {
                let oldest = mem.events.front().copied().unwrap_or(now);
                let until = oldest + window;
                let peak_mib = mem.peak_rss.max(rss) / (1024 * 1024);
                mem.failed_until = Some(until);
                mem.failure = Some(format!(
                    "exceeded its memory limit of {limit_mib} MiB {MEMORY_RESTARTS_PER_WINDOW} times \
                     within {} minutes (peak {peak_mib} MiB), so it is not restarted again until \
                     that window passes. Raise `max_rss_mb` in the opencraylspd config, enable fewer \
                     languages for this workspace, or point the harness at a smaller project",
                    window.as_secs() / 60
                ));
                true
            } else {
                mem.events.push_back(now);
                mem.total_restarts += 1;
                false
            }
        };
        entry.instance.stop().await;
        entry.docs.lock().await.docs.clear();
        // Same reasoning as `finish_retire`: the cache outlives the instance.
        self.diagnostics.forget_root(&entry.key.root);
        entry.mem().last_rss = None;
        if refused {
            tracing::error!(server = %entry.key.server, "memory restart budget exhausted; refusing to run it");
        }
        entry.life().draining = false;
    }

    /// Starts the file watcher once (no-op when `watch_interval_ms` is 0 or
    /// there is no async runtime). Holds only a `Weak` reference, so dropping
    /// the pool ends the loop.
    pub fn spawn_watcher(self: &Arc<Self>) {
        let interval = self.config.watch_interval_ms;
        if interval == 0 {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::info!("lsp: no async runtime; watcher not started");
            return;
        };
        if self.watcher_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        handle.spawn(async move {
            let mut snapshots: HashMap<InstanceKey, warmup::Snapshot> = HashMap::new();
            loop {
                tokio::time::sleep(Duration::from_millis(interval)).await;
                let Some(pool) = weak.upgrade() else {
                    break;
                };
                pool.watch_once(&mut snapshots).await;
            }
        });
    }

    /// One watcher pass: for each running instance, diff its root's files
    /// against the previous pass and tell the server what changed. Open
    /// documents among the changes are re-synced with their new text too.
    /// The first pass for an instance only records a baseline.
    pub(crate) async fn watch_once(&self, snapshots: &mut HashMap<InstanceKey, warmup::Snapshot>) {
        let entries: Vec<Arc<Entry>> = { self.entries.lock().await.values().cloned().collect() };
        snapshots.retain(|key, _| entries.iter().any(|e| &e.key == key));
        let cancel = CancellationToken::new();
        for entry in entries {
            let key = entry.key.clone();
            if !entry.instance.is_healthy().await {
                continue;
            }
            let Some(server) = self.config.servers.get(&key.server) else {
                continue;
            };
            let root = key.root.clone();
            let server_for_scan = server.clone();
            let exclude = self.config.warmup_exclude.clone();
            // The walk is blocking filesystem work: keep it off the runtime's
            // worker threads.
            let Ok(Some(now)) = tokio::task::spawn_blocking(move || {
                warmup::snapshot(&root, &server_for_scan, &exclude)
            })
            .await
            else {
                // Too big to snapshot reliably (or the scan panicked): forget
                // any baseline so no phantom diff is ever sent for this root.
                snapshots.remove(&key);
                continue;
            };
            let Some(before) = snapshots.insert(key.clone(), now) else {
                continue;
            };
            let after = &snapshots[&key];
            let changes = warmup::diff(&before, after);
            if changes.is_empty() {
                continue;
            }
            // A `git checkout` or `cargo clean` can change tens of thousands
            // of files; send them in bounded batches, never as one huge frame.
            let mut failed = false;
            for batch in changes.chunks(WATCH_BATCH) {
                if let Err(err) = entry
                    .instance
                    .notify(
                        "workspace/didChangeWatchedFiles",
                        warmup::watched_files_params(batch),
                        &cancel,
                    )
                    .await
                {
                    tracing::warn!(server = %key.server, error = %err, "lsp watcher: notify failed");
                    failed = true;
                    break;
                }
            }
            if failed {
                // Keep the old baseline so the next pass reports this batch
                // again instead of losing it for good.
                snapshots.insert(key.clone(), before);
                continue;
            }
            let open_changed = {
                let table = entry.docs.lock().await;
                changes
                    .iter()
                    .find(|(path, kind)| {
                        *kind == warmup::Change::Changed && table.contains(&uri_for_path(path))
                    })
                    .map(|(path, _)| path.clone())
            };
            if let Some(path) = open_changed {
                // Re-syncs every open document of this instance, not just `path`.
                let _ = self.sync_for_request(&entry, &path, &cancel).await;
            }
            tracing::debug!(server = %key.server, changes = changes.len(), "lsp watcher: sent file changes");
        }
    }
}

fn trusted(mtime: Option<SystemTime>) -> bool {
    mtime.is_some_and(|m| {
        SystemTime::now()
            .duration_since(m)
            .is_ok_and(|age| age >= MTIME_TRUST_AGE)
    })
}

/// Reads `file`, refusing bytes that a path swap smuggled in from outside
/// `boundary`.
///
/// One handle serves both the check and the read. That is the whole point, and
/// it is why this cannot be written as "read the file, then check the path":
/// `resolve_path` canonicalises a name the caller is about to open, and
/// anything can change between that check and the open — so the object that has
/// to be checked is the one the bytes came from.
///
/// `/proc/self/fd/<n>` names that object for an open descriptor, on the same
/// descriptor the bytes are read from. Opening the path a *second* time to
/// resolve it does not work: the second open is a different object if the name
/// was swapped in between, and the check then describes something other than
/// what was read. `read_within_refuses_bytes_from_a_handle_it_never_checked`
/// drives exactly that swap.
///
/// The read also happens *after* the check, so a file outside the boundary is
/// never pulled into memory at all.
///
/// A boundary of "" means no boundary was recorded (some tests build entries
/// directly); the read is then returned unchecked rather than always failing.
async fn read_within(
    file: &Path,
    boundary: &Path,
    allowed_roots: &[PathBuf],
) -> std::io::Result<Vec<u8>> {
    if boundary.as_os_str().is_empty() {
        return tokio::fs::read(file).await;
    }
    let mut handle = tokio::fs::File::open(file).await?;
    let opened = opened_path_of(&handle).await;
    let candidate = opened.unwrap_or_else(|| canonicalize_best_effort(file));
    let inside = candidate.starts_with(boundary)
        || allowed_roots.iter().any(|root| candidate.starts_with(root));
    if !inside {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "`{}` resolves outside the workspace boundary `{}` once opened; \
                 refusing to hand its contents to a language server",
                file.display(),
                boundary.display()
            ),
        ));
    }
    let mut bytes = Vec::new();
    handle.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

/// The path an open descriptor refers to, when the platform can tell us
/// (`/proc/self/fd`). `None` elsewhere, in which case the caller falls back to
/// re-canonicalising the name it used — best effort, and documented as such at
/// its only call site.
async fn opened_path_of(handle: &tokio::fs::File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    let link = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()));
    tokio::fs::read_link(&link).await.ok()
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

/// Finds the server root for `file`: walking upward from the file's directory
/// without crossing above `boundary`, the *topmost* directory holding any of
/// `markers` wins (a Cargo workspace must resolve to its top, not to a member
/// crate). No markers, or no marker found, means the boundary itself.
pub(crate) fn find_root(boundary: &Path, markers: &[String], file: &Path) -> PathBuf {
    if markers.is_empty() {
        return boundary.to_owned();
    }
    let start = file.parent().unwrap_or(file);
    let mut topmost: Option<PathBuf> = None;
    let mut dir: Option<&Path> = Some(start);
    while let Some(current) = dir {
        if current == boundary || current.starts_with(boundary) {
            if markers.iter().any(|m| current.join(m).exists()) {
                // Walking upward, so each match is higher than the last: the
                // final one is the topmost.
                topmost = Some(current.to_owned());
            }
            if current == boundary {
                break;
            }
            dir = current.parent();
        } else {
            // Left the boundary (an allowed-root file): stop, do not adopt an
            // outside project as the root.
            break;
        }
    }
    topmost.unwrap_or_else(|| boundary.to_owned())
}

/// Lexical normalization only (no disk access): resolves `.`/`..` so the
/// boundary check cannot be fooled by `sub/../../etc`.
pub(crate) fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Canonicalizes when the path (or an ancestor) exists; falls back to the
/// lexical form otherwise. Never fails: resolution must work for files about
/// to be created as well as files on disk.
pub(crate) fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    // Walk up to the nearest existing ancestor, canonicalize that, re-append.
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cursor) {
            let mut rebuilt = canonical;
            for part in missing.iter().rev() {
                rebuilt.push(part);
            }
            return normalize_lexically(&rebuilt);
        }
        if let Some(name) = cursor.file_name() {
            missing.push(name.to_owned());
            if let Some(parent) = cursor.parent() {
                cursor = parent;
                continue;
            }
        }
        return normalize_lexically(path);
    }
}

/// `file://` URI for one absolute path. Falls back to a lossy manual mapping
/// on platforms where the path is not strictly representable — a best-effort
/// URI still routes better than an error.
pub(crate) fn uri_for_path(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

/// Whether `command` resolves to an executable: an absolute/relative path that
/// exists, or a name found on `PATH`.
pub(crate) fn command_exists(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(command).is_file()))
        .unwrap_or(false)
}

/// Where `command` resolves to on `PATH`, regardless of trust.
///
/// Returns `None` only when the name is not found at all, so a caller can tell
/// "not installed" apart from "installed but not safe to run" — and name the
/// offending path in the error. [`trusted_command_path`] is the variant that
/// also demands the file be the user's own.
pub(crate) fn resolve_command_path(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_owned());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(command))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn server() -> ServerConfig {
        ServerConfig {
            command: "x".to_owned(),
            args: Vec::new(),
            env: Default::default(),
            extensions: Default::default(),
            root_markers: Vec::new(),
            workspace: None,
            initialization_options: None,
            settings: None,
        }
    }

    #[test]
    fn find_root_prefers_topmost_marker() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("ws");
        let inner = outer.join("member");
        write_file(&outer.join("Cargo.toml"), "[workspace]");
        write_file(&inner.join("Cargo.toml"), "[package]");
        let file = inner.join("src").join("main.rs");
        write_file(&file, "fn main() {}");
        let root = find_root(&outer, &["Cargo.toml".to_owned()], &file);
        assert_eq!(root, outer);
    }

    #[test]
    fn find_root_falls_back_to_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let boundary = dir.path().join("ws");
        let file = boundary.join("a.rs");
        write_file(&file, "");
        assert_eq!(find_root(&boundary, &[], &file), boundary);
        assert_eq!(
            find_root(&boundary, &["go.mod".to_owned()], &file),
            boundary
        );
    }

    #[test]
    fn find_root_never_crosses_above_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let boundary = dir.path().join("ws");
        write_file(&dir.path().join("Cargo.toml"), "[workspace]");
        let file = boundary.join("a.rs");
        write_file(&file, "");
        // The only Cargo.toml is *above* the boundary: must not be adopted.
        assert_eq!(
            find_root(&boundary, &["Cargo.toml".to_owned()], &file),
            boundary
        );
    }

    #[test]
    fn find_root_stops_at_allowed_root_files() {
        let dir = tempfile::tempdir().unwrap();
        let boundary = dir.path().join("ws");
        write_file(&dir.path().join("Cargo.toml"), "[workspace]");
        let outside = dir.path().join("elsewhere").join("a.rs");
        write_file(&outside, "");
        assert_eq!(
            find_root(&boundary, &["Cargo.toml".to_owned()], &outside),
            boundary
        );
    }

    #[test]
    fn canonicalize_falls_back_for_missing_relative_paths() {
        let out = canonicalize_best_effort(Path::new("no/such/dir/file.fl"));
        assert_eq!(out, PathBuf::from("no/such/dir/file.fl"));
        let out = normalize_lexically(Path::new("/a/b/../c/./d.fl"));
        assert_eq!(out, PathBuf::from("/a/c/d.fl"));
    }

    #[test]
    fn canonicalize_resolves_the_existing_prefix_of_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let out = canonicalize_best_effort(&dir.path().join("new/dir/x.rs"));
        assert_eq!(out, real.join("new/dir/x.rs"));
    }

    #[test]
    fn uri_for_absolute_path_is_a_file_uri() {
        assert_eq!(
            uri_for_path(Path::new("/tmp/a b.rs")),
            "file:///tmp/a%20b.rs"
        );
    }

    #[test]
    fn command_exists_checks_paths_and_path_lookup() {
        assert!(command_exists("sh"));
        assert!(command_exists("/bin/sh"));
        assert!(!command_exists("/definitely/not/here"));
        assert!(!command_exists("definitely-not-installed-xyz"));
    }

    #[test]
    fn trusted_needs_an_old_enough_mtime() {
        let now = SystemTime::now();
        assert!(!trusted(None));
        assert!(!trusted(Some(now)));
        assert!(trusted(Some(now - Duration::from_secs(60))));
        // A timestamp in the future can never be trusted.
        assert!(!trusted(Some(now + Duration::from_secs(60))));
    }

    #[test]
    fn hash_bytes_distinguishes_content() {
        assert_eq!(hash_bytes(b"a"), hash_bytes(b"a"));
        assert_ne!(hash_bytes(b"a"), hash_bytes(b"b"));
    }

    #[tokio::test]
    async fn sync_honors_pre_cancelled_token() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig::default()));
        let key = InstanceKey {
            server: "fake".to_owned(),
            root: dir.path().to_owned(),
        };
        let entry = pool.new_entry(&key, &server(), dir.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let file = dir.path().join("a.fl");
        write_file(&file, "x");
        assert_eq!(
            pool.sync_for_request(&entry, &file, &cancel)
                .await
                .unwrap_err(),
            LspError::Cancelled
        );
    }

    #[tokio::test]
    async fn acquire_honors_a_pre_cancelled_token() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig::default()));
        let key = InstanceKey {
            server: "fake".to_owned(),
            root: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            pool.acquire(&key, &server(), &cancel).await,
            Err(LspError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn sweep_with_zero_limit_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig {
            idle_shutdown_secs: 0,
            ..LspConfig::default()
        }));
        let key = InstanceKey {
            server: "fake".to_owned(),
            root: dir.path().to_owned(),
        };
        drop(
            pool.acquire(&key, &server(), &CancellationToken::new())
                .await
                .unwrap(),
        );
        pool.sweep_idle().await;
        assert_eq!(pool.instance_count().await, 1);
    }

    #[tokio::test]
    async fn sweep_reclaims_only_idle_unleased_entries() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig {
            idle_shutdown_secs: 1,
            ..LspConfig::default()
        }));
        let key = |name: &str| InstanceKey {
            server: name.to_owned(),
            root: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        let held = pool
            .acquire(&key("held"), &server(), &cancel)
            .await
            .unwrap();
        drop(
            pool.acquire(&key("idle"), &server(), &cancel)
                .await
                .unwrap(),
        );
        // Nothing is old enough yet.
        pool.sweep_idle().await;
        assert_eq!(pool.instance_count().await, 2);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        pool.sweep_idle().await;
        // The idle one is gone; the leased one survives even though it is old.
        assert_eq!(pool.instance_count().await, 1);
        drop(held);
    }

    #[tokio::test]
    async fn pick_victim_takes_the_least_recently_used_idle_entry() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig {
            max_instances: 2,
            ..LspConfig::default()
        }));
        let key = |name: &str| InstanceKey {
            server: name.to_owned(),
            root: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        drop(pool.acquire(&key("a"), &server(), &cancel).await.unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        drop(pool.acquire(&key("b"), &server(), &cancel).await.unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        // Touch `a` again so `b` becomes the older one.
        drop(pool.acquire(&key("a"), &server(), &cancel).await.unwrap());
        // A third key must evict `b`.
        drop(pool.acquire(&key("c"), &server(), &cancel).await.unwrap());
        let mut left: Vec<String> = pool
            .instance_infos()
            .await
            .into_iter()
            .map(|i| i.server)
            .collect();
        left.sort();
        assert_eq!(left, vec!["a", "c"]);
    }

    #[tokio::test]
    async fn a_leased_oldest_entry_does_not_shield_a_newer_idle_one_from_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::with_capacity_wait(
            Arc::new(LspConfig {
                max_instances: 2,
                ..LspConfig::default()
            }),
            Duration::from_millis(300),
        );
        let key = |name: &str| InstanceKey {
            server: name.to_owned(),
            root: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        // `a` is the oldest but stays leased; `b` is newer and idle.
        let held = pool.acquire(&key("a"), &server(), &cancel).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        drop(pool.acquire(&key("b"), &server(), &cancel).await.unwrap());
        // Making room for `c` must evict `b`, not give up on `a` being busy.
        pool.acquire(&key("c"), &server(), &cancel)
            .await
            .expect("the idle entry is evictable");
        let mut left: Vec<String> = pool
            .instance_infos()
            .await
            .into_iter()
            .map(|i| i.server)
            .collect();
        left.sort();
        assert_eq!(left, vec!["a", "c"]);
        drop(held);
    }

    #[tokio::test]
    async fn acquire_reports_capacity_when_everything_is_leased() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::with_capacity_wait(
            Arc::new(LspConfig {
                max_instances: 1,
                ..LspConfig::default()
            }),
            Duration::from_millis(100),
        );
        let key = |name: &str| InstanceKey {
            server: name.to_owned(),
            root: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        let _held = pool.acquire(&key("a"), &server(), &cancel).await.unwrap();
        let started = Instant::now();
        let err = pool
            .acquire(&key("b"), &server(), &cancel)
            .await
            .err()
            .expect("no slot is free");
        assert_eq!(err, LspError::Capacity { limit: 1 });
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn a_waiting_acquire_gets_the_slot_when_a_lease_ends() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::with_capacity_wait(
            Arc::new(LspConfig {
                max_instances: 1,
                ..LspConfig::default()
            }),
            Duration::from_secs(5),
        );
        let key = |name: &str| InstanceKey {
            server: name.to_owned(),
            root: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        let held = pool.acquire(&key("a"), &server(), &cancel).await.unwrap();
        let waiter = {
            let pool = pool.clone();
            let key = key("b");
            tokio::spawn(async move {
                pool.acquire(&key, &server(), &CancellationToken::new())
                    .await
                    .is_ok()
            })
        };
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(held);
        assert!(waiter.await.unwrap(), "the waiter must get the freed slot");
        assert_eq!(pool.instance_count().await, 1);
    }

    #[tokio::test]
    async fn a_cancelled_waiter_stops_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::with_capacity_wait(
            Arc::new(LspConfig {
                max_instances: 1,
                ..LspConfig::default()
            }),
            Duration::from_secs(30),
        );
        let key = |name: &str| InstanceKey {
            server: name.to_owned(),
            root: dir.path().to_owned(),
        };
        let _held = pool
            .acquire(&key("a"), &server(), &CancellationToken::new())
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });
        let started = Instant::now();
        let result = pool.acquire(&key("b"), &server(), &cancel).await;
        assert!(matches!(result, Err(LspError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_retiring_entry_is_not_reused_and_a_fresh_one_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig::default()));
        let key = InstanceKey {
            server: "a".to_owned(),
            root: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        let first = pool.acquire(&key, &server(), &cancel).await.unwrap();
        let first_entry = first.entry.clone();
        drop(first);
        first_entry.life().retiring = true;
        let pool2 = pool.clone();
        let key2 = key.clone();
        let waiter = tokio::spawn(async move {
            pool2
                .acquire(&key2, &server(), &CancellationToken::new())
                .await
                .map(|lease| lease.entry.clone())
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!waiter.is_finished(), "must wait while the entry retires");
        pool.finish_retire(&first_entry).await;
        let second = waiter.await.unwrap().unwrap();
        assert!(
            !Arc::ptr_eq(&first_entry, &second),
            "a fresh entry, not the old one"
        );
    }
    #[tokio::test]
    async fn a_draining_entry_is_neither_swept_nor_chosen_as_victim() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig {
            idle_shutdown_secs: 1,
            max_instances: 2,
            ..LspConfig::default()
        }));
        let key = |name: &str| InstanceKey {
            server: name.to_owned(),
            root: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        let lease = pool.acquire(&key("a"), &server(), &cancel).await.unwrap();
        let a = lease.entry.clone();
        drop(lease);
        a.life().draining = true;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        pool.sweep_idle().await;
        assert_eq!(
            pool.instance_count().await,
            1,
            "draining entries survive the sweep"
        );
        drop(pool.acquire(&key("b"), &server(), &cancel).await.unwrap());
        drop(pool.acquire(&key("c"), &server(), &cancel).await.unwrap());
        let mut left: Vec<String> = pool
            .instance_infos()
            .await
            .into_iter()
            .map(|i| i.server)
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec!["a", "c"],
            "b was evicted, the draining a was not"
        );
    }

    #[tokio::test]
    async fn enforcing_the_limit_on_an_already_draining_entry_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig::default()));
        let key = InstanceKey {
            server: "a".to_owned(),
            root: dir.path().to_owned(),
        };
        let lease = pool
            .acquire(&key, &server(), &CancellationToken::new())
            .await
            .unwrap();
        let entry = lease.entry.clone();
        drop(lease);
        entry.life().draining = true;
        pool.enforce_memory_limit(&entry, 10 << 30).await;
        assert_eq!(entry.mem().total_restarts, 0);
        assert!(entry.life().draining, "the other restart keeps its claim");
        entry.life().draining = false;
        entry.life().retiring = true;
        pool.enforce_memory_limit(&entry, 10 << 30).await;
        assert_eq!(entry.mem().total_restarts, 0);
    }

    #[test]
    fn an_expired_memory_refusal_clears_itself_and_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig::default()));
        let key = InstanceKey {
            server: "a".to_owned(),
            root: dir.path().to_owned(),
        };
        let entry = pool.new_entry(&key, &server(), dir.path());
        assert!(entry.memory_failure().is_none());
        {
            let mut mem = entry.mem();
            mem.failed_until = Some(Instant::now() + Duration::from_secs(60));
            mem.failure = Some("too much".to_owned());
            mem.total_restarts = 3;
            mem.events.push_back(Instant::now());
        }
        match entry.memory_failure() {
            Some(LspError::ServerFailed {
                restarts,
                last_error,
                ..
            }) => {
                assert_eq!(restarts, 3);
                assert_eq!(last_error, "too much");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(entry.mem().failure_active());
        entry.mem().failed_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(entry.memory_failure().is_none());
        assert!(
            entry.mem().events.is_empty(),
            "history restarts with the window"
        );
        assert!(!entry.mem().failure_active());
    }
    #[tokio::test]
    async fn the_idle_sweeper_reclaims_without_any_request() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig {
            idle_shutdown_secs: 1,
            ..LspConfig::default()
        }));
        let key = InstanceKey {
            server: "a".to_owned(),
            root: dir.path().to_owned(),
        };
        drop(
            pool.acquire(&key, &server(), &CancellationToken::new())
                .await
                .unwrap(),
        );
        pool.spawn_idle_sweeper();
        pool.spawn_idle_sweeper(); // idempotent
        assert_eq!(pool.instance_count().await, 1);
        let mut gone = false;
        for _ in 0..60 {
            if pool.instance_count().await == 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            gone,
            "nothing asked the pool anything, yet the idle entry must go"
        );
    }

    #[tokio::test]
    async fn the_idle_sweeper_is_off_when_idle_shutdown_is_disabled() {
        let pool = Pool::new(Arc::new(LspConfig {
            idle_shutdown_secs: 0,
            ..LspConfig::default()
        }));
        pool.spawn_idle_sweeper();
        assert!(!pool.idle_sweeper_started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn the_idle_sweeper_spares_a_leased_entry_and_stops_with_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(Arc::new(LspConfig {
            idle_shutdown_secs: 1,
            ..LspConfig::default()
        }));
        let key = InstanceKey {
            server: "a".to_owned(),
            root: dir.path().to_owned(),
        };
        let held = pool
            .acquire(&key, &server(), &CancellationToken::new())
            .await
            .unwrap();
        pool.spawn_idle_sweeper();
        tokio::time::sleep(Duration::from_millis(2600)).await;
        assert_eq!(
            pool.instance_count().await,
            1,
            "a leased entry is never swept"
        );
        drop(held);
        let weak = Arc::downgrade(&pool);
        drop(pool);
        // The loop holds only a Weak: with the pool gone it must not keep it alive.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(weak.upgrade().is_none());
    }

    // ---- diagnostics cache eviction ------------------------

    fn store(cache: &DiagnosticsCache, uri: &str) {
        cache.store(
            PositionEncoding::Utf16,
            uri.to_owned(),
            Some(1),
            vec![Diagnostic::default()],
        );
    }

    /// Retiring an instance must take its documents' diagnostics with
    /// it. The cache is shared by every instance, so before the fix the
    /// entries outlived the instance and were never removed again — the daemon
    /// does not restart, so the memory only grew.
    #[tokio::test]
    async fn retiring_an_instance_drops_its_cached_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        let pool = Pool::new(Arc::new(LspConfig::default()));
        let key = InstanceKey {
            server: "fake".to_owned(),
            root: root.clone(),
        };
        let mine = uri_for_path(&root.join("a.rs"));
        // A *different* root entirely — a sibling pool would hold this, not
        // the pool retiring `root`.
        let elsewhere = tempfile::tempdir().unwrap();
        let theirs = uri_for_path(&elsewhere.path().join("b.rs"));
        store(&pool.diagnostics, &mine);
        store(&pool.diagnostics, &theirs);
        assert_eq!(pool.diagnostics.len(), 2);

        let entry = pool.new_entry(&key, &server(), dir.path());
        pool.entries.lock().await.insert(key.clone(), entry.clone());
        pool.finish_retire(&entry).await;

        assert_eq!(
            pool.diagnostics.len(),
            1,
            "the retired root's diagnostics must be gone"
        );
        assert!(pool.diagnostics.get(&mine).is_none());
        // A sibling directory that merely shares a name prefix must survive:
        // the match is on a trailing separator, not a string prefix.
        assert!(pool.diagnostics.get(&theirs).is_some());
    }

    /// The same guarantee for the memory-restart path, which is the other
    /// place an instance is torn down.
    #[tokio::test]
    async fn a_memory_restart_drops_the_instances_cached_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        let pool = Pool::new(Arc::new(LspConfig::default()));
        let key = InstanceKey {
            server: "fake".to_owned(),
            root: root.clone(),
        };
        let uri = uri_for_path(&root.join("a.rs"));
        store(&pool.diagnostics, &uri);
        let entry = pool.new_entry(&key, &server(), dir.path());
        // No child process is running, so `stop()` is a no-op and the only
        // observable effect is the cache cleanup.
        pool.enforce_memory_limit(&entry, u64::MAX).await;
        assert!(pool.diagnostics.get(&uri).is_none());
    }

    /// The cache has a hard ceiling, so a server that
    /// publishes for a huge number of files cannot grow it without bound even
    /// while every instance is alive.
    #[tokio::test]
    async fn the_diagnostics_cache_never_exceeds_its_ceiling() {
        let cache = DiagnosticsCache::default();
        for n in 0..(DIAGNOSTICS_CACHE_MAX_ENTRIES + 500) {
            store(&cache, &format!("file:///ws/file{n}.rs"));
        }
        assert_eq!(cache.len(), DIAGNOSTICS_CACHE_MAX_ENTRIES);
        // The newest survive, the oldest are the ones dropped.
        assert!(
            cache
                .get(&format!(
                    "file:///ws/file{}.rs",
                    DIAGNOSTICS_CACHE_MAX_ENTRIES + 499
                ))
                .is_some()
        );
        assert!(cache.get("file:///ws/file0.rs").is_none());
    }

    /// Re-publishing a document must not leave a duplicate key in the arrival
    /// order, or the deque would grow forever and evict wrongly.
    #[tokio::test]
    async fn republishing_a_document_does_not_duplicate_it_in_the_order() {
        let cache = DiagnosticsCache::default();
        let uri = "file:///ws/a.rs";
        for _ in 0..1000 {
            store(&cache, uri);
        }
        assert_eq!(cache.len(), 1);
        let order = cache.map.lock().unwrap().order.len();
        assert_eq!(order, 1, "the arrival order must hold the URI once");
    }

    /// A prefix match must respect directory boundaries: retiring `/ws/a` may
    /// not take `/ws/ab`'s documents with it.
    #[tokio::test]
    async fn forget_root_respects_directory_boundaries() {
        let cache = DiagnosticsCache::default();
        let inside = "file:///ws/a/one.rs";
        let sibling = "file:///ws/ab/two.rs";
        store(&cache, inside);
        store(&cache, sibling);
        let removed = cache.forget_root(Path::new("/ws/a"));
        assert_eq!(removed, 1);
        assert!(cache.get(inside).is_none());
        assert!(
            cache.get(sibling).is_some(),
            "`/ws/ab` must not be swallowed by a `/ws/a` prefix"
        );
    }

    // ---- bytes that a path swap smuggled in are refused ----

    /// `read_within` must refuse a file whose *handle* resolves outside the
    /// boundary, even though the name it was reached by looked fine.
    ///
    /// This is the TOCTOU the boundary check alone cannot close: `resolve_path`
    /// canonicalises the name, and the swap happens between that and the open.
    /// The test stands in for the swap by handing `read_within` a name that
    /// has *already* been replaced by a symlink pointing outside — which is
    /// exactly the state an attacker leaves the path in.
    #[tokio::test]
    async fn read_within_refuses_a_file_resolved_outside_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let boundary = dir.path().join("ws");
        std::fs::create_dir_all(&boundary).unwrap();
        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, b"top secret").unwrap();
        // `ws/link.txt` is inside the boundary by name but points outside.
        std::os::unix::fs::symlink(&outside, boundary.join("link.txt")).unwrap();

        let err = read_within(&boundary.join("link.txt"), &boundary, &[])
            .await
            .expect_err("bytes from outside the boundary must be refused");
        assert!(
            err.to_string().contains("outside"),
            "the refusal must say why: {err}"
        );
    }

    /// The TOCTOU this check exists to close, exercised with the swap actually
    /// happening *inside* the call.
    ///
    /// `read_within_refuses_a_file_resolved_outside_the_boundary` hands it a
    /// name that was replaced before it was called, which only proves the
    /// check works. It cannot tell a check of the object the bytes came from
    /// apart from a check of *some* object: opening the path twice — once to
    /// read, once to check — passes as long as the second open agrees, even
    /// when the first one did not.
    ///
    /// The swap is made deterministic with a FIFO, because reading one blocks
    /// until a writer appears. That gives the test a way to hold the reader
    /// inside its own read: a non-blocking write-open of the FIFO only
    /// succeeds once a reader is parked on it, so the sequence is ordered
    /// rather than raced — link at the outside FIFO, reader parked, link
    /// repointed inside, reader released. The descriptor it then reads is the
    /// one outside the boundary, and any check of a *later* open sees the
    /// in-boundary target.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn read_within_refuses_bytes_from_a_handle_it_never_checked() {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;

        /// Linux `O_NONBLOCK`; a write-open of a FIFO with it fails with ENXIO
        /// unless a reader already has the FIFO open.
        const O_NONBLOCK: i32 = 0o4000;

        let dir = tempfile::tempdir().unwrap();
        let boundary = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(boundary.join("ws")).unwrap();
        let ws = boundary.join("ws");
        let outside_dir = boundary.join("outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let fifo = outside_dir.join("secret.fifo");
        // `mkfifo` via libc would be a new dependency; the shell is already a
        // dependency of every test that spawns a fake language server.
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success(),
            "mkfifo is required to build the race fixture"
        );

        let link = ws.join("link.rs");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();
        let real = ws.join("real.rs");
        std::fs::write(&real, b"inside\n").unwrap();

        let reader = {
            let (link, ws) = (link.clone(), ws.clone());
            tokio::spawn(async move { read_within(&link, &ws, &[]).await })
        };

        // Wait until the reader is parked on the FIFO. Holding this handle is
        // also what releases the read below.
        let mut writer = None;
        for _ in 0..400 {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(O_NONBLOCK)
                .open(&fifo)
            {
                Ok(handle) => {
                    writer = Some(handle);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        }
        let mut writer = writer.expect("the reader never opened the FIFO");

        // The swap: the name now points inside the boundary.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // ...and the reader may now finish, with bytes from the FIFO.
        let _ = writer.write_all(b"top secret");
        drop(writer);

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
            .await
            .expect("the reader was not released")
            .expect("the reader did not finish");
        match outcome {
            Ok(bytes) => panic!(
                "bytes read through a descriptor outside the boundary were handed on: {:?}",
                String::from_utf8_lossy(&bytes)
            ),
            Err(err) => assert!(
                err.to_string().contains("outside"),
                "the refusal must say why: {err}"
            ),
        }
    }

    /// A file that really is inside is read normally: the check must not break
    /// the common case, including through an in-boundary symlink.
    #[tokio::test]
    async fn read_within_accepts_a_file_inside_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let boundary = dir.path().join("ws");
        std::fs::create_dir_all(&boundary).unwrap();
        let real = boundary.join("a.rs");
        std::fs::write(&real, b"fn main() {}").unwrap();
        std::os::unix::fs::symlink(&real, boundary.join("alias.rs")).unwrap();

        for name in ["a.rs", "alias.rs"] {
            let bytes = read_within(&boundary.join(name), &boundary, &[])
                .await
                .unwrap_or_else(|err| panic!("{name} must be readable: {err}"));
            assert_eq!(bytes, b"fn main() {}", "wrong bytes for {name}");
        }
    }

    /// An `allowed_roots` entry is as legitimate as the boundary itself.
    #[tokio::test]
    async fn read_within_accepts_a_file_under_an_allowed_root() {
        let dir = tempfile::tempdir().unwrap();
        let boundary = dir.path().join("ws");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&boundary).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        let file = shared.join("x.rs");
        std::fs::write(&file, b"shared").unwrap();

        assert!(
            read_within(&boundary.join("nope"), &boundary, &[])
                .await
                .is_err(),
            "sanity: the same path is refused without the allowed root"
        );
        let bytes = read_within(&file, &boundary, std::slice::from_ref(&shared))
            .await
            .expect("an allowed root is readable");
        assert_eq!(bytes, b"shared");
    }
}
