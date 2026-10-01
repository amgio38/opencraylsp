//! Transcript tests for the MCP stdio protocol layer (`opencraylsp-mcp/src/mcp.rs`).
//!
//! Every test drives the server exactly like a harness would: write newline
//! delimited JSON-RPC into one end of a pipe, read responses from the other.
//! `stdout` purity is asserted by parsing every response line back as
//! JSON-RPC.

use std::sync::Arc;
use std::time::Duration;

use opencraylsp_mcp::fake_host::{Failure, FakeHost, wait_until};
use opencraylsp_mcp::mcp::{self, McpServer, SERVER_INSTRUCTIONS, TOOL_CALL_DEADLINE};
use opencraylsp_proto::{HostError, ToolDef};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};

const READY: Duration = Duration::from_secs(5);
const SHORT: Duration = Duration::from_millis(400);

/// A live server on one end of a pipe.
struct Session {
    writer: tokio::io::WriteHalf<DuplexStream>,
    lines: Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
    join: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Session {
    async fn start(host: Arc<FakeHost>) -> Self {
        Self::start_with(host, 256 * 1024).await
    }

    /// A server with the production defaults — no deadline or grace overridden.
    ///
    /// Most tests want exactly this: the defaults are what ships, and a suite
    /// that quietly replaced them would not be testing what runs.
    async fn start_with(host: Arc<FakeHost>, capacity: usize) -> Self {
        let (server_end, client_end) = tokio::io::duplex(capacity);
        let (server_in, server_out) = tokio::io::split(server_end);
        let (client_in, client_out) = tokio::io::split(client_end);
        let join = tokio::spawn(async move {
            McpServer::new(host, env!("CARGO_PKG_VERSION"))
                .serve(server_in, server_out)
                .await
        });
        Self {
            writer: client_out,
            lines: BufReader::new(client_in).lines(),
            join,
        }
    }

    /// A server whose requests are bounded by `call_deadline`, with the EOF
    /// grace derived from it the way production does.
    ///
    /// Deriving both from one number is the invariant the fix is about, so the
    /// harness must not be able to state them independently: a test that set
    /// them separately could re-create the very race this is guarding. Only
    /// tests that need a short deadline should come through here.
    async fn start_configured(
        host: Arc<FakeHost>,
        capacity: usize,
        call_deadline: Duration,
    ) -> Self {
        let (server_end, client_end) = tokio::io::duplex(capacity);
        let (server_in, server_out) = tokio::io::split(server_end);
        let (client_in, client_out) = tokio::io::split(client_end);
        let join = tokio::spawn(async move {
            McpServer::new(host, env!("CARGO_PKG_VERSION"))
                .with_call_deadline(call_deadline)
                .with_eof_grace(mcp::grace_for(call_deadline))
                .serve(server_in, server_out)
                .await
        });
        Self {
            writer: client_out,
            lines: BufReader::new(client_in).lines(),
            join,
        }
    }

    async fn raw(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send(&mut self, id: i64, method: &str, params: Value) {
        self.raw(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        )
        .await;
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.raw(&json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string())
            .await;
    }

    async fn recv(&mut self) -> Value {
        self.try_recv(READY)
            .await
            .expect("expected a response line")
    }

    async fn try_recv(&mut self, timeout: Duration) -> Option<Value> {
        match tokio::time::timeout(timeout, self.lines.next_line()).await {
            Ok(Ok(Some(line))) => Some(
                serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("stdout line is not JSON-RPC: {line:?} ({e})")),
            ),
            Ok(Ok(None)) => None,
            Ok(Err(e)) => panic!("read failed: {e}"),
            Err(_) => None,
        }
    }

    /// Half-close stdin so the server sees EOF.
    async fn eof(&mut self) {
        self.writer.shutdown().await.unwrap();
    }

    async fn wait_exit(&mut self, timeout: Duration) -> std::io::Result<()> {
        match tokio::time::timeout(timeout, &mut self.join).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(e)) => panic!("server task panicked: {e}"),
            Err(_) => panic!("server did not exit within {timeout:?}"),
        }
    }

    async fn initialize(&mut self) -> Value {
        self.send(1, "initialize", init_params("2025-06-18")).await;
        self.notify("notifications/initialized", json!({})).await;
        self.recv().await
    }
}

fn init_params(version: &str) -> Value {
    json!({
        "protocolVersion": version,
        "capabilities": {},
        "clientInfo": {"name": "test-harness", "version": "0"}
    })
}

async fn session() -> (Session, Arc<FakeHost>) {
    let host = FakeHost::with_default_tools();
    let s = Session::start(host.clone()).await;
    (s, host)
}

/// A session whose requests are bounded by `deadline`; see
/// [`Session::start_configured`].
async fn session_with_deadline(deadline: Duration) -> (Session, Arc<FakeHost>) {
    let host = FakeHost::with_default_tools();
    let s = Session::start_configured(host.clone(), 256 * 1024, deadline).await;
    (s, host)
}

// ---------------------------------------------------------------- initialize

#[tokio::test]
async fn init_supported_version_2025_06_18_is_echoed_back() {
    let (mut s, _h) = session().await;
    s.send(1, "initialize", init_params("2025-06-18")).await;
    let r = s.recv().await;
    assert_eq!(r["result"]["protocolVersion"], json!("2025-06-18"));
}

#[tokio::test]
async fn init_supported_version_2025_03_26_is_echoed_back() {
    let (mut s, _h) = session().await;
    s.send(1, "initialize", init_params("2025-03-26")).await;
    assert_eq!(
        s.recv().await["result"]["protocolVersion"],
        json!("2025-03-26")
    );
}

#[tokio::test]
async fn init_supported_version_2024_11_05_is_echoed_back() {
    let (mut s, _h) = session().await;
    s.send(1, "initialize", init_params("2024-11-05")).await;
    assert_eq!(
        s.recv().await["result"]["protocolVersion"],
        json!("2024-11-05")
    );
}

#[tokio::test]
async fn init_unknown_version_falls_back_to_latest() {
    let (mut s, _h) = session().await;
    s.send(1, "initialize", init_params("1999-01-01")).await;
    assert_eq!(
        s.recv().await["result"]["protocolVersion"],
        json!("2025-06-18")
    );
}

#[tokio::test]
async fn init_missing_protocol_version_falls_back_to_latest() {
    let (mut s, _h) = session().await;
    s.send(1, "initialize", json!({"capabilities": {}})).await;
    assert_eq!(
        s.recv().await["result"]["protocolVersion"],
        json!("2025-06-18")
    );
}

#[tokio::test]
async fn init_result_shape_has_tools_capability_list_changed_false() {
    let (mut s, _h) = session().await;
    let r = s.initialize().await;
    assert_eq!(
        r["result"]["capabilities"],
        json!({"tools": {"listChanged": false}})
    );
}

#[tokio::test]
async fn init_result_server_info_name_is_opencraylsp_mcp_with_crate_version() {
    let (mut s, _h) = session().await;
    let r = s.initialize().await;
    assert_eq!(r["result"]["serverInfo"]["name"], json!("opencraylsp-mcp"));
    assert_eq!(
        r["result"]["serverInfo"]["version"],
        json!(env!("CARGO_PKG_VERSION"))
    );
}

#[tokio::test]
async fn init_instructions_are_english_and_within_600_chars() {
    let (mut s, _h) = session().await;
    let r = s.initialize().await;
    let text = r["result"]["instructions"].as_str().expect("instructions");
    assert!(text.len() <= 600, "instructions too long: {}", text.len());
    assert!(
        text.is_ascii(),
        "instructions must be ASCII/English: {text}"
    );
    assert!(
        text.contains("lsp_") && text.to_lowercase().contains("grep"),
        "instructions must steer models from grep to lsp_*: {text}"
    );
    assert!(
        text.contains("[indexing]"),
        "must explain [indexing]: {text}"
    );
}

// ---------------------------------------------------------- initialized / ping

#[tokio::test]
async fn notifications_initialized_is_not_answered() {
    let (mut s, _h) = session().await;
    s.send(1, "initialize", init_params("2025-06-18")).await;
    s.recv().await;
    s.notify("notifications/initialized", json!({})).await;
    assert!(s.try_recv(SHORT).await.is_none(), "notification answered");
}

#[tokio::test]
async fn ping_before_initialize_is_answered() {
    let (mut s, _h) = session().await;
    s.send(7, "ping", json!({})).await;
    let r = s.recv().await;
    assert_eq!(r["result"], json!({}));
}

#[tokio::test]
async fn ping_after_initialize_is_answered_with_empty_result() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(7, "ping", json!({})).await;
    assert_eq!(s.recv().await["result"], json!({}));
}

#[tokio::test]
async fn ping_with_string_id_echoes_the_id_verbatim() {
    let (mut s, _h) = session().await;
    s.raw(r#"{"jsonrpc":"2.0","id":"ping-abc","method":"ping"}"#)
        .await;
    let r = s.recv().await;
    assert_eq!(r["id"], json!("ping-abc"));
}

#[tokio::test]
async fn ping_ignores_a_null_id_notification_style_line() {
    // A ping with `id: null` is still a request per JSON-RPC framing used here.
    let (mut s, _h) = session().await;
    s.raw(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)
        .await;
    let r = s.recv().await;
    assert_eq!(r["id"], Value::Null);
}

// -------------------------------------------------------------- tools/list

#[tokio::test]
async fn list_before_initialize_is_rejected_with_32002() {
    let (mut s, _h) = session().await;
    s.send(2, "tools/list", json!({})).await;
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32002));
}

#[tokio::test]
async fn list_returns_host_tools_with_read_only_annotation() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(2, "tools/list", json!({})).await;
    let r = s.recv().await;
    let tools = r["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools array, got {r}"));
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], json!("lsp_status"));
    assert_eq!(tools[0]["inputSchema"]["type"], json!("object"));
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], json!(true));
    assert_eq!(tools[1]["annotations"]["readOnlyHint"], json!(true));
}

#[tokio::test]
async fn list_ignores_cursor_and_never_returns_next_cursor() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(2, "tools/list", json!({"cursor": "anything"})).await;
    let r = s.recv().await;
    assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 2);
    assert!(
        r["result"].get("nextCursor").is_none(),
        "cursor pagination must not be advertised: {r}"
    );
}

#[tokio::test]
async fn list_with_absent_params_is_accepted() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.raw(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .await;
    assert_eq!(
        s.recv().await["result"]["tools"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn list_surfaces_host_failure_as_protocol_internal_error() {
    let (mut s, h) = session().await;
    s.initialize().await;
    h.fail_list(HostError::Unavailable("[daemon_unavailable] down".into()));
    s.send(2, "tools/list", json!({})).await;
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32603));
    assert!(r["error"]["message"].as_str().unwrap().contains("daemon"));
}

// -------------------------------------------------------------- tools/call

#[tokio::test]
async fn call_success_returns_text_content_and_is_error_false() {
    let (mut s, h) = session().await;
    s.initialize().await;
    s.send(
        2,
        "tools/call",
        json!({"name": "lsp_definition", "arguments": {"symbol": "foo"}}),
    )
    .await;
    let r = s.recv().await;
    assert_eq!(r["result"]["isError"], json!(false));
    assert_eq!(r["result"]["content"][0]["type"], json!("text"));
    assert_eq!(
        r["result"]["content"][0]["text"],
        json!("lsp_definition ran")
    );
    assert_eq!(h.calls()[0].arguments, json!({"symbol": "foo"}));
}

#[tokio::test]
async fn call_tool_output_error_maps_to_is_error_true() {
    let (mut s, h) = session().await;
    s.initialize().await;
    h.fail(
        "lsp_status",
        Failure::Tool("[no_server] no server for .xyz".into()),
    );
    s.send(2, "tools/call", json!({"name": "lsp_status"})).await;
    let r = s.recv().await;
    assert_eq!(r["result"]["isError"], json!(true));
    assert_eq!(
        r["result"]["content"][0]["text"],
        json!("[no_server] no server for .xyz")
    );
    assert!(
        r.get("error").is_none(),
        "tool failure must not be a JSON-RPC error"
    );
}

#[tokio::test]
async fn call_unknown_tool_is_a_protocol_error_32602() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(2, "tools/call", json!({"name": "lsp_nope"})).await;
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32602));
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown tool")
    );
    assert!(r.get("result").is_none());
}

#[tokio::test]
async fn call_missing_arguments_defaults_to_empty_object() {
    let (mut s, h) = session().await;
    s.initialize().await;
    s.send(2, "tools/call", json!({"name": "lsp_status"})).await;
    s.recv().await;
    assert_eq!(h.calls()[0].arguments, json!({}));
}

#[tokio::test]
async fn call_non_object_arguments_is_rejected_with_32602() {
    let (mut s, h) = session().await;
    s.initialize().await;
    s.send(
        2,
        "tools/call",
        json!({"name": "lsp_status", "arguments": "not an object"}),
    )
    .await;
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32602));
    assert_eq!(h.call_count("lsp_status"), 0, "host must not be called");
}

#[tokio::test]
async fn call_with_null_arguments_is_rejected_with_32602() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(
        2,
        "tools/call",
        json!({"name": "lsp_status", "arguments": null}),
    )
    .await;
    assert_eq!(s.recv().await["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn call_missing_name_is_rejected_with_32602() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(2, "tools/call", json!({"arguments": {}})).await;
    assert_eq!(s.recv().await["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn call_before_initialize_is_rejected_with_32002() {
    let (mut s, h) = session().await;
    s.send(2, "tools/call", json!({"name": "lsp_status"})).await;
    assert_eq!(s.recv().await["error"]["code"], json!(-32002));
    assert_eq!(h.calls().len(), 0);
}

#[tokio::test]
async fn host_unavailable_becomes_a_tool_error_not_a_protocol_error() {
    let (mut s, h) = session().await;
    s.initialize().await;
    h.fail(
        "lsp_status",
        Failure::Host(HostError::Unavailable(
            "[daemon_unavailable] no daemon".into(),
        )),
    );
    s.send(2, "tools/call", json!({"name": "lsp_status"})).await;
    let r = s.recv().await;
    assert!(
        r.get("error").is_none(),
        "HostError is not a protocol error: {r}"
    );
    assert_eq!(r["result"]["isError"], json!(true));
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("[daemon_unavailable]"),
        "text must carry the machine readable code: {r}"
    );
}

#[tokio::test]
async fn host_unknown_tool_error_maps_to_32602() {
    let (mut s, h) = session().await;
    s.initialize().await;
    h.fail(
        "lsp_status",
        Failure::Host(HostError::UnknownTool("lsp_status".into())),
    );
    s.send(2, "tools/call", json!({"name": "lsp_status"})).await;
    assert_eq!(s.recv().await["error"]["code"], json!(-32602));
}

// ------------------------------------------------------------- cancellation

#[tokio::test]
async fn cancelled_in_flight_request_is_never_answered_even_after_host_returns() {
    let (mut s, h) = session().await;
    s.initialize().await;
    let gate = h.gate("lsp_status");
    s.send(5, "tools/call", json!({"name": "lsp_status"})).await;
    assert!(wait_until(READY, || h.call_count("lsp_status") == 1).await);
    s.notify("notifications/cancelled", json!({"requestId": 5}))
        .await;
    gate.open();
    s.send(6, "ping", json!({})).await;
    let r = s.recv().await;
    assert_eq!(r["id"], json!(6), "only the ping may be answered");
    assert!(
        s.try_recv(SHORT).await.is_none(),
        "a cancelled request must not produce a response"
    );
}

#[tokio::test]
async fn a_host_that_ignores_cancellation_still_gets_no_reply() {
    // The client has moved on, so a late "here is your answer" would be
    // misattributed to whatever the client asked next. Suppression has to
    // happen in the protocol layer, not depend on the host cooperating.
    let (mut s, h) = session().await;
    s.initialize().await;
    h.set_ignores_cancellation(true);
    let gate = h.gate("lsp_status");
    s.send(5, "tools/call", json!({"name": "lsp_status"})).await;
    assert!(wait_until(READY, || h.call_count("lsp_status") == 1).await);
    s.notify("notifications/cancelled", json!({"requestId": 5}))
        .await;
    gate.open();
    s.send(6, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(6));
    assert!(
        s.try_recv(SHORT).await.is_none(),
        "a cancelled request must not be answered even if the host replies"
    );
}

#[tokio::test]
async fn a_cancel_racing_the_request_it_names_is_not_lost() {
    // The request and its cancellation arrive in a single write. The reader
    // loop handles the cancellation synchronously while the request is still
    // queued on a spawned task, so a token registered inside that task would
    // be registered too late and the cancellation would be dropped.
    for round in 0..30 {
        let (mut s, h) = session().await;
        s.initialize().await;
        let gate = h.gate("lsp_status");
        let both = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"lsp_status"}}),
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}),
        );
        s.raw(&both).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.open();
        s.send(6, "ping", json!({})).await;
        let r = s.recv().await;
        assert_eq!(
            r["id"],
            json!(6),
            "round {round}: only the ping may be answered, got {r}"
        );
        assert!(
            s.try_recv(SHORT).await.is_none(),
            "round {round}: the cancelled request was answered"
        );
        s.eof().await;
    }
}

#[tokio::test]
async fn cancelled_string_request_id_is_matched_too() {
    let (mut s, h) = session().await;
    s.initialize().await;
    let gate = h.gate("lsp_status");
    s.raw(
        r#"{"jsonrpc":"2.0","id":"call-9","method":"tools/call","params":{"name":"lsp_status"}}"#,
    )
    .await;
    assert!(wait_until(READY, || h.call_count("lsp_status") == 1).await);
    s.notify(
        "notifications/cancelled",
        json!({"requestId": "call-9", "reason": "user"}),
    )
    .await;
    gate.open();
    s.send(7, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(7));
    assert!(s.try_recv(SHORT).await.is_none());
}

#[tokio::test]
async fn cancelled_unknown_request_id_is_ignored() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.notify("notifications/cancelled", json!({"requestId": 9999}))
        .await;
    s.send(1, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(1));
}

#[tokio::test]
async fn cancelled_already_completed_request_is_ignored() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(3, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(3));
    s.notify("notifications/cancelled", json!({"requestId": 3}))
        .await;
    s.send(4, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(4));
}

#[tokio::test]
async fn cancellation_token_reaches_the_host() {
    let (mut s, h) = session().await;
    s.initialize().await;
    h.set_cancel_releases_gated(true);
    let _gate = h.gate("lsp_status");
    s.send(5, "tools/call", json!({"name": "lsp_status"})).await;
    assert!(wait_until(READY, || h.call_count("lsp_status") == 1).await);
    s.notify("notifications/cancelled", json!({"requestId": 5}))
        .await;
    assert!(
        wait_until(READY, || h.cancellations() == 1).await,
        "the token given to the host must be triggered"
    );
    s.send(6, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(6));
    assert!(s.try_recv(SHORT).await.is_none());
}

#[tokio::test]
async fn host_returning_cancelled_produces_no_response() {
    // The host may return Cancelled on its own (e.g. the client hung up on the
    // daemon). That must still be silent, per MCP.
    let (mut s, h) = session().await;
    s.initialize().await;
    h.fail("lsp_status", Failure::Host(HostError::Cancelled));
    s.send(8, "tools/call", json!({"name": "lsp_status"})).await;
    s.send(9, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(9));
    assert!(s.try_recv(SHORT).await.is_none());
}

#[tokio::test]
async fn cancelled_malformed_notification_is_ignored_without_writing() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.notify("notifications/cancelled", json!({"nope": 1}))
        .await;
    s.raw(r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#)
        .await;
    s.send(1, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(1));
}

// --------------------------------------------------------- malformed input

#[tokio::test]
async fn malformed_json_reports_32700_with_null_id_and_keeps_serving() {
    let (mut s, _h) = session().await;
    s.raw("{not json at all").await;
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32700));
    assert_eq!(r["id"], Value::Null);
    s.send(1, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(1));
}

#[tokio::test]
async fn blank_lines_are_skipped_and_whitespace_only_lines_are_parse_errors() {
    let (mut s, _h) = session().await;
    // A bare newline is not a message and must be ignored silently.
    s.raw("").await;
    // Whitespace is not JSON: it is a parse error with a null id, reported in
    // the order the lines arrived.
    s.raw("   ").await;
    s.send(1, "ping", json!({})).await;
    let first = s.recv().await;
    assert_eq!(first["error"]["code"], json!(-32700));
    assert_eq!(first["id"], Value::Null);
    assert_eq!(s.recv().await["id"], json!(1));
}

#[tokio::test]
async fn batch_array_is_rejected_with_32600() {
    let (mut s, _h) = session().await;
    s.raw(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).await;
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32600));
    assert!(r["error"]["message"].as_str().unwrap().contains("batch"));
}

#[tokio::test]
async fn oversized_line_is_rejected_with_32600_and_stream_recovers() {
    let host = FakeHost::with_default_tools();
    let mut s = Session::start_with(host, 64 * 1024).await;
    s.send(1, "initialize", init_params("2025-06-18")).await;
    s.recv().await;
    // Over the 8 MiB line limit. Written in chunks because the pipe is only
    // 64 KiB wide: the server must drain it to the newline and refuse it
    // rather than buffering 9 MiB and then failing.
    let chunk = "x".repeat(64 * 1024);
    s.writer
        .write_all(br#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"pad":""#)
        .await
        .unwrap();
    for _ in 0..144 {
        s.writer.write_all(chunk.as_bytes()).await.unwrap();
    }
    s.writer.write_all(br#""}}"#).await.unwrap();
    s.writer.write_all(b"\n").await.unwrap();
    s.writer.flush().await.unwrap();
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32600));
    assert!(r["error"]["message"].as_str().unwrap().contains("line"));
    s.send(3, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(3));
}

#[tokio::test]
async fn wrong_jsonrpc_version_is_rejected_with_32600() {
    let (mut s, _h) = session().await;
    s.raw(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).await;
    assert_eq!(s.recv().await["error"]["code"], json!(-32600));
}

#[tokio::test]
async fn non_object_message_is_rejected_with_32600() {
    let (mut s, _h) = session().await;
    s.raw("42").await;
    assert_eq!(s.recv().await["error"]["code"], json!(-32600));
}

#[tokio::test]
async fn missing_method_is_rejected_with_32600_and_keeps_the_id() {
    let (mut s, _h) = session().await;
    s.raw(r#"{"jsonrpc":"2.0","id":11}"#).await;
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32600));
    assert_eq!(r["id"], json!(11));
}

#[tokio::test]
async fn unknown_request_method_is_rejected_with_32601() {
    let (mut s, _h) = session().await;
    s.send(1, "does/not/exist", json!({})).await;
    assert_eq!(s.recv().await["error"]["code"], json!(-32601));
}

#[tokio::test]
async fn unknown_notification_is_ignored_silently() {
    let (mut s, _h) = session().await;
    s.notify("notifications/something_new", json!({})).await;
    s.send(1, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(1));
    assert!(s.try_recv(SHORT).await.is_none());
}

#[tokio::test]
async fn numeric_and_string_ids_round_trip_unchanged() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    for (i, id) in [json!(42), json!("abc"), json!(-7)].iter().enumerate() {
        s.raw(&json!({"jsonrpc": "2.0", "id": id, "method": "ping"}).to_string())
            .await;
        assert_eq!(s.recv().await["id"], *id, "id {i} must come back verbatim");
    }
}

// ------------------------------------------------- concurrency and shutdown

#[tokio::test]
async fn concurrent_20_calls_responses_match_requests() {
    let (mut s, h) = session().await;
    s.initialize().await;
    h.delay("lsp_definition", 20);
    for i in 0..20 {
        s.send(
            100 + i,
            "tools/call",
            json!({"name": "lsp_definition", "arguments": {"symbol": format!("s{i}")}}),
        )
        .await;
    }
    let mut seen = std::collections::HashSet::new();
    for _ in 0..20 {
        let r = s.recv().await;
        let id = r["id"].as_i64().unwrap();
        assert_eq!(
            r["result"]["content"][0]["text"],
            json!("lsp_definition ran")
        );
        assert!(seen.insert(id), "duplicate response for id {id}");
    }
    assert_eq!(seen, (100..120).collect::<std::collections::HashSet<_>>());
}

#[tokio::test]
async fn writer_serializes_100_parallel_requests_without_interleaving_lines() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    for i in 0..100 {
        s.send(1000 + i, "tools/call", json!({"name": "lsp_status"}))
            .await;
    }
    let mut ids = std::collections::HashSet::new();
    for _ in 0..100 {
        // A torn line would fail to parse inside `try_recv`, which panics.
        let r = s.recv().await;
        assert!(ids.insert(r["id"].as_i64().unwrap()));
    }
    assert_eq!(ids.len(), 100);
}

#[tokio::test]
async fn slow_call_does_not_block_ping() {
    let (mut s, h) = session().await;
    s.initialize().await;
    h.delay("lsp_definition", 700);
    s.send(
        1,
        "tools/call",
        json!({"name": "lsp_definition", "arguments": {"symbol": "x"}}),
    )
    .await;
    s.send(2, "ping", json!({})).await;
    let start = tokio::time::Instant::now();
    let r = s.recv().await;
    assert_eq!(
        r["id"],
        json!(2),
        "ping must answer while the call is in flight"
    );
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "ping waited {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn stdin_eof_does_not_pin_the_process_open_on_a_call_that_never_answers() {
    // A client that closes stdin and walks away must not be able to pin the
    // process open forever. That bound used to be the EOF grace alone, set to 2s
    // — and to exactly the connect deadline on the daemon path, which is the bug
    // this whole change is about: too short for a real call, and racing the very
    // timer the call was waiting on.
    //
    // It is now the per-call deadline, with the grace derived from it. A call
    // that can still answer is allowed to (see
    // `a_call_that_outlasts_the_old_two_second_grace_is_still_answered`); a call
    // that never will is ended by its own deadline, and because that is strictly
    // inside the grace, `cancel_all` never has to take a reply away.
    //
    // The short deadline keeps this quick; production uses `TOOL_CALL_DEADLINE`.
    let deadline = Duration::from_millis(300);
    let (mut s, h) = session_with_deadline(deadline).await;
    s.initialize().await;
    h.set_cancel_releases_gated(true);
    let gate = h.gate("lsp_status");
    s.send(1, "tools/call", json!({"name": "lsp_status"})).await;
    assert!(wait_until(READY, || h.call_count("lsp_status") == 1).await);
    s.eof().await;

    // The stuck call is answered rather than dropped: the caller sent a request
    // and is entitled to a reply, even if the only answer is that it ran out of
    // time.
    let reply = s
        .try_recv(READY)
        .await
        .expect("the stuck call must be answered, not dropped");
    assert_eq!(reply["id"], json!(1));
    assert_eq!(
        reply["result"]["isError"],
        json!(true),
        "a call that ran out of time is an error result: {reply}"
    );
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .expect("text")
            .starts_with("[timeout]"),
        "unexpected answer: {reply}"
    );
    gate.open();
    s.wait_exit(READY).await.expect("clean exit");
}

#[tokio::test]
async fn a_call_that_outlasts_the_old_two_second_grace_is_still_answered() {
    // The regression this whole change exists for. The old default grace was 2s
    // (and 8s on the daemon path, where it was set to exactly the connect
    // deadline), so a `tools/call` still running when the grace expired had its
    // reply suppressed by `cancel_all` — the caller got nothing at all, with no
    // error to explain it. A real language server is routinely slower than 2s on
    // a cold index, and the daemon path could lose the answer to a timer that
    // expired in the same instant.
    //
    // Time is paused, so the 3s delay below costs nothing: what is under test is
    // the relationship between the deadline and the grace, not the clock.
    let (mut s, h) = session().await;
    h.delay("lsp_status", 3_000);
    s.initialize().await;
    s.send(2, "tools/call", json!({"name": "lsp_status"})).await;
    // EOF straight away: the call is still in flight, exactly the case the old
    // grace mishandled.
    s.eof().await;

    let mut seen_ids = Vec::new();
    while let Some(value) = s.try_recv(READY).await {
        seen_ids.push(value["id"].as_i64().expect("a response id"));
        if value["id"] == json!(2) {
            assert!(
                value["result"].is_object(),
                "the call must be answered, not dropped: {value}"
            );
            assert!(
                !value["result"]["isError"].as_bool().unwrap_or(false),
                "the call was answered as an error: {value}"
            );
            break;
        }
    }
    assert!(
        seen_ids.contains(&2),
        "the tools/call reply was lost after EOF; saw ids {seen_ids:?}"
    );
    s.wait_exit(READY).await.expect("clean exit");
}

#[test]
fn the_eof_grace_outlasts_every_request_deadline() {
    // The invariant, stated once so a future edit cannot quietly break it: the
    // grace has to be strictly longer than any single request's deadline, or
    // `cancel_all` can take a reply away from a request that was about to
    // answer. Setting the two to the same value is what caused the original
    // bug, so equality is a failure here, not a pass.
    //
    // Checked on the server a caller would actually build — the defaults and the
    // constructor — not only on the helper, so a regression in how they are
    // wired together is caught too. The behavioural half of this is
    // `a_call_that_outlasts_the_old_two_second_grace_is_still_answered`.
    let host = FakeHost::with_default_tools();
    let server = McpServer::new(host, "0.1.0");
    assert!(
        server.eof_grace() > server.call_deadline(),
        "the default EOF grace ({:?}) must outlast the default call deadline ({:?})",
        server.eof_grace(),
        server.call_deadline()
    );
    assert_eq!(server.eof_grace(), mcp::default_eof_grace());
    assert_eq!(server.call_deadline(), TOOL_CALL_DEADLINE);

    // A shortened deadline keeps the same relationship, which is what the test
    // harness relies on when it asks for a fast server.
    let deadline = Duration::from_millis(1);
    let server = McpServer::new(FakeHost::with_default_tools(), "0.1.0")
        .with_call_deadline(deadline)
        .with_eof_grace(mcp::grace_for(deadline));
    assert!(server.eof_grace() > server.call_deadline());
    assert_eq!(mcp::grace_for(deadline) - deadline, mcp::EOF_SLACK);
}

#[tokio::test]
async fn stdin_eof_with_idle_server_exits_zero() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.eof().await;
    s.wait_exit(READY).await.expect("clean exit on EOF");
}

#[tokio::test]
async fn every_stdout_line_of_a_full_session_is_valid_json_rpc() {
    let host = FakeHost::with_default_tools();
    host.fail("lsp_status", Failure::Tool("[timeout] x".into()));
    let mut s = Session::start(host.clone()).await;

    s.raw("{bad json").await;
    s.raw(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).await;
    s.send(1, "ping", json!({})).await;
    s.send(2, "tools/list", json!({})).await;
    s.send(3, "initialize", init_params("2025-06-18")).await;
    s.notify("notifications/initialized", json!({})).await;
    s.send(4, "tools/list", json!({})).await;
    s.send(5, "tools/call", json!({"name": "lsp_status"})).await;
    s.send(6, "tools/call", json!({"name": "nope"})).await;
    s.send(
        7,
        "tools/call",
        json!({"name": "lsp_status", "arguments": 5}),
    )
    .await;
    s.notify("notifications/cancelled", json!({"requestId": 4242}))
        .await;
    s.send(8, "unknown/method", json!({})).await;
    s.notify("unknown/notification", json!({})).await;

    let mut transcript = Vec::new();
    // 10 replies: unparsable line, batch, ping, tools/list before initialize
    // (32002), initialize, tools/list, a call, an unknown tool, bad arguments
    // and an unknown method. The notification produces nothing.
    while transcript.len() < 10 {
        transcript.push(s.recv().await);
    }
    for line in &transcript {
        assert_eq!(
            line["jsonrpc"],
            json!("2.0"),
            "not a JSON-RPC 2.0 line: {line}"
        );
        let has_result = line.get("result").is_some();
        let has_error = line.get("error").is_some();
        assert!(
            has_result ^ has_error,
            "exactly one of result/error: {line}"
        );
        assert!(
            line.get("id").is_some(),
            "every reply carries an id: {line}"
        );
    }
    s.eof().await;
    s.wait_exit(READY).await.expect("clean exit");
}

#[tokio::test]
async fn initialize_twice_is_allowed_and_still_reports_ready() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(2, "initialize", init_params("2024-11-05")).await;
    assert_eq!(
        s.recv().await["result"]["protocolVersion"],
        json!("2024-11-05")
    );
    s.send(3, "tools/list", json!({})).await;
    assert_eq!(
        s.recv().await["result"]["tools"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn the_tool_catalogue_is_listed_once_per_connection() {
    // Every call would otherwise cost an extra round trip to the daemon.
    let (mut s, h) = session().await;
    s.initialize().await;
    s.send(1, "tools/list", json!({})).await;
    s.recv().await;
    assert_eq!(h.list_count(), 1);
    for i in 0..5 {
        s.send(10 + i, "tools/call", json!({"name": "lsp_status"}))
            .await;
        s.recv().await;
    }
    assert_eq!(h.call_count("lsp_status"), 5, "the calls did happen");
    assert_eq!(
        h.list_count(),
        1,
        "the catalogue is listed once per connection, not per call"
    );
}

#[tokio::test]
async fn a_catalogue_listing_failure_is_not_mistaken_for_an_unknown_tool() {
    let (mut s, h) = session().await;
    s.initialize().await;
    h.fail_list(HostError::Unavailable("[daemon_unavailable] down".into()));
    s.send(1, "tools/call", json!({"name": "lsp_status"})).await;
    let r = s.recv().await;
    assert!(
        r.get("error").is_none(),
        "a host that cannot be asked is not a 32602: {r}"
    );
    assert_eq!(r["result"]["isError"], json!(true));
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("daemon")
    );
    // Once the daemon is back the call works: the failure was not cached.
    h.heal_list();
    s.send(2, "tools/call", json!({"name": "lsp_status"})).await;
    let r = s.recv().await;
    assert_eq!(r["result"]["isError"], json!(false), "{r}");
    assert_eq!(h.list_count(), 2, "the failed listing was retried");
}

#[tokio::test]
async fn host_without_tools_rejects_every_call_but_still_lists() {
    let host = FakeHost::empty();
    let mut s = Session::start(host).await;
    s.initialize().await;
    s.send(1, "tools/list", json!({})).await;
    assert_eq!(s.recv().await["result"]["tools"], json!([]));
    s.send(2, "tools/call", json!({"name": "lsp_status"})).await;
    assert_eq!(s.recv().await["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn tool_definitions_are_forwarded_verbatim_from_the_host() {
    let host = FakeHost::empty();
    host.add_tool(ToolDef {
        name: "lsp_rename_preview".into(),
        description: "Preview a rename as a diff.".into(),
        input_schema: json!({"type": "object", "required": ["new_name"]}),
        annotations: Default::default(),
    });
    let mut s = Session::start(host).await;
    s.initialize().await;
    s.send(1, "tools/list", json!({})).await;
    let r = s.recv().await;
    let t = &r["result"]["tools"][0];
    assert_eq!(t["name"], json!("lsp_rename_preview"));
    assert_eq!(t["description"], json!("Preview a rename as a diff."));
    assert_eq!(t["inputSchema"]["required"], json!(["new_name"]));
}

#[tokio::test]
async fn cancel_reason_is_accepted_without_affecting_other_requests() {
    let (mut s, _h) = session().await;
    s.initialize().await;
    s.send(1, "tools/call", json!({"name": "lsp_status"})).await;
    s.recv().await;
    s.notify(
        "notifications/cancelled",
        json!({"requestId": 1, "reason": "user pressed escape"}),
    )
    .await;
    s.send(2, "ping", json!({})).await;
    assert_eq!(s.recv().await["id"], json!(2));
}

#[tokio::test]
async fn an_id_of_a_wrong_json_type_is_refused() {
    let (mut s, _h) = session().await;
    // JSON-RPC ids are strings or numbers; an object id is a broken envelope.
    s.raw(r#"{"jsonrpc":"2.0","id":{"weird":true},"method":"ping"}"#)
        .await;
    let r = s.recv().await;
    assert_eq!(r["error"]["code"], json!(-32600));
    assert_eq!(r["id"], Value::Null);
}

#[tokio::test]
async fn a_cancellation_sent_as_a_request_still_cancels_the_target() {
    let (mut s, h) = session().await;
    s.initialize().await;
    let gate = h.gate("lsp_status");
    s.send(5, "tools/call", json!({"name": "lsp_status"})).await;
    assert!(wait_until(READY, || h.call_count("lsp_status") == 1).await);
    // Not the notification form: a request carrying `notifications/cancelled`.
    s.send(6, "notifications/cancelled", json!({"requestId": 5}))
        .await;
    assert_eq!(
        s.recv().await["id"],
        json!(6),
        "the request form is answered"
    );
    gate.open();
    assert!(
        s.try_recv(SHORT).await.is_none(),
        "the target request must stay silent"
    );
}

#[tokio::test]
async fn host_failure_with_an_unprefixed_message_gets_the_code_prepended() {
    let (mut s, h) = session().await;
    s.initialize().await;
    // A host that forgets the `[code]` prefix must not reach the model bare.
    h.fail(
        "lsp_status",
        Failure::Host(HostError::Unavailable("connection refused".into())),
    );
    s.send(1, "tools/call", json!({"name": "lsp_status"})).await;
    let text = s.recv().await["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.starts_with("[daemon_unavailable] "),
        "missing code prefix: {text}"
    );
    assert!(text.contains("connection refused"));
}

#[tokio::test]
async fn eof_without_a_trailing_newline_still_serves_the_last_request() {
    let (mut s, _h) = session().await;
    s.writer
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        )
        .await
        .unwrap();
    s.writer.write_all(b"\n").await.unwrap();
    s.writer
        .write_all(br#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)
        .await
        .unwrap();
    s.writer.flush().await.unwrap();
    s.eof().await;
    // Both replies must arrive: a harness may not terminate with a newline.
    assert_eq!(s.recv().await["id"], json!(1));
    assert_eq!(s.recv().await["id"], json!(2));
    s.wait_exit(READY).await.expect("clean exit");
}

#[test]
fn instructions_constant_is_sane_on_its_own() {
    assert!(SERVER_INSTRUCTIONS.len() <= 600);
    assert!(SERVER_INSTRUCTIONS.contains("lsp_"));
}
