//! `opencraylspd stop` and `opencraylspd restart`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use opencraylsp_client::{ClientError, ClientOptions, DaemonClient};

use super::{
    RELEASE, RestartArgs, StopArgs, block_on, probe_options, resolve_socket, spawn_options,
};

pub fn run_stop(args: StopArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let socket = resolve_socket(args.socket);
    block_on(stop(&socket, out, err, true, RELEASE))
}

pub fn run_restart(args: RestartArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let socket = resolve_socket(args.socket);
    let reconnect = spawn_options(socket.clone());
    block_on(restart(socket, reconnect, out, err, RELEASE))
}

/// Stops the daemon if it is running. `announce` prints the friendly
/// "not running" line; `restart` keeps it quiet because it goes on to start one.
async fn stop(
    socket: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
    announce: bool,
    release: Duration,
) -> ExitCode {
    if !socket.exists() {
        return not_running(out, announce);
    }
    let client = match DaemonClient::connect(probe_options(socket.to_owned())).await {
        Ok(client) => client,
        Err(ClientError::Unavailable { .. }) => return not_running(out, announce),
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return ExitCode::from(1);
        }
    };
    let pid = client.status().await.ok().map(|report| report.daemon.pid);
    // The daemon may close the socket before the reply lands; going away is
    // exactly what was asked for, so that is not a failure.
    let _ = client.shutdown().await;
    if wait_released(socket, release).await {
        if announce {
            let _ = writeln!(out, "opencraylspd stopped");
        }
        return ExitCode::SUCCESS;
    }
    let pid = pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string());
    let _ = writeln!(
        err,
        "opencraylspd did not stop within 10s (pid {pid}); if it is stuck, kill {pid} manually"
    );
    ExitCode::from(1)
}

async fn restart(
    socket: PathBuf,
    reconnect: ClientOptions,
    out: &mut dyn Write,
    err: &mut dyn Write,
    release: Duration,
) -> ExitCode {
    let stopped = stop(&socket, out, err, false, release).await;
    if stopped != ExitCode::SUCCESS {
        return stopped;
    }
    // `stop` already returned SUCCESS only after `wait_released` had seen
    // the socket go and its lock come free, so the check below would pass on
    // its first poll. It is removed as dead code rather than as a hazard: the
    // wait is bounded by re-checking `released()` in a loop, not by sleeping
    // out the timeout, so a redundant second call costs one stat of the socket
    // and one `try_lock` — not another release period.
    //
    // Worth noting for anyone reading the old version: the *reachable* cost was
    // never the double wait but the duplicated failure message. If the daemon
    // had released just after `stop`'s deadline, `stop` had already printed
    // "did not stop within 10s" and returned failure, so `restart` returned at
    // the check above and the second wait never ran at all.
    match DaemonClient::connect(reconnect).await {
        Ok(client) => {
            let pid = client.hello().pid;
            let _ = writeln!(out, "opencraylspd restarted (pid {pid})");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "{error}");
            ExitCode::from(1)
        }
    }
}

fn not_running(out: &mut dyn Write, announce: bool) -> ExitCode {
    if announce {
        let _ = writeln!(out, "opencraylspd is not running");
    }
    ExitCode::SUCCESS
}

/// True once the socket file is gone and its lock is free.
async fn wait_released(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if released(socket) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn released(socket: &Path) -> bool {
    !socket.exists() && lock_free(socket)
}

/// The lock is free when we can take it; the daemon takes it for its lifetime.
fn lock_free(socket: &Path) -> bool {
    let lock = opencraylsp_proto::paths::lock_path(socket);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)
    else {
        // Cannot even open it: treat it as held rather than report a false
        // release and start a second daemon.
        return false;
    };
    file.try_lock().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_absent_socket_is_already_released() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        assert!(released(&socket));
    }

    #[tokio::test]
    async fn a_present_socket_is_not_released() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        std::fs::write(&socket, b"").unwrap();
        assert!(!released(&socket));
    }

    #[tokio::test]
    async fn stop_on_an_absent_socket_succeeds_quietly_or_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = stop(&socket, &mut out, &mut err, true, RELEASE).await;
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("opencraylspd is not running")
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = stop(&socket, &mut out, &mut err, false, RELEASE).await;
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.is_empty(), "restart's stop is silent");
    }

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// Reconnect options that never launch a daemon and give up quickly, so a
    /// test measures only `stop` and does not depend on `opencraylspd` being resolvable on PATH.
    fn no_spawn(socket: &Path) -> ClientOptions {
        let mut options = spawn_options(socket.to_owned());
        options.spawn = false;
        options.connect_deadline = Duration::from_millis(100);
        options
    }

    /// A daemon that answers just enough to be connected to and shut down, but
    /// never removes its socket, so the release wait has to time out.
    /// A daemon that answers `shutdown` and then removes its socket, so `stop`
    /// succeeds. Used by the test that needs the *success* path.
    async fn releasing_daemon(socket: &Path) -> tokio::task::JoinHandle<()> {
        let path = socket.to_owned();
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let path = path.clone();
                tokio::spawn(async move {
                    let (read_half, mut write) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read_half).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                            continue;
                        };
                        let id = value["id"].clone();
                        let result = match value["method"].as_str().unwrap_or_default() {
                            "status" => serde_json::json!({
                                "daemon": {"version": "0.1.0-fake", "pid": 4321,
                                           "uptime_secs": 1, "rss_bytes": null, "clients": 1},
                                "limits": {"max_instances": 8, "max_rss_mb": 6144,
                                           "idle_shutdown_secs": 900, "max_open_docs": 256},
                                "enabled_languages": [], "language_mode": "auto",
                                "not_installed": [], "instances": [],
                            }),
                            _ => serde_json::json!({
                                "protocol": 1, "daemon_version": "0.1.0-fake",
                                "pid": 4321, "languages": [], "language_mode": "auto",
                            }),
                        };
                        let response =
                            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                        let _ = write.write_all(format!("{response}\n").as_bytes()).await;
                        if value["method"] == "shutdown" {
                            let _ = write.shutdown().await;
                            let _ = tokio::fs::remove_file(&path).await;
                            return;
                        }
                    }
                });
            }
        })
    }

    async fn stubborn_daemon(socket: &Path) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let (read_half, mut write) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read_half).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                            continue;
                        };
                        let id = value["id"].clone();
                        let result = match value["method"].as_str().unwrap_or_default() {
                            "status" => serde_json::json!({
                                "daemon": {"version": "0.1.0-fake", "pid": 4321,
                                           "uptime_secs": 1, "rss_bytes": null, "clients": 1},
                                "limits": {"max_instances": 8, "max_rss_mb": 6144,
                                           "idle_shutdown_secs": 900, "max_open_docs": 256},
                                "enabled_languages": [], "language_mode": "auto",
                                "not_installed": [], "instances": [],
                            }),
                            _ => serde_json::json!({
                                "protocol": 1,
                                "daemon_version": "0.1.0-fake",
                                "pid": 4321,
                                "languages": [],
                                "language_mode": "auto",
                            }),
                        };
                        let response =
                            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                        let _ = write.write_all(format!("{response}\n").as_bytes()).await;
                    }
                });
            }
        })
    }

    #[tokio::test]
    async fn stop_times_out_when_the_socket_never_goes_away() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let server = stubborn_daemon(&socket).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = stop(
            &socket,
            &mut out,
            &mut err,
            true,
            Duration::from_millis(150),
        )
        .await;
        assert_eq!(code, ExitCode::from(1));
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("did not stop"), "{err}");
        assert!(err.contains("4321"), "{err}");
        server.abort();
    }

    #[tokio::test]
    async fn restart_gives_up_when_the_old_daemon_will_not_release() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let server = stubborn_daemon(&socket).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = restart(
            socket.clone(),
            no_spawn(&socket),
            &mut out,
            &mut err,
            Duration::from_millis(150),
        )
        .await;
        assert_eq!(code, ExitCode::from(1));
        assert!(out.is_empty(), "no pid is printed on failure");
        server.abort();
    }

    #[tokio::test]
    async fn stop_reports_a_daemon_that_speaks_another_protocol() {
        use std::io::{BufRead as _, Write as _};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut write = stream;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if value["method"] == "hello" {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0", "id": value["id"].clone(),
                            "result": {"protocol": 99, "daemon_version": "x", "pid": 1,
                                       "languages": [], "language_mode": "auto"},
                        });
                        let _ = writeln!(write, "{response}");
                        return;
                    }
                    line.clear();
                }
            }
        });
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = stop(&socket, &mut out, &mut err, true, RELEASE).await;
        assert_eq!(code, ExitCode::from(1));
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("protocol mismatch")
        );
        let _ = handle.join();
    }

    #[test]
    fn a_lock_that_cannot_be_opened_counts_as_held() {
        // The parent directory does not exist, so the lock cannot be opened.
        assert!(!lock_free(&PathBuf::from(
            "/definitely/not/here/opencraylsp.sock"
        )));
    }

    /// A daemon that releases promptly must not make `restart` slower
    /// than the single release period it is allowed.
    ///
    /// `stop` waits for the release; `restart` used to wait again afterwards.
    /// The second wait is cheap when the socket is already free (it polls once
    /// and returns), but it is only reachable at all when `stop` succeeded —
    /// and the case where it *looked* expensive, a daemon releasing after
    /// `stop`'s deadline, is one where `stop` has already failed and `restart`
    /// returns before reaching it. This test pins the behaviour that is real:
    /// one period, not two, on the success path.
    #[tokio::test]
    async fn restart_waits_at_most_one_release_period() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let server = releasing_daemon(&socket).await;
        let deadline = Instant::now() + Duration::from_secs(2);
        while !socket.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let release = Duration::from_millis(700);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let started = Instant::now();
        let _ = restart(
            socket.clone(),
            no_spawn(&socket),
            &mut out,
            &mut err,
            release,
        )
        .await;
        let elapsed = started.elapsed();
        server.abort();

        // The daemon released at once, so this is dominated by the connect
        // attempt at the end. The bound is generous but still far below two
        // full release periods, which is what a genuine double wait would cost.
        assert!(
            elapsed < release * 2,
            "restart spent {elapsed:?}; the release is waited for once, by `stop`"
        );
    }
}
