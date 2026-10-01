//! One connection to the daemon: framing, the handshake, request routing and
//! the reconnect rules.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use opencraylsp_proto::paths::current_uid;
use opencraylsp_proto::rpc::{MAX_LINE_BYTES, PROTOCOL_MISMATCH, SHUTTING_DOWN, UNKNOWN_LANGUAGE};
use opencraylsp_proto::{ClientInfo, HelloParams, HelloResult, PROTOCOL_VERSION};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{BACKOFF, ClientOptions};

/// Longest reply the client will read, matching the daemon's own limit.
const MAX_REPLY_BYTES: usize = MAX_LINE_BYTES;

/// Where a request's answer is delivered once the reader sees it.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, ClientError>>>>>;

/// Why a client could not do what it was asked to.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ClientError {
    /// The daemon could not be reached before the deadline.
    #[error(
        "[daemon_unavailable] could not reach opencraylspd at {socket} within {deadline:?}: {last_error}. \
         Try `opencraylspd status` or `opencraylspd serve`."
    )]
    Unavailable {
        socket: PathBuf,
        deadline: Duration,
        last_error: String,
    },

    /// The daemon speaks another protocol version.
    #[error(
        "[daemon_unavailable] protocol mismatch: daemon supports {supported:?}, this client speaks {client}; run `opencraylspd restart`"
    )]
    ProtocolMismatch { supported: Vec<u32>, client: u32 },

    /// `hello` named a language the daemon does not know.
    #[error("[daemon_unavailable] {input}; valid: {}", valid.join(", "))]
    UnknownLanguage { input: String, valid: Vec<String> },

    /// The daemon is going away; the caller may reconnect.
    #[error("[daemon_unavailable] the daemon is shutting down")]
    ShuttingDown,

    /// Whatever answers on the socket is not a daemon owned by this user. The
    /// client refuses to talk to it: another local user who pre-created the
    /// socket directory could otherwise read the workspace paths the client
    /// sends and feed it fabricated tool results.
    #[error(
        "[daemon_untrusted] refusing to use the daemon at {socket}: {reason}. \
         Remove the socket or point OPENCRAYLSP_SOCKET at a path you own."
    )]
    UntrustedDaemon { socket: PathBuf, reason: String },

    /// The connection dropped with the request in flight. Retried once by the
    /// caller; reaching the caller means the retry failed too.
    #[error("[daemon_unavailable] connection lost: {0}")]
    ConnectionLost(String),

    /// No answer within the client's generous bound. Single-sided failures
    /// (a stuck daemon, a handler that never returns) must not wait forever.
    #[error(
        "[timeout] opencraylspd did not answer `{method}` within {deadline:?}; the daemon may be stuck"
    )]
    Timeout { method: String, deadline: Duration },

    /// The caller cancelled the request.
    #[error("[cancelled] the request was cancelled")]
    Cancelled,

    /// The daemon answered with a JSON-RPC error.
    #[error("[daemon_error] {code}: {message}")]
    Rpc { code: i64, message: String },

    /// The daemon's answer did not fit the contract.
    #[error("[daemon_unavailable] unexpected reply from opencraylspd: {0}")]
    Protocol(String),

    /// Local I/O failed.
    #[error("[daemon_unavailable] i/o error: {0}")]
    Io(String),

    /// A reply could not be decoded into the expected type.
    #[error("[daemon_unavailable] cannot decode the reply: {0}")]
    Decode(String),
}

impl From<ClientError> for opencraylsp_proto::HostError {
    fn from(error: ClientError) -> Self {
        match error {
            // Cancellation is not a daemon problem, and the MCP layer treats it
            // differently: a cancelled call stays silent.
            ClientError::Cancelled => opencraylsp_proto::HostError::Cancelled,
            ClientError::Rpc { .. } => opencraylsp_proto::HostError::Unavailable(error.to_string()),
            other => opencraylsp_proto::HostError::Unavailable(other.to_string()),
        }
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::Decode(error.to_string())
    }
}

/// Whether the peer of a freshly connected socket may be trusted as this
/// user's daemon. Fails closed: if either side's uid cannot be determined the
/// answer is "no", because guessing would defeat the point of asking.
fn check_daemon_uid(peer: std::io::Result<u32>, me: Option<u32>) -> Result<(), String> {
    let peer = peer.map_err(|e| format!("cannot read the daemon's credentials: {e}"))?;
    let me = me.ok_or_else(|| "cannot determine the current user id".to_owned())?;
    if peer == me {
        Ok(())
    } else {
        Err(format!(
            "the socket is served by uid {peer}, not by this user (uid {me})"
        ))
    }
}

/// The reconnect-and-handshake path shared by [`DaemonClient::connect`] and the
/// retry after a lost connection.
pub(crate) async fn connect(
    options: &ClientOptions,
) -> Result<(Connection, HelloResult), ClientError> {
    let started = std::time::Instant::now();
    let deadline = started + options.connect_deadline;
    // Overwritten on the first attempt; the fallback only matters if the loop
    // somehow exits before one.
    let mut last_error;
    let mut spawned = false;
    let mut attempt = 0usize;
    // Why starting the daemon failed, when it was tried. Without it the final
    // message can only blame the socket, which hides the real cause (a missing
    // binary, a foreign lock file) from the user.
    let mut spawn_error: Option<String> = None;

    loop {
        match UnixStream::connect(&options.socket).await {
            Ok(stream) => {
                // Before a single byte is sent: the daemon must belong to this
                // user. The daemon checks its clients the same way.
                if let Err(reason) =
                    check_daemon_uid(stream.peer_cred().map(|c| c.uid()), current_uid())
                {
                    return Err(ClientError::UntrustedDaemon {
                        socket: options.socket.clone(),
                        reason,
                    });
                }
                let connection = Connection::new(stream, options.clone());
                // The handshake is inside the deadline too: a daemon that
                // accepts the socket and then says nothing must not hang the
                // client forever.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let hello = tokio::time::timeout(remaining, connection.hello()).await;
                return match hello {
                    Ok(Ok(hello)) => Ok((connection, hello)),
                    // A protocol or language problem is the caller's to fix;
                    // retrying would only produce the same answer. A lost
                    // handshake is just an unreachable daemon, which is what
                    // the message should say.
                    Ok(Err(ClientError::ConnectionLost(why))) => Err(ClientError::Unavailable {
                        socket: options.socket.clone(),
                        deadline: options.connect_deadline,
                        last_error: format!("the daemon hung up during the handshake: {why}"),
                    }),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(ClientError::Unavailable {
                        socket: options.socket.clone(),
                        deadline: options.connect_deadline,
                        last_error: "the daemon accepted the connection but never answered `hello`"
                            .to_owned(),
                    }),
                };
            }
            Err(e) => last_error = e.to_string(),
        }

        // Nothing there. If nobody owns the socket, start the daemon once.
        if !spawned && options.spawn {
            spawned = true;
            match crate::spawn::spawn_daemon(options) {
                Ok(_pid) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "could not start opencraylspd");
                    spawn_error = Some(e);
                }
            }
        }

        if std::time::Instant::now() >= deadline {
            // If we tried to start a daemon, say why it is not there: its
            // stderr is discarded, so the spawn error and its log are the only
            // clues.
            if options.spawn {
                if let Some(reason) = spawn_error.as_deref() {
                    last_error = format!("{last_error}; could not start opencraylspd: {reason}");
                }
                let log = options
                    .daemon_log
                    .clone()
                    .or_else(crate::spawn::default_log_path);
                if let Some(log) = log
                    && let Some(tail) = crate::spawn::log_tail(&log, 5)
                {
                    last_error = format!("{last_error}; daemon log {}: {tail}", log.display());
                }
            }
            return Err(ClientError::Unavailable {
                socket: options.socket.clone(),
                deadline: options.connect_deadline,
                last_error,
            });
        }
        let wait = BACKOFF[attempt.min(BACKOFF.len() - 1)];
        attempt += 1;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        tokio::time::sleep(wait.min(remaining)).await;
    }
}

/// A live connection: one reader task, a serialized writer, and a table of
/// requests waiting for answers.
#[derive(Debug)]
pub struct Connection {
    writer: mpsc::Sender<String>,
    next_id: AtomicU64,
    inflight: Pending,
    dead: Arc<AtomicBool>,
    /// Kept for the handshake, which is the only thing that needs them.
    options: ClientOptions,
}

impl Connection {
    /// Wraps `stream` and starts the reader.
    pub(crate) fn new(stream: UnixStream, options: ClientOptions) -> Self {
        let (read_half, write_half) = stream.into_split();
        let (tx, rx) = mpsc::channel::<String>(64);
        let inflight: Pending = Arc::default();
        let dead = Arc::new(AtomicBool::new(false));

        tokio::spawn(write_loop(write_half, rx));
        tokio::spawn(read_loop(
            BufReader::new(read_half),
            inflight.clone(),
            dead.clone(),
        ));

        Self {
            writer: tx,
            next_id: AtomicU64::new(1),
            inflight,
            dead,
            options,
        }
    }

    /// The id the next request will use.
    pub fn next_id(&self) -> u64 {
        self.next_id.load(Ordering::SeqCst)
    }

    /// Performs the `hello` handshake and returns what the daemon said.
    pub(crate) async fn hello(&self) -> Result<HelloResult, ClientError> {
        let params = HelloParams {
            protocol: PROTOCOL_VERSION,
            client: ClientInfo {
                name: self.options.client_name.clone(),
                version: self.options.client_version.clone(),
            },
            workspace: self.options.workspace.to_string_lossy().into_owned(),
            // Sent exactly as the user typed it: the daemon owns normalization
            // and validation.
            languages: self.options.languages.clone(),
        };
        let value = self
            .request(
                "hello",
                serde_json::to_value(params)?,
                &CancellationToken::new(),
            )
            .await?;
        let claimed = value.get("protocol").and_then(Value::as_u64);
        let hello: HelloResult = serde_json::from_value(value)
            .map_err(|e| ClientError::Protocol(format!("bad hello result: {e}")))?;
        // `HelloResult` will happily decode `protocol: 99`, so the version is
        // checked explicitly: a daemon that answers "success" while speaking
        // another revision is a mismatch, not a decoding curiosity.
        if hello.protocol != PROTOCOL_VERSION {
            return Err(ClientError::ProtocolMismatch {
                supported: vec![hello.protocol],
                client: PROTOCOL_VERSION,
            });
        }
        let _ = claimed;
        Ok(hello)
    }

    /// Sends one request and waits for its answer.
    ///
    /// Cancellation answers immediately with [`ClientError::Cancelled`] after
    /// telling the daemon to stop working on it; a late reply is discarded.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, ClientError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(ClientError::ConnectionLost(
                "the connection is closed".into(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inflight.lock().await.insert(id, tx);

        let line =
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        if self.writer.send(format!("{line}\n")).await.is_err() {
            self.forget(id).await;
            return Err(ClientError::ConnectionLost(
                "the daemon stopped reading".into(),
            ));
        }

        let deadline = self.options.request_deadline;
        let answer = tokio::time::timeout(deadline, async {
            tokio::select! {
                answer = rx => {
                    self.forget(id).await;
                    match answer {
                        Ok(answer) => answer,
                        // The reader dropped us: it saw the connection die.
                        Err(_) => Err(ClientError::ConnectionLost(
                            "the daemon closed the connection".into(),
                        )),
                    }
                }
                () = cancel.cancelled() => {
                    self.forget(id).await;
                    let _ = self.notify_cancel(id).await;
                    Err(ClientError::Cancelled)
                }
            }
        })
        .await;
        match answer {
            Ok(result) => result,
            // A single-sided failure (a stuck daemon, a handler that never
            // returns) must not become an infinite wait. Drop the waiter and
            // tell the daemon to stop; any late reply is discarded.
            Err(_) => {
                self.forget(id).await;
                let _ = self.notify_cancel(id).await;
                Err(ClientError::Timeout {
                    method: method.to_owned(),
                    deadline,
                })
            }
        }
    }

    /// Tells the daemon to abandon a request we no longer care about.
    async fn notify_cancel(&self, id: u64) -> Result<(), ClientError> {
        let line =
            json!({"jsonrpc": "2.0", "method": "$/cancel", "params": {"id": id}}).to_string();
        self.writer
            .send(format!("{line}\n"))
            .await
            .map_err(|_| ClientError::ConnectionLost("the daemon stopped reading".into()))
    }

    async fn forget(&self, id: u64) {
        // A poisoned map only costs us a pending entry, so recover the guard.
        self.inflight.lock().await.remove(&id);
    }
}

async fn write_loop(mut write: OwnedWriteHalf, mut rx: mpsc::Receiver<String>) {
    while let Some(line) = rx.recv().await {
        if write.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        let _ = write.flush().await;
    }
    let _ = write.shutdown().await;
}

async fn read_loop(reader: BufReader<OwnedReadHalf>, inflight: Pending, dead: Arc<AtomicBool>) {
    let mut reader = ReplyReader::new(reader);
    loop {
        let line = match reader.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            // Anything unreadable ends the connection; the caller's request is
            // failed by the dropped senders below.
            Err(_) => break,
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            // A line that is not JSON means the peer is not the daemon we
            // think it is, or something is splicing into the socket (F11).
            // Refusing to keep reading is the safe reading of that.
            tracing::warn!("the daemon sent a line that is not JSON; dropping the connection");
            break;
        };
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let Some(tx) = inflight.lock().await.remove(&id) else {
            // A reply for a request we already gave up on (cancelled, retried).
            continue;
        };
        let answer = if let Some(error) = value.get("error") {
            Err(rpc_error(error))
        } else {
            match value.get("result") {
                Some(result) => Ok(result.clone()),
                None => Err(ClientError::Protocol(
                    "reply has neither result nor error".into(),
                )),
            }
        };
        let _ = tx.send(answer);
    }
    dead.store(true, Ordering::SeqCst);
    inflight.lock().await.clear();
}

/// Reads newline-delimited replies, keeping whatever a read returned past the
/// first newline.
///
/// A plain `read_until` would grow without bound on a hostile peer, and reading
/// a chunk and returning at the first newline - the obvious approach - silently
/// drops every reply that arrived in the same chunk. Since responses are
/// allowed to share a read, that loses answers under concurrency, so the tail
/// is carried over to the next call.
struct ReplyReader<R> {
    inner: R,
    pending: Vec<u8>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> ReplyReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            eof: false,
        }
    }

    /// The next complete line, or `None` at end of stream. An oversized line
    /// is refused instead of buffered.
    async fn next_line(&mut self) -> Result<Option<String>, ()> {
        let mut scanned = 0usize;
        loop {
            if let Some(index) = self.pending[scanned..].iter().position(|b| *b == b'\n') {
                let end = scanned + index;
                if end > MAX_REPLY_BYTES {
                    return Err(());
                }
                let line = self.pending[..end].to_vec();
                self.pending.drain(..=end);
                return String::from_utf8(line).map(Some).map_err(|_| ());
            }
            scanned = self.pending.len();
            if scanned > MAX_REPLY_BYTES {
                return Err(());
            }
            if self.eof {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.pending);
                return String::from_utf8(line).map(Some).map_err(|_| ());
            }
            let mut chunk = [0u8; 8 * 1024];
            let n = self.inner.read(&mut chunk).await.map_err(|_| ())?;
            if n == 0 {
                self.eof = true;
            } else {
                self.pending.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

fn rpc_error(error: &Value) -> ClientError {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_owned();
    match code {
        PROTOCOL_MISMATCH => ClientError::ProtocolMismatch {
            supported: error
                .get("data")
                .and_then(|d| d.get("supported"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_u64)
                        .map(|v| v as u32)
                        .collect()
                })
                .unwrap_or_default(),
            client: PROTOCOL_VERSION,
        },
        UNKNOWN_LANGUAGE => {
            let valid = error
                .get("data")
                .and_then(|d| d.get("valid"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            ClientError::UnknownLanguage {
                input: message,
                valid,
            }
        }
        SHUTTING_DOWN => ClientError::ShuttingDown,
        other => ClientError::Rpc {
            code: other,
            message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::check_daemon_uid;
    use std::io;

    #[test]
    fn a_daemon_owned_by_this_user_is_trusted() {
        assert!(check_daemon_uid(Ok(1000), Some(1000)).is_ok());
    }

    #[test]
    fn a_daemon_owned_by_someone_else_is_refused() {
        let reason = check_daemon_uid(Ok(1001), Some(1000)).unwrap_err();
        assert!(reason.contains("uid 1001"), "{reason}");
        assert!(reason.contains("uid 1000"), "{reason}");
    }

    #[test]
    fn root_is_not_a_wildcard() {
        assert!(check_daemon_uid(Ok(0), Some(1000)).is_err());
        assert!(check_daemon_uid(Ok(1000), Some(0)).is_err());
    }

    #[test]
    fn unknown_credentials_fail_closed() {
        assert!(check_daemon_uid(Err(io::Error::other("no cred")), Some(1000)).is_err());
        assert!(check_daemon_uid(Ok(1000), None).is_err());
    }
}
