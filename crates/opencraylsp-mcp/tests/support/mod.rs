//! A scripted `opencraylspd` for integration tests: enough of protocol v1 to let the
//! real client connect, list tools and call one.
//!
//! Shared by several test binaries, so not every helper is used by each one.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// A fake daemon listening on a tempdir socket.
pub struct FakeDaemon {
    connections: Arc<Mutex<usize>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDaemon {
    pub async fn start(socket: PathBuf) -> Self {
        let listener = UnixListener::bind(&socket).expect("bind the fake daemon");
        let connections = Arc::new(Mutex::new(0usize));
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let counter = Arc::clone(&connections);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { return };
                        *counter.lock().unwrap() += 1;
                        tokio::spawn(serve(stream));
                    }
                }
            }
        });
        Self {
            connections,
            shutdown: Some(tx),
            task,
        }
    }

    /// How many connections were accepted.
    pub fn connections(&self) -> usize {
        *self.connections.lock().unwrap()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.task.abort();
    }
}

async fn serve(stream: UnixStream) {
    let (read_half, mut write) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = value["id"].clone();
        let method = value["method"].as_str().unwrap_or_default();
        let result = match method {
            "hello" => json!({
                "protocol": 1,
                "daemon_version": "0.1.0-fake",
                "pid": std::process::id(),
                "languages": ["rust"],
                "language_mode": "auto",
            }),
            "tools/list" => json!({
                "tools": [{
                    "name": "lsp_status",
                    "description": "status",
                    "input_schema": {"type": "object"},
                }],
            }),
            "tools/call" => json!({"text": "ran", "is_error": false}),
            "status" => json!({
                "daemon": {"version": "0.1.0-fake", "pid": 4242, "uptime_secs": 1,
                           "rss_bytes": 1024, "clients": 1},
                "limits": {"max_instances": 8, "max_rss_mb": 6144,
                           "idle_shutdown_secs": 900, "max_open_docs": 256},
                "enabled_languages": ["rust"],
                "language_mode": "auto",
                "not_installed": [],
                "instances": [],
            }),
            "shutdown" => json!({}),
            other => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("unknown method `{other}`")},
                });
                let _ = write.write_all(format!("{response}\n").as_bytes()).await;
                continue;
            }
        };
        let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let _ = write.write_all(format!("{response}\n").as_bytes()).await;
    }
}
