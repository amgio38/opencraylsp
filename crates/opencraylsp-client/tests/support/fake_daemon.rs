//! An in-test daemon that speaks the same wire protocol as `opencraylspd`.
//!
//! It is deliberately not a copy of `opencraylspd`: it exists so a test can put the
//! client into states the real daemon only reaches by accident - answering
//! late, hanging up mid-request, speaking a different protocol version, or
//! sending something that is not JSON at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// How the fake should misbehave, if at all.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// Reply to `hello` with this error code instead of a result.
    pub hello_error: Option<i64>,
    /// Extra `data` for the `hello` error (e.g. `{"valid": [...]}`).
    pub hello_error_data: Option<Value>,
    /// Version reported by a successful `hello`.
    pub protocol: Option<u32>,
    /// Languages to report in each `hello`, by 1-based ordinal; the last entry
    /// repeats. Lets a test tell the handshakes of two connections apart.
    pub hello_languages_by_hello: Option<Vec<Vec<String>>>,
    /// Wait this long before answering `hello`.
    pub hello_delay: Option<Duration>,
    /// Wait this long before answering `tools/call`.
    pub call_delay: Option<Duration>,
    /// Close the connection instead of answering the Nth request (1-based).
    pub drop_on_request: Option<usize>,
    /// Close the connection during the first `tools/call`, so a retry on a
    /// fresh connection can still succeed.
    pub drop_on_first_call: bool,
    /// Close the connection on every `tools/call`, so the single retry fails
    /// too.
    pub drop_every_call: bool,
    /// Answer `tools/call` with this error code.
    pub call_error: Option<i64>,
    /// Emit a line that is not JSON.
    pub garbage_line: bool,
    /// Emit a reply line longer than the client's limit.
    pub oversize_line: bool,
    /// Do not answer `hello` at all.
    pub ignore_hello: bool,
}

/// What the fake observed.
#[derive(Debug, Default)]
pub struct Observed {
    pub hellos: Vec<Value>,
    pub calls: Vec<(String, Value)>,
    pub methods: Vec<String>,
    pub connections: usize,
    pub cancelled: Vec<Value>,
}

/// A daemon listening on a tempdir socket.
pub struct FakeDaemon {
    observed: Arc<Mutex<Observed>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl FakeDaemon {
    /// Binds `socket` and starts serving with the given script.
    pub async fn start(socket: PathBuf, script: Script) -> Self {
        let listener = UnixListener::bind(&socket).expect("fake daemon binds");
        let observed: Arc<Mutex<Observed>> = Arc::default();
        let requests = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let script = Arc::new(Mutex::new(script));
        let observed_task = observed.clone();
        let requests_task = requests.clone();
        let calls_seen = Arc::new(AtomicBool::new(false));
        let calls_seen_task = calls_seen.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { return };
                        {
                            let mut seen = observed_task.lock().unwrap();
                            seen.connections += 1;
                        }
                        let script = script.clone();
                        let observed = observed_task.clone();
                        let requests = requests_task.clone();
                        let calls_seen = calls_seen_task.clone();
                        tokio::spawn(async move {
                            serve_connection(stream, script, observed, requests, calls_seen)
                                .await;
                        });
                    }
                }
            }
        });
        Self {
            observed,
            shutdown: Some(tx),
            task: Some(task),
        }
    }

    /// Binds and serves with the default (well-behaved) script.
    pub async fn start_default(socket: PathBuf) -> Self {
        Self::start(socket, Script::default()).await
    }

    /// The `hello` params of every connection.
    pub fn hellos(&self) -> Vec<Value> {
        self.observed.lock().unwrap().hellos.clone()
    }

    /// The `tools/call` requests, in arrival order.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.observed.lock().unwrap().calls.clone()
    }

    /// How many connections were accepted.
    pub fn connections(&self) -> usize {
        self.observed.lock().unwrap().connections
    }

    /// The `$/cancel` notifications the daemon received.
    pub fn cancellations(&self) -> Vec<Value> {
        self.observed.lock().unwrap().cancelled.clone()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve_connection(
    stream: UnixStream,
    script: Arc<Mutex<Script>>,
    observed: Arc<Mutex<Observed>>,
    requests: Arc<AtomicUsize>,
    calls_seen: Arc<AtomicBool>,
) {
    let (read_half, write) = stream.into_split();
    // One writer task, exactly like the real daemon, so concurrent replies
    // cannot interleave.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let writer = tokio::spawn(async move {
        let mut write = write;
        while let Some(line) = rx.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = write.flush().await;
        }
    });
    let mut lines = BufReader::new(read_half).lines();
    let mut pending = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = value["method"].as_str().unwrap_or_default().to_owned();
        let id = value["id"].clone();
        let n = requests.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut seen = observed.lock().unwrap();
            seen.methods.push(method.clone());
        }
        let script = script.lock().unwrap().clone();

        if method == "$/cancel" {
            observed
                .lock()
                .unwrap()
                .cancelled
                .push(value["params"]["id"].clone());
            continue;
        }
        if method == "hello" {
            observed
                .lock()
                .unwrap()
                .hellos
                .push(value["params"].clone());
            if script.ignore_hello {
                continue;
            }
        }
        // Which `hello` this is, for scripts that answer each one differently.
        let hello_ordinal = if method == "hello" {
            observed.lock().unwrap().hellos.len()
        } else {
            0
        };
        if method == "tools/call" {
            observed.lock().unwrap().calls.push((
                value["params"]["name"]
                    .clone()
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                value["params"]["arguments"].clone(),
            ));
        }

        // Hang up before answering?
        let drop_it = script.drop_on_request == Some(n)
            || (script.drop_on_first_call
                && method == "tools/call"
                && !calls_seen.swap(true, Ordering::SeqCst))
            || (script.drop_every_call && method == "tools/call");
        if drop_it {
            return;
        }

        if script.garbage_line && n == 1 {
            let _ = tx.send("this is not json\n".to_owned()).await;
            continue;
        }
        if script.oversize_line && n == 1 {
            let big = "x".repeat(5 * 1024 * 1024);
            let line = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"x\":\"{big}\"}}\n");
            let _ = tx.send(line).await;
            continue;
        }

        // Slow methods run in their own task, so the connection keeps reading
        // while one of them waits - that is what lets a `$/cancel` arrive.
        let tx = tx.clone();
        pending.push(tokio::spawn(async move {
            if let Some(delay) = script.call_delay.filter(|_| method == "tools/call") {
                tokio::time::sleep(delay).await;
            }
            let response = match method.as_str() {
                "hello" => {
                    if let Some(delay) = script.hello_delay {
                        tokio::time::sleep(delay).await;
                    }
                    let languages = script
                        .hello_languages_by_hello
                        .as_ref()
                        .and_then(|list| {
                            list.get(hello_ordinal.saturating_sub(1))
                                .or_else(|| list.last())
                        })
                        .cloned()
                        .unwrap_or_else(|| vec!["rust".to_owned()]);
                    match script.hello_error {
                        Some(code) => error(id, code, "scripted failure", script.hello_error_data),
                        None => ok(id, hello_result(script.protocol, languages)),
                    }
                }
                "tools/list" => ok(
                    id,
                    json!({"tools": [{"name": "lsp_status", "description": "status",
                        "input_schema": {"type": "object"}}]}),
                ),
                "tools/call" => match script.call_error {
                    Some(code) => error(id, code, "scripted tool failure", None),
                    None => ok(id, json!({"text": "ran", "is_error": false})),
                },
                "status" => ok(id, status_report()),
                "shutdown" => ok(id, json!({})),
                other => error(id, -32601, &format!("unknown method `{other}`"), None),
            };
            let _ = tx.send(format!("{response}\n")).await;
        }));
    }
    // Let the in-flight replies drain, then stop writing.
    drop(tx);
    for task in pending {
        task.abort();
    }
    writer.abort();
}

fn hello_result(protocol: Option<u32>, languages: Vec<String>) -> Value {
    json!({
        "protocol": protocol.unwrap_or(1),
        "daemon_version": "0.1.0-fake",
        "pid": std::process::id(),
        "languages": languages,
        "language_mode": "auto",
    })
}

fn status_report() -> Value {
    json!({
        "daemon": {"version": "0.1.0-fake", "pid": 4242, "uptime_secs": 12,
                   "rss_bytes": 1024, "clients": 1},
        "limits": {"max_instances": 8, "max_rss_mb": 6144,
                   "idle_shutdown_secs": 900, "max_open_docs": 256},
        "enabled_languages": ["rust"],
        "language_mode": "auto",
        "not_installed": [],
        "instances": [],
    })
}

fn ok(id: Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn error(id: Value, code: i64, message: &str, data: Option<Value>) -> String {
    let mut err = json!({"code": code, "message": message});
    if let Some(data) = data {
        err["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": err}).to_string()
}
