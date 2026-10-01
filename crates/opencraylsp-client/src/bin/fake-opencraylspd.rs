//! A stand-in for `opencraylspd serve`, used by the spawn tests of `opencraylsp-client`.
//!
//! It does what the real daemon does at the level the client can observe: take
//! `<socket>.lock`, bind the socket, answer `hello`, `tools/list`,
//! `tools/call`, `status` and `shutdown`, and exit when the socket goes away or
//! `shutdown` arrives. It starts no language server.
//!
//! Only compiled with the `test-fake-daemon` feature, so it never ships.

#![cfg(feature = "test-fake-daemon")]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Reports a fatal problem on stderr.
///
/// Written through `Write` rather than `eprintln!` because the workspace
/// forbids print macros outright; stdout is untouched, since this process is a
/// stand-in for a daemon that never writes to the protocol stream.
fn fail(message: &str) {
    let _ = std::io::stderr().write_all(format!("{message}\n").as_bytes());
}

/// `serve --socket <path>`; anything else is refused so a typo cannot hang.
fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            fail(&format!("fake-opencraylspd: cannot start the runtime: {e}"));
            return ExitCode::FAILURE;
        }
    };
    real_main(runtime)
}

fn real_main(runtime: tokio::runtime::Runtime) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("serve") {
        fail("fake-opencraylspd: usage: fake-opencraylspd serve --socket <path>");
        return ExitCode::from(2);
    }
    let socket = args
        .windows(2)
        .find(|pair| pair[0] == "--socket")
        .map(|pair| PathBuf::from(&pair[1]));
    let Some(socket) = socket else {
        fail("fake-opencraylspd: --socket <path> is required");
        return ExitCode::from(2);
    };
    // Record the argv so a test can prove what the client passed (e.g.
    // `--config`). One argument per line, next to the socket.
    let _ = std::fs::write(
        PathBuf::from(format!("{}.argv", socket.display())),
        format!("{}\n", args.join("\n")),
    );
    match runtime.block_on(run(socket)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            fail(&format!("fake-opencraylspd: {e}"));
            ExitCode::FAILURE
        }
    }
}

async fn run(socket: PathBuf) -> Result<(), String> {
    let socket_for_cleanup = socket.clone();
    // The single-instance lock, exactly like the real daemon: whoever holds it
    // owns the socket path, and everyone else must exit quietly.
    let lock_path = opencraylsp_proto::paths::lock_path(&socket);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    lock.try_lock()
        .map_err(|_| "another opencraylspd already owns this socket".to_owned())?;

    // A corpse socket is cleared by the daemon that owns the lock, never by a
    // client.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).map_err(|e| format!("cannot bind: {e}"))?;

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return Ok(());
        };
        let socket = socket_for_cleanup.clone();
        tokio::spawn(async move {
            if serve_connection(stream).await {
                // The daemon owns the socket path: it is the one
                // that removes it on the way out, never a client.
                let _ = std::fs::remove_file(&socket);
                std::process::exit(0);
            }
        });
    }
}

/// Returns `true` when the daemon should shut down.
async fn serve_connection(stream: UnixStream) -> bool {
    let (read_half, mut write) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = value["id"].clone();
        let response = match value["method"].as_str().unwrap_or_default() {
            "hello" => ok(
                id,
                json!({
                    "protocol": 1,
                    "daemon_version": "0.1.0-fake-opencraylspd",
                    "pid": std::process::id(),
                    "languages": ["rust"],
                    "language_mode": "auto",
                }),
            ),
            "tools/list" => ok(
                id,
                json!({"tools": [{
                    "name": "lsp_status",
                    "description": "Report daemon and instance state.",
                    "input_schema": {"type": "object"},
                }]}),
            ),
            "tools/call" => ok(id, json!({"text": "ran", "is_error": false})),
            "status" => ok(
                id,
                json!({
                    "daemon": {"version": "0.1.0-fake-opencraylspd", "pid": std::process::id(),
                               "uptime_secs": 1, "rss_bytes": 512, "clients": 1},
                    "limits": {"max_instances": 8, "max_rss_mb": 6144,
                               "idle_shutdown_secs": 900, "max_open_docs": 256},
                    "enabled_languages": ["rust"],
                    "language_mode": "auto",
                    "not_installed": [],
                    "instances": [],
                }),
            ),
            "shutdown" => {
                let _ = write
                    .write_all(format!("{}\n", ok(id, json!({}))).as_bytes())
                    .await;
                let _ = write.flush().await;
                return true;
            }
            other => err(id, -32601, &format!("unknown method `{other}`")),
        };
        if write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            return false;
        }
        let _ = write.flush().await;
    }
    false
}

fn ok(id: Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn err(id: Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}
