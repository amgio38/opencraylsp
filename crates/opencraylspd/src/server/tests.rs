//! In-process protocol tests: a real daemon on a temp socket, a raw client,
//! and scripted tool runners (no language server involved).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use opencraylsp_core::{LspBackend, LspConfig};
use opencraylsp_proto::paths::current_uid;
use opencraylsp_proto::rpc::{
    INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, NOT_INITIALIZED, PARSE_ERROR,
    PROTOCOL_MISMATCH, UNKNOWN_LANGUAGE, WORKSPACE_INVALID,
};
use opencraylsp_proto::{ToolAnnotations, ToolDef, ToolOutput};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::sync::CancellationToken;

use super::runner::{LspTools, ToolRunner};
use super::{ServeError, lifecycle::LifecycleError, serve};

/// Echoes the call; `slow` waits until cancelled (or 30 s) and records it.
#[derive(Default)]
struct ScriptedRunner {
    saw_cancel: Arc<AtomicBool>,
    slow_started: Arc<AtomicU32>,
}

#[async_trait]
impl ToolRunner for ScriptedRunner {
    fn defs(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            name: "echo".to_owned(),
            description: "test tool".to_owned(),
            input_schema: json!({"type": "object"}),
            annotations: ToolAnnotations::default(),
        }]
    }

    async fn call(
        &self,
        backend: &dyn LspBackend,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        match name {
            "slow" => {
                self.slow_started.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    () = cancel.cancelled() => {
                        self.saw_cancel.store(true, Ordering::SeqCst);
                        ToolOutput::error("[cancelled] scripted slow tool")
                    }
                    () = tokio::time::sleep(Duration::from_secs(30)) => ToolOutput::ok("slow done"),
                }
            }
            "boundary" => ToolOutput::ok(backend.boundary().display().to_string()),
            "explode" => panic!("scripted handler panic"),
            other => ToolOutput::ok(format!("{other}:{arguments}")),
        }
    }
}

struct Daemon {
    socket: PathBuf,
    ws: PathBuf,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<Result<(), ServeError>>,
    _dir: tempfile::TempDir,
}

/// The uid these tests run as. They assert on peer-credential checks, so a
/// box where it cannot be discovered cannot run them.
fn expect_uid() -> u32 {
    current_uid().expect("the test uid must be discoverable")
}

impl Daemon {
    async fn start() -> (Self, Arc<ScriptedRunner>) {
        Self::start_with(LspConfig::from_toml_str("").unwrap(), expect_uid()).await
    }

    async fn start_with(config: LspConfig, uid: u32) -> (Self, Arc<ScriptedRunner>) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let ws = std::fs::canonicalize(dir.path()).unwrap().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let runner = Arc::new(ScriptedRunner::default());
        let shutdown = CancellationToken::new();
        let handle = {
            let (socket, runner, shutdown) = (socket.clone(), runner.clone(), shutdown.clone());
            tokio::spawn(async move { serve(&socket, config, runner, shutdown, uid).await })
        };
        for _ in 0..200 {
            if UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (
            Self {
                socket,
                ws,
                shutdown,
                handle,
                _dir: dir,
            },
            runner,
        )
    }

    async fn client(&self) -> Client {
        Client::connect(&self.socket).await
    }

    async fn hello_client(&self, languages: Option<Value>) -> Client {
        let mut client = self.client().await;
        let mut params = json!({
            "protocol": 1,
            "client": {"name": "test", "version": "0"},
            "workspace": self.ws.display().to_string(),
        });
        if let Some(languages) = languages {
            params["languages"] = languages;
        }
        let reply = client.call("hello", params).await;
        assert!(reply.get("result").is_some(), "hello failed: {reply}");
        client
    }

    async fn stop(self) {
        self.shutdown.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.handle)
                .await
                .expect("daemon stops")
                .unwrap()
                .is_ok()
        );
    }
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next: u64,
}

impl Client {
    async fn connect(socket: &PathBuf) -> Self {
        let stream = UnixStream::connect(socket).await.expect("connect");
        let (read, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer,
            next: 0,
        }
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
    }

    /// Sends a request and returns its id.
    async fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_raw(line.to_string().as_bytes()).await;
        id
    }

    async fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_raw(line.to_string().as_bytes()).await;
    }

    /// The next message from the daemon, or `None` on EOF.
    async fn try_recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(10), self.reader.read_line(&mut line))
            .await
            .expect("daemon answers in time");
        match read {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(&line).expect("daemon speaks JSON")),
        }
    }

    async fn recv(&mut self) -> Value {
        self.try_recv().await.expect("a message, not EOF")
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        let reply = self.recv().await;
        assert_eq!(reply["id"], json!(id), "reply matches the request: {reply}");
        reply
    }
}

fn code(reply: &Value) -> i64 {
    reply["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("not an error: {reply}"))
}

#[tokio::test]
async fn hello_reports_protocol_pid_and_the_auto_detected_languages() {
    let (daemon, _) = Daemon::start().await;
    std::fs::write(daemon.ws.join("Cargo.toml"), "").unwrap();
    let mut client = daemon.client().await;
    let reply = client
        .call(
            "hello",
            json!({"protocol": 1, "client": {"name": "t", "version": "0"},
                   "workspace": daemon.ws.display().to_string()}),
        )
        .await;
    let result = &reply["result"];
    assert_eq!(result["protocol"], 1);
    assert_eq!(result["pid"], std::process::id());
    assert_eq!(result["language_mode"], "auto");
    assert_eq!(result["languages"], json!(["rust"]));
    daemon.stop().await;
}

#[tokio::test]
async fn hello_normalizes_explicit_languages_and_all() {
    let (daemon, _) = Daemon::start().await;
    for (input, mode, expected) in [
        (
            json!(["RS", "ts"]),
            "declared",
            json!(["rust", "typescript"]),
        ),
        (
            json!(["ts/js"]),
            "declared",
            json!(["javascript", "typescript"]),
        ),
        (json!(["rust,go"]), "declared", json!(["go", "rust"])),
    ] {
        let mut client = daemon.client().await;
        let reply = client
            .call(
                "hello",
                json!({"protocol": 1, "client": {"name": "t", "version": "0"},
                       "workspace": daemon.ws.display().to_string(), "languages": input}),
            )
            .await;
        assert_eq!(reply["result"]["language_mode"], mode, "{reply}");
        assert_eq!(reply["result"]["languages"], expected, "{reply}");
    }
    let mut client = daemon.client().await;
    let reply = client
        .call(
            "hello",
            json!({"protocol": 1, "client": {"name": "t", "version": "0"},
                   "workspace": daemon.ws.display().to_string(), "languages": ["all"]}),
        )
        .await;
    assert_eq!(reply["result"]["language_mode"], "all");
    assert_eq!(reply["result"]["languages"].as_array().unwrap().len(), 6);
    daemon.stop().await;
}

#[tokio::test]
async fn hello_with_an_unknown_language_lists_the_valid_ones() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.client().await;
    let reply = client
        .call(
            "hello",
            json!({"protocol": 1, "client": {"name": "t", "version": "0"},
                   "workspace": daemon.ws.display().to_string(), "languages": ["klingon"]}),
        )
        .await;
    assert_eq!(code(&reply), UNKNOWN_LANGUAGE);
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("klingon")
    );
    assert_eq!(reply["error"]["data"]["valid"][0], "rust");
    daemon.stop().await;
}

#[tokio::test]
async fn hello_rejects_a_protocol_mismatch_naming_the_supported_versions() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.client().await;
    let reply = client
        .call(
            "hello",
            json!({"protocol": 99, "client": {"name": "t", "version": "0"},
                   "workspace": daemon.ws.display().to_string()}),
        )
        .await;
    assert_eq!(code(&reply), PROTOCOL_MISMATCH);
    assert_eq!(reply["error"]["data"]["supported"], json!([1]));
    daemon.stop().await;
}

#[tokio::test]
async fn hello_rejects_workspaces_that_are_not_existing_absolute_directories() {
    let (daemon, _) = Daemon::start().await;
    let file = daemon.ws.join("a-file");
    std::fs::write(&file, "").unwrap();
    for bad in [
        "relative/dir".to_owned(),
        daemon.ws.join("missing").display().to_string(),
        file.display().to_string(),
    ] {
        let mut client = daemon.client().await;
        let reply = client
            .call(
                "hello",
                json!({"protocol": 1, "client": {"name": "t", "version": "0"}, "workspace": bad}),
            )
            .await;
        assert_eq!(code(&reply), WORKSPACE_INVALID, "{bad}: {reply}");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn hello_rejects_bad_params_and_a_second_hello() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.client().await;
    assert_eq!(
        code(&client.call("hello", json!({"nonsense": true})).await),
        INVALID_PARAMS
    );
    let mut ok = daemon.hello_client(None).await;
    let again = ok
        .call(
            "hello",
            json!({"protocol": 1, "client": {"name": "t", "version": "0"},
                   "workspace": daemon.ws.display().to_string()}),
        )
        .await;
    assert_eq!(code(&again), INVALID_REQUEST);
    daemon.stop().await;
}

#[tokio::test]
async fn nothing_but_hello_works_before_hello() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.client().await;
    for method in ["tools/list", "tools/call", "status", "shutdown", "whatever"] {
        let reply = client.call(method, json!({})).await;
        assert_eq!(code(&reply), NOT_INITIALIZED, "{method}");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn an_unknown_method_after_hello_is_method_not_found() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    let reply = client.call("no/such", json!({})).await;
    assert_eq!(code(&reply), METHOD_NOT_FOUND);
    daemon.stop().await;
}

#[tokio::test]
async fn tools_list_and_call_go_through_the_runner() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    let listed = client.call("tools/list", json!({})).await;
    assert_eq!(listed["result"]["tools"][0]["name"], "echo");
    assert_eq!(
        listed["result"]["tools"][0]["annotations"]["readOnlyHint"],
        true
    );
    let called = client
        .call("tools/call", json!({"name": "echo", "arguments": {"a": 1}}))
        .await;
    assert_eq!(
        called["result"],
        json!({"text": "echo:{\"a\":1}", "is_error": false})
    );
    // Missing arguments default to null rather than failing the call.
    let bare = client.call("tools/call", json!({"name": "echo"})).await;
    assert_eq!(bare["result"]["text"], "echo:null");
    let bad = client.call("tools/call", json!({"arguments": {}})).await;
    assert_eq!(code(&bad), INVALID_PARAMS);
    daemon.stop().await;
}

#[tokio::test]
async fn a_panicking_handler_still_answers_with_internal_error_and_the_daemon_lives() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    let called = client
        .call("tools/call", json!({"name": "explode", "arguments": {}}))
        .await;
    let text = called["result"]["text"].as_str().unwrap();
    assert!(text.starts_with("[internal_error]"), "{text}");
    assert_eq!(called["result"]["is_error"], true);
    // The same connection keeps working, and a new one can still connect.
    let again = client.call("tools/call", json!({"name": "echo"})).await;
    assert_eq!(again["result"]["text"], "echo:null");
    let mut other = daemon.hello_client(None).await;
    assert_eq!(
        other.call("status", json!({})).await["result"]["daemon"]["clients"],
        2
    );
    daemon.stop().await;
}

#[tokio::test]
async fn each_connection_gets_its_own_boundary() {
    let (daemon, _) = Daemon::start().await;
    let other = daemon.ws.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut a = daemon.hello_client(None).await;
    let mut b = daemon.client().await;
    b.call(
        "hello",
        json!({"protocol": 1, "client": {"name": "t", "version": "0"},
               "workspace": other.display().to_string()}),
    )
    .await;
    let ra = a.call("tools/call", json!({"name": "boundary"})).await;
    let rb = b.call("tools/call", json!({"name": "boundary"})).await;
    assert_eq!(ra["result"]["text"], daemon.ws.display().to_string());
    assert_eq!(rb["result"]["text"], other.display().to_string());
    daemon.stop().await;
}

#[tokio::test]
async fn status_reports_the_connection_and_its_languages() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(Some(json!(["go"]))).await;
    let reply = client.call("status", json!({})).await;
    let status = &reply["result"];
    assert_eq!(status["daemon"]["clients"], 1);
    assert_eq!(status["enabled_languages"], json!(["go"]));
    assert_eq!(status["language_mode"], "declared");
    assert_eq!(status["limits"]["max_instances"], 8);
    daemon.stop().await;
}

#[tokio::test]
async fn the_client_count_drops_when_a_connection_ends() {
    let (daemon, _) = Daemon::start().await;
    let watcher = daemon.hello_client(None).await;
    let mut observer = daemon.hello_client(None).await;
    assert_eq!(
        observer.call("status", json!({})).await["result"]["daemon"]["clients"],
        2
    );
    drop(watcher);
    let mut clients = 2;
    for _ in 0..100 {
        clients = observer.call("status", json!({})).await["result"]["daemon"]["clients"]
            .as_u64()
            .unwrap();
        if clients == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(clients, 1);
    daemon.stop().await;
}

#[tokio::test]
async fn a_cancel_notification_stops_the_matching_call_only() {
    let (daemon, runner) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    let slow = client.send("tools/call", json!({"name": "slow"})).await;
    while runner.slow_started.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    client.notify("$/cancel", json!({"id": slow})).await;
    let reply = client.recv().await;
    assert_eq!(reply["id"], json!(slow));
    assert_eq!(reply["result"]["text"], "[cancelled] scripted slow tool");
    assert!(runner.saw_cancel.load(Ordering::SeqCst));
    // The connection is still usable.
    let ok = client.call("tools/call", json!({"name": "echo"})).await;
    assert_eq!(ok["result"]["is_error"], false);
    daemon.stop().await;
}

#[tokio::test]
async fn cancelling_an_unknown_or_finished_id_is_harmless() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    client.notify("$/cancel", json!({"id": 9999})).await;
    client.notify("$/cancel", json!({})).await;
    client.notify("some/other", json!({})).await;
    let ok = client.call("tools/call", json!({"name": "echo"})).await;
    assert_eq!(ok["result"]["is_error"], false);
    daemon.stop().await;
}

#[tokio::test]
async fn a_cancel_before_hello_is_ignored() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.client().await;
    client.notify("$/cancel", json!({"id": 1})).await;
    let reply = client.call("status", json!({})).await;
    assert_eq!(code(&reply), NOT_INITIALIZED);
    daemon.stop().await;
}

#[tokio::test]
async fn dropping_the_connection_cancels_its_in_flight_calls() {
    let (daemon, runner) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    client.send("tools/call", json!({"name": "slow"})).await;
    while runner.slow_started.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(client);
    for _ in 0..200 {
        if runner.saw_cancel.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        runner.saw_cancel.load(Ordering::SeqCst),
        "the call was cancelled"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn replies_come_back_in_completion_order_matched_by_id() {
    let (daemon, runner) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    let slow = client.send("tools/call", json!({"name": "slow"})).await;
    while runner.slow_started.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let fast = client.send("tools/call", json!({"name": "echo"})).await;
    let first = client.recv().await;
    assert_eq!(
        first["id"],
        json!(fast),
        "the fast call overtakes the slow one"
    );
    client.notify("$/cancel", json!({"id": slow})).await;
    assert_eq!(client.recv().await["id"], json!(slow));
    daemon.stop().await;
}

#[tokio::test]
async fn a_bad_line_gets_a_parse_error_and_the_connection_survives() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    client.send_raw(b"{ this is not json").await;
    let reply = client.recv().await;
    assert_eq!(code(&reply), PARSE_ERROR);
    assert_eq!(reply["id"], Value::Null);
    let ok = client.call("tools/call", json!({"name": "echo"})).await;
    assert_eq!(ok["result"]["is_error"], false);
    daemon.stop().await;
}

#[tokio::test]
async fn valid_json_that_is_not_a_request_is_invalid_request() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    client.send_raw(b"[1, 2, 3]").await;
    assert_eq!(code(&client.recv().await), INVALID_REQUEST);
    client
        .send_raw(br#"{"jsonrpc":"1.0","id":1,"method":"status"}"#)
        .await;
    assert_eq!(code(&client.recv().await), INVALID_REQUEST);
    daemon.stop().await;
}

#[tokio::test]
async fn three_bad_lines_in_a_row_close_the_connection() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    for _ in 0..3 {
        client.send_raw(b"nope").await;
        assert_eq!(code(&client.recv().await), PARSE_ERROR);
    }
    assert!(client.try_recv().await.is_none(), "connection was closed");
    daemon.stop().await;
}

#[tokio::test]
async fn a_good_line_resets_the_bad_line_streak() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    for _ in 0..2 {
        client.send_raw(b"nope").await;
        client.recv().await;
    }
    client.call("status", json!({})).await;
    for _ in 0..2 {
        client.send_raw(b"nope").await;
        client.recv().await;
    }
    assert_eq!(
        client.call("status", json!({})).await["result"]["daemon"]["clients"],
        1
    );
    daemon.stop().await;
}

#[tokio::test]
async fn blank_lines_are_ignored() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    client.send_raw(b"").await;
    client.send_raw(b"   ").await;
    assert!(
        client
            .call("status", json!({}))
            .await
            .get("result")
            .is_some()
    );
    daemon.stop().await;
}

#[tokio::test]
async fn an_oversized_line_is_answered_and_the_connection_closed() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    let huge = vec![b'a'; opencraylsp_proto::rpc::MAX_LINE_BYTES + 16];
    // The daemon may close before reading everything; a write error is fine.
    let _ = client.writer.write_all(&huge).await;
    let _ = client.writer.write_all(b"\n").await;
    let reply = client.recv().await;
    assert_eq!(code(&reply), PARSE_ERROR);
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exceeds")
    );
    daemon.stop().await;
}

#[tokio::test]
async fn the_shutdown_request_replies_then_stops_the_daemon() {
    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    let reply = client.call("shutdown", json!({})).await;
    assert_eq!(reply["result"], json!({}));
    let result = tokio::time::timeout(Duration::from_secs(10), daemon.handle)
        .await
        .expect("daemon stops after shutdown")
        .unwrap();
    assert!(result.is_ok());
    assert!(!daemon.socket.exists(), "the socket file is removed");
}

#[tokio::test]
async fn cancelling_the_daemon_cancels_in_flight_calls_and_removes_the_socket() {
    let (daemon, runner) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;
    client.send("tools/call", json!({"name": "slow"})).await;
    while runner.slow_started.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let socket = daemon.socket.clone();
    daemon.stop().await;
    assert!(runner.saw_cancel.load(Ordering::SeqCst));
    assert!(!socket.exists());
}

#[tokio::test]
async fn requests_racing_a_shutdown_get_shutting_down() {
    let (daemon, _) = Daemon::start().await;
    let mut a = daemon.hello_client(None).await;
    let mut b = daemon.hello_client(None).await;
    a.call("shutdown", json!({})).await;
    // `b` was connected before shutdown began: it either sees the explicit
    // refusal or a closed connection, never a normal answer.
    // The daemon may already have closed `b`, so a failed write is acceptable.
    let _ = b
        .writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status\"}\n")
        .await;
    if let Some(reply) = b.try_recv().await {
        assert_eq!(code(&reply), opencraylsp_proto::rpc::SHUTTING_DOWN);
    }
    let _ = tokio::time::timeout(Duration::from_secs(10), daemon.handle).await;
}

#[tokio::test]
async fn a_second_daemon_on_the_same_socket_is_refused() {
    let (daemon, _) = Daemon::start().await;
    let second = serve(
        &daemon.socket,
        LspConfig::default(),
        Arc::new(LspTools),
        CancellationToken::new(),
        expect_uid(),
    )
    .await;
    assert!(matches!(
        second,
        Err(ServeError::Lifecycle(LifecycleError::AlreadyRunning(_)))
    ));
    // The first one is unharmed.
    let mut client = daemon.hello_client(None).await;
    assert!(
        client
            .call("status", json!({}))
            .await
            .get("result")
            .is_some()
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_peer_with_the_wrong_uid_is_dropped_without_an_answer() {
    let (daemon, _) = Daemon::start_with(LspConfig::default(), expect_uid() + 1).await;
    let mut client = daemon.client().await;
    let _ = client
        .writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status\"}\n")
        .await;
    assert!(
        client.try_recv().await.is_none(),
        "no reply for a foreign uid"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_hundred_clients_can_talk_at_once() {
    let (daemon, _) = Daemon::start().await;
    let mut tasks = Vec::new();
    for n in 0..100 {
        let socket = daemon.socket.clone();
        let ws = daemon.ws.display().to_string();
        tasks.push(tokio::spawn(async move {
            let mut client = Client::connect(&socket).await;
            let hello = client
                .call(
                    "hello",
                    json!({"protocol": 1, "client": {"name": "t", "version": "0"}, "workspace": ws}),
                )
                .await;
            assert!(hello.get("result").is_some());
            let echoed = client
                .call("tools/call", json!({"name": "echo", "arguments": {"n": n}}))
                .await;
            assert_eq!(echoed["result"]["text"], format!("echo:{{\"n\":{n}}}"));
            client
        }));
    }
    let mut clients = Vec::new();
    for task in tasks {
        clients.push(task.await.unwrap());
    }
    let mut first = clients.pop().unwrap();
    assert_eq!(
        first.call("status", json!({})).await["result"]["daemon"]["clients"],
        100
    );
    daemon.stop().await;
}

#[tokio::test]
async fn the_real_tool_catalog_is_served_when_no_runner_is_injected() {
    // `LspTools` is what the binary uses; its defs come from opencraylsp-tools.
    let defs = LspTools.defs();
    assert_eq!(
        defs.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
        opencraylsp_tools::tool_defs()
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>()
    );
    let backend = opencraylsp_core::mock::MockBackend::new("/ws");
    let out = LspTools
        .call(
            &backend,
            "no_such_tool",
            Value::Null,
            &CancellationToken::new(),
        )
        .await;
    assert!(
        out.is_error && out.text.starts_with("[invalid_args]"),
        "{out:?}"
    );
    assert!(out.text.contains("no_such_tool"), "{out:?}");
    assert_eq!(defs.len(), 11, "the v1 catalogue has eleven tools");
}
/// One connection issuing far more calls than the limit must be told to
/// retry, not have every one of them turned into a concurrent task.
///
/// Each request spawns a task that runs a real language-server query, so an
/// unbounded number costs the *other* connections latency and memory. The
/// `slow` tool parks each call for 30 s, so the in-flight count only goes
/// up: past the limit the daemon must refuse.
#[tokio::test]
async fn too_many_in_flight_requests_are_refused() {
    let (daemon, runner) = Daemon::start().await;
    let mut client = daemon.hello_client(None).await;

    // Fill up to just under the limit; those must all be accepted.
    let limit = crate::server::conn::MAX_INFLIGHT_PER_CONNECTION;
    for _ in 0..limit {
        client
            .send("tools/call", json!({"name": "slow", "arguments": {}}))
            .await;
    }
    // Wait for them to actually be running, so the count is real.
    for _ in 0..200 {
        if runner.slow_started.load(Ordering::SeqCst) as usize >= limit {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runner.slow_started.load(Ordering::SeqCst) as usize,
        limit,
        "every request under the limit must have started"
    );

    // One past the limit is refused with an answer, not dropped.
    let overflow_id = client
        .send("tools/call", json!({"name": "slow", "arguments": {}}))
        .await;
    let mut refused = false;
    for _ in 0..200 {
        let reply = client.try_recv().await.expect("a reply to the overflow");
        if reply["id"] == json!(overflow_id) {
            let message = reply["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("too many requests"),
                "the refusal must say why: {message}"
            );
            refused = true;
            break;
        }
    }
    assert!(
        refused,
        "the request over the limit must be answered, not dropped"
    );

    daemon.stop().await;
}

/// `hello` accepts a workspace reached through a symlink, and the files
/// under the real directory stay reachable.
///
/// This is the behaviour the symlink check protects. It passes both before and after the
/// change, because `Pool::bind` already canonicalizes — so it is pinned as a
/// *behaviour* the daemon must keep, not as evidence that the `hello`-side
/// canonicalize changed anything. What the `hello`-side change actually adds is
/// a resolution failure being reported there, which needs an unreadable
/// directory to provoke and is not worth a fixture that runs as root.
#[tokio::test]
async fn hello_accepts_a_symlinked_workspace_and_its_real_files() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real-ws");
    std::fs::create_dir_all(&real).unwrap();
    let link = dir.path().join("linked-ws");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let (daemon, _) = Daemon::start().await;
    let mut client = daemon.client().await;
    let reply = client
        .call(
            "hello",
            json!({
                "protocol": 1,
                "client": {"name": "t", "version": "0"},
                "workspace": link.display().to_string(),
            }),
        )
        .await;
    assert!(
        reply.get("error").is_none(),
        "a symlinked workspace must be accepted: {reply}"
    );

    let file = real.join("a.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    let answer = client
        .call(
            "tools/call",
            json!({"name": "lsp_references", "arguments": {"path": file.display().to_string()}}),
        )
        .await;
    let text = answer["result"]["text"].as_str().unwrap_or_default();
    assert!(
        !text.contains("outside the workspace boundary"),
        "a file inside the real directory must not be reported outside: {text}"
    );
    daemon.stop().await;
}
