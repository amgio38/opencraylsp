//! One connection's view of the shared [`Pool`]: the production
//! [`LspBackend`].
//!
//! A [`BoundBackend`] carries what differs per client connection — the
//! workspace boundary and the set of enabled languages  — and
//! delegates everything stateful to the pool. Two connections asking for the
//! same `(server, root)` therefore share one language-server process, while a
//! language one connection did not enable is invisible to it.
//!
//! Diagnostics keep the settle/timeout wait semantics described in `docs/ARCHITECTURE.md`: the
//! cache is fed by every instance's `publishDiagnostics`, and a report only
//! counts as "received" when it diagnoses the document version just synced.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use async_trait::async_trait;
use opencraylsp_proto::{DaemonInfo, Indexing, LanguageMode, Limits, StatusReport};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::backend::{
    DiagnosticsReport, LanguageInfo, LspBackend, LspError, PositionEncoding, Served,
};
use crate::config::{LspConfig, ServerConfig};
use crate::languages::{self, LanguageSelection};
use crate::memory::MemorySampler;
use crate::pool::{
    CachedDiagnostics, InstanceKey, Pool, canonicalize_best_effort, command_exists, find_root,
    normalize_lexically,
};
use crate::warmup;

/// How often the diagnostics wait loop re-checks the cache between wakeups.
const DIAGNOSTICS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// The languages a connection may use, and how that was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnabledLanguages {
    pub mode: LanguageMode,
    pub set: BTreeSet<String>,
    /// Languages whose project markers exist in the workspace (reported per
    /// language in `languages()`; independent of what is enabled).
    pub detected: BTreeSet<String>,
}

/// See the module docs.
pub struct BoundBackend {
    pool: Arc<Pool>,
    /// The workspace boundary: canonicalized best-effort so `/tmp` symlinks
    /// cannot smuggle paths out. A server's own `workspace` override never
    /// applies here.
    boundary: PathBuf,
    enabled: EnabledLanguages,
    /// Whether `shutdown` stops the pool (a standalone backend) or leaves it
    /// running for the other connections (a backend bound to a shared pool).
    owns_pool: bool,
}

impl std::fmt::Debug for BoundBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundBackend")
            .field("boundary", &self.boundary)
            .field("languages", &self.enabled.set)
            .finish()
    }
}

impl Pool {
    /// Binds a connection to this pool.
    ///
    /// `selection` decides the enabled languages: `Auto` uses the project
    /// markers found under `boundary`, `All` every language a configured
    /// server provides, `Explicit` exactly the named ones. Enabling a language
    /// whose server is not installed is allowed: the request then fails with
    /// `server_not_installed` and an install hint, which is more useful than
    /// pretending the language does not exist.
    pub fn bind(
        self: &Arc<Self>,
        boundary: impl Into<PathBuf>,
        selection: LanguageSelection,
    ) -> Arc<BoundBackend> {
        self.bind_inner(boundary.into(), selection, false)
    }

    fn bind_inner(
        self: &Arc<Self>,
        boundary: PathBuf,
        selection: LanguageSelection,
        owns_pool: bool,
    ) -> Arc<BoundBackend> {
        let boundary = canonicalize_best_effort(&boundary);
        let servers = &self.config.servers;
        let detected = languages::detect(&boundary, servers);
        let (mode, set) = match selection {
            LanguageSelection::Auto => (LanguageMode::Auto, detected.clone()),
            LanguageSelection::All => (LanguageMode::All, languages::known_languages(servers)),
            LanguageSelection::Explicit(set) => (LanguageMode::Declared, set),
        };
        // Entries created for this connection read within its boundary.
        self.note_boundary(&boundary);
        self.connections.fetch_add(1, Ordering::Relaxed);
        Arc::new(BoundBackend {
            pool: self.clone(),
            boundary,
            enabled: EnabledLanguages {
                mode,
                set,
                detected,
            },
            owns_pool,
        })
    }
}

impl Drop for BoundBackend {
    fn drop(&mut self) {
        self.pool.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

impl BoundBackend {
    /// A backend with its own private pool, every language enabled, and the
    /// process working directory as boundary. Used by embedded mode and tests;
    /// `shutdown` stops that pool.
    pub fn standalone(config: Arc<LspConfig>) -> Arc<Self> {
        let boundary = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Self::standalone_in(config, boundary)
    }

    /// Like [`Self::standalone`] with an explicit boundary.
    pub fn standalone_in(config: Arc<LspConfig>, boundary: impl Into<PathBuf>) -> Arc<Self> {
        Pool::new(config).bind_inner(boundary.into(), LanguageSelection::All, true)
    }

    /// The pool this connection is bound to.
    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// The languages this connection may use.
    pub fn enabled(&self) -> &EnabledLanguages {
        &self.enabled
    }

    /// Test hook: a backend rooted at `boundary` with no servers.
    #[cfg(test)]
    pub(crate) fn for_tests(boundary: PathBuf) -> Arc<Self> {
        Self::standalone_in(Arc::new(LspConfig::default()), boundary)
    }

    /// The boundary that applies to one server: its `workspace` override, or
    /// the connection's boundary.
    fn server_boundary(&self, server: &ServerConfig) -> PathBuf {
        server
            .workspace
            .as_ref()
            .map(|p| canonicalize_best_effort(p))
            .unwrap_or_else(|| self.boundary.clone())
    }

    /// Whether `path` sits inside the boundary or an allowed root.
    fn inside_allowed(&self, path: &Path) -> bool {
        path.starts_with(&self.boundary)
            || self
                .pool
                .allowed_roots
                .iter()
                .any(|root| path.starts_with(root))
    }
}
impl BoundBackend {
    /// Which project root a request with no file to anchor to belongs to.
    ///
    /// A file-anchored request walks up from the file ([`find_root`]). This one
    /// has no file, so the root has to be decided another way, and the boundary
    /// is only an honest answer when the boundary *is* a project. A workspace
    /// holding several projects one directory down is the ordinary case — a
    /// monorepo, or a directory of side-by-side checkouts — and starting a
    /// server at the boundary there indexes nothing while still costing
    /// gigabytes, so it is not a fallback that can be justified by "it always
    /// worked before".
    ///
    /// The order is: the boundary if it is a project (or the server needs no
    /// marker at all), else the server's most recently used instance, else the
    /// single project under the boundary, else an error. The middle step is what
    /// makes the common case cheap: a second question in the same session
    /// reuses the server that is already indexing it. The last step refuses
    /// rather than guesses, because among several projects no answer is
    /// better than the wrong one — the model is told the candidates and asked
    /// for a `path`.
    async fn workspace_root(
        &self,
        server: &str,
        config: &ServerConfig,
        language: &str,
        boundary: &Path,
    ) -> Result<PathBuf, LspError> {
        // No markers at all: the server works file by file, so there is no
        // project to find and the boundary is the only root there can be.
        if config.root_markers.is_empty() {
            return Ok(boundary.to_owned());
        }
        if config
            .root_markers
            .iter()
            .any(|m| boundary.join(m).exists())
        {
            return Ok(boundary.to_owned());
        }
        if let Some(root) = self.pool.most_recent_root_of(server).await {
            return Ok(root);
        }
        let mut projects: Vec<(PathBuf, PathBuf)> =
            warmup::discover(boundary, config, &self.pool.config);
        projects.sort_by(|a, b| a.0.cmp(&b.0));
        match projects.len() {
            1 => Ok(projects.remove(0).0),
            0 => Err(LspError::NoProject {
                language: language.to_owned(),
                boundary: boundary.display().to_string(),
                candidates: String::new(),
            }),
            _ => {
                let mut candidates = String::from("\nthese projects were found (pass `path`):");
                // A sample *file*, never the project directory: the server is
                // chosen by file extension, so a directory has none and the
                // printed call would fail with "no LSP server is configured for
                // . files" — worse than no hint at all.
                for (_, sample) in &projects {
                    let relative = sample.strip_prefix(boundary).unwrap_or(sample);
                    candidates.push_str(&format!(
                        "\n  {{\"path\": {:?}, \"line\": 1, \"column\": 1}}",
                        relative.display().to_string()
                    ));
                }
                Err(LspError::NoProject {
                    language: language.to_owned(),
                    boundary: boundary.display().to_string(),
                    candidates,
                })
            }
        }
    }
}

impl BoundBackend {
    fn enabled_names(&self) -> Vec<String> {
        self.enabled.set.iter().cloned().collect()
    }

    fn check_enabled(&self, language: &str) -> Result<(), LspError> {
        if self.enabled.set.contains(language) {
            Ok(())
        } else {
            Err(LspError::LanguageDisabled {
                language: language.to_owned(),
                enabled: self.enabled_names(),
            })
        }
    }

    /// Routes `file` to its server, instance key and language, refusing
    /// languages this connection did not enable.
    fn route(&self, file: &Path) -> Result<(InstanceKey, ServerConfig), LspError> {
        let extension = file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        let (name, server, _) = self
            .pool
            .config
            .server_for_extension(&extension)
            .ok_or(LspError::NoServerConfigured { extension })?;
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let language = languages::language_for(server, ext).unwrap_or_default();
        self.check_enabled(&language)?;
        let boundary = self.server_boundary(server);
        let root = find_root(&boundary, &server.root_markers, file);
        Ok((
            InstanceKey {
                server: name.to_owned(),
                root,
            },
            server.clone(),
        ))
    }

    /// Starts every discovered (server, project root) of an *enabled* language
    /// and opens one file in each, so the servers begin indexing now rather
    /// than on the first question. Failures are logged and skipped: one
    /// missing server must not keep the others cold.
    pub async fn warm_up(&self) {
        let config = &self.pool.config;
        let cancel = CancellationToken::new();
        let mut started = 0usize;
        let mut skipped = 0usize;
        let per_server: Vec<Vec<(PathBuf, PathBuf)>> = config
            .servers
            .values()
            .filter(|server| {
                languages::server_languages(server)
                    .iter()
                    .any(|l| self.enabled.set.contains(l))
            })
            .map(|server| warmup::discover(&self.server_boundary(server), server, config))
            .collect();
        // Round-robin across servers: a workspace with many Go modules must
        // not spend the whole budget before any other language gets a server.
        for (root, sample) in warmup::interleave(per_server) {
            if started >= config.warmup_max_instances {
                skipped += 1;
                continue;
            }
            let routed = self.route(&sample);
            let lease = match routed {
                Ok((key, server)) => self.pool.prepare(&key, &server, &cancel).await,
                Err(err) => Err(err),
            };
            match lease {
                Ok(lease) => {
                    started += 1;
                    let server = lease.entry.key.server.clone();
                    match self
                        .pool
                        .sync_for_request(&lease.entry, &sample, &cancel)
                        .await
                    {
                        Ok(_) => {
                            tracing::info!(%server, root = %root.display(), "lsp warm-up: server started, indexing");
                        }
                        Err(err) => {
                            tracing::warn!(%server, root = %root.display(), error = %err, "lsp warm-up: opening sample file failed");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(root = %root.display(), error = %err, "lsp warm-up: server did not start");
                }
            }
        }
        if skipped > 0 {
            tracing::warn!(
                started,
                skipped,
                "lsp warm-up: warmup_max_instances reached; remaining roots stay lazy"
            );
        }
    }

    /// Starts the background warm-up (when configured) and the pool's file
    /// watcher. Holds only weak references, so dropping the backend ends the
    /// work. Without a tokio runtime nothing is spawned and servers stay lazy.
    pub fn spawn_background(self: &Arc<Self>) {
        if self.pool.config.warmup {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let weak = Arc::downgrade(self);
                    handle.spawn(async move {
                        if let Some(backend) = weak.upgrade() {
                            backend.warm_up().await;
                        }
                    });
                }
                Err(_) => tracing::info!("lsp: no async runtime; warm-up not started"),
            }
        }
        self.pool.spawn_maintenance();
    }
}

#[async_trait]
impl LspBackend for BoundBackend {
    async fn request(
        &self,
        file: &Path,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Served, LspError> {
        if cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        let (key, server) = self.route(file)?;
        let lease = self.pool.prepare(&key, &server, cancel).await?;
        self.pool
            .sync_for_request(&lease.entry, file, cancel)
            .await?;
        let (value, encoding) = lease.entry.instance.request(method, params, cancel).await?;
        let indexing = settle(&key.server, &value, lease.entry.instance.indexing())?;
        Ok(Served {
            value,
            encoding,
            server: key.server,
            root: key.root,
            indexing,
        })
    }

    async fn request_workspace(
        &self,
        server: &str,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Served, LspError> {
        if cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        let config =
            self.pool
                .config
                .servers
                .get(server)
                .ok_or_else(|| LspError::UnknownServer {
                    server: server.to_owned(),
                })?;
        let provided = languages::server_languages(config);
        if !provided.iter().any(|l| self.enabled.set.contains(l)) {
            let language = provided
                .iter()
                .next()
                .cloned()
                .unwrap_or_else(|| server.to_owned());
            return Err(LspError::LanguageDisabled {
                language,
                enabled: self.enabled_names(),
            });
        }
        let boundary = self.server_boundary(config);
        let language = provided
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| server.to_owned());
        let root = self
            .workspace_root(server, config, &language, &boundary)
            .await?;
        let key = InstanceKey {
            server: server.to_owned(),
            root,
        };
        let lease = self.pool.prepare(&key, config, cancel).await?;
        // Some servers cannot answer `workspace/symbol` until a project is
        // loaded, and only opening a file loads one (tsserver answers "No
        // Project" otherwise). Open one source file of the server's language
        // when nothing is open yet.
        let nothing_open = lease.entry.docs.lock().await.len() == 0;
        if nothing_open
            && let Some(sample) =
                crate::warmup::sample_file(&key.root, config, &self.pool.config.warmup_exclude)
            && let Err(err) = self
                .pool
                .sync_for_request(&lease.entry, &sample, cancel)
                .await
        {
            tracing::debug!(server = %server, error = %err, "could not open a probe file");
        }
        let (value, encoding) = lease.entry.instance.request(method, params, cancel).await?;
        let indexing = settle(&key.server, &value, lease.entry.instance.indexing())?;
        Ok(Served {
            value,
            encoding,
            server: key.server,
            root: key.root,
            indexing,
        })
    }

    async fn diagnostics(
        &self,
        file: &Path,
        cancel: &CancellationToken,
    ) -> Result<DiagnosticsReport, LspError> {
        if cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        let (key, server) = self.route(file)?;
        let lease = self.pool.prepare(&key, &server, cancel).await?;
        let (uri, version) = self
            .pool
            .sync_for_request(&lease.entry, file, cancel)
            .await?;
        let sync_moment = Instant::now();
        // Trigger analysis: rust-analyzer runs its check pass on save.
        lease
            .entry
            .instance
            .notify(
                "textDocument/didSave",
                serde_json::json!({"textDocument": {"uri": uri}}),
                cancel,
            )
            .await?;
        let server_name = key.server;
        let report = self
            .wait_for_diagnostics(&server_name, &uri, version, sync_moment, cancel)
            .await?;
        // Nothing to report while the server is still indexing is not "clean":
        // the semantic pass has not run yet.
        if report.items.is_empty()
            && let Some(ix) = lease.entry.instance.indexing()
        {
            return Err(indexing_error(&server_name, ix));
        }
        Ok(report)
    }

    fn resolve_path(&self, file_path: &str) -> Result<PathBuf, LspError> {
        let joined = if Path::new(file_path).is_absolute() {
            PathBuf::from(file_path)
        } else {
            self.boundary.join(file_path)
        };
        let lexical = normalize_lexically(&joined);
        let outside = || LspError::OutsideWorkspace {
            path: file_path.to_owned(),
            boundary: self.boundary.display().to_string(),
        };
        if !self.inside_allowed(&lexical) {
            return Err(outside());
        }
        // `didOpen` ships file bytes to a child process, which is a read like
        // any other: resolve symlinks when the file exists so a link pointing
        // outside the boundary cannot smuggle bytes out. Positions in results
        // that point outside the boundary are still only reported as
        // coordinates, never read — that half is the tool layer's contract.
        let canonical = canonicalize_best_effort(&lexical);
        if !self.inside_allowed(&canonical) {
            return Err(outside());
        }
        Ok(canonical)
    }

    fn boundary(&self) -> PathBuf {
        self.boundary.clone()
    }

    fn languages(&self) -> Vec<LanguageInfo> {
        let mut out = Vec::new();
        for (name, server) in &self.pool.config.servers {
            let installed = command_exists(&server.command);
            for language in languages::server_languages(server) {
                if !self.enabled.set.contains(&language) {
                    continue;
                }
                out.push(LanguageInfo {
                    extensions: server
                        .extensions
                        .keys()
                        .filter(|ext| {
                            languages::language_for(server, ext).as_deref() == Some(&language)
                        })
                        .cloned()
                        .collect(),
                    detected: self.enabled.detected.contains(&language),
                    name: language,
                    server: name.clone(),
                    root_markers: server.root_markers.clone(),
                    installed,
                    enabled: true,
                });
            }
        }
        out.sort_by(|a, b| (&a.name, &a.server).cmp(&(&b.name, &b.server)));
        out
    }

    async fn status(&self) -> StatusReport {
        let config = &self.pool.config;
        let mut not_installed: Vec<String> = self
            .enabled
            .set
            .iter()
            .filter(|language| {
                !config.servers.values().any(|server| {
                    languages::server_languages(server).contains(*language)
                        && command_exists(&server.command)
                })
            })
            .cloned()
            .collect();
        not_installed.sort();
        StatusReport {
            daemon: DaemonInfo {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                pid: std::process::id(),
                uptime_secs: self.pool.uptime_secs(),
                rss_bytes: self_measured_rss().await,
                clients: self.pool.connections.load(Ordering::Relaxed),
                // The daemon's own ceiling, as opposed to `limits.max_rss_mb`
                // below, which governs the language servers.
                max_rss_mb: Some(config.daemon_max_rss_mb),
                rss_over_limit: self.pool.daemon_rss_over_limit(),
            },
            limits: Limits {
                max_instances: config.max_instances as u32,
                max_rss_mb: config.max_rss_mb,
                idle_shutdown_secs: config.idle_shutdown_secs,
                max_open_docs: config.max_open_docs as u32,
            },
            enabled_languages: self.enabled_names(),
            language_mode: self.enabled.mode,
            not_installed,
            instances: self.pool.instance_infos().await,
        }
    }

    async fn shutdown(&self) {
        if self.owns_pool {
            self.pool.shutdown().await;
        }
    }
}

impl BoundBackend {
    /// Waits for the diagnostics of `version` of `uri` with the settle/timeout
    /// semantics described in `docs/ARCHITECTURE.md`.
    async fn wait_for_diagnostics(
        &self,
        server_name: &str,
        uri: &str,
        version: i32,
        sync_moment: Instant,
        cancel: &CancellationToken,
    ) -> Result<DiagnosticsReport, LspError> {
        let config = &self.pool.config;
        let cache = &self.pool.diagnostics;
        let settle = std::time::Duration::from_millis(config.diagnostics_settle_ms);
        let deadline =
            Instant::now() + std::time::Duration::from_millis(config.diagnostics_timeout_ms);
        let server_name = server_name.to_owned();
        let uri = uri.to_owned();
        // A matching publish that arrived between sync and `didSave` already
        // counts — the quiet-period clock starts from its arrival.
        let mut quiet_since = cache
            .get(&uri)
            .filter(|cached| diagnoses_version(cached, version, sync_moment))
            .map(|cached| cached.arrived);
        loop {
            if cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            if let Some(cached) = cache.get(&uri)
                && diagnoses_version(&cached, version, sync_moment)
            {
                match quiet_since {
                    Some(since) if since.elapsed() >= settle => {
                        return Ok(DiagnosticsReport {
                            items: cached.items,
                            encoding: cached.encoding,
                            received_for_version: true,
                            timed_out: false,
                            server: server_name,
                        });
                    }
                    Some(_) => {}
                    None => quiet_since = Some(cached.arrived),
                }
            }
            if Instant::now() >= deadline {
                let cached = cache.get(&uri);
                let received = cached
                    .as_ref()
                    .is_some_and(|c| diagnoses_version(c, version, sync_moment));
                return Ok(DiagnosticsReport {
                    encoding: cached
                        .as_ref()
                        .map(|c| c.encoding)
                        .unwrap_or(PositionEncoding::Utf16),
                    items: cached.map(|c| c.items).unwrap_or_default(),
                    received_for_version: received,
                    timed_out: true,
                    server: server_name,
                });
            }
            tokio::select! {
                () = cache.notify.notified() => {}
                () = tokio::time::sleep(DIAGNOSTICS_POLL_INTERVAL) => {}
                () = cancel.cancelled() => return Err(LspError::Cancelled),
            }
        }
    }
}

/// This process's resident memory, or `None` where it cannot be read.
///
/// The daemon alone, never its language servers: `DaemonInfo::rss_bytes` is
/// reported next to `max_rss_mb` (the daemon's own ceiling), and the servers
/// are reported on their own rows against `max_rss_mb`. Reading the whole tree
/// would charge the same memory twice and make a healthy daemon look like a
/// runaway one as soon as rust-analyzer indexes a large crate.
///
/// `ProcSampler` reads `/proc`, so it answers `None` on a platform without one
/// (macOS, Windows) rather than guessing, which is what `rss_bytes: Option<u64>`
/// is for. The `/proc` read is blocking file I/O, so it runs off the async
/// workers: `status` is called on a client's request and must not park one.
async fn self_measured_rss() -> Option<u64> {
    tokio::task::spawn_blocking(|| {
        crate::memory::ProcSampler::default().self_rss_bytes(std::process::id())
    })
    .await
    .ok()
    .flatten()
}

/// Whether an LSP result carries no data: `null` or an empty list.
fn is_empty_result(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

fn indexing_error(server: &str, indexing: Indexing) -> LspError {
    LspError::Indexing {
        server: server.to_owned(),
        message: indexing.message,
        percent: indexing.percent,
    }
}

/// Decides what a finished request may report about indexing :
/// an empty answer from a server that is still working is not an answer at
/// all, so it becomes [`LspError::Indexing`]; a non-empty answer is returned
/// with the indexing marker attached.
fn settle(
    server: &str,
    value: &Value,
    indexing: Option<Indexing>,
) -> Result<Option<Indexing>, LspError> {
    match indexing {
        Some(indexing) if is_empty_result(value) => Err(indexing_error(server, indexing)),
        other => Ok(other),
    }
}

/// Whether `cached` diagnoses `version`: an exact version match, or — for
/// servers that never send versions — anything that arrived after we synced.
/// Without this fallback an old server would make every call time out and
/// report "unknown" forever.
fn diagnoses_version(cached: &CachedDiagnostics, version: i32, sync_moment: Instant) -> bool {
    match cached.version {
        Some(v) => v == version,
        None => cached.arrived >= sync_moment,
    }
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

    fn config(src: &str) -> Arc<LspConfig> {
        Arc::new(LspConfig::from_toml_str_without_presets(src).unwrap())
    }

    const TWO_SERVERS: &str = r#"
        [[server]]
        name = "ra"
        command = "definitely-not-installed-ra"
        extensions = { rs = "rust" }
        root_markers = ["Cargo.toml"]
        [[server]]
        name = "gp"
        command = "definitely-not-installed-gp"
        extensions = { go = "go" }
        root_markers = ["go.mod"]
    "#;

    #[test]
    fn resolve_path_joins_relative_and_refuses_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BoundBackend::for_tests(dir.path().to_owned());
        let boundary = manager.boundary();
        assert_eq!(
            manager.resolve_path("src/a.rs").unwrap(),
            boundary.join("src/a.rs")
        );
        assert!(matches!(
            manager.resolve_path("../../etc/passwd"),
            Err(LspError::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            manager.resolve_path("/etc/passwd"),
            Err(LspError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn resolve_path_defeats_symlink_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        write_file(&outside, "secret");
        let link = dir.path().join("ws").join("link.txt");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let manager = BoundBackend::for_tests(dir.path().join("ws"));
        assert!(matches!(
            manager.resolve_path("link.txt"),
            Err(LspError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn resolve_path_tolerates_dot_components() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BoundBackend::for_tests(dir.path().to_owned());
        let boundary = manager.boundary();
        assert_eq!(
            manager.resolve_path("src/./a.rs").unwrap(),
            boundary.join("src/a.rs")
        );
    }

    #[test]
    fn resolve_path_allows_configured_allowed_roots() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let src = format!("allowed_roots = [{:?}]", shared.display().to_string());
        let backend = BoundBackend::standalone_in(config(&src), dir.path().join("ws"));
        assert!(
            backend
                .resolve_path(shared.join("x.rs").to_str().unwrap())
                .is_ok()
        );
    }

    #[test]
    fn diagnoses_version_matches_exact_or_post_sync_arrival() {
        let now = Instant::now();
        let exact = CachedDiagnostics {
            items: Vec::new(),
            version: Some(3),
            encoding: PositionEncoding::Utf16,
            arrived: now,
        };
        assert!(diagnoses_version(&exact, 3, now));
        assert!(!diagnoses_version(&exact, 4, now));
        let unversioned_new = CachedDiagnostics {
            version: None,
            arrived: now,
            ..exact.clone()
        };
        assert!(diagnoses_version(&unversioned_new, 7, now));
        let unversioned_old = CachedDiagnostics {
            version: None,
            arrived: now - std::time::Duration::from_secs(60),
            ..exact
        };
        assert!(!diagnoses_version(&unversioned_old, 7, now));
    }

    #[tokio::test]
    async fn unknown_extension_is_an_honest_error() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BoundBackend::for_tests(dir.path().to_owned());
        let err = manager
            .request(
                Path::new("/tmp/x.zzz9"),
                "textDocument/hover",
                Value::Null,
                &CancellationToken::new(),
            )
            .await
            .expect_err("no server handles .zzz9");
        assert!(matches!(err, LspError::NoServerConfigured { .. }));
        assert!(err.to_string().contains("zzz9"));
    }

    #[test]
    fn debug_names_boundary_and_languages() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BoundBackend::standalone_in(config(TWO_SERVERS), dir.path());
        let text = format!("{manager:?}");
        assert!(
            text.contains("BoundBackend") && text.contains("rust"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn shutdown_with_no_servers_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let manager = BoundBackend::for_tests(dir.path().to_owned());
        manager.shutdown().await;
        manager.shutdown().await;
    }

    // ---- language selection  --------------------------------

    fn names(infos: &[LanguageInfo]) -> Vec<&str> {
        infos.iter().map(|l| l.name.as_str()).collect()
    }

    #[test]
    fn all_enables_every_configured_language() {
        let dir = tempfile::tempdir().unwrap();
        let backend = BoundBackend::standalone_in(config(TWO_SERVERS), dir.path());
        assert_eq!(names(&backend.languages()), vec!["go", "rust"]);
        assert_eq!(backend.enabled().mode, LanguageMode::All);
    }

    #[test]
    fn auto_enables_only_what_the_workspace_shows() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("Cargo.toml"), "[package]");
        let pool = Pool::new(config(TWO_SERVERS));
        let backend = pool.bind(dir.path(), LanguageSelection::Auto);
        assert_eq!(backend.enabled().mode, LanguageMode::Auto);
        let langs = backend.languages();
        assert_eq!(names(&langs), vec!["rust"]);
        assert!(langs[0].detected && langs[0].enabled && !langs[0].installed);
        assert_eq!(langs[0].extensions, vec!["rs"]);
        assert_eq!(langs[0].root_markers, vec!["Cargo.toml"]);
    }

    #[test]
    fn auto_in_an_empty_workspace_enables_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Pool::new(config(TWO_SERVERS)).bind(dir.path(), LanguageSelection::Auto);
        assert!(backend.languages().is_empty());
        assert!(backend.enabled().set.is_empty());
    }

    #[test]
    fn explicit_enables_exactly_the_named_languages() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("Cargo.toml"), "");
        let selection = LanguageSelection::Explicit(["go".to_owned()].into());
        let backend = Pool::new(config(TWO_SERVERS)).bind(dir.path(), selection);
        assert_eq!(backend.enabled().mode, LanguageMode::Declared);
        assert_eq!(names(&backend.languages()), vec!["go"]);
        // `rust` is detected but not enabled, and stays invisible.
        assert!(backend.enabled().detected.contains("rust"));
    }

    #[tokio::test]
    async fn a_disabled_language_is_refused_before_anything_starts() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.go");
        write_file(&file, "package a");
        let selection = LanguageSelection::Explicit(["rust".to_owned()].into());
        let pool = Pool::new(config(TWO_SERVERS));
        let backend = pool.bind(dir.path(), selection);
        let err = backend
            .request(
                &file,
                "textDocument/hover",
                Value::Null,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            LspError::LanguageDisabled {
                language: "go".into(),
                enabled: vec!["rust".into()],
            }
        );
        let err = backend
            .diagnostics(&file, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::LanguageDisabled { .. }));
        let err = backend
            .request_workspace(
                "gp",
                "workspace/symbol",
                Value::Null,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::LanguageDisabled { ref language, .. } if language == "go"));
        assert_eq!(pool.instance_count().await, 0, "nothing was started");
    }

    #[tokio::test]
    async fn an_enabled_language_with_a_missing_server_says_how_to_fix_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        write_file(&file, "fn a() {}");
        let backend = BoundBackend::standalone_in(config(TWO_SERVERS), dir.path());
        let err = backend
            .request(
                &file,
                "textDocument/hover",
                Value::Null,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, LspError::ServerNotInstalled { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn unknown_server_in_a_workspace_request_is_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let backend = BoundBackend::standalone_in(config(TWO_SERVERS), dir.path());
        let err = backend
            .request_workspace(
                "nope",
                "workspace/symbol",
                Value::Null,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::UnknownServer { .. }));
    }

    #[tokio::test]
    async fn status_reports_languages_limits_and_missing_servers() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("go.mod"), "module x");
        let pool = Pool::new(config(TWO_SERVERS));
        let backend = pool.bind(dir.path(), LanguageSelection::Auto);
        let status = backend.status().await;
        assert_eq!(status.enabled_languages, vec!["go"]);
        assert_eq!(status.language_mode, LanguageMode::Auto);
        assert_eq!(status.not_installed, vec!["go"]);
        assert_eq!(status.limits.max_instances, 8);
        assert_eq!(status.limits.idle_shutdown_secs, 900);
        assert_eq!(status.daemon.clients, 1);
        assert!(status.instances.is_empty());
    }

    /// The daemon's own resident memory, or `None` where the platform cannot
    /// tell us.
    ///
    /// The per-instance figures are what `max_rss_mb` governs, but the daemon
    /// itself holds every connection's state and every cached diagnostic, and
    /// that was previously reported as `n/a` on every platform that could have
    /// answered — a field that never carried information. Sampling it needs no
    /// new dependency: the same `/proc` reader the guard uses, pointed at this
    /// process.
    #[tokio::test]
    async fn status_reports_the_daemons_own_memory_where_the_platform_can() {
        if !crate::memory::proc_available() {
            // No `/proc`: `None` is the contract, not a failure.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(config(TWO_SERVERS));
        let backend = pool.bind(dir.path(), LanguageSelection::Auto);
        let status = backend.status().await;
        let rss = status
            .daemon
            .rss_bytes
            .expect("a process can always measure itself where /proc exists");
        assert!(
            rss > 1024 * 1024,
            "a test process holds more than 1 MiB, got {rss}"
        );
        // And it is *this* process, not this process plus its children: the
        // figure must match `/proc/self` alone. The two servers the pool binds
        // here run inside the test binary's own process tree, so a tree reading
        // would come back larger and the daemon's own ceiling would be judged
        // against the servers' memory.
        let own = std::fs::read_to_string("/proc/self/status").unwrap();
        let expected =
            crate::memory::parse_vm_rss_bytes(&own).expect("this process can measure itself");
        assert!(
            rss.abs_diff(expected) < 64 * 1024 * 1024,
            "daemon rss {rss} must be about this process alone ({expected}), not its children"
        );
    }

    #[tokio::test]
    async fn the_connection_count_tracks_bound_backends() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(config(TWO_SERVERS));
        let a = pool.bind(dir.path(), LanguageSelection::All);
        let b = pool.bind(dir.path(), LanguageSelection::All);
        assert_eq!(a.status().await.daemon.clients, 2);
        drop(b);
        assert_eq!(a.status().await.daemon.clients, 1);
    }

    #[tokio::test]
    async fn a_bound_backend_leaves_the_shared_pool_running_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::new(config(TWO_SERVERS));
        let a = pool.bind(dir.path(), LanguageSelection::All);
        a.shutdown().await;
        assert!(!a.owns_pool);
        // A standalone backend does own (and stop) its private pool.
        let owner = BoundBackend::standalone_in(config(TWO_SERVERS), dir.path());
        assert!(owner.owns_pool);
        owner.shutdown().await;
    }

    #[tokio::test]
    async fn request_honors_a_pre_cancelled_token() {
        let dir = tempfile::tempdir().unwrap();
        let backend = BoundBackend::for_tests(dir.path().to_owned());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let file = dir.path().join("a.rs");
        assert_eq!(
            backend
                .request(&file, "m", Value::Null, &cancel)
                .await
                .unwrap_err(),
            LspError::Cancelled
        );
        assert_eq!(
            backend
                .request_workspace("s", "m", Value::Null, &cancel)
                .await
                .unwrap_err(),
            LspError::Cancelled
        );
        assert_eq!(
            backend.diagnostics(&file, &cancel).await.unwrap_err(),
            LspError::Cancelled
        );
    }
}
