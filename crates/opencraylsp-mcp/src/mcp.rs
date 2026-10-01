//! MCP stdio protocol layer.
//!
//! A hand-written, tools-only MCP server: one JSON-RPC 2.0 message per line on
//! stdin/stdout, no SDK. Three rules shape everything here:
//!
//! * **stdout is the protocol.** Nothing but complete, newline-delimited
//!   JSON-RPC messages is ever written there, so all replies funnel through one
//!   writer task and logging goes to stderr via `tracing`.
//! * **protocol errors vs tool errors.** A broken envelope is a JSON-RPC
//!   error; a tool that ran and failed (or a host that could not be reached at
//!   all) is a *successful* result carrying `isError: true`.
//! * **cancellation is silent.** Once `notifications/cancelled` names a
//!   request, that request must never produce a reply, no matter how late the
//!   host finishes (MCP cancellation spec, requirement 3).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opencraylsp_proto::{HostError, ToolHost, ToolOutput};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Name reported in `serverInfo`.
pub const SERVER_NAME: &str = "opencraylsp-mcp";

/// Newest MCP revision we speak; also the fallback for unknown requests.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

/// Revisions we can speak. A client asking for one of these gets it verbatim.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Largest stdin line we will accept, in bytes. Longer lines are refused
/// without being buffered.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Longest a single call into the tool host may take before it is answered with
/// a timeout instead of being left to hang.
///
/// The host is a language server, and a language server can be arbitrarily slow:
/// a cold index, a first request for a crate that is being built, a server that
/// has wedged. Without a deadline of its own, such a call runs until the
/// process is killed, and the caller — an agent that has already moved on — gets
/// nothing at all.
///
/// Bounding it here is also what makes the EOF grace below provable rather than
/// hopeful: every in-flight request now finishes on its own, so a grace that
/// outlasts this deadline cannot cut a reply off.
pub const TOOL_CALL_DEADLINE: Duration = Duration::from_secs(120);

/// How much longer than [`TOOL_CALL_DEADLINE`] in-flight work may run after
/// stdin reaches EOF.
///
/// This has to be a strict margin, and it is the whole point of the constant. An
/// earlier version set the grace to exactly the client's connect deadline; the
/// two timers then expired at the same instant, so a call that answered one
/// millisecond late had its reply suppressed by `cancel_all` and the caller
/// never heard back at all. Tying the grace to a *request's* deadline plus a
/// slack — instead of to some unrelated timer that happens to be the same size —
/// makes that race impossible rather than merely unlikely.
pub const EOF_SLACK: Duration = Duration::from_secs(5);

/// How long in-flight calls may run after stdin EOF before they are cancelled,
/// for a server whose requests are bounded by `deadline`.
///
/// The grace must outlast `deadline`, or a request can be cancelled with its
/// answer in hand — which is the bug this replaces. Building the grace from the
/// deadline is what makes that impossible instead of merely unlikely, and it
/// means no caller can state the two numbers independently and get the race
/// back.
pub fn grace_for(deadline: Duration) -> Duration {
    deadline + EOF_SLACK
}

/// The default grace for every backend: [`grace_for`]`(TOOL_CALL_DEADLINE)`.
///
/// `initialize`, `ping` and the notifications do no host I/O, so
/// [`TOOL_CALL_DEADLINE`] bounds everything the grace has to outlast.
pub fn default_eof_grace() -> Duration {
    grace_for(TOOL_CALL_DEADLINE)
}

/// How long a cancelled call may take to unwind after stdin reaches EOF.
pub const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// Model-facing guidance, sent in `initialize.instructions`.
pub const SERVER_INSTRUCTIONS: &str = "\
Use the lsp_* tools for semantic code questions: where a symbol is defined, \
who calls it, its type, or what the compiler complains about. They are \
type-aware and cross-file, so they beat grep for names, types and call \
relationships. Target symbols by name (\"parse_config\"), not line:column. \
lsp_status shows which servers run and what they are doing. If a result \
starts with [indexing] the server is still indexing: wait a few seconds and \
retry instead of concluding the symbol does not exist. All tools are read-only.";

// JSON-RPC / MCP error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const NOT_INITIALIZED: i64 = -32002;

/// The MCP stdio server: one [`ToolHost`] exposed over stdin/stdout.
pub struct McpServer {
    host: Arc<dyn ToolHost>,
    version: String,
    /// Grace for in-flight calls after stdin EOF; see [`Self::with_eof_grace`].
    eof_grace: Duration,
    /// Ceiling on one call into the host; see [`TOOL_CALL_DEADLINE`].
    call_deadline: Duration,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl McpServer {
    /// `version` is reported verbatim in `serverInfo`.
    pub fn new(host: Arc<dyn ToolHost>, version: impl Into<String>) -> Self {
        Self {
            host,
            version: version.into(),
            eof_grace: default_eof_grace(),
            call_deadline: TOOL_CALL_DEADLINE,
        }
    }

    /// The ceiling on one call into the tool host.
    ///
    /// Kept in step with the EOF grace on purpose: [`Self::with_eof_grace`]
    /// must outlast this, or a request can be cancelled with its answer in
    /// hand. Tests shorten both together.
    pub fn with_call_deadline(mut self, deadline: Duration) -> Self {
        self.call_deadline = deadline;
        self
    }

    /// How long in-flight calls may run after stdin EOF before being cancelled.
    ///
    /// Must be at least [`TOOL_CALL_DEADLINE`], otherwise a slow-but-alive call
    /// is cancelled instead of answered. [`default_eof_grace`] does that by
    /// construction; prefer it over naming a number here.
    pub fn with_eof_grace(mut self, grace: Duration) -> Self {
        self.eof_grace = grace;
        self
    }

    /// The grace this server will actually use.
    pub fn eof_grace(&self) -> Duration {
        self.eof_grace
    }

    /// The ceiling this server puts on one call into the host.
    pub fn call_deadline(&self) -> Duration {
        self.call_deadline
    }

    /// Serves until stdin reaches EOF, then returns. `Ok(())` means a clean
    /// shutdown: every in-flight call was cancelled and awaited.
    pub async fn serve<R, W>(&self, input: R, output: W) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let state = Arc::new(State::default());

        // One writer, fed by a channel: replies can never interleave.
        let (tx, mut replies) = mpsc::channel::<String>(256);
        let writer: tokio::task::JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            let mut writer = output;
            while let Some(line) = replies.recv().await {
                writer.write_all(line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
            Ok(())
        });

        let mut tasks = tokio::task::JoinSet::new();
        let mut reader = LineReader::new(input);

        loop {
            match reader.next_line().await? {
                ReadOutcome::Eof => break,
                ReadOutcome::Empty => continue,
                ReadOutcome::TooLong => {
                    send(
                        &tx,
                        &error_response(Value::Null, INVALID_REQUEST, "line too long"),
                    )
                    .await;
                    continue;
                }
                ReadOutcome::Line(bytes) => match parse_message(&bytes) {
                    Ok(Message::Notification { method, params }) => {
                        self.handle_notification(&method, params, &state).await;
                    }
                    Ok(Message::Request { id, method, params }) => {
                        // Register before spawning: a cancellation naming this
                        // id may be handled by the reader loop before the task
                        // ever runs, and a token registered inside the task
                        // would arrive too late to stop it.
                        let token = CancellationToken::new();
                        state.register(&id, token.clone());
                        let state = state.clone();
                        let tx = tx.clone();
                        let host = self.host.clone();
                        let version = self.version.clone();
                        let call_deadline = self.call_deadline;
                        tasks.spawn(async move {
                            Request {
                                id,
                                method,
                                params,
                                host,
                                state,
                                token,
                                tx,
                                version,
                                call_deadline,
                            }
                            .run()
                            .await;
                        });
                    }
                    Err(fault) => {
                        let response = match fault {
                            MessageFault::Unparsable => {
                                error_response(Value::Null, PARSE_ERROR, "parse error")
                            }
                            MessageFault::Batch => error_response(
                                Value::Null,
                                INVALID_REQUEST,
                                "batching is not supported",
                            ),
                            MessageFault::Invalid(id) => {
                                error_response(id, INVALID_REQUEST, "invalid request")
                            }
                        };
                        send(&tx, &response).await;
                    }
                },
            }
        }

        // EOF: let queued work finish, then cancel whatever is genuinely
        // stuck. Cancelling first would throw away replies the client asked
        // for and never got - a harness that pipes a transcript and closes
        // stdin would see nothing at all.
        drain(&mut tasks, self.eof_grace).await;
        state.cancel_all();
        drain(&mut tasks, CANCEL_GRACE).await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop(tx);
        let _ = writer.await;
        Ok(())
    }

    async fn handle_notification(&self, method: &str, params: Value, state: &Arc<State>) {
        match method {
            "notifications/initialized" => state.initialized.store(true, Ordering::SeqCst),
            "notifications/cancelled" => {
                if let Some(token) = params.get("requestId").and_then(|id| state.token_for(id)) {
                    let reason = params["reason"].as_str().unwrap_or_default();
                    tracing::debug!(reason, "cancelling request");
                    token.cancel();
                }
            }
            // Unknown notifications are fire-and-forget: ignore them (MCP).
            _ => tracing::debug!(method, "ignoring unknown notification"),
        }
    }
}

/// Awaits every spawned request, giving up after `limit`.
async fn drain(tasks: &mut tokio::task::JoinSet<()>, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while !tasks.is_empty() {
        if tokio::time::timeout_at(deadline, tasks.join_next())
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Connection state: initialization flag, in-flight cancellation table, and
/// the tool names seen on the first successful listing.
#[derive(Default)]
struct State {
    initialized: AtomicBool,
    in_flight: Mutex<HashMap<String, CancellationToken>>,
    /// `None` = not listed yet (or listing failed); `Some` = known names.
    tool_names: Mutex<Option<Vec<String>>>,
}

impl State {
    fn key(id: &Value) -> String {
        // `1` and `"1"` must not collide, so key on the canonical JSON form.
        serde_json::to_string(id).unwrap_or_default()
    }

    fn register(&self, id: &Value, token: CancellationToken) {
        self.lock().insert(Self::key(id), token);
    }

    fn token_for(&self, id: &Value) -> Option<CancellationToken> {
        self.lock().get(&Self::key(id)).cloned()
    }

    fn finish(&self, id: &Value) -> Option<CancellationToken> {
        self.lock().remove(&Self::key(id))
    }

    fn cancel_all(&self) {
        for token in self.lock().values() {
            token.cancel();
        }
    }

    /// Records a catalogue that was just listed.
    fn remember_tools(&self, names: Vec<String>) {
        *self.tool_names.lock().unwrap_or_else(|p| p.into_inner()) = Some(names);
    }

    /// Whether `name` is one of this host's tools, listing them once and
    /// remembering the answer. `Err` means the host could not be asked at all,
    /// which is a tool failure and not "no such tool". A failed listing is not
    /// cached: the daemon may simply not be up yet.
    async fn knows_tool(&self, host: &dyn ToolHost, name: &str) -> Result<bool, HostError> {
        if let Some(names) = self
            .tool_names
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return Ok(names.iter().any(|n| n == name));
        }
        let tools = host.list_tools().await?;
        let names: Vec<String> = tools.into_iter().map(|t| t.name).collect();
        let known = names.iter().any(|n| n == name);
        self.remember_tools(names);
        Ok(known)
    }

    /// Poisoning means some other task panicked while holding this lock. The
    /// map is a plain `HashMap` of tokens, so the contents are still sound and
    /// taking the server down over it would be worse than carrying on.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.in_flight.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// What one read of the input stream produced.
#[derive(Debug)]
enum ReadOutcome {
    Line(Vec<u8>),
    /// A blank line: nothing to do.
    Empty,
    /// Longer than [`MAX_LINE_BYTES`]; the line was discarded, not buffered.
    TooLong,
    Eof,
}

/// Reads newline-delimited lines while refusing anything over the limit
/// without accumulating it in memory.
///
/// A plain `read_until` would grow without bound on a hostile or buggy client,
/// so bytes are pulled in fixed chunks and dropped as soon as the line is over
/// the limit. The tail of a chunk is kept, since one read can carry several
/// lines.
///
/// Once a line is over the limit it is being *discarded*, so nothing of it is
/// worth keeping: only the position of its newline matters. From that point on
/// the buffer is released after every scan, which is what stops a client that
/// never sends a newline from growing `pending` without bound. (The opencraylspd side
/// gets this for free from `read_line`, which returns `TooLong` as soon as the
/// limit is passed instead of looking for the line's end first.)
struct LineReader<R> {
    inner: R,
    /// Bytes already read but not yet returned to the caller.
    pending: Vec<u8>,
    chunk: [u8; 8 * 1024],
    eof: bool,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            chunk: [0u8; 8 * 1024],
            eof: false,
        }
    }

    async fn next_line(&mut self) -> std::io::Result<ReadOutcome> {
        let mut line: Vec<u8> = Vec::new();
        let mut too_long = false;
        let mut scanned = 0usize;

        loop {
            // Consume whatever is already buffered before reading more.
            let buffered = &self.pending[scanned..];
            let mut advanced = scanned;
            for &byte in buffered {
                if byte == b'\n' {
                    return self.finish(&line, too_long, advanced + 1);
                }
                if line.len() < MAX_LINE_BYTES {
                    line.push(byte);
                } else {
                    // Over the limit: stop accumulating the line itself.
                    too_long = true;
                    line.clear();
                    line.shrink_to_fit();
                }
                advanced += 1;
            }
            scanned = advanced;
            // Whatever was scanned is never needed again — either it belongs
            // to the line being returned, or the line is already too long and
            // is being thrown away. Releasing it here is what bounds memory
            // during a long over-limit line.
            if scanned > 0 {
                self.pending.drain(..scanned);
                scanned = 0;
            }
            if self.eof {
                // Nothing buffered at all is a clean end of stream, not a blank
                // line: the reader must not spin reporting `Empty` forever.
                if line.is_empty() && !too_long {
                    return Ok(ReadOutcome::Eof);
                }
                return self.finish(&line, too_long, 0);
            }

            let n = self.inner.read(&mut self.chunk).await?;
            if n == 0 {
                self.eof = true;
                continue;
            }
            self.pending.extend_from_slice(&self.chunk[..n]);
        }
    }

    fn finish(
        &mut self,
        line: &[u8],
        too_long: bool,
        consumed: usize,
    ) -> std::io::Result<ReadOutcome> {
        if consumed < self.pending.len() {
            self.pending.drain(..consumed);
        } else {
            self.pending.clear();
        }
        if too_long {
            Ok(ReadOutcome::TooLong)
        } else if line.is_empty() {
            Ok(ReadOutcome::Empty)
        } else {
            Ok(ReadOutcome::Line(line.to_vec()))
        }
    }
}

/// A parsed request or notification.
enum Message {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// Why a line could not be dispatched.
#[derive(Debug)]
enum MessageFault {
    /// Not JSON at all (or not a JSON value): `-32700`, id `null`.
    Unparsable,
    /// A JSON-RPC batch array: not supported here, nor by MCP.
    Batch,
    /// JSON, but not a valid request envelope: `-32600`, id echoed if usable.
    Invalid(Value),
}

/// Validates the envelope and splits requests from notifications.
fn parse_message(bytes: &[u8]) -> Result<Message, MessageFault> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| MessageFault::Unparsable)?;
    // A JSON-RPC batch is not supported by this server (nor by MCP).
    if value.is_array() {
        return Err(MessageFault::Batch);
    }
    let Value::Object(obj) = &value else {
        return Err(MessageFault::Invalid(Value::Null));
    };

    // Per JSON-RPC 2.0 an id is a string or a number; `null` is discouraged
    // but still an id, so it is answered with a null id. Anything else is a
    // malformed request.
    let id = match obj.get("id") {
        None => None,
        Some(Value::Null) => Some(Value::Null),
        Some(v) if v.is_string() || v.is_number() => Some(v.clone()),
        Some(_) => return Err(MessageFault::Invalid(Value::Null)),
    };

    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(MessageFault::Invalid(id.unwrap_or(Value::Null)));
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Err(MessageFault::Invalid(id.unwrap_or(Value::Null)));
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    match id {
        Some(id) => Ok(Message::Request {
            id,
            method: method.to_owned(),
            params,
        }),
        None => Ok(Message::Notification {
            method: method.to_owned(),
            params,
        }),
    }
}

async fn send(tx: &mpsc::Sender<String>, response: &Value) {
    match serde_json::to_string(response) {
        Ok(line) => {
            let _ = tx.send(line).await;
        }
        Err(e) => tracing::error!(error = %e, "could not serialize a response"),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn text_content(output: &ToolOutput) -> Value {
    json!({
        "content": [{"type": "text", "text": output.text}],
        "isError": output.is_error,
    })
}

/// One request and everything it needs to be answered.
struct Request {
    id: Value,
    method: String,
    params: Value,
    host: Arc<dyn ToolHost>,
    state: Arc<State>,
    /// Registered by the reader loop before this task was spawned.
    token: CancellationToken,
    tx: mpsc::Sender<String>,
    version: String,
    /// Ceiling on this request's call into the host; see
    /// [`McpServer::with_call_deadline`].
    call_deadline: Duration,
}

impl Request {
    /// Runs the request to completion and writes its reply. `None` from
    /// [`dispatch`] means the request is answered with silence.
    async fn run(self) {
        let Request {
            id,
            method,
            params,
            host,
            state,
            token,
            tx,
            version,
            call_deadline,
        } = self;

        let response = dispatch(
            &id,
            &method,
            params,
            &host,
            &state,
            &token,
            &version,
            call_deadline,
        )
        .await;

        // Unregister first so a late `cancel_all` cannot cancel a finished
        // request.
        let token = state.finish(&id).unwrap_or(token);
        if token.is_cancelled() {
            tracing::debug!(%method, "request cancelled; suppressing the response");
            return;
        }
        if let Some(response) = response {
            send(&tx, &response).await;
        }
    }
}

/// Negotiates the protocol version: echo what the client asked for when we
/// speak it, otherwise offer the newest one we support.
fn negotiate(requested: Option<&str>) -> &'static str {
    match requested {
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .find(|s| **s == v)
            .copied()
            .unwrap_or(LATEST_PROTOCOL_VERSION),
        _ => LATEST_PROTOCOL_VERSION,
    }
}

/// Returns the response to write, or `None` to answer nothing.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    id: &Value,
    method: &str,
    params: Value,
    host: &Arc<dyn ToolHost>,
    state: &Arc<State>,
    token: &CancellationToken,
    version: &str,
    call_deadline: Duration,
) -> Option<Value> {
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let negotiated = negotiate(requested);
            Some(result_response(
                id.clone(),
                json!({
                    "protocolVersion": negotiated,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": SERVER_NAME, "version": version},
                    "instructions": SERVER_INSTRUCTIONS,
                }),
            ))
        }
        "ping" => Some(result_response(id.clone(), json!({}))),
        "tools/list" | "tools/call" if !state.initialized.load(Ordering::SeqCst) => Some(
            error_response(id.clone(), NOT_INITIALIZED, "server not initialized"),
        ),
        "tools/list" => match bounded(host.list_tools(), call_deadline).await {
            Bounded::Ok(tools) => {
                // Remember the catalogue so later calls need no extra round trip.
                state.remember_tools(tools.iter().map(|t| t.name.clone()).collect());
                let tools: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema,
                            "annotations": {"readOnlyHint": t.annotations.read_only_hint},
                        })
                    })
                    .collect();
                // No cursor support: the whole catalogue, no `nextCursor`.
                Some(result_response(id.clone(), json!({"tools": tools})))
            }
            Bounded::Cancelled => None,
            Bounded::Elapsed => Some(error_response(
                id.clone(),
                INTERNAL_ERROR,
                &format!(
                    "the tool host did not answer within {}s",
                    call_deadline.as_secs()
                ),
            )),
            Bounded::Host(e) => Some(error_response(
                id.clone(),
                INTERNAL_ERROR,
                &format!("could not list tools: {e}"),
            )),
        },
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error_response(
                    id.clone(),
                    INVALID_PARAMS,
                    "missing tool name",
                ));
            };
            // Absent means "no arguments"; present-but-not-an-object is a
            // client bug we refuse rather than guess about.
            let arguments = match params.get("arguments") {
                None => Value::Object(Default::default()),
                Some(v) if v.is_object() => v.clone(),
                Some(_) => {
                    return Some(error_response(
                        id.clone(),
                        INVALID_PARAMS,
                        "`arguments` must be an object",
                    ));
                }
            };

            // The catalogue is fixed for the lifetime of a daemon, so it is
            // listed once per connection instead of on every call.
            match state.knows_tool(host.as_ref(), name).await {
                Ok(true) => {}
                Ok(false) => {
                    return Some(error_response(
                        id.clone(),
                        INVALID_PARAMS,
                        &format!("unknown tool `{name}`"),
                    ));
                }
                Err(e) => return Some(tool_failure(id, &e)),
            }

            match bounded(host.call_tool(name, arguments, token), call_deadline).await {
                Bounded::Ok(output) => Some(result_response(id.clone(), text_content(&output))),
                // A cancelled call is silent: the client already moved on.
                Bounded::Cancelled => None,
                // A call that ran out of time is a tool failure *with* an
                // answer, not a dropped request: the caller is waiting for this
                // id and would wait forever if we said nothing. This is also
                // what keeps the EOF grace honest — every request ends one way
                // or the other, so none of them can still be running when the
                // grace expires and `cancel_all` takes the reply away.
                //
                // Built as a `ToolOutput` rather than a `HostError` so it does
                // not get relabelled `[daemon_unavailable]` by `tool_failure`.
                Bounded::Elapsed => Some(result_response(
                    id.clone(),
                    text_content(&ToolOutput::error(format!(
                        "[timeout] `{name}` did not answer within {}s",
                        call_deadline.as_secs()
                    ))),
                )),
                Bounded::Host(HostError::UnknownTool(n)) => Some(error_response(
                    id.clone(),
                    INVALID_PARAMS,
                    &format!("unknown tool `{n}`"),
                )),
                Bounded::Host(e) => Some(tool_failure(id, &e)),
            }
        }
        // Only reachable when a client sends it as a request; behave sanely.
        "notifications/cancelled" => {
            if let Some(other) = params.get("requestId").and_then(|i| state.token_for(i)) {
                other.cancel();
            }
            Some(result_response(id.clone(), json!({})))
        }
        _ => Some(error_response(
            id.clone(),
            METHOD_NOT_FOUND,
            &format!("unknown method `{method}`"),
        )),
    }
}

/// How a bounded call into the tool host ended.
enum Bounded<T> {
    Ok(T),
    /// The request's own cancellation token fired: the client moved on, so the
    /// answer is dropped rather than sent.
    Cancelled,
    /// [`TOOL_CALL_DEADLINE`] ran out first.
    Elapsed,
    /// The host itself failed.
    Host(HostError),
}

/// Runs `call` under `deadline`.
///
/// The token is deliberately *not* raced here. A cancelled request must end
/// silently, and it does: `Request::run` drops the reply once the token has
/// fired, whatever this returns. Racing the token against the host instead would
/// cut the host's own future short the instant it is cancelled, so a host that
/// honours cancellation by unwinding — releasing a file handle, cancelling a
/// child process, returning [`HostError::Cancelled`] — would never get to run
/// that code, and `cancellations` would stay at zero. Letting the host answer
/// keeps that contract intact; `deadline` is what stops a host that ignores
/// cancellation from pinning the task forever.
async fn bounded<F, T>(call: F, deadline: Duration) -> Bounded<T>
where
    F: std::future::Future<Output = Result<T, HostError>>,
{
    tokio::select! {
        result = call => match result {
            Ok(value) => Bounded::Ok(value),
            Err(HostError::Cancelled) => Bounded::Cancelled,
            Err(error) => Bounded::Host(error),
        },
        () = tokio::time::sleep(deadline) => Bounded::Elapsed,
    }
}

/// Host failures are tool-level: a successful result with `isError: true`.
fn tool_failure(id: &Value, error: &HostError) -> Value {
    let text = match error {
        HostError::Unavailable(msg) if msg.starts_with('[') => msg.clone(),
        HostError::Unavailable(msg) => format!("[daemon_unavailable] {msg}"),
        other => format!("[daemon_unavailable] {other}"),
    };
    result_response(id.clone(), text_content(&ToolOutput::error(text)))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn negotiate_echoes_each_supported_version() {
        for v in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(negotiate(Some(v)), v);
        }
    }

    #[test]
    fn negotiate_falls_back_to_the_newest_supported_version() {
        assert_eq!(negotiate(Some("1999-01-01")), LATEST_PROTOCOL_VERSION);
        assert_eq!(negotiate(None), LATEST_PROTOCOL_VERSION);
    }

    #[test]
    fn request_and_notification_are_told_apart_by_the_id() {
        let msg = parse_message(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert!(matches!(msg, Message::Request { .. }));
        let msg =
            parse_message(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(matches!(msg, Message::Notification { .. }));
    }

    #[test]
    fn unparsable_lines_are_parse_errors_and_batches_are_invalid_requests() {
        assert!(matches!(
            parse_message(b"{oops"),
            Err(MessageFault::Unparsable)
        ));
        assert!(matches!(parse_message(b"[1,2]"), Err(MessageFault::Batch)));
    }

    #[tokio::test]
    async fn an_oversized_line_is_refused_without_buffering_it() {
        let mut line = br#"{"jsonrpc":"2.0","id":1,"method":"ping","pad":""#.to_vec();
        line.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES + 1));
        line.extend_from_slice(br#""}"#);
        line.push(b'\n');
        let outcome = LineReader::new(line.as_slice()).next_line().await.unwrap();
        assert!(matches!(outcome, ReadOutcome::TooLong));
    }

    #[tokio::test]
    async fn a_line_at_the_limit_is_still_read() {
        let mut line = vec![b'x'; MAX_LINE_BYTES];
        line.push(b'\n');
        match LineReader::new(line.as_slice()).next_line().await.unwrap() {
            ReadOutcome::Line(got) => assert_eq!(got.len(), MAX_LINE_BYTES),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    /// An over-limit line is being *discarded*, so its bytes must not
    /// accumulate while the reader keeps looking for the newline.
    ///
    /// The reader used to bound the `line` vector but keep every scanned byte
    /// in `pending` until a newline arrived, so a client that never sent one
    /// made the buffer grow without bound — exactly the DoS the limit is
    /// supposed to prevent. `pending` must stay near one chunk while the
    /// oversized line streams in.
    #[tokio::test]
    async fn an_over_limit_line_does_not_accumulate_in_the_buffer() {
        // 16 MiB of over-limit input with no newline: the reader has to read
        // all of it, and must not keep any of it.
        let total = MAX_LINE_BYTES * 2;
        let reader = LineReader::new(std::io::Cursor::new(vec![b'x'; total]));
        let task = tokio::spawn(async move {
            let mut reader = reader;
            let outcome = reader.next_line().await.unwrap();
            // Whatever the outcome, the buffer must not have held the stream.
            (outcome, reader.pending.len())
        });
        let (outcome, pending) = task.await.unwrap();
        // No newline in the input, so this ends at EOF with the line refused.
        assert!(
            matches!(outcome, ReadOutcome::TooLong),
            "an over-limit line with no newline must be refused, got {outcome:?}"
        );
        assert!(
            pending <= 8 * 1024,
            "the scan buffer must stay near one chunk, but held {pending} bytes \
             after {total} bytes of over-limit input"
        );
    }

    /// The tail of a chunk must still be available: several lines arriving in
    /// one read are all returned, and the over-limit fix must not drop them.
    #[tokio::test]
    async fn several_lines_in_one_read_are_all_returned() {
        let mut reader = LineReader::new(&b"first\nsecond\nthird\n"[..]);
        for expected in ["first", "second", "third"] {
            match reader.next_line().await.unwrap() {
                ReadOutcome::Line(got) => {
                    assert_eq!(String::from_utf8(got).unwrap(), expected);
                }
                other => panic!("expected {expected:?}, got {other:?}"),
            }
        }
        assert!(matches!(
            reader.next_line().await.unwrap(),
            ReadOutcome::Eof
        ));
    }

    #[test]
    fn ids_are_keyed_so_numbers_and_strings_never_collide() {
        assert_ne!(State::key(&json!(1)), State::key(&json!("1")));
        assert_eq!(State::key(&json!(1)), State::key(&json!(1)));
    }

    #[test]
    fn instructions_are_within_the_mcp_budget() {
        assert!(SERVER_INSTRUCTIONS.len() <= 600);
        assert!(SERVER_INSTRUCTIONS.is_ascii());
        assert!(SERVER_INSTRUCTIONS.contains("lsp_"));
        assert!(SERVER_INSTRUCTIONS.contains("[indexing]"));
    }
}
