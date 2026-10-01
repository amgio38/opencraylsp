//! A scriptable fake language server for the opencraylsp-core test suite.
//!
//! Why a separate process instead of an in-process mock: the transport,
//! lifecycle, and crash paths only exist across a real stdio boundary (a
//! dead pipe, a mid-frame EOF, an exit code). An in-process fake would test
//! the JSON shapes but none of the failure semantics in the contract table.
//!
//! Why this file is *not* part of the release product: it is wired as a
//! `[[bin]]` with `required-features = ["test-fake-lsp"]`, so normal builds
//! (`cargo build`, `cargo build --release`, the `release-static` line the
//! plugin joins) never compile it. Only
//! `cargo test -p opencraylsp-core --features test-fake-lsp` builds it, and
//! the integration tests locate it through `CARGO_BIN_EXE_fake-lsp-server`.
//!
//! Behavior switches (all optional, composable):
//! - `--fail-32801=N`: answer the first N requests after `initialize` with
//!   `-32801 ContentModified`, then behave normally (retry-path tests).
//! - `--fail-init-32801=N`: same, but for the `initialize` request itself.
//! - `--crash-after=N`: `exit(1)` after sending N responses; `N=0` exits
//!   before reading anything (crash/restart-cap tests).
//! - `--die-during=METHOD`: `exit(1)` the moment METHOD arrives, without
//!   answering it (mid-request crash tests).
//! - `--fail-method=METHOD`: always answer METHOD with `-32602`, covering the
//!   non-retryable error mapping (applies to `initialize` too).
//! - `--ask-extra`: after `initialize`, also ask `client/registerCapability`
//!   and `window/workDoneProgress/create`, recording the replies.
//! - `--ignore-exit`: stay alive past the `exit` notification, so tests can
//!   prove `shutdown()` falls back to killing the child.
//! - `--ask-config`: before answering `initialize`, send one
//!   `workspace/configuration` request and record the client's reply.
//! - `--delay-ms=M` with `--delay-method=METHOD`: sleep M ms before answering
//!   that method (cancellation/timeout tests). Without `--delay-method` every
//!   request is delayed, including `initialize` and `shutdown`.
//! - `--push-diagnostics`: publish one diagnostic on `didOpen`/`didChange`/
//!   `didSave`, carrying the document version the client sent (diagnostics
//!   wait-semantics tests). A document whose latest text contains `CLEAN` is
//!   published with zero diagnostics, so tests can assert the error-then-clean
//!   arc.
//! - `--encoding=ENC`: advertise `ENC` as `positionEncoding` (default utf-32).
//! - `--record-config=PATH`: append the `workspace/configuration` reply here.
//! - `--record-events=PATH`: append one line per notification/request here, so
//!   tests can assert the exact `didOpen`/`didChange`/`didSave` sequence.
//! - `--stderr=TEXT`: write TEXT to stderr at startup, before anything else
//!   (a server explaining why it is about to die).
//! - `--alloc-mb=N`: touch N MiB of memory at startup and hold it, so the
//!   process tree's resident size really grows (memory-guard tests).
//! - `--progress-ms=N`: after `initialized`, announce work-done progress (token
//!   `--progress-token`, title `--progress-title`) that reports 50% halfway and ends
//!   after N ms, like a server indexing in the background.
//! - `--tag=ID`: ignored; only exists so tests can find this process with
//!   `pgrep -f` when asserting no child is left behind.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};

#[derive(Debug, Default, Clone)]
struct Options {
    fail_32801: u64,
    fail_init_32801: u64,
    fail_method: String,
    crash_after: Option<u64>,
    die_during: String,
    ask_config: bool,
    ask_extra: bool,
    ignore_exit: bool,
    delay_ms: u64,
    delay_method: String,
    push_diagnostics: bool,
    encoding: String,
    record_config: Option<String>,
    record_events: Option<String>,
    progress_ms: u64,
    alloc_mb: usize,
    stderr_text: String,
    progress_token: String,
    progress_title: String,
}

impl Options {
    fn parse(args: &[String]) -> Self {
        let mut opts = Options {
            encoding: "utf-32".to_owned(),
            progress_token: "fake/index".to_owned(),
            progress_title: "Indexing".to_owned(),
            ..Options::default()
        };
        for arg in args {
            if let Some(n) = arg.strip_prefix("--fail-32801=") {
                opts.fail_32801 = n.parse().unwrap_or(0);
            } else if let Some(n) = arg.strip_prefix("--fail-init-32801=") {
                opts.fail_init_32801 = n.parse().unwrap_or(0);
            } else if let Some(m) = arg.strip_prefix("--fail-method=") {
                opts.fail_method = m.to_owned();
            } else if let Some(n) = arg.strip_prefix("--crash-after=") {
                opts.crash_after = n.parse().ok();
            } else if let Some(m) = arg.strip_prefix("--die-during=") {
                opts.die_during = m.to_owned();
            } else if arg == "--ask-config" {
                opts.ask_config = true;
            } else if arg == "--ask-extra" {
                opts.ask_extra = true;
            } else if arg == "--ignore-exit" {
                opts.ignore_exit = true;
            } else if let Some(m) = arg.strip_prefix("--delay-ms=") {
                opts.delay_ms = m.parse().unwrap_or(0);
            } else if let Some(m) = arg.strip_prefix("--delay-method=") {
                opts.delay_method = m.to_owned();
            } else if arg == "--push-diagnostics" {
                opts.push_diagnostics = true;
            } else if let Some(e) = arg.strip_prefix("--encoding=") {
                opts.encoding = e.to_owned();
            } else if let Some(p) = arg.strip_prefix("--record-config=") {
                opts.record_config = Some(p.to_owned());
            } else if let Some(p) = arg.strip_prefix("--record-events=") {
                opts.record_events = Some(p.to_owned());
            } else if let Some(t) = arg.strip_prefix("--stderr=") {
                opts.stderr_text = t.to_owned();
            } else if let Some(n) = arg.strip_prefix("--alloc-mb=") {
                opts.alloc_mb = n.parse().unwrap_or(0);
            } else if let Some(n) = arg.strip_prefix("--progress-ms=") {
                opts.progress_ms = n.parse().unwrap_or(0);
            } else if let Some(t) = arg.strip_prefix("--progress-token=") {
                opts.progress_token = t.to_owned();
            } else if let Some(t) = arg.strip_prefix("--progress-title=") {
                opts.progress_title = t.to_owned();
            }
        }
        opts
    }
}

fn record(events: &Option<String>, line: &str) {
    if let Some(path) = events
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn send(value: &serde_json::Value, responses_sent: &mut u64, opts: &Options) {
    let text = serde_json::to_string(value).unwrap_or_default();
    let framed = format!("Content-Length: {}\r\n\r\n{text}", text.len());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(framed.as_bytes());
    let _ = out.flush();
    *responses_sent += 1;
    if let Some(limit) = opts.crash_after
        && *responses_sent >= limit
    {
        std::process::exit(1);
    }
}

/// `--progress-ms=N`: announce work under `--progress-token`, report 50%
/// immediately, and end it N ms later from a background thread (a real server
/// indexes while it keeps answering requests).
fn start_progress(opts: &Options, responses_sent: &mut u64) {
    let token = opts.progress_token.clone();
    send(
        &serde_json::json!({"jsonrpc": "2.0", "id": 9_000_001, "method": "window/workDoneProgress/create",
                            "params": {"token": token}}),
        responses_sent,
        opts,
    );
    send(
        &serde_json::json!({"jsonrpc": "2.0", "method": "$/progress", "params": {
            "token": token,
            "value": {"kind": "begin", "title": opts.progress_title, "message": "0/2", "percentage": 0}}}),
        responses_sent,
        opts,
    );
    let (opts, token, wait) = (opts.clone(), token, opts.progress_ms);
    std::thread::spawn(move || {
        let mut sent = 0;
        std::thread::sleep(std::time::Duration::from_millis(wait / 2));
        send(
            &serde_json::json!({"jsonrpc": "2.0", "method": "$/progress", "params": {
                "token": token, "value": {"kind": "report", "message": "1/2", "percentage": 50}}}),
            &mut sent,
            &opts,
        );
        std::thread::sleep(std::time::Duration::from_millis(wait - wait / 2));
        send(
            &serde_json::json!({"jsonrpc": "2.0", "method": "$/progress", "params": {
                "token": token, "value": {"kind": "end"}}}),
            &mut sent,
            &opts,
        );
    });
}

fn read_message(reader: &mut BufReader<std::io::StdinLock<'_>>) -> Option<serde_json::Value> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim().to_owned();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("Content-Length")
        {
            length = value.trim().parse().ok();
        }
    }
    let length = length?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn publish(uri: &str, version: i64, clean: bool, responses_sent: &mut u64, opts: &Options) {
    // A document whose latest text says CLEAN is reported without errors, so
    // tests can assert the error-then-clean arc of the diagnostics flow.
    let diagnostics = if clean {
        serde_json::Value::Array(Vec::new())
    } else {
        serde_json::json!([{
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 5}
            },
            "severity": 1,
            "message": "fake error: unexpected token",
            "source": "fake-lsp-server",
        }])
    };
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": uri,
            "version": version,
            "diagnostics": diagnostics,
        },
    });
    send(&notification, responses_sent, opts);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = Options::parse(&args);
    if !opts.stderr_text.is_empty() {
        let mut err = std::io::stderr();
        let _ = writeln!(err, "{}", opts.stderr_text);
        let _ = err.flush();
    }
    if opts.crash_after == Some(0) {
        std::process::exit(1);
    }
    if opts.alloc_mb > 0 {
        // Touch every page so the memory is resident, then keep it for the
        // life of the process.
        let mut ballast = vec![0u8; opts.alloc_mb << 20];
        for page in (0..ballast.len()).step_by(4096) {
            ballast[page] = 1;
        }
        std::mem::forget(ballast);
    }
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut responses_sent: u64 = 0;
    let mut failed_32801: u64 = 0;
    let mut failed_init_32801: u64 = 0;
    let mut versions: HashMap<String, i64> = HashMap::new();
    let mut texts: HashMap<String, String> = HashMap::new();
    let mut request_id: u64 = 0;

    while let Some(message) = read_message(&mut reader) {
        let method = message
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_owned();
        let id = message
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        // Checked before any dispatch so it covers notifications (`didOpen`)
        // as well as requests.
        if !opts.die_during.is_empty() && method == opts.die_during {
            record(&opts.record_events, &format!("dying during {method}"));
            std::process::exit(1);
        }

        if id.is_null() {
            // Notification.
            match method.as_str() {
                "textDocument/didOpen" => {
                    let doc = &params["textDocument"];
                    let uri = doc["uri"].as_str().unwrap_or("").to_owned();
                    let version = doc["version"].as_i64().unwrap_or(1);
                    versions.insert(uri.clone(), version);
                    let text = doc["text"].as_str().unwrap_or("").to_owned();
                    let clean = text.contains("CLEAN");
                    texts.insert(uri.clone(), text);
                    record(&opts.record_events, &format!("didOpen {uri} {version}"));
                    if opts.push_diagnostics && !uri.is_empty() {
                        publish(&uri, version, clean, &mut responses_sent, &opts);
                    }
                }
                "textDocument/didChange" => {
                    let doc = &params["textDocument"];
                    let uri = doc["uri"].as_str().unwrap_or("").to_owned();
                    let version = doc["version"].as_i64().unwrap_or(1);
                    versions.insert(uri.clone(), version);
                    let text = params["contentChanges"][0]["text"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let clean = text.contains("CLEAN");
                    texts.insert(uri.clone(), text);
                    record(&opts.record_events, &format!("didChange {uri} {version}"));
                    if opts.push_diagnostics && !uri.is_empty() {
                        publish(&uri, version, clean, &mut responses_sent, &opts);
                    }
                }
                "textDocument/didSave" => {
                    let uri = params["textDocument"]["uri"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let version = versions.get(&uri).copied().unwrap_or(1);
                    let clean = texts.get(&uri).is_some_and(|t| t.contains("CLEAN"));
                    record(&opts.record_events, &format!("didSave {uri} {version}"));
                    if opts.push_diagnostics && !uri.is_empty() {
                        publish(&uri, version, clean, &mut responses_sent, &opts);
                    }
                }
                "exit" => {
                    if opts.ignore_exit {
                        // Stay alive past `exit`: the client must fall back to
                        // killing the child instead of hanging forever.
                        record(&opts.record_events, "ignored exit");
                    } else {
                        std::process::exit(0);
                    }
                }
                "initialized" => {
                    if opts.progress_ms > 0 {
                        start_progress(&opts, &mut responses_sent);
                    }
                }
                "textDocument/didClose" => {
                    let uri = params["textDocument"]["uri"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    record(&opts.record_events, &format!("didClose {uri}"));
                }
                _ => {
                    record(&opts.record_events, &format!("notification:{method}"));
                }
            }
            continue;
        }

        // Request.
        request_id += 1;
        let _ = request_id;
        // Delay first so it also covers `initialize`/`shutdown`: a slow
        // startup is exactly what the timeout tests need to observe.
        if opts.delay_ms > 0 && (opts.delay_method.is_empty() || method == opts.delay_method) {
            std::thread::sleep(std::time::Duration::from_millis(opts.delay_ms));
        }
        if method == "shutdown" {
            record(&opts.record_events, "request:shutdown");
            send(
                &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null}),
                &mut responses_sent,
                &opts,
            );
            continue;
        }
        if method == "initialize" && opts.fail_method == "initialize" {
            record(&opts.record_events, "request:initialize -> -32602");
            send(
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": "fake failure: invalid params"},
                }),
                &mut responses_sent,
                &opts,
            );
            continue;
        }
        if method == "initialize" {
            if failed_init_32801 < opts.fail_init_32801 {
                failed_init_32801 += 1;
                record(
                    &opts.record_events,
                    &format!("request:initialize -> -32801 ({failed_init_32801})"),
                );
                send(
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32801, "message": "ContentModified"},
                    }),
                    &mut responses_sent,
                    &opts,
                );
                continue;
            }
            if opts.ask_config {
                // Ask before answering: the client must reply for init to proceed.
                let probe = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "fake-config-1",
                    "method": "workspace/configuration",
                    "params": {"items": [{"section": "fake"}]},
                });
                send(&probe, &mut responses_sent, &opts);
                // The client's reply arrives as the next message; consume it here.
                if let Some(reply) = read_message(&mut reader)
                    && let Some(path) = &opts.record_config
                {
                    let _ = std::fs::write(
                        path,
                        serde_json::to_string(reply.get("result").unwrap_or(&reply))
                            .unwrap_or_default(),
                    );
                }
            }
            if opts.ask_extra {
                // Exercise the client's other two server-to-client handlers.
                for (probe_id, probe_method) in [
                    ("fake-reg-1", "client/registerCapability"),
                    ("fake-progress-1", "window/workDoneProgress/create"),
                ] {
                    send(
                        &serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": probe_id,
                            "method": probe_method,
                            "params": {},
                        }),
                        &mut responses_sent,
                        &opts,
                    );
                    if let Some(reply) = read_message(&mut reader) {
                        record(
                            &opts.record_events,
                            &format!(
                                "{probe_method} -> {}",
                                reply.get("result").unwrap_or(&reply)
                            ),
                        );
                    }
                }
            }
            record(&opts.record_events, "request:initialize");
            let encoding = opts.encoding.clone();
            send(
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"capabilities": {"positionEncoding": encoding}},
                }),
                &mut responses_sent,
                &opts,
            );
            continue;
        }
        if !opts.fail_method.is_empty() && method == opts.fail_method {
            record(&opts.record_events, &format!("request:{method} -> -32602"));
            send(
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": "fake failure: invalid params"},
                }),
                &mut responses_sent,
                &opts,
            );
            continue;
        }
        if failed_32801 < opts.fail_32801 {
            failed_32801 += 1;
            record(
                &opts.record_events,
                &format!("request:{method} -> -32801 ({failed_32801})"),
            );
            send(
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32801, "message": "ContentModified"},
                }),
                &mut responses_sent,
                &opts,
            );
            continue;
        }
        record(&opts.record_events, &format!("request:{method}"));
        let result = if method == "textDocument/definition" {
            let uri = params["textDocument"]["uri"]
                .as_str()
                .unwrap_or("file:///unknown");
            serde_json::json!([{
                "uri": uri,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 5},
                },
            }])
        } else {
            serde_json::Value::Null
        };
        send(
            &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
            &mut responses_sent,
            &opts,
        );
    }
}
