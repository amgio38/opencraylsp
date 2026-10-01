//! One language server's lifecycle: state machine, restart cap, `-32801`
//! retries, timeouts, cancellation, and orderly shutdown.
//!
//! State machine: `Stopped -> Starting -> Running`, any state `-> Error` on
//! failure, `Error -> Starting` on the next use. A crash must never leave the
//! instance `Running`: that is the zombie state the reference implementation's
//! comments call out, where a dead server is never restarted because everyone
//! believes it is alive. Here the flip happens two ways — the reaper fails an
//! in-flight request with `Disconnected`, and `ensure_running` probes liveness
//! before handing out the client — so the crash is observed on the next call
//! at the latest, with or without traffic in flight.
//!
//! Restart accounting: every failed start (and every observed mid-flight
//! crash) bumps `restarts`; a successful `initialize` resets it to zero. Past
//! `max_restarts` the instance refuses with `ServerFailed` and never spawns
//! again in this process — without the cap, each query after a crash would
//! fork a fresh child. A missing executable is deterministic and
//! is *not* counted: it returns `ServerNotInstalled` with an install hint on
//! every call instead. (A command this user may not safely execute is refused
//! before the spawn and reported the same way each time; it costs one failed
//! spawn per attempt, which is what a caller retrying would expect.)

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::Instant;

use lsp_types::Diagnostic;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::backend::{LspError, PositionEncoding};
use crate::client::{ClientError, ERROR_CONTENT_MODIFIED, LspClient};
use crate::config::ServerConfig;
use crate::progress::ProgressTracker;
use opencraylsp_proto::Indexing;

/// Delays between `-32801 ContentModified` retries: the server
/// is still indexing, which is transient and expected, not a failure.
const RETRY_DELAYS_MS: [u64; 3] = [500, 1000, 2000];

/// Upper bound on `-32801` attempts: the initial try plus one per delay.
const MAX_ATTEMPTS: usize = RETRY_DELAYS_MS.len() + 1;

/// Per-instance limits, copied out of `LspConfig` so this module never reaches
/// back into global configuration.
#[derive(Debug, Clone, Copy)]
pub struct InstanceLimits {
    pub startup_timeout_ms: u64,
    pub startup_grace_ms: u64,
    pub request_timeout_ms: u64,
    pub max_restarts: u32,
    /// Upper bound on one write to the server's stdin. Without it a server
    /// that stops reading parks every writer forever — including the pool's
    /// file watcher, whose cancellation token is never fired.
    pub write_timeout_ms: u64,
}

/// One cached `textDocument/publishDiagnostics` event, forwarded to the
/// manager. The callback receives the raw URI string, the document version if
/// the server sent one, and the diagnostics; routing and cache policy belong
/// to the manager, not here.
pub type DiagnosticsSink = Arc<dyn Fn(String, Option<i32>, Vec<Diagnostic>) + Send + Sync>;

/// Observable lifecycle state. Cloned out for tests and the manager's idle
/// sweep — never held across an `.await`.
#[derive(Debug, Clone, PartialEq)]
pub enum InstanceState {
    Stopped,
    Starting,
    Running { encoding: PositionEncoding },
    Error { message: String },
}

/// One `(server name, root)` language-server instance: the production unit the
/// manager routes files to.
pub struct LspServerInstance {
    name: String,
    server: ServerConfig,
    root: PathBuf,
    limits: InstanceLimits,
    diag_sink: DiagnosticsSink,
    /// Mirrors the negotiated encoding for the manager's diagnostics cache.
    /// Shared (not copied) because a restart renegotiates: the sink reads the
    /// current value at each arrival instead of the value at creation time.
    encoding_cell: Arc<std::sync::Mutex<PositionEncoding>>,
    state: tokio::sync::Mutex<InstanceState>,
    /// Serializes `start()`: concurrent first queries for the same instance
    /// must share one spawn, not fork one child each.
    start_lock: tokio::sync::Mutex<()>,
    client: tokio::sync::Mutex<Option<Arc<LspClient>>>,
    restarts: AtomicU32,
    /// The most recent start/crash failure, kept apart from `state` because
    /// `start_inner` moves the state to `Starting` before it can be asked
    /// "what went wrong last time?".
    last_error: std::sync::Mutex<Option<String>>,
    last_use: std::sync::Mutex<Instant>,
    progress: Arc<ProgressTracker>,
}

impl LspServerInstance {
    /// Builds a stopped instance. Spawns nothing — the first request starts
    /// the server lazily.
    pub fn new(
        name: String,
        server: ServerConfig,
        root: PathBuf,
        limits: InstanceLimits,
        encoding_cell: Arc<std::sync::Mutex<PositionEncoding>>,
        diag_sink: DiagnosticsSink,
    ) -> Self {
        Self {
            name,
            server,
            root,
            limits,
            encoding_cell,
            diag_sink,
            state: tokio::sync::Mutex::new(InstanceState::Stopped),
            start_lock: tokio::sync::Mutex::new(()),
            client: tokio::sync::Mutex::new(None),
            restarts: AtomicU32::new(0),
            last_error: std::sync::Mutex::new(None),
            last_use: std::sync::Mutex::new(Instant::now()),
            progress: Arc::new(ProgressTracker::new(std::time::Duration::from_millis(
                limits.startup_grace_ms,
            ))),
        }
    }

    /// The configured server name (`[plugins.lsp.servers.<name>]`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The workspace root this instance was started for.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// How many failed starts/crashes have accumulated since the last success.
    pub fn restarts(&self) -> u32 {
        self.restarts.load(Ordering::Relaxed)
    }

    /// A snapshot of the lifecycle state (for the manager and tests).
    pub async fn state(&self) -> InstanceState {
        self.state.lock().await.clone()
    }

    /// True only when the state says running *and* the child is still alive.
    /// The state alone is not trusted: a crash between the reaper poll and
    /// this call is observed here instead of one request later.
    pub async fn is_healthy(&self) -> bool {
        let state = self.state.lock().await;
        if !matches!(*state, InstanceState::Running { .. }) {
            return false;
        }
        let client = self.client.lock().await;
        client.as_ref().is_some_and(|c| c.is_alive())
    }

    /// Seconds since the last successful request or notification.
    pub fn idle_secs(&self) -> u64 {
        self.last_use
            .lock()
            .ok()
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    /// The negotiated position encoding, if running. Needed for `Served`.
    pub async fn encoding(&self) -> Option<PositionEncoding> {
        match *self.state.lock().await {
            InstanceState::Running { encoding } => Some(encoding),
            _ => None,
        }
    }

    /// Returns a live client, starting (or restarting) the server if needed.
    /// Concurrent callers share one start via `start_lock`.
    pub async fn ensure_running(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Arc<LspClient>, LspError> {
        if let Some(client) = self.live_client().await {
            self.touch();
            return Ok(client);
        }
        let _guard = self.start_lock.lock().await;
        // A concurrent caller may have started it while we waited.
        if let Some(client) = self.live_client().await {
            self.touch();
            return Ok(client);
        }
        self.start_inner(cancel).await?;
        let client = self.live_client().await.ok_or_else(|| {
            LspError::Io(format!(
                "LSP server `{}` started but has no client",
                self.name
            ))
        })?;
        self.touch();
        Ok(client)
    }

    /// Sends one request with `-32801` retries (500/1000/2000 ms), a per-call
    /// timeout, and cancellation that abandons the wait without touching the
    /// server. The retry loop covers `initialize`-time races too, because a
    /// just-spawned rust-analyzer answers `-32801` while its first index pass
    /// is still running.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<(Value, PositionEncoding), LspError> {
        let client = self.ensure_running(cancel).await?;
        let encoding = self.encoding().await.unwrap_or(PositionEncoding::Utf16);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let params = params.clone();
            // The deadline is passed *into* the client rather than wrapped
            // around the call: an outer timeout drops the future mid-flight
            // and leaks its pending entry, which grows without bound exactly
            // when a server is slow. The client owns the bound and the cleanup.
            let outcome = tokio::select! {
                reply = client.send_request_within(
                    method,
                    params,
                    cancel,
                    std::time::Duration::from_millis(self.limits.request_timeout_ms),
                ) => reply,
                () = cancel.cancelled() => return Err(LspError::Cancelled),
            };
            match outcome {
                Ok(value) => {
                    self.touch();
                    return Ok((value, encoding));
                }
                Err(ClientError::Rpc { code, message: _ })
                    if code == ERROR_CONTENT_MODIFIED && attempt < MAX_ATTEMPTS =>
                {
                    let delay = RETRY_DELAYS_MS[attempt - 1];
                    tracing::debug!(
                        "lsp `{}`: {method} hit ContentModified, retrying in {delay} ms (attempt {attempt}/{MAX_ATTEMPTS})",
                        self.name
                    );
                    tokio::select! {
                        () = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
                        () = cancel.cancelled() => return Err(LspError::Cancelled),
                    }
                }
                Err(ClientError::Rpc { code, message }) if code == ERROR_CONTENT_MODIFIED => {
                    return Err(LspError::Rpc {
                        server: self.name.clone(),
                        code,
                        message: format!(
                            "{message} (the server is still indexing after {MAX_ATTEMPTS} attempts)"
                        ),
                    });
                }
                Err(ClientError::Rpc { code, message }) => {
                    return Err(LspError::Rpc {
                        server: self.name.clone(),
                        code,
                        message,
                    });
                }
                Err(ClientError::Cancelled) => return Err(LspError::Cancelled),
                Err(ClientError::RequestTimeout { method, deadline }) => {
                    // The pending entry is already gone and the server was
                    // told to stop. A slow index pass is not a crash, so the
                    // instance stays running and the next call may succeed.
                    return Err(LspError::Timeout {
                        server: self.name.clone(),
                        method,
                        ms: deadline.as_millis() as u64,
                    });
                }
                // An abandoned write leaves a partial frame in the pipe, so
                // the stream is no longer framed correctly. The connection
                // cannot be reused: record the crash and let the next request
                // start a fresh server.
                Err(ClientError::WriteTimeout { .. }) => {
                    self.record_crash("the server stopped reading its stdin".to_owned())
                        .await;
                    return Err(LspError::Io(format!(
                        "LSP server `{}` stopped reading its input; it will restart on the next request",
                        self.name
                    )));
                }
                Err(ClientError::Disconnected { status }) => {
                    self.record_crash(format!("process exited ({status})"))
                        .await;
                    return Err(LspError::Io(format!(
                        "LSP server `{}` exited during `{method}` ({status}); it will restart on the next request",
                        self.name
                    )));
                }
                Err(ClientError::Io(message)) => {
                    // A dead pipe usually means the child is gone; probe so
                    // the next call restarts instead of reusing a corpse.
                    if !client.is_alive() {
                        self.record_crash(message.clone()).await;
                    }
                    return Err(LspError::Io(message));
                }
                Err(ClientError::NotStarted) => {
                    return Err(LspError::Io(format!(
                        "LSP server `{}` is not running",
                        self.name
                    )));
                }
            }
        }
    }

    /// Sends a notification (document sync). The server must be running first:
    /// a `didOpen` into the void would silently desynchronize every later
    /// query, so startup failures propagate instead of being swallowed.
    ///
    /// The write is bounded: a server that has stopped reading its stdin
    /// would otherwise park this call forever, and callers like the pool's
    /// file watcher pass a token that is never cancelled, so the hang would
    /// take the watcher down with it.
    pub async fn notify(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<(), LspError> {
        let client = self.ensure_running(cancel).await?;
        let write_deadline = std::time::Duration::from_millis(self.limits.write_timeout_ms);
        tokio::select! {
            result = client.send_notification_within(method, params, write_deadline) => {
                match result {
                    Ok(()) => {
                        self.touch();
                        Ok(())
                    }
                    Err(ClientError::Disconnected { status }) => {
                        self.record_crash(format!("process exited ({status})")).await;
                        Err(LspError::Io(format!(
                            "LSP server `{}` exited during `{method}` ({status})",
                            self.name
                        )))
                    }
                    // An abandoned write leaves a partial frame behind, so the
                    // stream is no longer framed correctly. Same treatment as
                    // a dead pipe: flip to `Error` so the next request starts a
                    // fresh server instead of writing into a corrupt stream.
                    Err(ClientError::WriteTimeout { .. }) => {
                        self.record_crash("the server stopped reading its stdin".to_owned())
                            .await;
                        Err(LspError::Io(format!(
                            "LSP server `{}` stopped reading its input during `{method}`; it will restart on the next request",
                            self.name
                        )))
                    }
                    // `send_notification_within` never returns `Cancelled` or
                    // `NotStarted` (no token is passed, and a missing stdin
                    // surfaces as `Io`), so every other failure is transport.
                    Err(err) => Err(LspError::Io(err.to_string())),
                }
            }
            () = cancel.cancelled() => Err(LspError::Cancelled),
        }
    }

    /// Orderly stop: `shutdown` + `exit` + reap, then back to `Stopped` so a
    /// later query restarts cleanly. Idempotent.
    pub async fn stop(&self) {
        // Wait for an in-flight start to finish so its client is not
        // orphaned and its outcome is not recorded as a crash.
        let _start = self.start_lock.lock().await;
        let client = self.client.lock().await.take();
        if let Some(client) = client {
            client.shutdown().await;
        }
        *self.state.lock().await = InstanceState::Stopped;
        self.progress.clear();
    }

    /// The live client if the state says running and the child answers.
    async fn live_client(&self) -> Option<Arc<LspClient>> {
        let state = self.state.lock().await;
        if !matches!(*state, InstanceState::Running { .. }) {
            return None;
        }
        let client = self.client.lock().await.clone()?;
        client.is_alive().then_some(client)
    }

    /// Records a crash observed out-of-band (dead pipe, failed poll): the
    /// state flips to `Error` immediately so nothing ever believes a corpse
    /// is `Running`, and the restart budget pays one.
    async fn record_crash(&self, message: String) {
        self.restarts.fetch_add(1, Ordering::Relaxed);
        *self.client.lock().await = None;
        self.progress.clear();
        self.remember_error(&message);
        *self.state.lock().await = InstanceState::Error { message };
    }

    /// The serialized start path. Every exit returns the state to either
    /// `Running` or `Error` — never stranded in `Starting`.
    async fn start_inner(&self, cancel: &CancellationToken) -> Result<(), LspError> {
        *self.state.lock().await = InstanceState::Starting;
        if cancel.is_cancelled() {
            *self.state.lock().await = InstanceState::Stopped;
            return Err(LspError::Cancelled);
        }
        let restarts = self.restarts.load(Ordering::Relaxed);
        if restarts > self.limits.max_restarts {
            let err = LspError::ServerFailed {
                server: self.name.clone(),
                restarts,
                last_error: self.last_error_message().await,
            };
            *self.state.lock().await = InstanceState::Error {
                message: err.to_string(),
            };
            return Err(err);
        }
        match self.spawn_and_initialize(cancel).await {
            Ok(encoding) => {
                self.restarts.store(0, Ordering::Relaxed);
                if let Ok(mut cell) = self.encoding_cell.lock() {
                    *cell = encoding;
                }
                *self.state.lock().await = InstanceState::Running { encoding };
                tracing::debug!("lsp `{}` started at {}", self.name, self.root.display());
                Ok(())
            }
            Err(LspError::Cancelled) => {
                *self.state.lock().await = InstanceState::Stopped;
                Err(LspError::Cancelled)
            }
            Err(err) => {
                // A missing executable is deterministic: report it with an
                // install hint every time, without burning restart budget.
                let counted = !matches!(err, LspError::ServerNotInstalled { .. });
                if counted {
                    self.restarts.fetch_add(1, Ordering::Relaxed);
                }
                *self.client.lock().await = None;
                self.remember_error(&err.to_string());
                *self.state.lock().await = InstanceState::Error {
                    message: err.to_string(),
                };
                Err(err)
            }
        }
    }

    fn remember_error(&self, message: &str) {
        if let Ok(mut last) = self.last_error.lock() {
            *last = Some(message.to_owned());
        }
    }

    async fn last_error_message(&self) -> String {
        if let Some(message) = self.last_error.lock().ok().and_then(|l| l.clone()) {
            return message;
        }
        match *self.state.lock().await {
            InstanceState::Error { ref message } => message.clone(),
            _ => "no further detail".to_owned(),
        }
    }

    /// Spawns the child, wires server-to-client handlers, and runs
    /// `initialize` under the startup timeout. On any failure the child is
    /// shut down (best effort) so a half-started server never lingers.
    async fn spawn_and_initialize(
        &self,
        cancel: &CancellationToken,
    ) -> Result<PositionEncoding, LspError> {
        self.progress.reset();
        // The command may be a bare name resolved from `PATH`, so the
        // file it lands on must be the user's own and not replaceable by
        // group or other. Checked before the spawn, and treated as a
        // deterministic configuration fault (never counted as a restart).
        if let Some(path) = crate::pool::resolve_command_path(&self.server.command) {
            let trust = opencraylsp_proto::trust::program_trust(&path);
            if trust != opencraylsp_proto::trust::Trust::Owned {
                // Reported as `Io`, not a new variant: `opencraylsp-tools` maps every
                // `LspError` to a machine-readable code by exhaustive match,
                // so a new variant would be a breaking change for that crate.
                // A refused program is a transport-level refusal here, and the
                // message — which names the fix — is what reaches the user.
                return Err(LspError::Io(format!(
                    "LSP server `{}` was refused: {}. Fix the `command` in the \\
                     opencraylspd config to point at a program you own",
                    self.name,
                    trust.explain(&path)
                )));
            }
        }
        let client = LspClient::spawn(
            &self.server.command,
            &self.server.args,
            &self.server.env,
            &self.root,
        )
        .await
        .map_err(|err| self.spawn_error(err))?;
        let client = Arc::new(client);
        *self.client.lock().await = Some(client.clone());
        self.wire_server_requests(&client);
        self.wire_diagnostics(&client);
        self.wire_progress(&client);
        let params = self.initialize_params();
        let outcome = tokio::select! {
            reply = tokio::time::timeout(
                std::time::Duration::from_millis(self.limits.startup_timeout_ms),
                self.initialize_with_retry(&client, params, cancel),
            ) => reply,
            () = cancel.cancelled() => {
                // A half-started server must not outlive the cancelled start.
                client.shutdown().await;
                *self.client.lock().await = None;
                return Err(LspError::Cancelled);
            }
        };
        match outcome {
            Ok(Ok(encoding)) => {
                self.progress.mark_ready();
                Ok(encoding)
            }
            Ok(Err(err)) => {
                client.shutdown().await;
                Err(err)
            }
            Err(_) => {
                client.shutdown().await;
                Err(LspError::Timeout {
                    server: self.name.clone(),
                    method: "initialize".to_owned(),
                    ms: self.limits.startup_timeout_ms,
                })
            }
        }
    }

    /// `initialize` shares the `-32801` retry loop: a fresh server can answer
    /// ContentModified before its first index pass completes.
    async fn initialize_with_retry(
        &self,
        client: &Arc<LspClient>,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<PositionEncoding, LspError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let reply = tokio::select! {
                reply = client.send_request("initialize", params.clone(), cancel) => reply,
                () = cancel.cancelled() => return Err(LspError::Cancelled),
            };
            match reply {
                Ok(value) => return self.finish_initialize(client, value).await,
                Err(ClientError::Rpc { code, message: _ })
                    if code == ERROR_CONTENT_MODIFIED && attempt < MAX_ATTEMPTS =>
                {
                    let delay = RETRY_DELAYS_MS[attempt - 1];
                    tokio::select! {
                        () = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
                        () = cancel.cancelled() => return Err(LspError::Cancelled),
                    }
                }
                Err(ClientError::Rpc { code, message }) => {
                    return Err(LspError::Rpc {
                        server: self.name.clone(),
                        code,
                        message,
                    });
                }
                Err(ClientError::Cancelled) => return Err(LspError::Cancelled),
                Err(ClientError::Disconnected { status }) => {
                    // The server's own words are the only clue to *why* it
                    // left; without them the model (and the operator) sees
                    // "stdout closed" and can do nothing about it.
                    let stderr = client.stderr_tail().await;
                    let why = if stderr.is_empty() {
                        String::new()
                    } else {
                        format!("; it said on stderr: {stderr}")
                    };
                    return Err(LspError::Io(format!(
                        "LSP server `{}` exited during initialize ({status}){why}",
                        self.name
                    )));
                }
                Err(err) => return Err(LspError::Io(err.to_string())),
            }
        }
    }

    /// Sends `initialized` and negotiates the position encoding from the
    /// server's capabilities. Anything but UTF-16 is warned about: the tool
    /// layer converts with the line text either way, but a non-UTF-16 server
    /// is unusual enough to deserve a log line.
    async fn finish_initialize(
        &self,
        client: &Arc<LspClient>,
        value: Value,
    ) -> Result<PositionEncoding, LspError> {
        let encoding = value
            .get("capabilities")
            .and_then(|caps| caps.get("positionEncoding"))
            .and_then(Value::as_str);
        let encoding = PositionEncoding::from_negotiated(encoding);
        if encoding != PositionEncoding::Utf16 {
            tracing::warn!(
                "lsp `{}` negotiated position encoding {encoding:?}; the tool converts, but this is unusual",
                self.name
            );
        }
        client
            .send_notification("initialized", Value::Object(Default::default()))
            .await
            .map_err(|err| LspError::Io(err.to_string()))?;
        Ok(encoding)
    }

    /// Answers the three server-to-client requests a modern server expects:
    /// `workspace/configuration` (configured settings, or one null per item),
    /// `client/registerCapability`, and `window/workDoneProgress/create`.
    fn wire_server_requests(&self, client: &Arc<LspClient>) {
        let settings = self.server.settings.clone().unwrap_or(Value::Null);
        client.on_request(
            "workspace/configuration",
            Arc::new(move |params: Value| {
                let count = params
                    .get("items")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(1);
                Value::Array(vec![settings.clone(); count])
            }),
        );
        client.on_request("client/registerCapability", Arc::new(|_| Value::Null));
        client.on_request("window/workDoneProgress/create", Arc::new(|_| Value::Null));
        // Read-only by design: an edit a server asks us to apply is
        // declined explicitly rather than answered with method-not-found.
        client.on_request(
            "workspace/applyEdit",
            Arc::new(
                |_| serde_json::json!({"applied": false, "failureReason": "opencraylspd is read-only"}),
            ),
        );
    }

    /// Feeds `$/progress` into the tracker that answers "still indexing?".
    fn wire_progress(&self, client: &Arc<LspClient>) {
        let tracker = self.progress.clone();
        client.on_notification(
            "$/progress",
            Arc::new(move |params: Value| tracker.on_progress(&params)),
        );
    }

    /// The server's pid while it runs.
    pub async fn pid(&self) -> Option<u32> {
        self.client.lock().await.as_ref().and_then(|c| c.pid())
    }

    /// What the server is busy with, or `None` when it is idle .
    pub fn indexing(&self) -> Option<Indexing> {
        self.progress.snapshot()
    }

    /// Forwards `textDocument/publishDiagnostics` to the manager's cache.
    /// A malformed payload is logged and dropped — one bad notification must
    /// not poison the cache or the connection.
    fn wire_diagnostics(&self, client: &Arc<LspClient>) {
        let sink = self.diag_sink.clone();
        client.on_notification(
            "textDocument/publishDiagnostics",
            Arc::new(move |params: Value| {
                handle_publish(&sink, &params);
            }),
        );
    }

    /// The `initialize` payload: `workspaceFolders` for modern
    /// servers, `rootUri`/`rootPath` for the ones still reading the deprecated
    /// fields, and the position-encoding offer of UTF-16 only.
    fn initialize_params(&self) -> Value {
        let root_uri = url::Url::from_file_path(&self.root)
            .map(|u| u.to_string())
            .unwrap_or_default();
        let root_string = self.root.display().to_string();
        let name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.name)
            .to_owned();
        serde_json::json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "opencraylspd", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "rootPath": root_string,
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "initializationOptions": self.server.initialization_options.clone().unwrap_or(Value::Null),
            "capabilities": {
                "window": {"workDoneProgress": true},
                "workspace": {"configuration": false, "workspaceFolders": false, "didChangeWatchedFiles": {"dynamicRegistration": false}},
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                        "didSave": true
                    },
                    "publishDiagnostics": {
                        "relatedInformation": true,
                        "tagSupport": {"valueSet": [1, 2]},
                        "versionSupport": true,
                        "codeDescriptionSupport": true,
                        "dataSupport": false
                    },
                    "hover": {
                        "dynamicRegistration": false,
                        "contentFormat": ["markdown", "plaintext"]
                    },
                    "definition": {"dynamicRegistration": false, "linkSupport": true},
                    "references": {"dynamicRegistration": false},
                    "documentSymbol": {
                        "dynamicRegistration": false,
                        "hierarchicalDocumentSymbolSupport": true
                    },
                    "callHierarchy": {"dynamicRegistration": false}
                },
                // offer UTF-16 only. The tool layer must know
                // the encoding before the first request, so no negotiation
                // dance — the server's answer is still honored via
                // `positionEncoding` in its capabilities.
                "general": {"positionEncodings": ["utf-16"]}
            }
        })
    }

    fn spawn_error(&self, err: ClientError) -> LspError {
        let text = err.to_string();
        // `tokio::process` surfaces a missing executable synchronously; match
        // the signature, not the locale, so this survives non-English systems:
        // Rust reports ENOENT as "No such file or directory (os error 2)".
        if text.contains("(os error 2)") {
            LspError::ServerNotInstalled {
                server: self.name.clone(),
                command: self.server.command.clone(),
            }
        } else {
            LspError::Io(format!(
                "LSP server `{}` could not be started: {text}",
                self.name
            ))
        }
    }

    fn touch(&self) {
        if let Ok(mut last) = self.last_use.lock() {
            *last = Instant::now();
        }
    }
}

/// Parses one `textDocument/publishDiagnostics` payload and forwards it.
/// Split out of `wire_diagnostics` so the malformed-payload paths are unit
/// testable without a child process.
pub(crate) fn handle_publish(sink: &DiagnosticsSink, params: &Value) {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if uri.is_empty() {
        tracing::warn!("lsp: publishDiagnostics without a uri; dropped");
        return;
    }
    let version = params
        .get("version")
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok());
    let items: Vec<Diagnostic> = params
        .get("diagnostics")
        .and_then(|d| serde_json::from_value(d.clone()).ok())
        .unwrap_or_default();
    sink(uri, version, items);
}

impl std::fmt::Debug for LspServerInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspServerInstance")
            .field("name", &self.name)
            .field("root", &self.root)
            .field("restarts", &self.restarts())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    fn stopped_instance(command: &str) -> LspServerInstance {
        LspServerInstance::new(
            "fake".to_owned(),
            ServerConfig {
                command: command.to_owned(),
                args: Vec::new(),
                env: Default::default(),
                extensions: [("fl".to_owned(), "fake".to_owned())].into(),
                root_markers: Vec::new(),
                workspace: None,
                initialization_options: None,
                settings: None,
            },
            PathBuf::from("/tmp/lsp-b-test"),
            InstanceLimits {
                startup_timeout_ms: 1000,
                startup_grace_ms: 0,
                request_timeout_ms: 1000,
                max_restarts: 3,
                write_timeout_ms: 1000,
            },
            Arc::new(StdMutex::new(PositionEncoding::Utf16)),
            Arc::new(|_, _, _| {}),
        )
    }

    #[tokio::test]
    async fn fresh_instance_is_stopped_and_idle() {
        let instance = stopped_instance("x");
        assert_eq!(instance.name(), "fake");
        assert_eq!(instance.root(), &PathBuf::from("/tmp/lsp-b-test"));
        assert_eq!(instance.restarts(), 0);
        assert_eq!(instance.state().await, InstanceState::Stopped);
        assert!(!instance.is_healthy().await);
        assert_eq!(instance.encoding().await, None);
        assert!(instance.idle_secs() < 60);
        assert!(format!("{:?}", instance).contains("fake"));
        // Stopping a stopped instance is a no-op, never an error.
        instance.stop().await;
        assert_eq!(instance.state().await, InstanceState::Stopped);
    }

    #[tokio::test]
    async fn ensure_running_honors_pre_cancelled_token() {
        let instance = stopped_instance("definitely-not-installed-ls-xyz");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            instance.ensure_running(&cancel).await.unwrap_err(),
            LspError::Cancelled
        );
        // Nothing was spawned; the state is back to Stopped, not Error.
        assert_eq!(instance.state().await, InstanceState::Stopped);
        assert_eq!(instance.restarts(), 0);
    }

    #[tokio::test]
    async fn missing_command_maps_to_not_installed_without_budget() {
        let instance = stopped_instance("definitely-not-installed-ls-xyz");
        let err = instance
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::ServerNotInstalled { .. }));
        // Deterministic failures never burn the restart budget.
        assert_eq!(instance.restarts(), 0);
        assert!(matches!(
            instance.state().await,
            InstanceState::Error { .. }
        ));
    }

    #[test]
    fn spawn_error_distinguishes_missing_binary() {
        let instance = stopped_instance("some-ls");
        let missing = instance.spawn_error(ClientError::Io(
            "failed to spawn `some-ls`: No such file or directory (os error 2)".to_owned(),
        ));
        assert!(matches!(missing, LspError::ServerNotInstalled { .. }));
        assert!(missing.to_string().contains("some-ls"));
        let other = instance.spawn_error(ClientError::Io("boom".to_owned()));
        assert!(matches!(other, LspError::Io(_)));
    }

    #[test]
    fn publish_routes_versions_and_drops_garbage() {
        let received = Arc::new(StdMutex::new(Vec::new()));
        let received_clone = received.clone();
        let sink: DiagnosticsSink = Arc::new(move |uri, version, items| {
            received_clone.lock().unwrap().push((uri, version, items));
        });
        handle_publish(
            &sink,
            &json!({
                "uri": "file:///a.fl",
                "version": 3,
                "diagnostics": [{
                    "range": {"start": {"line": 1, "character": 2},
                              "end": {"line": 1, "character": 4}},
                    "message": "m",
                }],
            }),
        );
        let events = received.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "file:///a.fl");
        assert_eq!(events[0].1, Some(3));
        assert_eq!(events[0].2.len(), 1);
        drop(events);
        // No uri: dropped, never forwarded.
        handle_publish(&sink, &json!({"version": 1, "diagnostics": []}));
        // Unusable version (a string, or wider than i32): forwarded as unknown.
        handle_publish(
            &sink,
            &json!({"uri": "u", "version": "v", "diagnostics": []}),
        );
        handle_publish(
            &sink,
            &json!({"uri": "u", "version": 9_999_999_999i64, "diagnostics": []}),
        );
        // Diagnostics that do not parse: forwarded as an empty list.
        handle_publish(&sink, &json!({"uri": "u", "diagnostics": [42]}));
        let events = received.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert!(
            events[1..]
                .iter()
                .all(|(_, v, items)| v.is_none() && items.is_empty())
        );
    }

    // ---- a server command another user could swap out is refused ----

    /// Writes an executable script at `path` with the given mode.
    fn write_program(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn instance_for(path: &std::path::Path) -> LspServerInstance {
        let dir = tempfile::tempdir().unwrap();
        LspServerInstance::new(
            "srv".to_owned(),
            ServerConfig {
                command: path.display().to_string(),
                args: Vec::new(),
                env: Default::default(),
                extensions: [("fl".to_owned(), "fake".to_owned())].into(),
                root_markers: Vec::new(),
                workspace: None,
                initialization_options: None,
                settings: None,
            },
            dir.path().to_owned(),
            InstanceLimits {
                startup_timeout_ms: 1000,
                startup_grace_ms: 0,
                request_timeout_ms: 1000,
                max_restarts: 3,
                write_timeout_ms: 1000,
            },
            Arc::new(StdMutex::new(PositionEncoding::Utf16)),
            Arc::new(|_, _, _| {}),
        )
    }

    /// A world-writable `command` is refused before anything is executed: the
    /// config points at a file anyone can replace, which would run their code
    /// as this user.
    #[tokio::test]
    async fn a_world_writable_server_command_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("evil-ls");
        write_program(&bin, 0o777);
        let instance = instance_for(&bin);
        let err = instance
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, LspError::Io(text) if text.contains("chmod")),
            "the refusal must name the fix, got {err:?}"
        );
        assert!(err.to_string().contains("you own"), "{err}");
    }

    /// A group-writable command is refused the same way.
    #[tokio::test]
    async fn a_group_writable_server_command_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("shared-ls");
        write_program(&bin, 0o775);
        let instance = instance_for(&bin);
        let err = instance
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, LspError::Io(text) if text.contains("was refused")),
            "want the refusal to be reported, got {err:?}"
        );
    }

    /// The check is about who can *replace* the program, not about refusing
    /// unusual modes: a 0755 binary this user owns passes the check and is
    /// spawned as before (it then fails to speak LSP, which is a different
    /// error entirely).
    #[tokio::test]
    async fn an_owned_server_command_passes_the_trust_check() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fine-ls");
        write_program(&bin, 0o755);
        let instance = instance_for(&bin);
        let err = instance
            .ensure_running(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            !matches!(&err, LspError::Io(text) if text.contains("was refused")),
            "a trustworthy binary must not be refused for trust reasons, got {err:?}"
        );
    }
}
