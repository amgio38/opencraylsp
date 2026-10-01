//! Cleaning up daemons the client spawned.
//!
//! A daemon started by [`crate::spawn`] detaches into its own process group,
//! so it outlives the client that started it - which is exactly
//! what we want in production and exactly what makes a test leak processes.
//! [`SpawnGuard`] is the test's answer: it remembers the pid, and on drop -
//! including the panic path - asks the daemon to shut down and then makes sure
//! the process is gone.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A daemon started during a test, killed when the test ends.
#[derive(Debug)]
pub struct SpawnGuard {
    pids: Mutex<Vec<u32>>,
    socket: PathBuf,
}

impl SpawnGuard {
    /// Guards one socket. Nothing is recorded until [`SpawnGuard::record`] runs.
    pub fn new(socket: PathBuf) -> Self {
        Self {
            pids: Mutex::new(Vec::new()),
            socket,
        }
    }

    /// Remembers a pid the client spawned.
    pub fn record(&self, pid: u32) {
        self.pids.lock().unwrap().push(pid);
    }

    /// Pids this guard is responsible for.
    pub fn pids(&self) -> Vec<u32> {
        self.pids.lock().unwrap().clone()
    }

    /// Asks the daemon to shut down, then waits for it to be reaped.
    ///
    /// Asynchronous on purpose: the sleep has to yield to the runtime, or the
    /// task the client spawned to `wait()` on the child never runs and the
    /// process would sit in the table as a zombie for the rest of the test.
    pub async fn shutdown_and_wait(&self) {
        let _ = request_shutdown(&self.socket);
        self.wait_until_gone().await;
    }

    /// Signals anything left and gives it a moment to disappear.
    async fn wait_until_gone(&self) {
        for pid in self.pids() {
            // SIGTERM first: the daemon removes its socket on the way out.
            signal_pid(pid, false);
            let deadline = Instant::now() + Duration::from_secs(2);
            while is_alive(pid) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if is_alive(pid) {
                signal_pid(pid, true);
                let deadline = Instant::now() + Duration::from_secs(2);
                while is_alive(pid) && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    /// Synchronous best effort, for the drop path where nothing may await.
    fn kill_now(&self) {
        for pid in self.pids() {
            if is_alive(pid) {
                signal_pid(pid, true);
            }
        }
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        // Runs on the panic path too, which is the whole point: a failing test
        // must not leave daemons behind for the next run to trip over. Drop
        // cannot await, so this only signals; the tests that care about a
        // clean process table call `shutdown_and_wait` first.
        self.kill_now();
    }
}

/// Whether a process with this pid exists.
pub fn is_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Sends the daemon protocol's `shutdown` and waits for the socket to go.
///
/// Returns whether the socket disappeared within the timeout.
pub fn request_shutdown(socket: &std::path::Path) -> std::io::Result<bool> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket)?;
    let mut writer = stream.try_clone()?;
    let hello = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "hello",
        "params": {
            "protocol": 1,
            "client": {"name": "opencraylsp-client-test", "version": "0"},
            "workspace": "/tmp",
        }
    });
    writeln!(writer, "{hello}")?;
    writer.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    writeln!(
        writer,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": {}})
    )?;
    writer.flush()?;
    line.clear();
    let _ = reader.read_line(&mut line);

    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(!socket.exists())
}

/// Signals a process. `std` has no kill, and the rules forbid matching our own
/// command line with `pkill -f`, so the signal goes out by pid.
fn signal_pid(pid: u32, force: bool) {
    // The pid is a number, never a pattern, so this cannot match anything but
    // the process it names.
    let signal = if force { "-9" } else { "-15" };
    // Absolute paths: the test's PATH may not contain either, and a bare
    // `kill` that fails silently would leak the very process we are cleaning.
    for kill in ["/bin/kill", "/usr/bin/kill"] {
        if std::path::Path::new(kill).exists()
            && std::process::Command::new(kill)
                .arg(signal)
                .arg(pid.to_string())
                .status()
                .is_ok_and(|status| status.success())
        {
            return;
        }
    }
}
