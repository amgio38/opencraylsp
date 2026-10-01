//! One client connection: line framing, JSON-RPC dispatch, cancellation.
//!
//! Wire format : one JSON-RPC 2.0 message per line. Requests on a
//! connection run concurrently and are answered in completion order; the `id`
//! ties a response to its request. Dropping the connection cancels everything
//! it still had in flight, but never the language servers, which other
//! connections may be using.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use opencraylsp_core::languages;
use opencraylsp_core::{BoundBackend, Pool};
use opencraylsp_proto::rpc::{
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, MAX_LINE_BYTES, METHOD_NOT_FOUND,
    NOT_INITIALIZED, PARSE_ERROR, PROTOCOL_MISMATCH, Request, Response, SHUTTING_DOWN,
    UNKNOWN_LANGUAGE, WORKSPACE_INVALID,
};
use opencraylsp_proto::{
    CallParams, HelloParams, HelloResult, ListResult, PROTOCOL_VERSION, ToolOutput,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::runner::ToolRunner;

/// Consecutive unparseable lines after which the connection is dropped.
const MAX_CONSECUTIVE_BAD_LINES: u32 = 3;

/// Responses queued per connection before producers wait for the writer.
const OUTBOX_DEPTH: usize = 256;

/// Most tool calls one connection may have in flight at once.
///
/// Every request spawns a task that runs a real language server query, so an
/// unbounded number of them is an unbounded number of concurrent index passes,
/// reads and child-process round trips. One connection issuing thousands of
/// calls therefore costs the *other* connections latency and memory, and the
/// pool's own caps (`max_instances`, `max_rss_mb`) do not bound it: those count
/// instances, not requests. Refusing with an error is honest and cheap — the
/// caller can retry — whereas queueing would hide the overload.
pub(crate) const MAX_INFLIGHT_PER_CONNECTION: usize = 64;

/// State shared by every connection of one daemon.
pub struct ServerState {
    pub pool: Arc<Pool>,
    pub runner: Arc<dyn ToolRunner>,
    /// Cancelled to begin shutdown: stops accepting and cancels every
    /// connection's in-flight requests (each connection token is a child).
    pub shutdown: CancellationToken,
    pub shutting_down: AtomicBool,
    /// Only peers with this uid may talk to the daemon.
    pub expected_uid: u32,
}

impl ServerState {
    /// Marks the daemon as shutting down and starts the shutdown sequence.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown.cancel();
    }
}

impl std::fmt::Debug for ServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerState")
            .field("expected_uid", &self.expected_uid)
            .finish()
    }
}

/// Whether a peer with `peer_uid` may connect to a daemon run by `expected`.
pub fn authorize(peer_uid: u32, expected: u32) -> bool {
    peer_uid == expected
}

/// One framed line, or why there is none.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Text(Vec<u8>),
    /// The line exceeded [`MAX_LINE_BYTES`] before its newline arrived.
    TooLong,
    Eof,
}

/// Reads one `\n`-terminated line without ever buffering more than `max`
/// bytes of it. A final line lacking a newline is still returned.
async fn read_line<R: AsyncBufReadExt + Unpin>(reader: &mut R, max: usize) -> io::Result<Line> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                Line::Eof
            } else {
                Line::Text(line)
            });
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let take = newline.unwrap_or(available.len());
        if line.len() + take > max {
            return Ok(Line::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        let consumed = newline.map_or(take, |i| i + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Line::Text(line));
        }
    }
}

/// Serves one accepted connection to completion.
pub async fn handle_connection(stream: UnixStream, state: Arc<ServerState>) {
    match stream.peer_cred() {
        Ok(cred) if authorize(cred.uid(), state.expected_uid) => {}
        Ok(cred) => {
            tracing::warn!(
                peer_uid = cred.uid(),
                "rejected a connection from another user"
            );
            return;
        }
        Err(err) => {
            tracing::warn!(error = %err, "cannot read peer credentials; rejecting");
            return;
        }
    }
    let (read_half, write_half) = stream.into_split();
    let token = state.shutdown.child_token();
    let (tx, rx) = mpsc::channel::<Response>(OUTBOX_DEPTH);
    let writer = tokio::spawn(write_loop(write_half, rx));
    let mut session = Session {
        state,
        tx,
        token: token.clone(),
        backend: None,
        inflight: Arc::default(),
        bad_lines: 0,
        close: false,
    };
    session.run(BufReader::new(read_half)).await;
    // Whatever this connection still had running is now pointless.
    token.cancel();
    drop(session);
    let _ = writer.await;
}

/// Encodes one response, or explains why it could not.
///
/// Split out so the failure branch can be tested directly: every value this
/// daemon builds happens to encode, so the encoding-failure path is unreachable through the
/// real types and would otherwise never be exercised.
#[cfg_attr(not(test), inline)]
pub(crate) fn encode_response(response: &Response) -> Result<Vec<u8>, serde_json::Error> {
    // While the crate is under test, one response in the queue is answered with
    // a value `serde_json` cannot encode, so the fallback below is reachable
    // from a test. Production always takes the real encoder.
    #[cfg(test)]
    if RESPONSE_THAT_CANNOT_ENCODE.with(std::cell::Cell::take) {
        return Err(serde::ser::Error::custom("cannot encode this answer"));
    }
    serde_json::to_vec(response)
}

#[cfg(test)]
thread_local! {
    static RESPONSE_THAT_CANNOT_ENCODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test hook: make the next encoded response fail.
#[cfg(test)]
pub(crate) fn fail_next_encoding() {
    RESPONSE_THAT_CANNOT_ENCODE.with(|flag| flag.set(true));
}

/// Writes queued responses until every sender is gone, so a response queued
/// just before the connection ended (the `shutdown` reply, say) still goes out.
///
/// `pub(crate)` so the encoding-failure path can be driven directly over
/// a socket pair from the crate's own tests.
pub(crate) async fn write_loop(mut write: OwnedWriteHalf, mut rx: mpsc::Receiver<Response>) {
    while let Some(response) = rx.recv().await {
        // A response that will not serialize must not vanish. It is
        // replaced with an error frame the caller can read, because silently
        // dropping it leaves a request with no answer and no explanation —
        // the client waits out its whole deadline for something that was
        // already lost. Serialization of our own response types does not fail
        // in practice, so reaching this is itself a bug worth surfacing.
        let mut line = match encode_response(&response) {
            Ok(line) => line,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "a response could not be serialized; answering with an error instead"
                );
                let mut fallback = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": response.id,
                    "error": {
                        "code": INTERNAL_ERROR,
                        "message": format!("the daemon could not encode its answer: {err}"),
                    },
                })
                .to_string()
                .into_bytes();
                fallback.push(b'\n');
                if write.write_all(&fallback).await.is_err() {
                    break;
                }
                continue;
            }
        };
        line.push(b'\n');
        if write.write_all(&line).await.is_err() {
            break;
        }
    }
    let _ = write.shutdown().await;
}

struct Session {
    state: Arc<ServerState>,
    tx: mpsc::Sender<Response>,
    token: CancellationToken,
    /// Set by a successful `hello`.
    backend: Option<Arc<BoundBackend>>,
    inflight: Arc<std::sync::Mutex<HashMap<String, CancellationToken>>>,
    bad_lines: u32,
    close: bool,
}

impl Session {
    async fn run(&mut self, mut reader: BufReader<OwnedReadHalf>) {
        loop {
            let line = tokio::select! {
                line = read_line(&mut reader, MAX_LINE_BYTES) => line,
                () = self.token.cancelled() => return,
            };
            match line {
                Ok(Line::Text(bytes)) => {
                    if bytes.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    self.handle_line(&bytes).await;
                    if self.close {
                        return;
                    }
                }
                Ok(Line::TooLong) => {
                    self.send(Response::err(
                        Value::Null,
                        PARSE_ERROR,
                        format!("line exceeds {MAX_LINE_BYTES} bytes"),
                        None,
                    ))
                    .await;
                    return;
                }
                Ok(Line::Eof) | Err(_) => return,
            }
        }
    }

    async fn send(&self, response: Response) {
        let _ = self.tx.send(response).await;
    }

    async fn bad_line(&mut self, code: i64, message: String) {
        self.bad_lines += 1;
        self.send(Response::err(Value::Null, code, message, None))
            .await;
        if self.bad_lines >= MAX_CONSECUTIVE_BAD_LINES {
            self.close = true;
        }
    }

    async fn handle_line(&mut self, bytes: &[u8]) {
        let value: Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(err) => {
                return self
                    .bad_line(PARSE_ERROR, format!("parse error: {err}"))
                    .await;
            }
        };
        let request: Request = match serde_json::from_value::<Request>(value) {
            Ok(request) if request.jsonrpc == "2.0" => request,
            Ok(_) => {
                return self
                    .bad_line(INVALID_REQUEST, "jsonrpc must be \"2.0\"".to_owned())
                    .await;
            }
            Err(err) => {
                return self
                    .bad_line(INVALID_REQUEST, format!("invalid request: {err}"))
                    .await;
            }
        };
        self.bad_lines = 0;
        match request.id.clone() {
            None => self.notification(&request),
            Some(id) => self.request(id, request).await,
        }
    }

    fn notification(&self, request: &Request) {
        if request.method != "$/cancel" {
            return;
        }
        let Some(id) = request.params.get("id") else {
            return;
        };
        let inflight = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(token) = inflight.get(&id.to_string()) {
            token.cancel();
        }
    }

    async fn request(&mut self, id: Value, request: Request) {
        if self.state.shutting_down.load(Ordering::SeqCst) {
            return self
                .send(Response::err(
                    id,
                    SHUTTING_DOWN,
                    "the daemon is shutting down",
                    None,
                ))
                .await;
        }
        if request.method == "hello" {
            return self.hello(id, request.params).await;
        }
        let Some(backend) = self.backend.clone() else {
            return self
                .send(Response::err(
                    id,
                    NOT_INITIALIZED,
                    "send `hello` before any other request",
                    None,
                ))
                .await;
        };
        match request.method.as_str() {
            "tools/list" => {
                let tools = self.state.runner.defs();
                self.send(Response::ok(id, json!(ListResult { tools })))
                    .await;
            }
            "tools/call" => self.tools_call(id, request.params, backend).await,
            "status" => {
                use opencraylsp_core::LspBackend;
                let report = backend.status().await;
                self.send(Response::ok(id, json!(report))).await;
            }
            "shutdown" => {
                self.send(Response::ok(id, json!({}))).await;
                self.state.begin_shutdown();
            }
            other => {
                self.send(Response::err(
                    id,
                    METHOD_NOT_FOUND,
                    format!("unknown method `{other}`"),
                    None,
                ))
                .await;
            }
        }
    }

    async fn hello(&mut self, id: Value, params: Value) {
        if self.backend.is_some() {
            return self
                .send(Response::err(
                    id,
                    INVALID_REQUEST,
                    "`hello` was already completed",
                    None,
                ))
                .await;
        }
        let params: HelloParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(err) => {
                return self
                    .send(Response::err(
                        id,
                        INVALID_PARAMS,
                        format!("invalid hello: {err}"),
                        None,
                    ))
                    .await;
            }
        };
        if params.protocol != PROTOCOL_VERSION {
            return self
                .send(Response::err(
                    id,
                    PROTOCOL_MISMATCH,
                    format!(
                        "protocol mismatch: daemon speaks {PROTOCOL_VERSION}, client speaks {}",
                        params.protocol
                    ),
                    Some(json!({"supported": [PROTOCOL_VERSION]})),
                ))
                .await;
        }
        let workspace = std::path::PathBuf::from(&params.workspace);
        if !workspace.is_absolute() || !workspace.is_dir() {
            return self
                .send(Response::err(
                    id,
                    WORKSPACE_INVALID,
                    format!(
                        "workspace `{}` is not an existing absolute directory",
                        params.workspace
                    ),
                    None,
                ))
                .await;
        }
        // The boundary is what every later file check is made against, so
        // the daemon resolves it itself rather than storing a caller-supplied
        // form of it — and refuses one it cannot resolve, instead of binding a
        // backend whose boundary is whatever a callee later makes of it.
        //
        // Honest scope: `Pool::bind` already canonicalizes, so this does not
        // change which files are reachable. What it adds is a resolution failure
        // being reported *here*, by `hello`, with a message naming the path,
        // rather than being swallowed further down. Kept as defence in depth
        // rather than as a behavioural fix, and deliberately not sold as one.
        let workspace = match std::fs::canonicalize(&workspace) {
            Ok(canonical) => canonical,
            Err(err) => {
                return self
                    .send(Response::err(
                        id,
                        WORKSPACE_INVALID,
                        format!("cannot resolve workspace `{}`: {err}", params.workspace),
                        None,
                    ))
                    .await;
            }
        };
        let known = languages::known_languages(&self.state.pool.config().servers);
        let selection = match languages::normalize(params.languages.as_deref(), &known) {
            Ok(selection) => selection,
            Err(err) => {
                return self
                    .send(Response::err(
                        id,
                        UNKNOWN_LANGUAGE,
                        err.to_string(),
                        Some(json!({"valid": err.valid})),
                    ))
                    .await;
            }
        };
        let backend = self.state.pool.bind(workspace, selection);
        let result = HelloResult {
            protocol: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            languages: backend.enabled().set.iter().cloned().collect(),
            language_mode: backend.enabled().mode,
        };
        self.backend = Some(backend);
        self.send(Response::ok(id, json!(result))).await;
    }

    async fn tools_call(&mut self, id: Value, params: Value, backend: Arc<BoundBackend>) {
        let call: CallParams = match serde_json::from_value(params) {
            Ok(call) => call,
            Err(err) => {
                return self
                    .send(Response::err(
                        id,
                        INVALID_PARAMS,
                        format!("invalid call: {err}"),
                        None,
                    ))
                    .await;
            }
        };
        let key = id.to_string();
        // Refuse rather than queue once too many calls are running. The
        // check and the insert happen under one lock so two requests arriving
        // together cannot both see a free slot. The guard is dropped before the
        // reply is sent — it must not be held across an `.await`, and the
        // error path needs `&mut self`.
        let token = self.token.child_token();
        let overloaded = {
            let mut inflight = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
            if inflight.len() >= MAX_INFLIGHT_PER_CONNECTION {
                true
            } else {
                inflight.insert(key.clone(), token.clone());
                false
            }
        };
        if overloaded {
            return self
                .send(Response::err(
                    id,
                    INVALID_PARAMS,
                    format!(
                        "too many requests in flight (limit {MAX_INFLIGHT_PER_CONNECTION}); retry shortly"
                    ),
                    None,
                ))
                .await;
        }
        let tx = self.tx.clone();
        let inflight = self.inflight.clone();
        let runner = self.state.runner.clone();
        tokio::spawn(async move {
            // The handler runs in its own task so a panic surfaces as a
            // `JoinError` here instead of silently killing the responder: the
            // caller always gets an answer and the in-flight entry is always
            // removed.
            let name = call.name.clone();
            let handler = tokio::spawn(async move {
                runner
                    .call(&*backend, &call.name, call.arguments, &token)
                    .await
            });
            let output = match handler.await {
                Ok(output) => output,
                Err(err) => {
                    tracing::error!(tool = %name, error = %err, "tool handler failed");
                    ToolOutput::error(format!(
                        "[internal_error] the `{name}` handler failed unexpectedly ({}); \
                         the daemon is still running, see its log for details",
                        if err.is_panic() { "panic" } else { "aborted" }
                    ))
                }
            };
            inflight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&key);
            let _ = tx.send(Response::ok(id, json!(output))).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader as TokioBufReader;

    /// A response that cannot be encoded is answered with an error frame, and
    /// the responses queued behind it still go out.
    #[tokio::test]
    async fn a_response_that_cannot_be_encoded_still_answers() {
        use tokio::io::AsyncBufReadExt;
        let (server, client) = tokio::net::UnixStream::pair().expect("pair");
        let (_server_read, server_write) = server.into_split();
        let (tx, rx) = mpsc::channel(4);
        tx.send(Response::ok(
            serde_json::json!(1),
            serde_json::json!("lost"),
        ))
        .await
        .expect("queue");
        tx.send(Response::ok(
            serde_json::json!(2),
            serde_json::json!("kept"),
        ))
        .await
        .expect("queue");
        drop(tx);
        // The flag is thread-local and `#[tokio::test]` runs on this thread, so it
        // applies to the first response `write_loop` encodes.
        fail_next_encoding();
        write_loop(server_write, rx).await;

        let mut lines = tokio::io::BufReader::new(client).lines();
        let first: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().expect("first frame"))
                .expect("json");
        assert_eq!(first["id"], 1, "{first}");
        assert_eq!(first["error"]["code"], INTERNAL_ERROR, "{first}");
        let second: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().expect("second frame"))
                .expect("json");
        assert_eq!(second["result"], "kept", "{second}");
    }

    async fn line_of(input: &[u8], max: usize) -> Line {
        let mut reader = TokioBufReader::new(input);
        read_line(&mut reader, max).await.unwrap()
    }

    #[tokio::test]
    async fn read_line_splits_on_newlines_and_reports_eof() {
        let mut reader = TokioBufReader::new(&b"one\ntwo\n"[..]);
        assert_eq!(
            read_line(&mut reader, 100).await.unwrap(),
            Line::Text(b"one".to_vec())
        );
        assert_eq!(
            read_line(&mut reader, 100).await.unwrap(),
            Line::Text(b"two".to_vec())
        );
        assert_eq!(read_line(&mut reader, 100).await.unwrap(), Line::Eof);
    }

    #[tokio::test]
    async fn read_line_returns_a_final_line_without_a_newline() {
        assert_eq!(line_of(b"tail", 100).await, Line::Text(b"tail".to_vec()));
        assert_eq!(line_of(b"", 100).await, Line::Eof);
    }

    #[tokio::test]
    async fn read_line_refuses_to_buffer_past_the_limit() {
        assert_eq!(line_of(b"0123456789\n", 5).await, Line::TooLong);
        assert_eq!(line_of(b"0123456789", 5).await, Line::TooLong);
        assert_eq!(line_of(b"01234\n", 5).await, Line::Text(b"01234".to_vec()));
    }

    #[tokio::test]
    async fn read_line_limit_holds_across_buffer_refills() {
        // A tiny reader buffer forces the line to arrive in many chunks.
        let data = [b'x'; 64];
        let mut reader = TokioBufReader::with_capacity(8, &data[..]);
        assert_eq!(read_line(&mut reader, 40).await.unwrap(), Line::TooLong);
        let mut short = TokioBufReader::with_capacity(8, &b"abcdefghijklmnop\n"[..]);
        assert_eq!(
            read_line(&mut short, 40).await.unwrap(),
            Line::Text(b"abcdefghijklmnop".to_vec())
        );
    }

    #[test]
    fn only_the_daemons_own_uid_is_authorized() {
        assert!(authorize(1000, 1000));
        assert!(!authorize(0, 1000));
        assert!(!authorize(1001, 1000));
    }
}
