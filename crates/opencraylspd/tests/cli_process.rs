//! `opencraylspd status`/`stop`/`restart` driven as a real process against a real
//! `opencraylspd serve`.
//!
//! Every test gets its own tempdir for the socket, the log and `HOME`, and
//! removes the XDG variables, so neither the real home nor the default socket
//! is ever touched. A [`Daemon`] guard kills whatever this test started, and
//! each test asserts the pid is gone before it returns - a leaked language
//! server or daemon fails the suite instead of lingering.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use opencraylsp_proto::StatusReport;

fn base_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylspd"));
    command
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn home(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn cli(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = base_command(&home(dir))
        .args(args)
        .output()
        .expect("run opencraylspd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A daemon this test started, killed on drop even if the test panics.
struct Daemon {
    dir: tempfile::TempDir,
    socket: PathBuf,
    child: Option<Child>,
    pid: u32,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let log = dir.path().join("opencraylsp.log");
        let child = base_command(&home(dir.path()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(["serve", "--socket"])
            .arg(&socket)
            .arg("--log-file")
            .arg(&log)
            .spawn()
            .expect("opencraylspd starts");
        let pid = child.id();
        let started = Instant::now();
        while UnixStream::connect(&socket).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "opencraylspd never listened on {}",
                socket.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            dir,
            socket,
            child: Some(child),
            pid,
        }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    /// Kills the child and reaps it; safe during unwinding.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// `stop`, then insist the pid is really gone.
    fn stop_and_assert(&mut self) {
        self.stop();
        assert!(
            wait_pid_gone(self.pid, Duration::from_secs(5)),
            "daemon pid {} still exists after the test",
            self.pid
        );
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        // A daemon started by `restart` is not a `Child` of ours; ask the CLI
        // to stop whatever still owns this socket, so nothing is left behind
        // even when the test panics.
        let _ = base_command(&home(self.dir.path()))
            .args(["stop", "--socket"])
            .arg(&self.socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn wait_pid_gone(pid: u32, timeout: Duration) -> bool {
    let path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + timeout;
    while path.exists() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn wait_socket_gone(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while socket.exists() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn pid_from_restart(stdout: &str) -> u32 {
    let after = stdout.split("pid ").nth(1).expect("pid in output");
    after
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("pid digits")
}

#[test]
fn status_reports_not_running_without_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    let (code, out, _) = cli(
        dir.path(),
        &["status", "--socket", socket.to_str().unwrap()],
    );
    assert_eq!(code, 1);
    assert!(out.contains("opencraylspd is not running"), "{out}");
}

#[test]
fn status_json_without_a_daemon_still_prints_json_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    let (code, out, err) = cli(
        dir.path(),
        &["status", "--json", "--socket", socket.to_str().unwrap()],
    );
    assert_eq!(code, 1, "not running is a failure exit");
    let value: serde_json::Value =
        serde_json::from_str(&out).expect("stdout must be JSON even with no daemon");
    assert_eq!(value["running"], false, "{value}");
    assert_eq!(value["socket"], socket.to_str().unwrap(), "{value}");
    assert!(err.contains("opencraylspd is not running"), "{err}");
}

#[test]
fn stop_without_a_daemon_is_a_success() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    let (code, out, _) = cli(dir.path(), &["stop", "--socket", socket.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("opencraylspd is not running"), "{out}");
}

#[test]
fn status_json_and_stop_work_against_a_real_daemon() {
    let mut daemon = Daemon::start();
    let dir = daemon.dir().to_owned();
    let socket = daemon.socket().to_str().unwrap().to_owned();

    let (code, out, err) = cli(&dir, &["status", "--socket", &socket]);
    assert_eq!(code, 0, "status: {err}");
    assert!(out.contains("daemon  pid"), "{out}");

    let (code, out, err) = cli(&dir, &["status", "--json", "--socket", &socket]);
    assert_eq!(code, 0, "status --json: {err}");
    let report: StatusReport = serde_json::from_str(&out).expect("the JSON is a StatusReport");
    assert_eq!(report.daemon.pid, daemon.pid);

    let (code, out, err) = cli(&dir, &["stop", "--socket", &socket]);
    assert_eq!(code, 0, "stop: {err}");
    assert!(out.contains("opencraylspd stopped"), "{out}");
    assert!(
        wait_socket_gone(daemon.socket(), Duration::from_secs(5)),
        "stop left the socket behind"
    );
    daemon.stop_and_assert();
}

#[test]
fn restart_kills_the_old_daemon_and_starts_a_reachable_new_one() {
    let mut daemon = Daemon::start();
    let old_pid = daemon.pid;
    let dir = daemon.dir().to_owned();
    let socket = daemon.socket().to_str().unwrap().to_owned();

    let (code, out, err) = cli(&dir, &["restart", "--socket", &socket]);
    assert_eq!(code, 0, "restart: {err}");
    let new_pid = pid_from_restart(&out);
    assert_ne!(new_pid, old_pid, "restart must replace the daemon");
    // Reap our child first: an exited-but-unreaped process is still a zombie
    // in `/proc`, which would look alive.
    daemon.stop();
    assert!(
        wait_pid_gone(old_pid, Duration::from_secs(5)),
        "the old daemon {old_pid} is still alive"
    );
    assert!(
        PathBuf::from(format!("/proc/{new_pid}")).exists(),
        "the new daemon {new_pid} is not running"
    );

    // The new socket must be live: a status against it has to succeed.
    let (code, out, err) = cli(&dir, &["status", "--socket", &socket]);
    assert_eq!(code, 0, "status after restart: {err}");
    assert!(out.contains("daemon  pid"), "{out}");

    // The restarted daemon was not our child, so stop it explicitly.
    let (code, _, err) = cli(&dir, &["stop", "--socket", &socket]);
    assert_eq!(code, 0, "stop after restart: {err}");
    assert!(
        wait_pid_gone(new_pid, Duration::from_secs(5)),
        "the restarted daemon {new_pid} leaked"
    );
    daemon.stop_and_assert();
}

#[test]
fn version_prints_the_binary_and_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (code, out, err) = cli(dir.path(), &["version"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("opencraylspd "), "{out}");
    assert!(out.contains("protocol"), "{out}");
}

#[test]
fn doctor_lists_the_presets_and_the_workspace_languages() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    let (code, out, err) = cli(
        dir.path(),
        &["doctor", "--socket", socket.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("rust-analyzer"), "{out}");
    assert!(out.contains("socket:"), "{out}");
    assert!(out.contains("config:"), "{out}");
    assert!(out.contains("auto-detected languages"), "{out}");
}

#[test]
fn doctor_json_is_a_structured_report() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    let (code, out, err) = cli(
        dir.path(),
        &["doctor", "--json", "--socket", socket.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).expect("doctor --json");
    assert!(value["servers"].is_array(), "{value}");
    assert!(value["socket"].is_string(), "{value}");
    assert!(value["workspace"].is_string(), "{value}");
}

#[test]
fn doctor_refuses_a_broken_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = home(dir.path()).join(".config").join("opencraylsp");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "this is not = toml [").unwrap();
    let socket = dir.path().join("absent.sock");
    let (code, _, err) = cli(
        dir.path(),
        &["doctor", "--socket", socket.to_str().unwrap()],
    );
    assert_eq!(code, 2);
    assert!(err.contains("cannot load the config"), "{err}");
}

#[test]
fn a_stale_socket_is_reported_as_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("stale.sock");
    std::fs::write(&socket, b"").unwrap();
    let socket = socket.to_str().unwrap();

    let (code, out, _) = cli(dir.path(), &["status", "--socket", socket]);
    assert_eq!(code, 1);
    assert!(out.contains("opencraylspd is not running"), "{out}");

    let (code, out, _) = cli(dir.path(), &["stop", "--socket", socket]);
    assert_eq!(code, 0);
    assert!(out.contains("opencraylspd is not running"), "{out}");
}
