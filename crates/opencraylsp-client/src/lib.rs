//! Client side of the opencraylspd daemon protocol: connecting, starting the daemon
//! and reconnecting.
//!
//! [`DaemonClient`] owns one connection to `opencraylspd`: it speaks JSON-RPC over the
//! daemon's unix socket, asks for the languages this connection wants, starts
//! the daemon when nobody else has, and reconnects once when a request is lost.
//! [`DaemonHost`] adapts it to the [`ToolHost`] trait `opencraylsp-mcp` consumes, so the
//! MCP layer never learns that a daemon exists.

mod connection;
mod spawn;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use opencraylsp_proto::{
    CallParams, HelloResult, HostError, ListResult, StatusReport, ToolDef, ToolHost, ToolOutput,
};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

pub use connection::{ClientError, Connection};
pub use spawn::resolve_daemon_bin;

/// The retry schedule: 50, 100, 200, 400, 800, 1600 ms, with the
/// last interval repeated until the deadline.
pub const BACKOFF: [Duration; 6] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
];

/// The retry schedule, for callers that want to reason about it.
pub fn backoff_schedule() -> &'static [Duration] {
    &BACKOFF
}

/// How long [`DaemonClient::connect`] keeps trying before giving up.
pub const DEFAULT_CONNECT_DEADLINE: Duration = Duration::from_secs(8);

/// The generous upper bound on one request once connected.
///
/// The daemon's own `request_timeout_ms` defaults to 30 s, so this is that
/// times two plus a 30 s margin: a single-sided failure (a stuck daemon, a
/// handler that never returns) must not become an infinite wait, but a merely
/// slow answer must not be cut short either. See `Connection::request`.
pub const DEFAULT_REQUEST_DEADLINE: Duration = Duration::from_secs(90);

/// How the client reaches the daemon and what it asks for.
#[derive(Clone)]
pub struct ClientOptions {
    /// The daemon's socket. Defaults to [`opencraylsp_proto::paths::default_socket_path`].
    pub socket: PathBuf,
    /// Absolute path of this connection's workspace boundary.
    pub workspace: PathBuf,
    /// Raw language input as the user typed it (`rust`, `ts`, `all`, ...). The
    /// daemon normalizes aliases and validates names; `None` means
    /// auto-detect and sends no list at all.
    pub languages: Option<Vec<String>>,
    pub client_name: String,
    pub client_version: String,
    /// Explicit daemon binary; otherwise [`resolve_daemon_bin`] guesses.
    pub daemon_bin: Option<PathBuf>,
    /// Config file to pass to a daemon this client starts. `None` means the
    /// daemon uses its own default path.
    pub daemon_config: Option<PathBuf>,
    /// Whether to start the daemon when it cannot be reached.
    pub spawn: bool,
    /// Total time spent retrying, including the backoff schedule.
    pub connect_deadline: Duration,
    /// Upper bound on one request once connected; a reply after this is a
    /// [`ClientError::Timeout`]. Defaults to [`DEFAULT_REQUEST_DEADLINE`].
    pub request_deadline: Duration,
    /// Where a daemon this client starts writes its log, for the diagnostics
    /// appended to a failed connect. `None` uses the daemon's default path.
    pub daemon_log: Option<PathBuf>,
    /// Called with the pid of a daemon this client started. Lets a supervisor
    /// (`opencraylspd status`, a test) learn about a daemon it did not exec itself.
    pub spawn_observer: Option<Arc<dyn Fn(u32) + Send + Sync>>,
}

impl std::fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOptions")
            .field("socket", &self.socket)
            .field("workspace", &self.workspace)
            .field("languages", &self.languages)
            .field("client_name", &self.client_name)
            .field("daemon_bin", &self.daemon_bin)
            .field("daemon_config", &self.daemon_config)
            .field("spawn", &self.spawn)
            .field("connect_deadline", &self.connect_deadline)
            .field("request_deadline", &self.request_deadline)
            .field("daemon_log", &self.daemon_log)
            .field(
                "spawn_observer",
                &self.spawn_observer.as_ref().map(|_| "set"),
            )
            .finish()
    }
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self::defaults()
    }
}

impl ClientOptions {
    /// The real defaults: the installed socket path and an 8 second deadline.
    pub fn defaults() -> Self {
        Self {
            socket: opencraylsp_proto::paths::default_socket_path(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            languages: None,
            client_name: "opencraylsp-mcp".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            daemon_bin: None,
            daemon_config: None,
            spawn: true,
            connect_deadline: DEFAULT_CONNECT_DEADLINE,
            request_deadline: DEFAULT_REQUEST_DEADLINE,
            daemon_log: None,
            spawn_observer: None,
        }
    }

    /// Options for a test: no socket, no spawning.
    ///
    /// The socket is deliberately empty. A test that forgets to pass one would
    /// otherwise silently talk to the daemon installed on this machine, which
    /// is exactly the accident that once pushed the director's phone out of the
    /// running instance.
    pub fn default_for_tests() -> Self {
        Self {
            socket: PathBuf::new(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            languages: None,
            client_name: "opencraylsp-mcp-test".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            daemon_bin: None,
            daemon_config: None,
            spawn: false,
            connect_deadline: Duration::from_millis(500),
            request_deadline: DEFAULT_REQUEST_DEADLINE,
            daemon_log: None,
            spawn_observer: None,
        }
    }
}

/// State shared by every clone of a [`DaemonClient`].
///
/// The connection lives here rather than in the client itself because a lost
/// connection is replaced, and the replacement has to be visible to clones
/// (`DaemonHost` holds one) instead of being thrown away after a single retry.
/// `hello` travels with the connection for the same reason: it describes the
/// handshake that is currently live.
#[derive(Debug)]
struct Shared {
    connection: RwLock<Arc<Connection>>,
    hello: RwLock<Arc<HelloResult>>,
    /// Held while dialling, so concurrent callers that lost the same connection
    /// produce one reconnect between them rather than one each.
    reconnect: AsyncMutex<()>,
    /// How many reconnect attempts have finished. A waiter records this before
    /// queueing; if it grows while the waiter is queued, the attempt it waited
    /// behind was its own, so it adopts that result instead of dialling again.
    attempts: AtomicU64,
    /// The most recent attempt's outcome and its attempt number. Shared with
    /// the waiters that were queued behind it - including failures, so a burst
    /// against a dead daemon costs one timeout, not one per caller.
    last_outcome: Mutex<Option<(u64, Result<(), ClientError>)>>,
}

/// One connection to `opencraylspd`.
///
/// Clones share the connection, so a reconnect performed by one clone is
/// adopted by the others.
///
/// # Starting the daemon
///
/// This client only promises that a *single* daemon ends up owning the socket,
/// not that `spawn` is invoked exactly once. The `<socket>.lock` file is taken
/// by the daemon itself, and a client can only probe the lock before it forks;
/// two clients racing through that window may both try, after which the loser
/// exits immediately (see `opencraylspd`'s `a_second_serve...exits quietly`). This is
/// deliberate: making `spawn` strictly once would need cross-process mutual
/// exclusion on the client side, which is not worth its cost.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    shared: Arc<Shared>,
    options: ClientOptions,
}

impl DaemonClient {
    /// Connects, starting the daemon if that is allowed and needed.
    ///
    /// Retries follow the backoff schedule: 50, 100, 200, 400, 800 and
    /// 1600 ms, repeating the last interval until `connect_deadline`.
    pub async fn connect(options: ClientOptions) -> Result<Self, ClientError> {
        let (connection, hello) = connection::connect(&options).await?;
        Ok(Self {
            shared: Arc::new(Shared {
                connection: RwLock::new(Arc::new(connection)),
                hello: RwLock::new(Arc::new(hello)),
                reconnect: AsyncMutex::new(()),
                attempts: AtomicU64::new(0),
                last_outcome: Mutex::new(None),
            }),
            options,
        })
    }

    /// The live connection, cloned out of the shared slot.
    fn connection(&self) -> Arc<Connection> {
        read_shared(&self.shared.connection).clone()
    }

    /// What the daemon said about the live connection.
    ///
    /// A reconnect replaces this with the new handshake's answer, so a caller
    /// polling after a daemon restart sees the current languages.
    pub fn hello(&self) -> Arc<HelloResult> {
        read_shared(&self.shared.hello).clone()
    }

    /// The options this client was built from.
    pub fn options(&self) -> &ClientOptions {
        &self.options
    }

    /// The languages the daemon enabled for the live connection.
    pub fn languages(&self) -> Vec<String> {
        self.hello().languages.clone()
    }

    /// The next request id. Exposed so a test can check ids are unique.
    pub fn next_request_id(&self) -> u64 {
        self.connection().next_id()
    }

    /// The daemon's status report.
    pub async fn status(&self) -> Result<StatusReport, ClientError> {
        let value = self
            .request("status", Value::Null, &CancellationToken::new())
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Asks the daemon to shut down gracefully.
    ///
    /// Not retried: "the daemon is going away" is the answer, so reconnecting
    /// and asking again would contradict the request.
    pub async fn shutdown(&self) -> Result<(), ClientError> {
        self.request_once("shutdown", Value::Null, &CancellationToken::new())
            .await?;
        Ok(())
    }

    /// The tools the daemon offers.
    pub async fn list_tools(&self) -> Result<Vec<ToolDef>, ClientError> {
        let value = self
            .request("tools/list", Value::Null, &CancellationToken::new())
            .await?;
        let list: ListResult = serde_json::from_value(value)?;
        Ok(list.tools)
    }

    /// Runs one tool.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, ClientError> {
        let params = serde_json::to_value(CallParams {
            name: name.to_owned(),
            arguments,
        })?;
        let value = self.request("tools/call", params, cancel).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Sends one idempotent request, retrying once if the connection is lost
    /// after a lost connection. Every read-only entry point - `status`, `list_tools`,
    /// `call_tool` and this generic `call` - goes through here, so recovery
    /// cannot depend on which wrapper the caller happened to use.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, ClientError> {
        self.request(method, params, cancel).await
    }

    /// The retrying request path. `hello` never comes here: a failed handshake
    /// has its own meaning and is not retried.
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, ClientError> {
        let connection = self.connection();
        match connection.request(method, params.clone(), cancel).await {
            Err(ClientError::ConnectionLost(_)) | Err(ClientError::ShuttingDown) => {
                tracing::debug!(method, "connection lost mid-request; reconnecting once");
                // A fresh handshake also re-binds the workspace, which the
                // daemon needs after it restarted. Only the retry policy and
                // the endpoint are carried over; a reconnect must not pretend
                // to be a different client.
                let connection = self.reconnect(&connection).await?;
                connection.request(method, params, cancel).await
            }
            other => other,
        }
    }

    /// One attempt on the current connection, with no recovery.
    async fn request_once(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, ClientError> {
        self.connection().request(method, params, cancel).await
    }

    /// Returns the connection to use after `stale` was found to be dead.
    ///
    /// If another caller already replaced it, that replacement is returned
    /// as-is. Otherwise exactly one caller dials while the rest wait on the
    /// reconnect mutex and adopt the result - success *or* failure, so a burst
    /// against a dead daemon costs one `connect_deadline`, not one per caller.
    /// Callers that arrive after the attempt has finished try again themselves:
    /// the daemon may have been restarted in the meantime.
    async fn reconnect(&self, stale: &Arc<Connection>) -> Result<Arc<Connection>, ClientError> {
        let current = self.connection();
        if !Arc::ptr_eq(&current, stale) {
            return Ok(current);
        }
        // How many attempts had finished before we queued. If this grows while
        // we wait, the attempt that wakes us is the one we were waiting for.
        let seen = self.shared.attempts.load(Ordering::SeqCst);
        let _dialling = self.shared.reconnect.lock().await;
        // Re-check under the lock: the caller we waited behind may have
        // finished and already published a new connection.
        let current = self.connection();
        if !Arc::ptr_eq(&current, stale) {
            return Ok(current);
        }
        if let Some((attempt, outcome)) = lock_mutex(&self.shared.last_outcome).clone()
            && attempt > seen
        {
            // Adopt the attempt we were queued behind instead of re-dialling.
            return match outcome {
                Ok(()) => Ok(self.connection()),
                Err(error) => Err(error),
            };
        }
        let result = connection::connect(&self.options).await;
        match result {
            Ok((connection, hello)) => {
                // Publish the handshake before the connection, so a reader that
                // just picked up the new connection also sees the matching hello.
                *write_shared(&self.shared.hello) = Arc::new(hello);
                *write_shared(&self.shared.connection) = Arc::new(connection);
                self.record_attempt(Ok(()));
                Ok(self.connection())
            }
            Err(error) => {
                self.record_attempt(Err(error.clone()));
                Err(error)
            }
        }
    }

    /// Publishes one attempt's outcome to the waiters queued behind it. The
    /// attempt number is what keeps a failure from being cached for good: only
    /// callers whose `seen` predates it reuse it.
    fn record_attempt(&self, outcome: Result<(), ClientError>) {
        let attempt = self.shared.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        *lock_mutex(&self.shared.last_outcome) = Some((attempt, outcome));
    }
}

/// Reads a shared slot, recovering a poisoned lock: the data is still valid,
/// only a previous writer panicked while holding it.
fn read_shared<T>(slot: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    slot.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The writing counterpart of [`read_shared`].
fn write_shared<T>(slot: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    slot.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Locks a plain mutex, recovering from poisoning like the helpers above.
fn lock_mutex<T>(slot: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A [`DaemonClient`] seen as a [`ToolHost`] by the MCP layer.
#[derive(Debug, Clone)]
pub struct DaemonHost {
    client: DaemonClient,
}

impl DaemonHost {
    pub fn new(client: DaemonClient) -> Self {
        Self { client }
    }

    /// The underlying client, for `opencraylspd status` and friends.
    pub fn client(&self) -> &DaemonClient {
        &self.client
    }
}

#[async_trait]
impl ToolHost for DaemonHost {
    /// On failure the tool list falls back to an empty catalogue so the model
    /// still sees the server is alive and can report why calls will not work.
    async fn list_tools(&self) -> Result<Vec<ToolDef>, HostError> {
        self.client.list_tools().await.map_err(HostError::from)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, HostError> {
        self.client
            .call_tool(name, arguments, cancel)
            .await
            .map_err(HostError::from)
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn default_is_the_installed_endpoint_and_spawns() {
        let options = ClientOptions::default();
        assert_eq!(
            options.socket,
            opencraylsp_proto::paths::default_socket_path()
        );
        assert!(options.spawn, "the real default may start a daemon");
        assert_eq!(options.connect_deadline, DEFAULT_CONNECT_DEADLINE);
    }

    #[test]
    fn default_for_tests_spawns_nothing_and_has_no_socket() {
        let options = ClientOptions::default_for_tests();
        assert!(options.socket.as_os_str().is_empty());
        assert!(!options.spawn);
    }

    #[test]
    fn debug_output_names_the_fields_without_leaking_a_closure() {
        let options = ClientOptions::default_for_tests();
        let text = format!("{options:?}");
        assert!(text.contains("ClientOptions"), "{text}");
        assert!(text.contains("client_name"), "{text}");
        // The observer is reported as a flag, not dumped.
        assert!(text.contains("spawn_observer"), "{text}");
    }

    #[test]
    fn the_backoff_schedule_is_the_documented_one() {
        let millis: Vec<u64> = backoff_schedule()
            .iter()
            .map(|d| d.as_millis() as u64)
            .collect();
        assert_eq!(millis, vec![50, 100, 200, 400, 800, 1600]);
    }

    #[test]
    fn a_cancelled_client_error_becomes_a_cancelled_host_error() {
        // The MCP layer answers a cancelled call with silence, so this mapping
        // is what keeps a cancelled call from being reported as a daemon
        // problem.
        let host: HostError = ClientError::Cancelled.into();
        assert!(matches!(host, HostError::Cancelled));
    }

    #[test]
    fn a_daemon_error_becomes_a_daemon_unavailable_host_error() {
        let host: HostError = ClientError::Rpc {
            code: -32602,
            message: "unknown tool".into(),
        }
        .into();
        match host {
            HostError::Unavailable(text) => {
                assert!(text.contains("[daemon_error]"), "{text}");
            }
            other => panic!("want Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_decode_failure_says_so() {
        let error =
            ClientError::from(serde_json::from_str::<serde_json::Value>("nonsense").unwrap_err());
        assert!(matches!(error, ClientError::Decode(_)), "{error:?}");
        assert!(error.to_string().contains("cannot decode"));
    }
}
