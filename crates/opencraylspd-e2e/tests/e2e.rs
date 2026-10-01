//! End-to-end suite: a real `opencraylspd`, a real `opencraylsp-mcp`, and the
//! scriptable fake language server.
//!
//! Every test owns a tempdir and nothing leaves a process behind: no `pkill`,
//! no `pgrep`, processes are addressed by pid.

#![allow(dead_code)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{
    Limits, McpClient, ServerSpec, TestEnv, is_error, result_text, run_once, standard_servers,
    wait_until,
};
use tokio::time::sleep;

fn workspace_file(env: &TestEnv, name: &str, text: &str) -> PathBuf {
    let path = env.workspace().join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("workspace subdir");
    }
    std::fs::write(&path, text).expect("write workspace file");
    path
}

fn abs(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

/// Polls an async condition until it holds or `timeout` passes.
async fn wait_async(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn instance_pid(report: &Value, server: &str) -> Option<u32> {
    report["instances"]
        .as_array()?
        .iter()
        .find(|instance| instance["server"] == server)
        .and_then(|instance| instance["pid"].as_u64())
        .map(|pid| pid as u32)
}

fn pid_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

/// `opencraylspd status --json` against an explicit socket (the default one belongs to
/// the `TestEnv`, not to a daemon a client started elsewhere).
fn status_at(env: &TestEnv, socket: &Path) -> Value {
    let output = env
        .command(&support::binaries::binaries().opencraylspd)
        .args(["status", "--json", "--socket"])
        .arg(socket)
        .output()
        .expect("run opencraylspd status");
    assert!(
        output.status.success(),
        "opencraylspd status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("status --json is JSON")
}

/// Stops a daemon a client started at an explicit socket and waits for the
/// socket to go away.
fn stop_daemon_at(env: &TestEnv, socket: &Path) {
    let _ = env
        .command(&support::binaries::binaries().opencraylspd)
        .args(["stop", "--socket"])
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    assert!(
        wait_until(|| !socket.exists(), Duration::from_secs(10)),
        "the daemon at {} did not stop",
        socket.display()
    );
}

// ----------------------------------------------------------------- T1

#[tokio::test]
async fn t1_three_clients_share_one_language_server() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();

    let a = McpClient::start(&env, &["--languages", "rust"]);
    let b = McpClient::start(&env, &["--languages", "rust"]);
    let c = McpClient::start(&env, &["--languages", "rust"]);
    for client in [&a, &b, &c] {
        client.initialize().await;
        let reply = client
            .call(
                "lsp_hover",
                json!({"path": abs(&main_rs), "line": 1, "column": 4}),
            )
            .await;
        assert!(!is_error(&reply), "hover failed: {}", result_text(&reply));
        assert!(
            result_text(&reply).contains("helper"),
            "unexpected hover: {}",
            result_text(&reply)
        );
    }

    let report = env.status();
    let instances = report["instances"]
        .as_array()
        .unwrap_or_else(|| panic!("no instances in {report}"));
    assert_eq!(instances.len(), 1, "expected one shared instance: {report}");
    assert_eq!(instances[0]["server"], "rust-analyzer");
    let pid = instances[0]["pid"].as_u64().expect("instance pid");
    assert!(
        pid_alive(pid as u32),
        "fake server pid {pid} is not running"
    );

    a.shutdown();
    b.shutdown();
    c.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T2

#[tokio::test]
async fn t2_killing_one_client_leaves_the_others_working() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();

    let a = McpClient::start(&env, &["--languages", "rust"]);
    let b = McpClient::start(&env, &["--languages", "rust"]);
    let c = McpClient::start(&env, &["--languages", "rust"]);
    for client in [&a, &b, &c] {
        client.initialize().await;
        client
            .call(
                "lsp_hover",
                json!({"path": abs(&main_rs), "line": 1, "column": 4}),
            )
            .await;
    }
    let before = env.status()["daemon"]["clients"].as_u64().expect("clients");
    let b_pid = b.pid();
    b.shutdown();

    assert!(
        wait_async(
            || {
                let now = env.status()["daemon"]["clients"].as_u64();
                now == Some(before - 1)
            },
            Duration::from_secs(10)
        )
        .await,
        "the daemon never noticed client {b_pid} leaving"
    );

    for client in [&a, &c] {
        let reply = client
            .call(
                "lsp_hover",
                json!({"path": abs(&main_rs), "line": 1, "column": 4}),
            )
            .await;
        assert!(
            !is_error(&reply),
            "surviving client failed: {}",
            result_text(&reply)
        );
    }

    a.shutdown();
    c.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T3

#[tokio::test]
async fn t3_idle_instances_are_reclaimed_and_the_next_call_is_not_empty() {
    let mut env = TestEnv::new();
    env.write_config(
        &standard_servers(),
        &Limits {
            idle_shutdown_secs: Some(2),
            ..Limits::default()
        },
    );
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;
    let reply = client
        .call(
            "lsp_hover",
            json!({"path": abs(&main_rs), "line": 1, "column": 4}),
        )
        .await;
    assert!(!is_error(&reply), "{}", result_text(&reply));
    let pid = instance_pid(&env.status(), "rust-analyzer").expect("instance up");

    assert!(
        wait_async(
            || env.status()["instances"]
                .as_array()
                .is_none_or(|i| i.is_empty()),
            Duration::from_secs(10)
        )
        .await,
        "the idle instance was never reclaimed: {}",
        env.status()
    );
    assert!(
        wait_until(|| !pid_alive(pid), Duration::from_secs(5)),
        "reclaimed fake server pid {pid} is still alive"
    );

    let reply = client
        .call(
            "lsp_hover",
            json!({"path": abs(&main_rs), "line": 1, "column": 4}),
        )
        .await;
    let text = result_text(&reply);
    assert!(!text.is_empty(), "a cold start must never answer empty");
    assert!(
        text.contains("helper") || text.contains("indexing"),
        "unexpected cold-start answer: {text}"
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T4

#[tokio::test]
async fn t4_the_memory_guard_restarts_then_refuses() {
    let mut env = TestEnv::new();
    let servers = vec![
        ServerSpec::fake("rust-analyzer", &[("rs", "rust")])
            .root_marker("Cargo.toml")
            .arg("--alloc-mb=80"),
    ];
    env.write_config(
        &servers,
        &Limits {
            max_rss_mb: Some(32),
            memory_sample_ms: Some(200),
            ..Limits::default()
        },
    );
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;

    let mut refused = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let reply = client
            .call(
                "lsp_hover",
                json!({"path": abs(&main_rs), "line": 1, "column": 4}),
            )
            .await;
        let text = result_text(&reply);
        if is_error(&reply) && text.contains("max_rss_mb") {
            refused = true;
            break;
        }
        sleep(Duration::from_millis(400)).await;
    }
    assert!(
        refused,
        "the memory guard never refused after restarts; status: {}",
        env.status()
    );
    let report = env.status();
    let restarts = report["instances"]
        .as_array()
        .and_then(|instances| instances.first())
        .and_then(|instance| instance["memory_restarts"].as_u64())
        .unwrap_or(0);
    assert!(restarts >= 1, "no restart was counted: {report}");

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T5

#[tokio::test]
async fn t5_references_follow_the_synced_document() {
    // The fake answers from the text the client synced, not from the disk, so
    // this fails if the daemon stops sending `didChange`.
    let mut env = TestEnv::new();
    let events = env.dir().join("events.log");
    let servers = vec![
        ServerSpec::fake("rust-analyzer", &[("rs", "rust")])
            .root_marker("Cargo.toml")
            .arg(format!("--record-events={}", events.display())),
    ];
    env.write_config(&servers, &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;

    let args = json!({
        "path": abs(&main_rs), "line": 1, "column": 4, "include_declaration": true,
    });
    let before = result_text(&client.call("lsp_references", args.clone()).await);
    assert!(
        before.contains("main.rs:1"),
        "expected the declaration first: {before}"
    );

    std::fs::write(&main_rs, "fn helper() {}\nfn helper() {}\n").expect("edit");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut changed = false;
    while Instant::now() < deadline {
        let text = result_text(&client.call("lsp_references", args.clone()).await);
        if text.contains("main.rs:2") {
            changed = true;
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    assert!(changed, "references did not follow the synced document");

    let events = std::fs::read_to_string(&events).unwrap_or_default();
    let uri = format!("file://{}", abs(&main_rs));
    let version_of = |prefix: &str| -> Option<i64> {
        events
            .lines()
            .filter(|line| line.starts_with(prefix))
            .filter_map(|line| line.rsplit(' ').next())
            .filter_map(|v| v.parse::<i64>().ok())
            .next_back()
    };
    let opened = version_of(&format!("didOpen {uri}"))
        .unwrap_or_else(|| panic!("no didOpen was recorded:\n{events}"));
    let changed_version = version_of(&format!("didChange {uri}"))
        .unwrap_or_else(|| panic!("no didChange was recorded after the edit:\n{events}"));
    assert!(
        changed_version > opened,
        "didChange version {changed_version} did not advance past didOpen {opened}"
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T7

#[tokio::test]
async fn t7_paths_outside_the_workspace_are_rejected() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    workspace_file(&env, "main.rs", "fn helper() {}\n");
    let link = env.workspace().join("escape.rs");
    std::os::unix::fs::symlink("/etc/hosts", &link).expect("symlink");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;

    for path in ["/etc/passwd.rs", "../../escape.rs", &abs(&link)] {
        let reply = client.call("lsp_outline", json!({"path": path})).await;
        let text = result_text(&reply);
        assert!(
            text.contains("[outside_workspace]"),
            "path {path} was not rejected: {text}"
        );
    }

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T9

#[tokio::test]
async fn t9_an_ambiguous_symbol_lists_candidates_instead_of_guessing() {
    let mut env = TestEnv::new();
    let servers = vec![
        ServerSpec::fake("rust-analyzer", &[("rs", "rust")])
            .root_marker("Cargo.toml")
            .arg("--ambiguous=Widget"),
    ];
    env.write_config(&servers, &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    workspace_file(&env, "main.rs", "struct Widget;\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;
    let reply = client
        .call("lsp_definition", json!({"symbol": "Widget"}))
        .await;
    let text = result_text(&reply);
    assert!(
        !is_error(&reply),
        "ambiguous picks must not be errors: {text}"
    );
    assert!(text.contains("[ambiguous]"), "expected [ambiguous]: {text}");
    assert!(
        text.matches("Widget").count() >= 2,
        "expected two candidates: {text}"
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T12

#[tokio::test]
async fn t12_a_killed_daemon_is_started_again_on_the_next_call() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;
    client
        .call(
            "lsp_hover",
            json!({"path": abs(&main_rs), "line": 1, "column": 4}),
        )
        .await;

    let daemon_pid = env.status()["daemon"]["pid"].as_u64().expect("daemon pid");
    let status = std::process::Command::new("kill")
        .args(["-9", &daemon_pid.to_string()])
        .status()
        .expect("kill daemon");
    assert!(status.success());
    env.reap();
    assert!(
        wait_until(|| !pid_alive(daemon_pid as u32), Duration::from_secs(5)),
        "daemon {daemon_pid} did not die"
    );

    // The daemon is raised again lazily; the first answer may be a cold-start
    // `[indexing]`, so wait for a real one.
    let args = json!({"path": abs(&main_rs), "line": 1, "column": 4});
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut text: String;
    loop {
        text = result_text(&client.call("lsp_hover", args.clone()).await);
        if !text.contains("[daemon_unavailable]") && !text.contains("[indexing]") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the call after the kill never succeeded: {text}"
        );
        sleep(Duration::from_millis(300)).await;
    }
    assert!(text.contains("helper"), "unexpected answer: {text}");
    assert!(client.pid_alive(), "opencraylsp-mcp died with the daemon");
    let new_pid = env.status()["daemon"]["pid"]
        .as_u64()
        .expect("new daemon pid");
    assert_ne!(
        new_pid, daemon_pid,
        "a fresh daemon should have been started"
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T13

#[tokio::test]
async fn t13_a_daemon_speaking_another_protocol_is_reported() {
    let env = TestEnv::new();
    let socket = env.dir().join("protocol-99.sock");
    let listener =
        std::os::unix::net::UnixListener::bind(&socket).expect("bind protocol-99 listener");
    let server = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            let mut line = String::new();
            while std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) > 0 {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    line.clear();
                    continue;
                };
                if value["method"] == "hello" {
                    let response = json!({
                        "jsonrpc": "2.0", "id": value["id"].clone(),
                        "result": {"protocol": 99, "daemon_version": "x", "pid": 1,
                                   "languages": [], "language_mode": "auto"},
                    });
                    use std::io::Write as _;
                    let _ = writeln!(writer, "{response}");
                    break;
                }
                line.clear();
            }
        }
    });

    let client = McpClient::start_with_socket(&env, &socket, &["--languages", "rust"]);
    client.initialize().await;
    let reply = client.call("lsp_status", json!({})).await;
    let text = result_text(&reply);
    assert!(
        text.contains("[daemon_unavailable]") && text.contains("protocol mismatch"),
        "unexpected reply: {text}"
    );
    assert!(
        text.contains("opencraylspd restart"),
        "no restart hint: {text}"
    );

    client.shutdown();
    drop(server);
}

// ----------------------------------------------------------------- T14

#[tokio::test]
async fn t14_concurrent_calls_across_clients_do_not_mix_up() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    let mut files = Vec::new();
    for index in 0..100 {
        files.push(workspace_file(
            &env,
            &format!("f{index}.rs"),
            &format!("TOKEN_{index}\n"),
        ));
    }
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    env.start_daemon();

    let started = Instant::now();
    let mut clients = Vec::new();
    for _ in 0..5 {
        let client = Arc::new(McpClient::start(&env, &["--languages", "rust"]));
        client.initialize().await;
        clients.push(client);
    }

    let mut tasks = Vec::new();
    for (client_index, client) in clients.iter().enumerate() {
        for slot in 0..20 {
            let index = client_index * 20 + slot;
            let client = Arc::clone(client);
            let path = abs(&files[index]);
            let token = format!("TOKEN_{index}");
            tasks.push(tokio::spawn(async move {
                let reply = client
                    .call("lsp_hover", json!({"path": path, "line": 1, "column": 1}))
                    .await;
                (token, result_text(&reply), is_error(&reply))
            }));
        }
    }
    for task in tasks {
        let (token, text, error) = task.await.expect("join");
        assert!(!error, "{token} was a tool error: {text}");
        assert!(
            text.contains(&token),
            "reply for {token} was {text:?} - responses were mixed up"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "100 concurrent calls took {:?}",
        started.elapsed()
    );

    for client in clients {
        Arc::try_unwrap(client).expect("last owner").shutdown();
    }
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T17a

#[tokio::test]
async fn t17a_declared_languages_do_not_get_others_started() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    workspace_file(&env, "main.rs", "fn helper() {}\n");
    workspace_file(&env, "go.mod", "module e2e\n");
    let go_file = workspace_file(&env, "main.go", "package main\n");
    let php_file = workspace_file(&env, "index.php", "<?php\n");
    env.start_daemon();

    let rust_client = McpClient::start(&env, &["--languages", "rust"]);
    let other_client = McpClient::start(&env, &["--languages", "go,php"]);
    rust_client.initialize().await;
    other_client.initialize().await;

    let reply = rust_client
        .call("lsp_outline", json!({"path": abs(&go_file)}))
        .await;
    let text = result_text(&reply);
    assert!(
        text.contains("[language_disabled]") && text.contains("rust"),
        "rust client should not use go: {text}"
    );

    let reply = other_client
        .call("lsp_outline", json!({"path": abs(&php_file)}))
        .await;
    let text = result_text(&reply);
    assert!(
        !text.contains("[language_disabled]"),
        "php should be enabled: {text}"
    );

    // A symbol search without a path must only touch enabled languages.
    rust_client
        .call("lsp_find_symbol", json!({"query": "Nothing"}))
        .await;
    let report = env.status();
    let servers: Vec<String> = report["instances"]
        .as_array()
        .expect("instances")
        .iter()
        .map(|instance| instance["server"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        !servers.contains(&"gopls".to_owned()),
        "a rust-only client must not start go: {report}"
    );
    assert!(
        servers.contains(&"rust-analyzer".to_owned()),
        "the rust server should be running: {report}"
    );

    rust_client.shutdown();
    other_client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T17b

#[tokio::test]
async fn t17b_typescript_and_javascript_share_one_instance() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "tsconfig.json", "{}\n");
    let ts_file = workspace_file(&env, "a.ts", "const alpha = 1;\n");
    let js_file = workspace_file(&env, "b.js", "const beta = 2;\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "ts,js"]);
    client.initialize().await;
    for path in [&ts_file, &js_file] {
        let reply = client.call("lsp_outline", json!({"path": abs(path)})).await;
        let text = result_text(&reply);
        assert!(
            !text.contains("[language_disabled]"),
            "{} is enabled and must not be rejected: {text}",
            path.display()
        );
    }
    let report = env.status();
    let instances = report["instances"].as_array().expect("instances");
    assert_eq!(instances.len(), 1, "ts and js share one server: {report}");
    assert_eq!(instances[0]["server"], "typescript-language-server");

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T17c

#[tokio::test]
async fn t17c_an_unknown_language_exits_two_without_connecting() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    env.start_daemon();
    let before = env.status()["daemon"]["clients"].as_u64().expect("clients");

    let output = run_once(&env, &["--languages", "klingon"]);
    assert_eq!(output.status.code(), Some(2), "unknown language is fatal");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("valid:"), "stderr: {stderr}");
    assert_eq!(
        stderr.matches("valid:").count(),
        1,
        "the message must not be nested twice: {stderr}"
    );

    let after = env.status()["daemon"]["clients"].as_u64().expect("clients");
    assert_eq!(after, before, "an unknown language must not connect");

    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T17d

#[tokio::test]
async fn t17d_auto_starts_nothing_until_a_call() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &[]);
    client.initialize().await;
    assert!(
        env.status()["instances"]
            .as_array()
            .is_none_or(|i| i.is_empty()),
        "nothing may start before a call: {}",
        env.status()
    );

    let reply = client
        .call(
            "lsp_hover",
            json!({"path": abs(&main_rs), "line": 1, "column": 4}),
        )
        .await;
    assert!(
        !is_error(&reply),
        "auto rust failed: {}",
        result_text(&reply)
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T17e

/// A fresh machine: the socket's parent directory does not exist yet, and no
/// daemon is running. `opencraylsp-mcp` must create the directory, start `opencraylspd`, and
/// answer its first `tools/call` — the harness is never allowed to wait.
#[tokio::test]
async fn t17e_a_fresh_home_and_a_missing_socket_directory_still_connect() {
    let env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");

    let socket = env
        .dir()
        .join("nested")
        .join("deeper")
        .join("opencraylsp.sock");
    assert!(
        !socket.parent().expect("socket parent").exists(),
        "the test needs a socket whose directory does not exist yet"
    );

    let client = McpClient::start_with_socket(&env, &socket, &["--languages", "rust"]);
    // `initialize` returns without waiting for a daemon; the first call is what
    // has to bring one up, socket directory and all.
    client.initialize().await;

    let reply = client.call("lsp_status", json!({})).await;
    assert!(
        !is_error(&reply),
        "lsp_status failed on a fresh machine: {}",
        result_text(&reply)
    );
    assert!(
        socket.exists(),
        "opencraylsp-mcp did not bring up a daemon at {}",
        socket.display()
    );

    let pid = status_at(&env, &socket)["daemon"]["pid"]
        .as_u64()
        .expect("daemon pid") as u32;
    client.shutdown();
    stop_daemon_at(&env, &socket);
    assert!(
        wait_until(|| !pid_alive(pid), Duration::from_secs(5)),
        "the daemon {pid} outlived the test"
    );
}

// ----------------------------------------------------------------- T17f

/// `workspace/symbol` needs a loaded project on some servers, so the daemon
/// opens one source file before the first such request. That open is
/// remembered per instance, so a later `workspace/symbol` does not probe
/// again even after the probed file has left the open-document LRU.
#[tokio::test]
async fn t17f_the_workspace_symbol_probe_opens_a_file_only_once() {
    let mut env = TestEnv::new();
    let events = env.dir().join("events.log");
    let servers = vec![
        ServerSpec::fake("rust-analyzer", &[("rs", "rust")])
            .root_marker("Cargo.toml")
            .arg(format!("--record-events={}", events.display())),
    ];
    let limits = Limits {
        // One open document, so opening another file evicts the probe.
        max_open_docs: Some(1),
        ..Limits::default()
    };
    env.write_config(&servers, &limits);
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let probe = workspace_file(&env, "a.rs", "pub fn helper() {}\n");
    let other = workspace_file(&env, "b.rs", "pub fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;

    // 1. A workspace/symbol request probes (opens) `a.rs`.
    client
        .call("lsp_find_symbol", json!({ "query": "helper" }))
        .await;
    // 2. A request on `b.rs` opens it and evicts the probe from the LRU.
    client
        .call(
            "lsp_references",
            json!({ "path": abs(&other), "line": 1, "column": 4 }),
        )
        .await;
    // 3. Another workspace/symbol request must not probe again.
    client
        .call("lsp_find_symbol", json!({ "query": "helper" }))
        .await;
    client.shutdown();

    let events = std::fs::read_to_string(&events).unwrap_or_default();
    let count = |verb: &str, path: &PathBuf| -> usize {
        let uri = format!("file://{}", abs(path));
        events
            .lines()
            .filter(|line| line.starts_with(&format!("{verb} {uri}")))
            .count()
    };
    let probe_opens = count("didOpen", &probe);
    assert!(
        probe_opens >= 1,
        "the probe file was never opened; workspace/symbol had no project:\n{events}"
    );
    assert_eq!(
        count("didOpen", &other),
        1,
        "the request on b.rs should have opened it once:\n{events}"
    );
    assert!(
        count("didClose", &probe) >= 1,
        "the probe should have been evicted by max_open_docs = 1:\n{events}"
    );
    assert_eq!(
        probe_opens, 1,
        "the probe file was opened {probe_opens} times; the instance must remember that a \
         project file is already open:\n{events}"
    );

    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- stop

#[tokio::test]
async fn stopping_the_daemon_ends_every_language_server() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;
    client
        .call(
            "lsp_hover",
            json!({"path": abs(&main_rs), "line": 1, "column": 4}),
        )
        .await;
    let pid = instance_pid(&env.status(), "rust-analyzer").expect("instance up");

    env.stop_daemon();
    assert!(!env.socket().exists(), "the socket file was left behind");
    assert!(
        wait_until(|| !pid_alive(pid), Duration::from_secs(5)),
        "the fake language server {pid} outlived the daemon"
    );

    client.shutdown();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- meta

#[test]
fn every_test_gets_a_home_inside_its_tempdir() {
    let env = TestEnv::new();
    assert!(
        env.home().starts_with(env.dir()),
        "HOME {} is not under {}",
        env.home().display(),
        env.dir().display()
    );
    let output = env
        .command(Path::new("/usr/bin/env"))
        .output()
        .expect("run env");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.lines()
            .any(|line| line == format!("HOME={}", env.home().display())),
        "a child did not see the tempdir HOME: {text}"
    );
    assert!(
        !text.contains("XDG_CONFIG_HOME") && !text.contains("XDG_RUNTIME_DIR"),
        "XDG variables leaked into a child: {text}"
    );
}

// ----------------------------------------------------------------- T10

#[tokio::test]
async fn t10_the_mcp_session_matches_the_golden_transcript() {
    let mut env = TestEnv::new();
    env.write_config(&standard_servers(), &Limits::default());
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;
    let tools = client.list_tools().await;
    assert_eq!(tools.len(), 11, "the catalogue is the 11 lsp_* tools");
    client.call("lsp_nope", json!({"path": "x.rs"})).await;
    client.ping().await;
    // A cancellation is a notification: no reply, and the session keeps going.
    client.cancel(999_999).await;
    assert!(
        !client.ping().await["result"].is_null(),
        "the session is still alive after a cancellation"
    );

    // The golden file holds the four request/response exchanges (the ping after
    // the cancellation is compared too, which proves the session continued).
    let golden_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/mcp_session.jsonl");
    let mut actual = client.raw_lines();
    // Drop the trailing liveness ping's two lines? No: keep them, the golden
    // documents them.
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_path.parent().unwrap()).expect("golden dir");
        std::fs::write(&golden_path, format!("{}\n", actual.join("\n"))).expect("write golden");
    }
    let expected = std::fs::read_to_string(&golden_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", golden_path.display()));
    let expected: Vec<String> = expected.lines().map(str::to_owned).collect();
    actual.retain(|line| !line.is_empty());
    assert_eq!(
        actual, expected,
        "the MCP transcript changed; re-run with UPDATE_GOLDEN=1 after reviewing"
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- P1-1 (client bound)

/// A daemon that answers `hello` and then goes silent must not hang the
/// client: the request bound turns it into `[timeout]` (P1-1, client side).
#[tokio::test]
async fn a_silent_daemon_is_bounded_by_the_request_deadline() {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("silent.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (read_half, mut write) = stream.into_split();
        let mut lines = tokio::io::BufReader::new(read_half).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            // Answer only the handshake; every request after it is dropped.
            if value["method"] == "hello" {
                let response = serde_json::json!({
                    "jsonrpc": "2.0", "id": value["id"].clone(),
                    "result": {"protocol": 1, "daemon_version": "0.1.0-silent", "pid": 1,
                               "languages": [], "language_mode": "auto"},
                });
                let _ = write.write_all(format!("{response}\n").as_bytes()).await;
                let _ = write.flush().await;
            }
        }
    });

    let mut options = opencraylsp_client::ClientOptions::default_for_tests();
    options.socket = socket.clone();
    options.workspace = dir.path().to_owned();
    options.spawn = false;
    options.connect_deadline = Duration::from_secs(2);
    options.request_deadline = Duration::from_millis(300);
    let client = opencraylsp_client::DaemonClient::connect(options)
        .await
        .expect("connect");

    let started = Instant::now();
    let error = client
        .call_tool(
            "lsp_status",
            json!({}),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("the daemon never answers");
    assert!(
        format!("{error}").starts_with("[timeout]"),
        "unexpected error: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the bound was not applied: {:?}",
        started.elapsed()
    );
    server.abort();
}

// ------------------------------------------- the daemon's own memory ceiling

/// A daemon that exceeds `daemon_max_rss_mb` shuts itself down, and the next
/// tool call brings a fresh one back.
///
/// The ceiling here is 1 MB, which no daemon can live under — that is the
/// point: the guard has to be reachable without arranging a real leak. What is
/// *not* faked is everything around it. A real `opencraylspd` samples its own
/// `/proc/self/status`, finds itself over the line, drains, removes its socket
/// and exits; a real `opencraylsp-mcp` notices the socket is gone and starts a
/// replacement. The test asserts on process ids and on answers, never on a
/// mock.
#[tokio::test]
async fn a_daemon_over_its_own_ceiling_exits_and_the_client_brings_a_new_one() {
    let mut env = TestEnv::new();
    // 1 MB: unreachable, so the first sample trips the guard. Sampled every
    // 250 ms so the test does not wait out the 5 s default.
    env.write_config(
        &standard_servers(),
        &Limits {
            daemon_max_rss_mb: Some(1),
            memory_sample_ms: Some(250),
            ..Limits::default()
        },
    );
    workspace_file(&env, "Cargo.toml", "[package]\nname = \"e2e\"\n");
    let main_rs = workspace_file(&env, "main.rs", "fn helper() {}\n");
    env.start_daemon();
    let first_pid = env.status()["daemon"]["pid"].as_u64().expect("daemon pid") as u32;
    // And the ceiling really is reported, before anything happens.
    assert_eq!(
        env.status()["daemon"]["max_rss_mb"],
        json!(1),
        "the ceiling must be visible in status"
    );

    // The daemon must leave on its own, without anybody asking it to. The
    // socket going is the first sign, since that is what a client sees; the
    // process itself is reaped first, because a child that has exited but not
    // been waited for still has a `/proc` entry and would read as alive.
    assert!(
        wait_until(|| !env.socket().exists(), Duration::from_secs(15)),
        "the socket outlived the daemon; log:\n{}",
        env.daemon_log_tail(50)
    );
    env.reap();
    assert!(
        wait_async(|| !pid_alive(first_pid), Duration::from_secs(10)).await,
        "the daemon over its own ceiling never exited; log:\n{}",
        env.daemon_log_tail(50)
    );
    // It exited because it was over the ceiling, and it said why.
    let log = env.daemon_log_tail(200);
    assert!(
        log.contains("daemon exceeds its own memory ceiling"),
        "the exit must be logged with its reason; log:\n{log}"
    );
    env.reap();

    // And the client's next call is what brings a replacement back.
    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;
    let args = json!({"path": abs(&main_rs), "line": 1, "column": 4});
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut text: String;
    loop {
        text = result_text(&client.call("lsp_hover", args.clone()).await);
        if !text.contains("[daemon_unavailable]") && !text.contains("[indexing]") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no daemon ever came back: {text}\nlog:\n{}",
            env.daemon_log_tail(50)
        );
        sleep(Duration::from_millis(300)).await;
    }
    assert!(text.contains("helper"), "unexpected answer: {text}");
    assert!(client.pid_alive(), "opencraylsp-mcp died with the daemon");
    let second_pid = env.status()["daemon"]["pid"]
        .as_u64()
        .expect("replacement pid");
    assert_ne!(
        second_pid, first_pid as u64,
        "the replacement must be a new process"
    );

    // The replacement is over the ceiling too, and the history now says so: it
    // is on its second over-limit exit, so it still restarts. This is what
    // keeps the loop from being closed on the very first one.
    // The replacement is not this test's child, so it cannot be reaped; its
    // socket disappearing is the observable fact instead.
    assert!(
        wait_until(|| !env.socket().exists(), Duration::from_secs(15)),
        "the replacement is over the ceiling too and must leave; log:\n{}",
        env.daemon_log_tail(50)
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

// ----------------------------------------------------------------- T18

/// A workspace whose boundary is not a project: the only cargo workspace sits
/// one directory down. A request that names no file must go to that project and
/// must not leave a second server running on the boundary, which would index
/// nothing while holding gigabytes.
#[tokio::test]
async fn t18_a_workspace_request_uses_the_project_below_the_boundary() {
    let mut env = TestEnv::new();
    let servers =
        vec![ServerSpec::fake("rust-analyzer", &[("rs", "rust")]).root_marker("Cargo.toml")];
    env.write_config(&servers, &Limits::default());
    // No Cargo.toml at the boundary itself.
    workspace_file(&env, "proj/Cargo.toml", "[package]\nname = \"e2e\"\n");
    workspace_file(&env, "proj/main.rs", "fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;
    // The first call starts the server and is told it is still indexing; the
    // point of the test is which root it started at, so wait for the index
    // rather than asserting on an answer that is legitimately empty for now.
    let mut answered = false;
    for _ in 0..20 {
        let reply = client
            .call("lsp_find_symbol", json!({"query": "helper"}))
            .await;
        let text = result_text(&reply);
        if text.starts_with("[indexing]") {
            sleep(Duration::from_millis(500)).await;
            continue;
        }
        assert!(
            !is_error(&reply),
            "the project below the boundary must be used: {text}"
        );
        assert!(text.contains("helper"), "unexpected answer: {text}");
        answered = true;
        break;
    }
    assert!(
        answered,
        "the index never finished; log:\n{}",
        env.daemon_log_tail(50)
    );

    let roots: Vec<String> = env.status()["instances"]
        .as_array()
        .expect("instances")
        .iter()
        .map(|instance| instance["root"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        roots,
        vec![abs(&env.workspace().join("proj"))],
        "exactly one instance, on the project, not on the boundary"
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}

/// Several projects under one boundary and a request that names no file: there
/// is no honest way to pick, so the answer names them and asks for a `path` —
/// and starts nothing.
#[tokio::test]
async fn t18b_several_projects_are_named_instead_of_one_being_guessed() {
    let mut env = TestEnv::new();
    let servers =
        vec![ServerSpec::fake("rust-analyzer", &[("rs", "rust")]).root_marker("Cargo.toml")];
    env.write_config(&servers, &Limits::default());
    workspace_file(&env, "one/Cargo.toml", "[package]\nname = \"one\"\n");
    workspace_file(&env, "one/main.rs", "fn helper() {}\n");
    workspace_file(&env, "two/Cargo.toml", "[package]\nname = \"two\"\n");
    workspace_file(&env, "two/main.rs", "fn helper() {}\n");
    env.start_daemon();

    let client = McpClient::start(&env, &["--languages", "rust"]);
    client.initialize().await;
    let reply = client
        .call("lsp_find_symbol", json!({"query": "helper"}))
        .await;
    let text = result_text(&reply);
    assert!(
        text.contains("[no_project]"),
        "expected [no_project]: {text}"
    );
    assert!(text.contains("one"), "the candidates must be named: {text}");
    assert!(text.contains("two"), "the candidates must be named: {text}");
    assert!(
        env.status()["instances"]
            .as_array()
            .is_none_or(|i| i.is_empty()),
        "a request that cannot be routed must start nothing: {}",
        env.status()
    );

    // The point of printing the candidates is that one of them can be used. A
    // hint the model cannot paste is worse than none, so this test pastes one.
    let retry: Value = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .expect("the answer must print a ready-to-use call")
        .parse()
        .expect("that call must be JSON");
    let path = env
        .workspace()
        .join(retry["path"].as_str().expect("a path"));
    assert!(
        path.is_file(),
        "the printed path must be a file: the server is chosen by extension, so a project \
         directory would fail with `no LSP server is configured for . files`"
    );

    let reply = client.call("lsp_hover", retry).await;
    let answered = result_text(&reply);
    assert!(
        !is_error(&reply),
        "the printed retry must work as it stands: {answered}"
    );
    let roots: Vec<String> = env.status()["instances"]
        .as_array()
        .expect("instances")
        .iter()
        .map(|instance| instance["root"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        roots,
        vec![abs(&env.workspace().join("one"))],
        "and only that project's server may be running"
    );

    client.shutdown();
    env.stop_daemon();
    env.assert_daemon_gone();
}
