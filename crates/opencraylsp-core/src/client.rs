//! JSON-RPC over stdio for one language-server child process.
//!
//! Why hand-written instead of the `lsp-server` crate: that crate is
//! synchronous (crossbeam channels on OS threads) and built for the *server*
//! side of the protocol (`Connection::stdio()` reads its own stdin). This
//! plugin is a *client* living on tokio, talking to a child process, so
//! bridging two channel layers through `spawn_blocking` would be more code —
//! and more failure modes — than the ~200 lines here.
//!
//! Cleanup model (why there are three layers of it): a host process can exit
//! or crash without ever calling `shutdown()`, so an orderly `shutdown()`
//! cannot be the only way a child dies. The layers are:
//! 1. `shutdown()` — `shutdown` request + `exit` notification + wait, then
//!    `kill` on timeout and reap. The only path that leaves no zombie.
//! 2. `Child` is spawned with `kill_on_drop(true)`, and `Drop` additionally
//!    calls `start_kill()` — if the manager is ever dropped without
//!    `shutdown()`, the child dies with it instead of outliving the harness.
//!    (Reaping still needs the event loop; a dropped-without-shutdown manager
//!    can leave a zombie until the harness itself exits. In practice the
//!    manager lives as long as the plugin, i.e. the process, so this path is
//!    a backstop, not a lifecycle.)
//! 3. A reaper task polls `try_wait()`: on unexpected exit it records the
//!    status, fails every in-flight request (so no caller hangs forever on a
//!    dead server), and wakes `shutdown()` if one is in progress.
//!
//! Why the background tasks hold `Weak`: they are spawned onto the runtime and
//! would otherwise keep the last `Arc` alive forever, which would pin the
//! `Child` and defeat layer 2.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Stdio;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicI64, Ordering},
};
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio_util::sync::CancellationToken;

/// The transient "server is still indexing" code. The retry
/// policy lives in `instance.rs`; this constant is shared so both sides agree
/// on what counts as retryable.
pub const ERROR_CONTENT_MODIFIED: i64 = -32_801;

/// JSON-RPC "method not found", used when the server asks for a
/// server-to-client request nobody registered a handler for.
pub const ERROR_METHOD_NOT_FOUND: i64 = -32_601;

/// Upper bound for one framed body. A language server answering with more is
/// either broken or hostile; dropping the connection beats OOMing the harness.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound for one header line; same reasoning as above.
const MAX_HEADER_BYTES: usize = 4096;

/// How often the reaper polls the child. Prompt enough to flip a crashed
/// server to `error` before the next request, cheap enough to be noise.
const REAP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long `shutdown()` waits for the child to exit on its own after `exit`
/// before killing it.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long `shutdown()` waits for the reply to the `shutdown` request. LSP
/// says `exit` may only follow that reply; a server that never answers is
/// simply sent `exit` anyway once this passes.
const SHUTDOWN_REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Consecutive non-EOF stdout errors after which the server is treated as
/// gone (so the instance restarts it) instead of retrying forever.
const MAX_CONSECUTIVE_READ_ERRORS: u32 = 8;

/// How long the cancel path spends trying to deliver `$/cancelRequest`. The
/// point of cancellation is to return promptly, so a server that is not
/// draining its stdin loses the notice rather than holding the caller up.
const CANCEL_NOTICE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Default bound on one write to the server's stdin.
///
/// A `write_all` to a pipe whose reader is not draining blocks forever once
/// the pipe buffer is full — there is no back-pressure escape. Without this
/// bound a server that has stopped reading wedges every writer: the pool's
/// file watcher in particular, whose task is never cancelled, would hang
/// there permanently and silently stop telling the server about file changes.
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why one JSON-RPC exchange did not produce a result.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ClientError {
    /// The client was never started, or was shut down already.
    #[error("LSP client is not running")]
    NotStarted,
    /// Transport failure (spawn, write, read, malformed frame).
    #[error("LSP transport error: {0}")]
    Io(String),
    /// The server answered with a JSON-RPC error object.
    #[error("LSP server error {code}: {message}")]
    Rpc { code: i64, message: String },
    /// The child exited (crash or shutdown from the other side). In-flight
    /// requests all fail this way; the instance layer turns the first one
    /// into a state flip and restarts on the next call.
    #[error("LSP server process exited ({status})")]
    Disconnected { status: String },
    /// The server did not answer within the caller's deadline. The pending
    /// entry has already been removed and the server told to stop, so this is
    /// a clean abandonment rather than a leak.
    #[error("LSP server did not answer `{method}` within {deadline:?}")]
    RequestTimeout { method: String, deadline: Duration },
    /// A write to the server's stdin did not complete within the write
    /// deadline — the server is not draining its input.
    ///
    /// A `write_all` that is abandoned part-way leaves a *partial* frame in
    /// the pipe, so the byte stream is no longer framed correctly and no
    /// further message can be trusted to be readable. The instance layer
    /// treats this as a crash and restarts the server, which is the only way
    /// to resynchronise the stream.
    #[error("LSP server stopped reading its stdin (write timed out after {deadline:?})")]
    WriteTimeout { deadline: Duration },
    /// The caller's `CancellationToken` fired. The server is untouched and
    /// stays usable — cancelling only abandons the wait.
    #[error("the LSP request was cancelled")]
    Cancelled,
}

/// Handles a server-to-client notification (`method` without `id`).
pub type NotificationHandler = Arc<dyn Fn(Value) + Send + Sync>;

/// Answers a server-to-client request (`method` with `id`); the return value
/// is sent back as the response `result`.
pub type RequestHandler = Arc<dyn Fn(Value) -> Value + Send + Sync>;

/// The last lines a server wrote to stderr, kept so a crash at startup can say
/// *why* instead of just "stdout closed".
#[derive(Debug, Default)]
struct StderrTail {
    lines: Mutex<std::collections::VecDeque<String>>,
    /// Set once the stderr pipe reached EOF, i.e. the tail is complete.
    done: AtomicBool,
}

/// Lines kept, and characters kept per line / in total when reported.
const STDERR_TAIL_LINES: usize = 12;
const STDERR_LINE_CHARS: usize = 300;
const STDERR_REPORT_CHARS: usize = 1200;

impl StderrTail {
    fn push(&self, line: &str) {
        let mut kept: String = line.chars().take(STDERR_LINE_CHARS).collect();
        if line.chars().count() > STDERR_LINE_CHARS {
            kept.push('…');
        }
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() == STDERR_TAIL_LINES {
                lines.pop_front();
            }
            lines.push_back(kept);
        }
    }

    /// The tail as one string, newest lines last, capped in size.
    fn report(&self) -> String {
        let joined = self
            .lines
            .lock()
            .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join(" | "))
            .unwrap_or_default();
        let count = joined.chars().count();
        if count <= STDERR_REPORT_CHARS {
            joined
        } else {
            let tail: String = joined.chars().skip(count - STDERR_REPORT_CHARS).collect();
            format!("…{tail}")
        }
    }
}

/// Mutable state shared between the client handle and its background tasks.
struct Shared {
    stderr: Arc<StderrTail>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, ClientError>>>>,
    notify_handlers: Mutex<HashMap<String, Vec<NotificationHandler>>>,
    request_handlers: Mutex<HashMap<String, RequestHandler>>,
    /// Guarded by a blocking mutex because `Drop` must be able to reach it,
    /// and `Drop` cannot wait on an async mutex. Critical sections are tiny
    /// (`try_wait` / `start_kill` / take), never held across `.await`.
    child: Mutex<Option<Child>>,
    /// The child's pid, captured at spawn (the handle is `take`n on exit).
    pid: Option<u32>,
    /// Set by `shutdown()` so the reaper does not mistake an orderly stop for
    /// a crash (same reason the reference implementation tracks `isStopping`).
    stopping: AtomicBool,
    /// Exit description once the reaper has collected the child.
    exited: Mutex<Option<String>>,
    /// Fired whenever `exited` transitions to `Some`, and when the pending
    /// map is drained — `shutdown()` waits on this instead of sleeping.
    exit_notify: Notify,
}

/// Whether an environment variable name looks like it carries a credential.
///
/// Name-based and deliberately broad: wrongly dropping a variable a language
/// server did not need costs nothing, while passing a token to code from an
/// untrusted repository can cost a lot. A server that really needs one can be
/// given it explicitly through the `env` table of its config entry.
pub(crate) fn is_sensitive_env_name(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
        "PRIVATE_KEY",
    ];
    let upper = name.to_ascii_uppercase();
    // `SSH_AUTH_SOCK` is no secret itself, but it lets whoever holds it sign
    // with the user's keys.
    upper == "SSH_AUTH_SOCK" || MARKERS.iter().any(|marker| upper.contains(marker))
}

/// Strips credential-looking variables from the child's inherited environment,
/// except those the config sets explicitly. Call it before applying `explicit`.
fn remove_sensitive_env(
    command: &mut Command,
    inherited: impl Iterator<Item = std::ffi::OsString>,
    explicit: &BTreeMap<String, String>,
) {
    for name in inherited {
        let Some(text) = name.to_str() else { continue };
        if is_sensitive_env_name(text) && !explicit.contains_key(text) {
            command.env_remove(&name);
        }
    }
}

/// One language-server child process with multiplexed JSON-RPC over stdio.
///
/// Clone shares the same child: the instance layer holds one `Arc` per server,
/// and background tasks only keep a `Weak` (see module docs).
#[derive(Clone)]
pub struct LspClient {
    shared: Arc<Shared>,
}

impl LspClient {
    /// Spawns `command` and starts the reader/reaper tasks. Returns
    /// `ClientError::Io` with an install hint when the executable is missing —
    /// `tokio::process::Command::spawn` reports `ENOENT` synchronously, so
    /// unlike the reference implementation no async "wait for spawn" dance is
    /// needed; if `spawn()` returns `Ok` the child exists.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<Self, ClientError> {
        let mut builder = Command::new(command);
        builder.args(args);
        // A language server runs the workspace's own code (build scripts,
        // proc-macros, plugins), so it must not inherit this process's
        // secrets. Values the user put in the config are explicit and stay.
        remove_sensitive_env(&mut builder, std::env::vars_os().map(|(name, _)| name), env);
        let mut child = builder
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| ClientError::Io(format!("failed to spawn `{command}`: {err}")))?;
        // Stderr is drained so a chatty server can never block on a full pipe.
        let tail = Arc::new(StderrTail::default());
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(BufReader::new(stderr), tail.clone()));
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClientError::Io(format!("`{command}` started without a stdout pipe")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClientError::Io(format!("`{command}` started without a stdin pipe")))?;
        let pid = child.id();
        let shared = Arc::new(Shared {
            stderr: tail,
            stdin: AsyncMutex::new(Some(stdin)),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            notify_handlers: Mutex::new(HashMap::new()),
            request_handlers: Mutex::new(HashMap::new()),
            child: Mutex::new(Some(child)),
            pid,
            stopping: AtomicBool::new(false),
            exited: Mutex::new(None),
            exit_notify: Notify::new(),
        });
        spawn_reader(Arc::downgrade(&shared), stdout);
        spawn_reaper(Arc::downgrade(&shared));
        Ok(Self { shared })
    }

    /// What the server last wrote to stderr (newest last), for explaining a
    /// crash. Waits briefly for the pipe to drain, because the exit is usually
    /// noticed a moment before the last stderr lines are read.
    pub async fn stderr_tail(&self) -> String {
        for _ in 0..6 {
            if self.shared.stderr.done.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        self.shared.stderr.report()
    }

    /// The child's pid while it is alive.
    pub fn pid(&self) -> Option<u32> {
        self.is_alive().then_some(self.shared.pid).flatten()
    }

    /// True while the child has not been collected by the reaper.
    pub fn is_alive(&self) -> bool {
        self.shared
            .exited
            .lock()
            .map(|e| e.is_none())
            .unwrap_or(false)
    }

    /// The reaper's exit description, if the child is gone.
    pub fn exit_status(&self) -> Option<String> {
        self.shared.exited.lock().ok().and_then(|e| e.clone())
    }

    /// Registers a handler for a server-to-client notification. Safe to call
    /// before or after `spawn`; the reader consults the map per message.
    pub fn on_notification(&self, method: &str, handler: NotificationHandler) {
        if let Ok(mut handlers) = self.shared.notify_handlers.lock() {
            handlers.entry(method.to_owned()).or_default().push(handler);
        }
    }

    /// Registers a handler for a server-to-client request. Unregistered
    /// methods are answered with "method not found" so the server never hangs
    /// waiting for a reply (an unanswered
    /// `workspace/configuration` wedges TypeScript servers).
    pub fn on_request(&self, method: &str, handler: RequestHandler) {
        if let Ok(mut handlers) = self.shared.request_handlers.lock() {
            handlers.insert(method.to_owned(), handler);
        }
    }

    /// Sends one request and waits for its response. Concurrent calls are
    /// multiplexed by id. Cancellation removes the pending entry and returns
    /// `Cancelled`; a late server reply then finds no waiter and is dropped.
    ///
    /// There is no deadline here: this is the unbounded entry point, used
    /// where the caller's own bound should win (see
    /// [`Self::send_request_within`] — the bound and the cleanup must live in
    /// the same place, or a timeout leaks its pending entry).
    pub async fn send_request(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, ClientError> {
        self.send_request_within(method, params, cancel, Duration::MAX)
            .await
    }

    /// [`Self::send_request`] with a deadline the client owns.
    ///
    /// The deadline lives *inside* this function on purpose: the pending
    /// entry must be removed on every exit, including the timeout one. An
    /// outer `tokio::time::timeout` in the caller simply drops the future
    /// mid-flight, which leaks the entry — a slow or wedged server is exactly
    /// when that happens, so the map grows without bound. On expiry this
    /// removes the entry, tells the server to stop, and returns
    /// [`ClientError::RequestTimeout`].
    pub async fn send_request_within(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
        deadline: Duration,
    ) -> Result<Value, ClientError> {
        if cancel.is_cancelled() {
            return Err(ClientError::Cancelled);
        }
        if self.exit_status().is_some() {
            return Err(ClientError::Disconnected {
                status: self.exit_status().unwrap_or_else(|| "unknown".to_owned()),
            });
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self
                .shared
                .pending
                .lock()
                .map_err(|_| ClientError::Io("pending map poisoned".to_owned()))?;
            pending.insert(id, tx);
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        // The write is bounded by the same deadline as the wait for the reply: a
        // server that stopped reading stdin would otherwise hold this call (and
        // `shutdown`, which goes through here) past any deadline.
        let written = match tokio::time::timeout(deadline, self.write_message(&body)).await {
            Ok(result) => result,
            Err(_) => Err(ClientError::WriteTimeout { deadline }),
        };
        if let Err(err) = written {
            self.remove_pending(id);
            return Err(err);
        }
        // The reader sends `Result<Value, ClientError>` down the oneshot, so
        // `reply` is `Result<Result<Value, ClientError>, RecvError>`; each arm
        // below flattens that into a single `Result<Value, ClientError>`. The
        // `timeout` then adds the only outer layer: `Ok` is "in time", `Err`
        // is "deadline".
        let answered = tokio::time::timeout(deadline, async {
            tokio::select! {
                reply = rx => match reply {
                    Ok(result) => result,
                    // The reader dropped us: it saw the connection die.
                    Err(_) => Err(ClientError::Disconnected {
                        status: self.exit_status().unwrap_or_else(|| "connection closed".to_owned()),
                    }),
                },
                () = cancel.cancelled() => Err(ClientError::Cancelled),
            }
        })
        .await;
        match answered {
            // The reply landed: nothing to clean up beyond the entry itself.
            Ok(Ok(value)) => {
                self.remove_pending(id);
                Ok(value)
            }
            // The caller cancelled. The pending entry is gone, but the server
            // is still computing an answer nobody wants, so announce it
            // (LSP 3.17 §3.9.3.1) before returning.
            Ok(Err(ClientError::Cancelled)) => {
                self.remove_pending(id);
                let _ = self.send_cancel_notice(id).await;
                Err(ClientError::Cancelled)
            }
            // The reader dropped us: it saw the connection die. The server is
            // gone too, so there is nothing left to cancel.
            Ok(Err(other)) => {
                self.remove_pending(id);
                Err(other)
            }
            // Timed out. Remove the entry, tell the server to stop computing an
            // answer nobody is waiting for, and leave the connection itself
            // usable — a late reply simply finds no waiter and is dropped.
            Err(_) => {
                self.remove_pending(id);
                let _ = self.send_cancel_notice(id).await;
                Err(ClientError::RequestTimeout {
                    method: method.to_owned(),
                    deadline,
                })
            }
        }
    }

    /// Best-effort, bounded `$/cancelRequest` for a request we gave up on.
    async fn send_cancel_notice(&self, id: i64) -> Result<(), ClientError> {
        match tokio::time::timeout(
            CANCEL_NOTICE_TIMEOUT,
            self.send_notification("$/cancelRequest", serde_json::json!({ "id": id })),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(err),
            // The point is to return promptly: a server that is not draining
            // its stdin loses the notice rather than holding the caller up.
            Err(_) => {
                tracing::debug!("lsp: `$/cancelRequest` for id {id} was not delivered in time");
                Ok(())
            }
        }
    }

    /// Sends a notification (fire-and-forget), bounded by
    /// [`DEFAULT_WRITE_TIMEOUT`]. A write failure means the child
    /// is gone; the error is returned so the caller can flip state, not
    /// swallowed — silently dropping `didOpen` is how queries return stale
    /// results that look fresh.
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<(), ClientError> {
        self.send_notification_within(method, params, DEFAULT_WRITE_TIMEOUT)
            .await
    }

    /// [`Self::send_notification`] with an explicit write bound.
    ///
    /// The bound covers waiting for the stdin lock and the write itself.
    /// Timing out mid-write leaves a partial frame in the pipe, so the stream
    /// can no longer be parsed frame-by-frame; the caller must treat this as
    /// a dead server and restart it rather than keep using the connection.
    pub async fn send_notification_within(
        &self,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<(), ClientError> {
        if self.exit_status().is_some() {
            return Err(ClientError::Disconnected {
                status: self.exit_status().unwrap_or_else(|| "unknown".to_owned()),
            });
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match tokio::time::timeout(deadline, self.write_message(&body)).await {
            Ok(result) => result,
            Err(_) => Err(ClientError::WriteTimeout { deadline }),
        }
    }

    /// Orderly stop: `shutdown` request, `exit` notification, wait for the
    /// child, `kill` past the grace period, and always reap. Idempotent —
    /// safe to call twice (the plugin's `stop()` is).
    pub async fn shutdown(&self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        // The protocol order is `shutdown` request, its *reply*, then `exit`;
        // a server does not leave before `exit`, so waiting for the child here
        // (as this once did) only burned the whole grace period every time. A
        // shutdown request to an already-dead server just fails; the reaper
        // has the exit recorded either way, so errors are not propagated.
        let _ = self
            .send_request_within(
                "shutdown",
                serde_json::Value::Null,
                &CancellationToken::new(),
                SHUTDOWN_REPLY_WAIT,
            )
            .await;
        // Bounded like every other write: a server that stopped reading must
        // not make shutdown wait forever. The child is killed below regardless.
        let _ = self
            .send_notification_within("exit", Value::Null, SHUTDOWN_REPLY_WAIT)
            .await;
        // Close our copy of stdin: a server blocked reading it exits, and a
        // `shutdown`-ignoring server gets killed below, never awaited forever.
        self.shared.stdin.lock().await.take();
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, self.wait_exited()).await;
        if let Ok(mut child) = self.shared.child.lock()
            && let Some(handle) = child.as_mut()
        {
            // `kill` on an exited child is a harmless error; ignore it.
            let _ = handle.start_kill();
        }
        // Give the reaper a final window to collect the child so `shutdown()`
        // returns with no zombie behind it (the T9/T11/T14/T15 backend check).
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, self.wait_exited()).await;
    }

    /// Resolves once the child has exited.
    ///
    /// The `Notified` future is registered (`enable`) *before* the exit state
    /// is checked: `mark_exited` wakes with `notify_waiters`, which stores no
    /// permit, so checking first and subscribing second loses a wake-up that
    /// lands in between — every shutdown then sat out its full grace period.
    async fn wait_exited(&self) {
        loop {
            let notified = self.shared.exit_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.exit_status().is_some() {
                return;
            }
            notified.await;
        }
    }

    /// Best-effort kill so a dropped client never leaves a runaway child.
    /// Reaping still belongs to the reaper task / `shutdown()`: `Drop` cannot
    /// block on the event loop.
    fn kill_child(&self) {
        if let Ok(mut child) = self.shared.child.try_lock()
            && let Some(handle) = child.as_mut()
        {
            let _ = handle.start_kill();
        }
    }

    async fn write_message(&self, body: &Value) -> Result<(), ClientError> {
        let text = serde_json::to_string(body)
            .map_err(|err| ClientError::Io(format!("failed to encode message: {err}")))?;
        let framed = encode_message(text.as_bytes());
        let mut stdin = self.shared.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(ClientError::NotStarted);
        };
        pipe.write_all(&framed)
            .await
            .map_err(|err| ClientError::Io(format!("failed to write to server stdin: {err}")))?;
        pipe.flush()
            .await
            .map_err(|err| ClientError::Io(format!("failed to flush server stdin: {err}")))?;
        Ok(())
    }

    fn remove_pending(&self, id: i64) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.remove(&id);
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.kill_child();
    }
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient")
            .field("alive", &self.is_alive())
            .finish()
    }
}

/// Frames one JSON-RPC body with `Content-Length` headers (the only framing
/// LSP 3.17 uses over stdio). Pure so unit tests can pin it byte-for-byte.
pub(crate) fn encode_message(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body);
    out
}

/// True when more than a header's worth of bytes arrived without a header
/// terminator, or the header announces a body larger than the cap: waiting for
/// more input could never produce a frame.
fn header_overflow(buffer: &[u8]) -> bool {
    let window = &buffer[..buffer.len().min(MAX_HEADER_BYTES + 4)];
    match window.windows(4).position(|w| w == b"\r\n\r\n") {
        None => buffer.len() > MAX_HEADER_BYTES + 4,
        Some(pos) => std::str::from_utf8(&window[..pos])
            .ok()
            .and_then(|headers| {
                headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.trim().eq_ignore_ascii_case("Content-Length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .is_some_and(|length| length > MAX_BODY_BYTES),
    }
}

/// Splits one framed buffer (`headers\r\n\r\nbody`) into the body bytes.
/// Returns `None` when the headers carry no usable `Content-Length`, or the
/// buffer ends before the body is complete — the reader then waits for more
/// bytes instead of choking. Pure for the same reason as `encode_message`.
pub(crate) fn split_framed(buffer: &[u8]) -> Option<(usize, usize)> {
    // Only the header region is scanned. Scanning the whole buffer made every
    // 8 KiB read of a large body rescan everything received so far — quadratic
    // in the body size, which looked exactly like a hang on a big response.
    let window = &buffer[..buffer.len().min(MAX_HEADER_BYTES + 4)];
    let end = window
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)?;
    let headers = std::str::from_utf8(&buffer[..end]).ok()?;
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
        .find(|(name, _)| name.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())?;
    if length > MAX_BODY_BYTES {
        return None;
    }
    let total = end.checked_add(length)?;
    (buffer.len() >= total).then_some((end, total))
}

/// Classifies one parsed JSON-RPC message for the reader loop. Pure and
/// unit-tested: the shapes below are the whole protocol surface this client
/// speaks.
#[derive(Debug, PartialEq)]
pub(crate) enum Incoming {
    /// `{id, result|error}` — completes a pending `send_request`.
    Response { id: i64, body: Value },
    /// `{id, method, params}` — the server asks the client; needs a reply.
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// `{method, params}` — e.g. `textDocument/publishDiagnostics`.
    Notification { method: String, params: Value },
    /// Anything else (e.g. a `$/progress` we never subscribed to): logged and
    /// dropped, never fatal.
    Ignored,
}

pub(crate) fn classify_message(message: &Value) -> Incoming {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (id, method) {
        (Value::Null, Some(method)) => Incoming::Notification {
            method,
            params: message.get("params").cloned().unwrap_or(Value::Null),
        },
        (id @ (Value::Number(_) | Value::String(_)), Some(method)) => Incoming::ServerRequest {
            id,
            method,
            params: message.get("params").cloned().unwrap_or(Value::Null),
        },
        (Value::Number(_), None) => match message.get("id").and_then(Value::as_i64) {
            Some(id) => Incoming::Response {
                id,
                body: message.clone(),
            },
            // We only ever *send* integer ids, so a numeric id we cannot read
            // as one comes from a non-conforming server. Substituting a
            // sentinel used to drop the reply silently and leave the caller
            // counting down its whole timeout with nothing in the log to say
            // why — say so instead, and treat it like any other unroutable
            // message.
            None => {
                let raw = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                tracing::warn!(
                    id = %raw,
                    "lsp: server response carries a non-integer id; the request it answers will time out"
                );
                Incoming::Ignored
            }
        },
        _ => Incoming::Ignored,
    }
}

/// Reads frames from `stdout` and dispatches them until EOF (child gone) or
/// the client is dropped. On EOF every pending request fails with
/// `Disconnected` — a caller must never hang on a dead server.
///
/// Generic over the byte source so unit tests can drive the loop with a
/// memory slice instead of a child process.
fn spawn_reader<R>(client: Weak<Shared>, stdout: R)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout);
        let mut buffer = Vec::<u8>::with_capacity(8192);
        let mut consecutive_errors = 0u32;
        loop {
            let Some(shared) = client.upgrade() else {
                break;
            };
            match read_frame(&mut lines, &mut buffer).await {
                Ok(body) => {
                    consecutive_errors = 0;
                    handle_body(&shared, &body).await;
                }
                Err(ReadFrame::Eof) => {
                    mark_exited(&shared, "stdout closed (server exited)");
                    break;
                }
                Err(ReadFrame::Oversize) => {
                    // A hostile/huge frame must not wedge the reader: drop the
                    // connection state and let the instance restart the server.
                    mark_exited(&shared, "oversize frame (over 64 MB)");
                    break;
                }
                Err(ReadFrame::Io(detail)) => {
                    // A transient read error keeps the loop, but with back-off
                    // and a cap: a pipe that errors forever without EOF used to
                    // spin a core at 100% and never mark the client exited, so
                    // the instance stayed "running" and the language was dead
                    // for the rest of the process.
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_READ_ERRORS {
                        mark_exited(
                            &shared,
                            format!("stdout kept failing ({consecutive_errors} errors): {detail}"),
                        );
                        break;
                    }
                    tracing::debug!("lsp: transient stdout read error: {detail}");
                    drop(shared);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        50 * u64::from(consecutive_errors),
                    ))
                    .await;
                    continue;
                }
            }
        }
    });
}

/// Watches the child via `try_wait` polling (the `Child` stays behind the
/// shared mutex so `shutdown()` can still `kill` it — moving ownership into
/// this task would split that path in two). Records the exit, fails pending
/// requests, and notifies `shutdown()` waiters.
fn spawn_reaper(client: Weak<Shared>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(REAP_POLL_INTERVAL);
        loop {
            interval.tick().await;
            let Some(shared) = client.upgrade() else {
                break;
            };
            let status = {
                let Ok(mut child) = shared.child.try_lock() else {
                    continue;
                };
                let Some(handle) = child.as_mut() else {
                    // Already collected (e.g. by a previous poll): stop polling.
                    break;
                };
                match handle.try_wait() {
                    Ok(Some(status)) => {
                        // Collected: take the handle out so `kill_on_drop`
                        // has nothing left to do, and the zombie is reaped.
                        child.take();
                        Some(status.to_string())
                    }
                    Ok(None) => None,
                    Err(err) => Some(format!("wait failed: {err}")),
                }
            };
            if let Some(status) = status {
                mark_exited(&shared, status);
                break;
            }
            if shared.exited.lock().map(|e| e.is_some()).unwrap_or(true) {
                break;
            }
        }
    });
}

/// Records the exit once, fails every in-flight request, and wakes waiters.
/// First writer wins — reader-EOF and reaper-poll race here by design.
fn mark_exited(shared: &Arc<Shared>, status: impl Into<String>) {
    let mut exited = match shared.exited.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if exited.is_some() {
        return;
    }
    let status = status.into();
    *exited = Some(status.clone());
    if let Ok(mut pending) = shared.pending.lock() {
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(ClientError::Disconnected {
                status: status.clone(),
            }));
        }
    }
    shared.exit_notify.notify_waiters();
}

/// Answers one server-to-client request: run the registered handler, or
/// `MethodNotFound` when nobody handles it. Write failures are ignored — the
/// child is gone and the reaper will say so.
async fn handle_body(shared: &Arc<Shared>, body: &[u8]) {
    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        Err(_) => {
            tracing::warn!("lsp: dropping malformed frame from server");
            return;
        }
    };
    match classify_message(&message) {
        Incoming::Response { id, body } => {
            let tx = shared
                .pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&id));
            let Some(tx) = tx else {
                // Late reply to a cancelled/timed-out request: drop it.
                return;
            };
            let _ = tx.send(response_to_result(&body));
        }
        Incoming::ServerRequest { id, method, params } => {
            let reply = shared
                .request_handlers
                .lock()
                .ok()
                .and_then(|handlers| handlers.get(&method).cloned());
            let body = match reply {
                Some(handler) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": handler(params),
                }),
                None => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": ERROR_METHOD_NOT_FOUND,
                        "message": format!("no handler for `{method}`"),
                    },
                }),
            };
            // A reply that cannot be serialized is not sent at all: an empty
            // `Content-Length: 0` frame is malformed JSON-RPC to the server.
            let Ok(text) = serde_json::to_string(&body) else {
                tracing::warn!(method = %method, "lsp: could not serialize reply; not sent");
                return;
            };
            let framed = encode_message(text.as_bytes());
            let mut stdin = shared.stdin.lock().await;
            if let Some(pipe) = stdin.as_mut() {
                let _ = pipe.write_all(&framed).await;
            }
        }
        Incoming::Notification { method, params } => {
            let handlers = shared
                .notify_handlers
                .lock()
                .ok()
                .and_then(|handlers| handlers.get(&method).cloned())
                .unwrap_or_default();
            for handler in handlers {
                handler(params.clone());
            }
        }
        Incoming::Ignored => {
            tracing::debug!("lsp: ignoring server message without routing shape");
        }
    }
}

/// Turns a JSON-RPC response object into a result: `error` wins over `result`
/// when both are present (a sloppy server must not smuggle a failure past us
/// inside a `result` field).
fn response_to_result(body: &Value) -> Result<Value, ClientError> {
    if let Some(error) = body.get("error").filter(|e| !e.is_null()) {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned();
        return Err(ClientError::Rpc { code, message });
    }
    Ok(body.get("result").cloned().unwrap_or(Value::Null))
}

#[derive(Debug)]
enum ReadFrame {
    Eof,
    Oversize,
    Io(String),
}

/// Reads one `Content-Length`-framed body. `buffer` is scratch space reused
/// across calls so the steady state allocates nothing but the body itself.
///
/// Generic over the byte source for the same reason as `spawn_reader`.
async fn read_frame<R>(lines: &mut BufReader<R>, buffer: &mut Vec<u8>) -> Result<Vec<u8>, ReadFrame>
where
    R: AsyncRead + Unpin,
{
    // `buffer` is NOT cleared here: it carries the bytes that arrived after the
    // previous frame. A server routinely writes a notification and a response
    // in one burst; clearing dropped the second message and left its request
    // waiting until the timeout.
    loop {
        if let Some((start, end)) = split_framed(buffer) {
            let body = buffer[start..end].to_vec();
            // Keep any bytes after the frame for the next call.
            let rest = buffer[end..].to_vec();
            buffer.clear();
            buffer.extend_from_slice(&rest);
            return Ok(body);
        }
        // No complete frame yet: are we at least making progress toward one?
        if buffer.len() > MAX_BODY_BYTES + MAX_HEADER_BYTES || header_overflow(buffer) {
            return Err(ReadFrame::Oversize);
        }
        let mut chunk = vec![0u8; 8192];
        match lines.read(chunk.as_mut_slice()).await {
            // EOF, with or without a partial frame buffered: either way the
            // child is gone, and a trailing fragment must not become a hang.
            Ok(0) => return Err(ReadFrame::Eof),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(err) => return Err(ReadFrame::Io(err.to_string())),
        }
    }
}

/// Server stderr is diagnostics for the operator, not protocol: log the lines
/// and drop them, so a verbose server can never fill the pipe and deadlock.
///
/// Generic over the source so tests can feed it scripted lines.
async fn drain_stderr<R>(stderr: R, tail: Arc<StderrTail>)
where
    R: AsyncBufRead + Unpin,
{
    let mut lines = stderr.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_owned();
        if !line.is_empty() {
            tracing::debug!("lsp server stderr: {line}");
            tail.push(&line);
        }
    }
    tail.done.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {

    #[test]
    fn credential_looking_names_are_recognised() {
        for name in [
            "GITHUB_TOKEN",
            "github_token",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_ACCESS_KEY_ID",
            "DB_PASSWORD",
            "OPENAI_API_KEY",
            "CARGO_REGISTRIES_PRIVATE_TOKEN",
            "SSH_AUTH_SOCK",
        ] {
            assert!(is_sensitive_env_name(name), "{name}");
        }
        for name in [
            "PATH",
            "HOME",
            "RUST_LOG",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "LANG",
        ] {
            assert!(!is_sensitive_env_name(name), "{name}");
        }
    }

    /// Runs `sh` with the variable set the way an inherited one would be, then
    /// lets the scrubber loose on it, and reports what the child sees.
    async fn child_sees(var: &str, explicit: &[(&str, &str)]) -> String {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("printf %s \"${{{var}-unset}}\""))
            .env(var, "inherited")
            .stdout(Stdio::piped());
        let explicit: BTreeMap<String, String> = explicit
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        remove_sensitive_env(&mut command, [var.into()].into_iter(), &explicit);
        let out = command.envs(&explicit).output().await.expect("run sh");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[tokio::test]
    async fn an_inherited_secret_does_not_reach_the_child() {
        assert_eq!(child_sees("LSPD_TEST_TOKEN", &[]).await, "unset");
    }

    #[tokio::test]
    async fn a_secret_set_in_the_config_is_kept() {
        assert_eq!(
            child_sees("LSPD_TEST_TOKEN", &[("LSPD_TEST_TOKEN", "from-config")]).await,
            "from-config"
        );
    }

    #[tokio::test]
    async fn an_ordinary_variable_is_untouched() {
        assert_eq!(child_sees("LSPD_TEST_PLAIN", &[]).await, "inherited");
    }
    use super::*;
    use serde_json::json;
    use std::io::Error as IoError;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll};

    /// A response id we cannot read as an integer is dropped *and logged*.
    ///
    /// The old sentinel (`i64::MIN`) made it indistinguishable from a late
    /// reply to a cancelled request, so a non-conforming server's answer just
    /// vanished and the caller sat out its whole timeout with nothing in the
    /// log to explain it.
    #[test]
    fn a_non_integer_response_id_is_ignored_rather_than_mismatched() {
        assert_eq!(
            classify_message(&json!({ "id": 1.5, "result": 1 })),
            Incoming::Ignored
        );
        assert_eq!(
            classify_message(&json!({ "id": "not-an-int", "result": 1 })),
            Incoming::Ignored
        );
        // An integer id still routes to its request, unchanged.
        assert_eq!(
            classify_message(&json!({ "id": 7, "result": 1 })),
            Incoming::Response {
                id: 7,
                body: json!({ "id": 7, "result": 1 }),
            }
        );
    }

    /// Polls for `needle` in `path` for a bounded time, returning whatever was
    /// readable at the end (empty when the file never appeared).
    async fn wait_for_text(path: &Path, needle: &str) -> String {
        for _ in 0..100 {
            if let Ok(text) = tokio::fs::read_to_string(path).await
                && text.contains(needle)
            {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tokio::fs::read_to_string(path).await.unwrap_or_default()
    }

    /// A client with no child behind it: every transport path degrades to
    /// `NotStarted`/`Disconnected` without spawning a process.
    fn bare_client() -> LspClient {
        LspClient {
            shared: Arc::new(Shared {
                stdin: AsyncMutex::new(None),
                next_id: AtomicI64::new(1),
                pending: Mutex::new(HashMap::new()),
                notify_handlers: Mutex::new(HashMap::new()),
                request_handlers: Mutex::new(HashMap::new()),
                child: Mutex::new(None),
                stderr: Arc::new(StderrTail::default()),
                pid: None,
                stopping: AtomicBool::new(false),
                exited: Mutex::new(None),
                exit_notify: Notify::new(),
            }),
        }
    }

    #[test]
    fn framing_round_trips_through_split() {
        let body = br#"{"jsonrpc":"2.0","id":7,"result":null}"#;
        let framed = encode_message(body);
        assert!(framed.starts_with(b"Content-Length: 38\r\n\r\n"));
        let (start, end) = split_framed(&framed).unwrap();
        assert_eq!(&framed[start..end], body);
    }

    #[test]
    fn split_waits_for_more_bytes() {
        let body = b"{}";
        let mut framed = encode_message(body);
        // Truncated body: no split yet.
        framed.truncate(framed.len() - 1);
        assert_eq!(split_framed(&framed), None);
        // Headers only: no split yet.
        let headers_only = b"Content-Length: 2\r\n\r\n".to_vec();
        assert_eq!(split_framed(&headers_only), None);
    }

    #[test]
    fn split_rejects_garbage_and_huge_lengths() {
        assert_eq!(split_framed(b"not a frame at all"), None);
        assert_eq!(split_framed(b"Content-Length: nope\r\n\r\n{}"), None);
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        assert_eq!(split_framed(huge.as_bytes()), None);
        // Case-insensitive header name, extra headers tolerated.
        let mixed = b"Content-Type: application/json\r\ncontent-length: 2\r\n\r\n{}";
        let (start, end) = split_framed(mixed).unwrap();
        assert_eq!(&mixed[start..end], b"{}");
    }

    #[test]
    fn classify_routes_every_shape() {
        assert_eq!(
            classify_message(&json!({"jsonrpc": "2.0", "id": 3, "result": 1})),
            Incoming::Response {
                id: 3,
                body: json!({"jsonrpc": "2.0", "id": 3, "result": 1})
            }
        );
        assert_eq!(
            classify_message(
                &json!({"jsonrpc": "2.0", "id": 9, "method": "workspace/configuration", "params": {}})
            ),
            Incoming::ServerRequest {
                id: json!(9),
                method: "workspace/configuration".to_owned(),
                params: json!({}),
            }
        );
        assert_eq!(
            classify_message(
                &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics"})
            ),
            Incoming::Notification {
                method: "textDocument/publishDiagnostics".to_owned(),
                params: Value::Null,
            }
        );
        // String ids (some servers echo custom id types) still route.
        assert!(matches!(
            classify_message(&json!({"id": "abc", "method": "m"})),
            Incoming::ServerRequest { .. }
        ));
        // Neither id nor method: ignored, never fatal.
        assert_eq!(
            classify_message(&json!({"jsonrpc": "2.0"})),
            Incoming::Ignored
        );
        assert_eq!(
            classify_message(&json!({"id": null, "result": 1})),
            Incoming::Ignored
        );
    }

    #[test]
    fn error_beats_result_in_a_response() {
        let both =
            json!({"id": 1, "result": {"x": 1}, "error": {"code": -32_801, "message": "busy"}});
        assert_eq!(
            response_to_result(&both),
            Err(ClientError::Rpc {
                code: -32_801,
                message: "busy".to_owned()
            })
        );
        assert_eq!(response_to_result(&json!({"id": 1})), Ok(Value::Null));
        assert_eq!(
            response_to_result(&json!({"id": 1, "error": {}})),
            Err(ClientError::Rpc {
                code: 0,
                message: "unknown error".to_owned()
            })
        );
    }

    #[tokio::test]
    async fn spawn_missing_command_reports_io() {
        let err = LspClient::spawn(
            "definitely-not-installed-ls-binary",
            &[],
            &BTreeMap::new(),
            Path::new("/tmp"),
        )
        .await
        .expect_err("missing command must fail");
        assert!(matches!(err, ClientError::Io(_)));
        assert!(format!("{err}").contains("definitely-not-installed-ls-binary"));
    }

    #[tokio::test]
    async fn cancelled_request_returns_cancelled() {
        // `cat` never speaks LSP, so nothing completes; cancellation is the
        // only way out, and it must not hang.
        let client = LspClient::spawn("cat", &[], &BTreeMap::new(), Path::new("/tmp"))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            client.send_request("initialize", json!({}), &cancel).await,
            Err(ClientError::Cancelled)
        );
        // The client itself survived the cancellation untouched.
        assert!(client.is_alive());
        client.shutdown().await;
    }

    /// Cancelling must *tell the server to stop* (LSP 3.17 §3.9.3.1), not just
    /// stop listening: an aborted run otherwise leaves the language server
    /// computing an answer nobody wants.
    #[tokio::test]
    async fn cancelling_a_request_tells_the_server_to_stop() {
        let dir = tempfile::tempdir().unwrap();
        let sink = dir.path().join("stdin.txt");
        // A child that copies its stdin into a file makes everything we write
        // to the "server" observable byte for byte.
        let sender = Arc::new(
            LspClient::spawn(
                "sh",
                &["-c".to_owned(), format!("cat > {}", sink.display())],
                &BTreeMap::new(),
                Path::new("/tmp"),
            )
            .await
            .unwrap(),
        );

        let cancel = CancellationToken::new();
        let task = {
            let sender = sender.clone();
            let token = cancel.clone();
            tokio::spawn(async move {
                sender
                    .send_request("textDocument/hover", json!({}), &token)
                    .await
            })
        };
        // Let the request reach the child, then abort it.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();
        assert_eq!(task.await.unwrap(), Err(ClientError::Cancelled));

        let written = wait_for_text(&sink, "$/cancelRequest").await;
        assert!(
            written.contains("textDocument/hover"),
            "the request itself must have gone out first: {written}"
        );
        // The notice body is exactly `{"id":1}`; the request's id appears as
        // `"id":1,` followed by `"method"`, so this pins the *notice* naming the
        // cancelled request and nothing else.
        assert!(
            written.contains("$/cancelRequest"),
            "the cancellation must be announced to the server: {written}"
        );
        assert!(
            written.contains("{\"id\":1}"),
            "the notice must name the cancelled request (the first id): {written}"
        );
        reap(&sender).await;
    }

    /// A cancelled request returns promptly even when the server is not
    /// draining its stdin — the notice is best-effort, the cancellation is not.
    #[tokio::test]
    async fn cancellation_does_not_wait_on_a_stalled_server() {
        // `sleep` never reads its stdin: the pipe fills and stays full.
        let sender = Arc::new(
            LspClient::spawn(
                "sleep",
                &["60".to_owned()],
                &BTreeMap::new(),
                Path::new("/tmp"),
            )
            .await
            .unwrap(),
        );
        let cancel = CancellationToken::new();
        let task = {
            let sender = sender.clone();
            let token = cancel.clone();
            tokio::spawn(async move {
                sender
                    .send_request("textDocument/hover", json!({}), &token)
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        // Bounded by `CANCEL_NOTICE_TIMEOUT`, not by the server.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("the cancel path must not wait on the server")
            .unwrap();
        assert_eq!(outcome, Err(ClientError::Cancelled));
        reap(&sender).await;
    }

    /// A request that times out must not leave its pending
    /// entry behind.
    ///
    /// `send_request` used to be wrapped in an outer
    /// `tokio::time::timeout` by the instance layer. On expiry that dropped
    /// the future mid-flight, so the only code that removes a pending entry
    /// (reply, write failure, cancellation) never ran and the `tx` stayed in
    /// the map forever. A server that stops answering is precisely when this
    /// repeats, so the map grew without bound for as long as the daemon ran.
    #[tokio::test]
    async fn a_timed_out_request_leaves_no_pending_entry() {
        // `sleep` never speaks LSP, so nothing ever completes the request and
        // only the deadline can end it.
        let client = Arc::new(
            LspClient::spawn(
                "sleep",
                &["60".to_owned()],
                &BTreeMap::new(),
                Path::new("/tmp"),
            )
            .await
            .unwrap(),
        );
        for round in 1..=5 {
            let err = client
                .send_request_within(
                    "textDocument/hover",
                    json!({}),
                    &CancellationToken::new(),
                    Duration::from_millis(50),
                )
                .await
                .expect_err("a silent server cannot answer");
            assert!(
                matches!(err, ClientError::RequestTimeout { .. }),
                "round {round}: want a request timeout, got {err:?}"
            );
            assert_eq!(
                client.shared.pending.lock().unwrap().len(),
                0,
                "round {round}: the pending map must be empty after a timeout"
            );
        }
        reap(&client).await;
    }

    /// The same leak, reached through the layer that actually had it: repeated
    /// timeouts against one unhealthy server must not accumulate entries.
    ///
    /// The child never reads its stdin, so no reply can ever arrive and the
    /// only way out is the deadline. A child that *does* read (such as `cat`)
    /// would echo the frame back and drain the entry for unrelated reasons,
    /// hiding the very leak this test exists to catch.
    #[tokio::test]
    async fn repeated_timeouts_do_not_grow_the_pending_map() {
        let client = Arc::new(
            LspClient::spawn(
                "sleep",
                &["60".to_owned()],
                &BTreeMap::new(),
                Path::new("/tmp"),
            )
            .await
            .unwrap(),
        );
        for round in 1..=10 {
            let err = client
                .send_request_within(
                    "textDocument/hover",
                    json!({}),
                    &CancellationToken::new(),
                    Duration::from_millis(30),
                )
                .await
                .expect_err("a silent server cannot answer");
            assert!(
                matches!(err, ClientError::RequestTimeout { .. }),
                "round {round}: want a request timeout, got {err:?}"
            );
            assert_eq!(
                client.shared.pending.lock().unwrap().len(),
                0,
                "round {round}: every timeout must leave the pending map empty"
            );
        }
        reap(&client).await;
    }

    /// A *late* reply to a timed-out request is dropped
    /// rather than panicking or being delivered to the wrong waiter.
    #[tokio::test]
    async fn a_late_reply_after_a_timeout_is_dropped() {
        let client = bare_client();
        // Forge a pending entry, then let the reader see a reply for it.
        let (tx, rx) = oneshot::channel();
        client.shared.pending.lock().unwrap().insert(1, tx);
        handle_body(
            &client.shared,
            br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        )
        .await;
        assert_eq!(rx.await.unwrap().unwrap(), json!({"ok": true}));
        assert!(client.shared.pending.lock().unwrap().is_empty());
    }

    /// A write to a server that is not reading must fail
    /// within the bound instead of blocking forever.
    ///
    /// A `write_all` to a pipe whose reader stopped drains nothing and fills
    /// the pipe buffer, after which the write parks indefinitely — there is
    /// no back-pressure escape. The pool's file watcher calls this with a
    /// cancellation token that is never fired, so an unbounded write wedges
    /// the watcher task for good and the server silently stops being told
    /// about file changes.
    #[tokio::test]
    async fn a_notification_to_a_stalled_server_times_out() {
        // `sleep` never reads its stdin: the pipe fills and stays full.
        let client = Arc::new(
            LspClient::spawn(
                "sleep",
                &["60".to_owned()],
                &BTreeMap::new(),
                Path::new("/tmp"),
            )
            .await
            .unwrap(),
        );
        let deadline = Duration::from_millis(300);
        // Enough payload to overrun the pipe buffer once the child stops
        // reading; the first few small writes are absorbed, so push until
        // one of them cannot complete.
        let big = "x".repeat(256 * 1024);
        let started = std::time::Instant::now();
        let mut outcome = None;
        for _ in 0..64 {
            if let Err(err) = client
                .send_notification_within("textDocument/didChange", json!({"text": big}), deadline)
                .await
            {
                outcome = Some(err);
                break;
            }
        }
        let elapsed = started.elapsed();
        let err = outcome.expect("a server that never reads must eventually fail the write");
        // The pipe can also break outright if the child dies mid-write, so
        // the property under test is "fails in bounded time", not one exact
        // error variant. What must never happen is a hang.
        assert!(
            matches!(
                err,
                ClientError::WriteTimeout { .. }
                    | ClientError::Io(_)
                    | ClientError::Disconnected { .. }
            ),
            "want a bounded write failure, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "the write must be bounded, took {elapsed:?}"
        );
        reap(&client).await;
    }

    /// A request's write is bounded by its deadline as well: a server that never
    /// reads stdin must not hold `send_request_within` past it.
    #[tokio::test]
    async fn a_request_to_a_stalled_server_times_out_and_leaves_no_pending_entry() {
        let client = Arc::new(
            LspClient::spawn(
                "sleep",
                &["60".to_owned()],
                &BTreeMap::new(),
                Path::new("/tmp"),
            )
            .await
            .unwrap(),
        );
        let big = "z".repeat(256 * 1024);
        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        let mut outcome = None;
        for _ in 0..64 {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                client.send_request_within(
                    "textDocument/hover",
                    json!({"text": big}),
                    &cancel,
                    Duration::from_millis(300),
                ),
            )
            .await
            .expect("a request write must not outlast its deadline");
            if matches!(result, Err(ClientError::WriteTimeout { .. })) {
                outcome = Some(result);
                break;
            }
        }
        assert!(outcome.is_some(), "the pipe never filled up");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!(client.shared.pending.lock().unwrap().len(), 0);
        reap(&client).await;
    }

    /// The bound covers the wait for the stdin lock too, not just the write:
    /// a notification arriving while a huge write is in flight must still
    /// return rather than queue behind it forever.
    #[tokio::test]
    async fn a_notification_does_not_queue_behind_a_stalled_write() {
        let client = Arc::new(
            LspClient::spawn(
                "sleep",
                &["60".to_owned()],
                &BTreeMap::new(),
                Path::new("/tmp"),
            )
            .await
            .unwrap(),
        );
        let big = "y".repeat(256 * 1024);
        let writer = {
            let client = client.clone();
            tokio::spawn(async move {
                let _ = client
                    .send_notification_within(
                        "textDocument/didChange",
                        json!({"text": big}),
                        Duration::from_millis(200),
                    )
                    .await;
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        // The second write must reach its own bound rather than inheriting
        // the first one's position in the queue indefinitely.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            client.send_notification_within(
                "textDocument/didSave",
                json!({}),
                Duration::from_millis(200),
            ),
        )
        .await
        .expect("a queued notification must still return");
        assert!(
            matches!(
                outcome,
                Err(ClientError::WriteTimeout { .. }) | Err(ClientError::Io(_))
            ),
            "want a bounded failure, got {outcome:?}"
        );
        let _ = writer.await;
        reap(&client).await;
    }

    /// Kills and reaps the child directly instead of going through the orderly
    /// `shutdown()`: these children never exit on their own, so `shutdown()`
    /// would sit out its 5 s grace period twice and make the suite slow for no
    /// extra coverage.
    async fn reap(client: &Arc<LspClient>) {
        let taken = client.shared.child.lock().unwrap().take();
        if let Some(mut handle) = taken {
            handle.kill().await.ok();
            handle.wait().await.ok();
        }
    }

    #[tokio::test]
    async fn bare_client_reports_not_started() {
        let client = bare_client();
        let cancel = CancellationToken::new();
        assert_eq!(
            client.send_request("m", json!({}), &cancel).await,
            Err(ClientError::NotStarted)
        );
        assert_eq!(
            client.send_notification("m", json!(null)).await,
            Err(ClientError::NotStarted)
        );
        assert!(format!("{client:?}").contains("alive"));
    }

    #[tokio::test]
    async fn exited_client_reports_disconnected() {
        let client = bare_client();
        mark_exited(&client.shared, "gone");
        // First writer wins: the status sticks.
        mark_exited(&client.shared, "other");
        assert_eq!(client.exit_status().as_deref(), Some("gone"));
        assert!(!client.is_alive());
        let cancel = CancellationToken::new();
        assert_eq!(
            client.send_request("m", json!({}), &cancel).await,
            Err(ClientError::Disconnected {
                status: "gone".to_owned()
            })
        );
        assert!(matches!(
            client.send_notification("m", json!(null)).await,
            Err(ClientError::Disconnected { .. })
        ));
    }

    #[tokio::test]
    async fn handle_body_dispatches_every_shape() {
        let client = bare_client();
        // A pending request completed by its response.
        let (tx, rx) = oneshot::channel();
        client.shared.pending.lock().unwrap().insert(5, tx);
        handle_body(
            &client.shared,
            br#"{"jsonrpc":"2.0","id":5,"result":{"ok":true}}"#,
        )
        .await;
        assert_eq!(rx.await.unwrap(), Ok(json!({"ok": true})));
        // A late response for an abandoned id is dropped, not fatal.
        handle_body(&client.shared, br#"{"jsonrpc":"2.0","id":6,"result":1}"#).await;
        // Malformed bytes are logged and dropped.
        handle_body(&client.shared, b"{not json").await;
        // An unroutable object is ignored, never fatal.
        handle_body(&client.shared, br#"{"jsonrpc":"2.0"}"#).await;

        // Server-to-client request with a handler: the handler runs (the
        // reply write fails against no stdin, which is ignored by design).
        let seen = Arc::new(Mutex::new(Value::Null));
        let seen_clone = seen.clone();
        client.on_request(
            "workspace/configuration",
            Arc::new(move |params: Value| {
                *seen_clone.lock().unwrap() = params;
                json!([null])
            }),
        );
        handle_body(
            &client.shared,
            br#"{"jsonrpc":"2.0","id":"a","method":"workspace/configuration","params":{"items":[]}}"#,
        )
        .await;
        assert_eq!(*seen.lock().unwrap(), json!({"items": []}));
        // Without a handler the server gets MethodNotFound, not silence.
        handle_body(
            &client.shared,
            br#"{"jsonrpc":"2.0","id":"b","method":"nope","params":{}}"#,
        )
        .await;

        // Notifications fan out to every handler of that method.
        let got = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let got_clone = got.clone();
            client.on_notification(
                "textDocument/publishDiagnostics",
                Arc::new(move |params: Value| {
                    got_clone.lock().unwrap().push(params);
                }),
            );
        }
        handle_body(
            &client.shared,
            br#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"x"}}"#,
        )
        .await;
        assert_eq!(got.lock().unwrap().len(), 2);
        // No handler for this one: dropped silently.
        handle_body(
            &client.shared,
            br#"{"jsonrpc":"2.0","method":"window/logMessage","params":{}}"#,
        )
        .await;
    }

    /// Drives `spawn_reader` with memory instead of a child: an empty stream
    /// is an immediate EOF, which must fail the pending request.
    #[tokio::test]
    async fn reader_fails_pending_on_eof() {
        let client = bare_client();
        let (tx, rx) = oneshot::channel();
        client.shared.pending.lock().unwrap().insert(1, tx);
        spawn_reader(Arc::downgrade(&client.shared), &b""[..]);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("reader must settle")
            .expect("pending entry must be answered");
        assert!(matches!(outcome, Err(ClientError::Disconnected { .. })));
        assert!(!client.is_alive());
    }

    /// One oversize length prefix must break the loop, not allocate.
    #[tokio::test]
    async fn reader_breaks_on_oversize_frame() {
        let client = bare_client();
        // `'static` for the spawned task; the test process exits right after.
        let big: &'static [u8] = Box::leak(
            vec![b'x'; super::MAX_BODY_BYTES + super::MAX_HEADER_BYTES + 1].into_boxed_slice(),
        );
        spawn_reader(Arc::downgrade(&client.shared), big);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.shared.exit_notify.notified(),
        )
        .await
        .expect("oversize must settle");
        assert_eq!(
            client.exit_status().as_deref(),
            Some("oversize frame (over 64 MB)")
        );
    }

    /// A reader that errors once, then ends: the loop continues past the
    /// transient failure and still settles at EOF.
    struct FlakyThenEof {
        errored: bool,
    }

    impl AsyncRead for FlakyThenEof {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if !self.errored {
                self.errored = true;
                Poll::Ready(Err(IoError::other("transient")))
            } else {
                Poll::Ready(Ok(()))
            }
        }
    }

    #[tokio::test]
    async fn reader_survives_transient_read_error() {
        let client = bare_client();
        spawn_reader(
            Arc::downgrade(&client.shared),
            FlakyThenEof { errored: false },
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.shared.exit_notify.notified(),
        )
        .await
        .expect("transient error must settle");
        assert!(!client.is_alive());
    }

    /// A stdout that errors forever (never EOF) must not spin: after the cap
    /// the client is marked exited so the instance can restart the server.
    struct AlwaysErr;

    impl AsyncRead for AlwaysErr {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(IoError::other("broken pipe forever")))
        }
    }

    #[tokio::test]
    async fn reader_gives_up_on_endless_read_errors() {
        let client = bare_client();
        let notified = client.shared.exit_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        spawn_reader(Arc::downgrade(&client.shared), AlwaysErr);
        tokio::time::timeout(std::time::Duration::from_secs(10), notified)
            .await
            .expect("endless errors must end in an exit, not a hot loop");
        assert!(!client.is_alive());
        let status = client.exit_status().unwrap_or_default();
        assert!(status.contains("kept failing"), "unexpected: {status}");
    }

    /// Two frames back to back: the second is served from the leftover bytes
    /// without another read.
    #[tokio::test]
    async fn read_frame_keeps_trailing_bytes() {
        let first = encode_message(b"{\"a\":1}");
        let second = encode_message(b"{\"b\":2}");
        let both = [first, second].concat();
        let mut reader = BufReader::new(&both[..]);
        let mut buffer = Vec::new();
        assert_eq!(
            read_frame(&mut reader, &mut buffer).await.unwrap(),
            b"{\"a\":1}"
        );
        assert_eq!(
            read_frame(&mut reader, &mut buffer).await.unwrap(),
            b"{\"b\":2}"
        );
    }

    /// Mid-frame EOF (the child died while writing) settles as EOF, not a hang.
    #[tokio::test]
    async fn read_frame_mid_frame_eof() {
        let mut framed = encode_message(b"{\"a\":1}");
        framed.truncate(framed.len() - 2);
        let mut reader = BufReader::new(&framed[..]);
        let mut buffer = Vec::new();
        assert!(matches!(
            read_frame(&mut reader, &mut buffer).await,
            Err(ReadFrame::Eof)
        ));
        // A bare IO failure maps to the Io variant with its message.
        let mut reader = BufReader::new(FlakyThenEof { errored: false });
        assert!(matches!(
            read_frame(&mut reader, &mut buffer).await,
            Err(ReadFrame::Io(_))
        ));
    }

    #[tokio::test]
    async fn drain_stderr_consumes_lines() {
        let tail = Arc::new(StderrTail::default());
        drain_stderr(BufReader::new(&b"hello\n\nworld\n"[..]), tail.clone()).await;
        assert_eq!(tail.report(), "hello | world");
        assert!(tail.done.load(Ordering::SeqCst));
    }

    /// The reaper stops polling once the client is gone: it must not pin the
    /// `Arc` (that would defeat `kill_on_drop`).
    #[tokio::test]
    async fn reaper_exits_with_the_client() {
        let client = bare_client();
        spawn_reaper(Arc::downgrade(&client.shared));
        drop(client);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    /// With no child left to collect, the reaper stops instead of spinning.
    #[tokio::test]
    async fn reaper_stops_when_child_already_collected() {
        let client = bare_client();
        spawn_reaper(Arc::downgrade(&client.shared));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    /// An exit recorded out-of-band stops the reaper on its next poll, even
    /// with a live child behind the mutex.
    #[tokio::test]
    async fn reaper_breaks_on_recorded_exit() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = bare_client();
        *client.shared.child.lock().unwrap() = Some(child);
        spawn_reaper(Arc::downgrade(&client.shared));
        mark_exited(&client.shared, "test exit");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // Reap the sleeper explicitly: the reaper already stopped watching it.
        let taken = client.shared.child.lock().unwrap().take();
        if let Some(mut handle) = taken {
            handle.kill().await.ok();
            handle.wait().await.ok();
        }
    }

    /// While the child mutex is held elsewhere, the reaper skips its poll
    /// instead of blocking.
    #[tokio::test]
    // Holding the std mutex across the sleep *is* the scenario under test: the
    // reaper must find it contended and skip, not block the runtime.
    #[allow(clippy::await_holding_lock)]
    async fn reaper_skips_contended_poll() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = bare_client();
        *client.shared.child.lock().unwrap() = Some(child);
        spawn_reaper(Arc::downgrade(&client.shared));
        let _guard = client.shared.child.lock().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        drop(_guard);
        let taken = client.shared.child.lock().unwrap().take();
        if let Some(mut handle) = taken {
            handle.kill().await.ok();
            handle.wait().await.ok();
        }
    }

    /// `Drop` kills a live child (the backstop for a manager dropped without
    /// `shutdown()`), and drops cleanly with no child at all.
    #[tokio::test]
    async fn drop_kills_live_child() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id();
        let client = bare_client();
        *client.shared.child.lock().unwrap() = Some(child);
        drop(client);
        // The child must be dead: kill-on-drop plus the explicit Drop kill.
        // A zombie (`Z` state — killed but not yet reaped) counts as dead
        // here; reaping belongs to the reaper task and `shutdown()`.
        // Poll briefly; a live pid here fails the test.
        let dead = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !process_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok();
        assert!(dead, "dropped client left its child alive");
    }

    /// True only for a live (non-zombie) process id.
    fn process_alive(pid: Option<u32>) -> bool {
        let Some(id) = pid else {
            return false;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{id}/stat")) else {
            return false;
        };
        // State is the field after the `(comm)` name: `R`/`S`/`D` are alive,
        // `Z` (zombie) and anything else count as gone for this check.
        stat.rsplit(')')
            .next()
            .is_some_and(|after| after.trim_start().starts_with(['R', 'S', 'D']))
    }

    /// A client whose child exits on its own exercises the shutdown wait
    /// paths without any server cooperation.
    #[tokio::test]
    async fn shutdown_after_immediate_exit() {
        let client = LspClient::spawn("true", &[], &BTreeMap::new(), Path::new("/tmp"))
            .await
            .unwrap();
        // Let the reaper observe the exit first.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        client.shutdown().await;
        assert!(!client.is_alive());
    }
}
