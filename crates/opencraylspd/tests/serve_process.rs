//! The real `opencraylspd` binary: startup, single instance, signals and exit codes.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn opencraylspd() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylspd"));
    // Never let a test read or write the real home.
    command
        .env("HOME", "/nonexistent-home-for-tests")
        .env_remove("XDG_CONFIG_HOME");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

struct Running {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
    dir: tempfile::TempDir,
}

impl Running {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let log = dir.path().join("opencraylsp.log");
        let child = opencraylspd()
            .args(["serve", "--socket"])
            .arg(&socket)
            .arg("--log-file")
            .arg(&log)
            .spawn()
            .expect("opencraylspd starts");
        let started = Instant::now();
        while UnixStream::connect(&socket).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "opencraylspd never listened"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            child,
            socket,
            log,
            dir,
        }
    }

    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "opencraylspd did not exit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn round_trip(socket: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn the_binary_serves_the_protocol_and_leaves_a_private_socket() {
    use std::os::unix::fs::PermissionsExt;
    let running = Running::start();
    let mode = std::fs::metadata(&running.socket)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let reply = round_trip(
        &running.socket,
        json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{
            "protocol":1,"client":{"name":"t","version":"0"},
            "workspace": running.dir.path().display().to_string()}}),
    );
    assert_eq!(reply["result"]["protocol"], 1);
    assert_eq!(reply["result"]["pid"], running.child.id());
}

#[test]
fn sigterm_shuts_down_cleanly_and_removes_the_socket() {
    let mut running = Running::start();
    running.signal("TERM");
    let status = running.wait_exit();
    assert!(status.success(), "{status:?}");
    assert!(!running.socket.exists());
    let log = std::fs::read_to_string(&running.log).unwrap();
    assert!(
        log.contains("SIGTERM received") && log.contains("opencraylspd stopped"),
        "{log}"
    );
}

#[test]
fn sigint_is_handled_like_sigterm() {
    let mut running = Running::start();
    running.signal("INT");
    assert!(running.wait_exit().success());
    assert!(!running.socket.exists());
}

#[test]
fn a_second_serve_on_the_same_socket_exits_quietly_and_the_first_survives() {
    let mut running = Running::start();
    let second = opencraylspd()
        .args(["serve", "--socket"])
        .arg(&running.socket)
        .arg("--log-file")
        .arg(running.dir.path().join("second.log"))
        .status()
        .unwrap();
    assert!(
        second.success(),
        "already running is not an error: {second:?}"
    );
    // The first daemon is still answering.
    let reply = round_trip(
        &running.socket,
        json!({"jsonrpc":"2.0","id":1,"method":"status","params":{}}),
    );
    assert_eq!(reply["error"]["code"], -32002);
    assert!(running.child.try_wait().unwrap().is_none());
}

#[test]
fn a_broken_config_stops_the_daemon_with_exit_code_two() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("bad.toml");
    std::fs::write(&config, "this is = [not valid").unwrap();
    let status = opencraylspd()
        .args(["serve", "--socket"])
        .arg(dir.path().join("s.sock"))
        .arg("--config")
        .arg(&config)
        .arg("--log-file")
        .arg(dir.path().join("l.log"))
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
    assert!(!dir.path().join("s.sock").exists());
}

#[test]
fn a_missing_explicit_config_is_also_exit_code_two() {
    let dir = tempfile::tempdir().unwrap();
    let status = opencraylspd()
        .args(["serve", "--socket"])
        .arg(dir.path().join("s.sock"))
        .arg("--config")
        .arg(dir.path().join("nope.toml"))
        .arg("--log-file")
        .arg(dir.path().join("l.log"))
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}

#[test]
fn the_config_file_changes_what_hello_reports() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("c.toml");
    std::fs::write(
        &config,
        "[[server]]\nname='zig'\ncommand='zls'\nextensions={zig='zig'}\n",
    )
    .unwrap();
    let socket = dir.path().join("s.sock");
    let mut child = opencraylspd()
        .args(["serve", "--socket"])
        .arg(&socket)
        .arg("--config")
        .arg(&config)
        .arg("--log-file")
        .arg(dir.path().join("l.log"))
        .spawn()
        .unwrap();
    let started = Instant::now();
    while UnixStream::connect(&socket).is_err() {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }
    // `zig` is a custom language provided by the configured server.
    let reply = round_trip(
        &socket,
        json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{
            "protocol":1,"client":{"name":"t","version":"0"},
            "workspace": dir.path().display().to_string(), "languages":["zig"]}}),
    );
    assert_eq!(reply["result"]["languages"], json!(["zig"]), "{reply}");
    let _ = child.kill();
    let _ = child.wait();
}
