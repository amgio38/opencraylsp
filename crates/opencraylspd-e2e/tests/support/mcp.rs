//! `McpClient`: a real `opencraylsp-mcp` process spoken to over stdio.
//!
//! Deliberately `std::process` + threads for the process and its pipes, with
//! tokio oneshots only for awaiting a reply: a `tokio::process` child ties
//! reaping to the runtime, which a current-thread `#[tokio::test]` makes
//! awkward. One reader thread dispatches replies to waiters by JSON-RPC id, so
//! many requests can be in flight at once (T14). Every raw line is kept for
//! the golden transcript (T10).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::binaries::binaries;
use super::env::TestEnv;

const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// A running `opencraylsp-mcp`.
pub struct McpClient {
    child: Child,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    raw: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Spawns `opencraylsp-mcp` against `env`'s daemon and workspace.
    pub fn start(env: &TestEnv, extra_args: &[&str]) -> Self {
        Self::start_with_socket(env, env.socket(), extra_args)
    }

    /// Like [`Self::start`] but points at an explicit socket.
    pub fn start_with_socket(env: &TestEnv, socket: &std::path::Path, extra_args: &[&str]) -> Self {
        let mut command = env.command(&binaries().opencraylsp_mcp);
        command
            .arg("--socket")
            .arg(socket)
            .arg("--workspace")
            .arg(env.workspace())
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn opencraylsp-mcp");
        let stdin = child.stdin.take().expect("opencraylsp-mcp stdin");
        let stdout = child.stdout.take().expect("opencraylsp-mcp stdout");
        let stderr = child.stderr.take().expect("opencraylsp-mcp stderr");

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();
        let raw: Arc<Mutex<Vec<String>>> = Arc::default();
        let stderr_text: Arc<Mutex<String>> = Arc::default();

        let mut readers = Vec::new();
        {
            let pending = Arc::clone(&pending);
            let raw = Arc::clone(&raw);
            readers.push(std::thread::spawn(move || {
                let lines = BufReader::new(stdout).lines();
                for line in lines.map_while(Result::ok) {
                    raw.lock().unwrap().push(line.clone());
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(id) = value.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(value);
                    }
                }
            }));
        }
        {
            let stderr_text = Arc::clone(&stderr_text);
            readers.push(std::thread::spawn(move || {
                let lines = BufReader::new(stderr).lines();
                for line in lines.map_while(Result::ok) {
                    let mut text = stderr_text.lock().unwrap();
                    text.push_str(&line);
                    text.push('\n');
                }
            }));
        }

        Self {
            child,
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            raw,
            stderr: stderr_text,
            readers,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Raw stdout lines seen so far.
    pub fn raw_lines(&self) -> Vec<String> {
        self.raw.lock().unwrap().clone()
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn write(&self, value: &Value) {
        let mut stdin = self.stdin.lock().unwrap();
        let line = serde_json::to_string(value).expect("serialize");
        let _ = stdin.write_all(line.as_bytes());
        let _ = stdin.write_all(b"\n");
        let _ = stdin.flush();
    }

    fn register(&self) -> (u64, oneshot::Receiver<Value>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// Sends a request and returns the whole JSON-RPC reply.
    pub async fn request(&self, method: &str, params: Value) -> Value {
        let (_id, rx) = self.begin(method, params).await;
        self.await_reply(rx).await
    }

    /// Starts a request without waiting, so a test can cancel it.
    pub async fn begin(&self, method: &str, params: Value) -> (u64, oneshot::Receiver<Value>) {
        let (id, rx) = self.register();
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        (id, rx)
    }

    /// Waits for the reply to a [`Self::begin`] request.
    pub async fn await_reply(&self, rx: oneshot::Receiver<Value>) -> Value {
        match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => panic!(
                "opencraylsp-mcp closed while waiting for a reply (raw: {:?})",
                self.raw_lines()
            ),
            Err(_) => panic!(
                "timed out waiting for a reply (raw: {:?})",
                self.raw_lines()
            ),
        }
    }

    pub async fn initialize(&self) -> Value {
        let reply = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "opencraylspd-e2e", "version": "0"},
                }),
            )
            .await;
        self.notify("notifications/initialized", json!({})).await;
        reply
    }

    pub async fn notify(&self, method: &str, params: Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub async fn list_tools(&self) -> Vec<Value> {
        let reply = self.request("tools/list", json!({})).await;
        reply["result"]["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub async fn ping(&self) -> Value {
        self.request("ping", json!({})).await
    }

    /// Calls one tool and returns the whole JSON-RPC reply.
    pub async fn call(&self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    pub async fn cancel(&self, request_id: u64) {
        self.notify(
            "notifications/cancelled",
            json!({"requestId": request_id, "reason": "test"}),
        )
        .await;
    }

    /// Kills the process and reaps it.
    pub fn shutdown(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// True while the process is still running.
    pub fn pid_alive(&self) -> bool {
        std::path::PathBuf::from(format!("/proc/{}", self.child.id())).exists()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

/// Runs `opencraylsp-mcp` once and returns its output, for the exit-code cases.
pub fn run_once(env: &TestEnv, extra_args: &[&str]) -> Output {
    let mut command = env.command(&binaries().opencraylsp_mcp);
    command
        .arg("--socket")
        .arg(env.socket())
        .arg("--workspace")
        .arg(env.workspace())
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().expect("run opencraylsp-mcp")
}

/// The text a `tools/call` reply carries, whether it succeeded or failed.
pub fn result_text(reply: &Value) -> String {
    if let Some(text) = reply["result"]["content"][0]["text"].as_str() {
        return text.to_owned();
    }
    if let Some(error) = reply["error"].as_object() {
        return error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
    }
    String::new()
}

/// Whether a `tools/call` reply is a tool-level error (`isError: true`).
pub fn is_error(reply: &Value) -> bool {
    reply["result"]["isError"].as_bool().unwrap_or(false)
}
