//! `TestEnv`: a tempdir with a real `opencraylspd` in it.
//!
//! Socket, config, workspace, daemon log and `HOME` all live under one
//! tempdir. `start_daemon` runs the real binary; `Drop` stops it and waits for
//! the pid to disappear, so a failing test cannot leak a daemon.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::binaries::binaries;

/// One `[[server]]` entry pointing at a fake language server.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub extensions: Vec<(String, String)>,
    pub root_markers: Vec<String>,
}

impl ServerSpec {
    /// A fake server for `name`, serving `extensions`.
    pub fn fake(name: &str, extensions: &[(&str, &str)]) -> Self {
        Self {
            name: name.to_owned(),
            command: binaries().fake_lsp.clone(),
            args: Vec::new(),
            extensions: extensions
                .iter()
                .map(|(ext, lang)| ((*ext).to_owned(), (*lang).to_owned()))
                .collect(),
            root_markers: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn root_marker(mut self, marker: &str) -> Self {
        self.root_markers.push(marker.to_owned());
        self
    }

    fn to_toml(&self) -> String {
        let mut out = String::from("[[server]]\n");
        out.push_str(&format!("name = \"{}\"\n", self.name));
        out.push_str(&format!("command = \"{}\"\n", self.command.display()));
        if !self.args.is_empty() {
            let args: Vec<String> = self.args.iter().map(|a| format!("\"{a}\"")).collect();
            out.push_str(&format!("args = [{}]\n", args.join(", ")));
        }
        if !self.root_markers.is_empty() {
            let markers: Vec<String> = self
                .root_markers
                .iter()
                .map(|m| format!("\"{m}\""))
                .collect();
            out.push_str(&format!("root_markers = [{}]\n", markers.join(", ")));
        }
        out.push_str("[server.extensions]\n");
        for (ext, lang) in &self.extensions {
            out.push_str(&format!("{ext} = \"{lang}\"\n"));
        }
        out
    }
}

/// Resource limits written into the config.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    pub idle_shutdown_secs: Option<u64>,
    pub max_rss_mb: Option<u64>,
    pub memory_sample_ms: Option<u64>,
    pub max_instances: Option<usize>,
    pub max_open_docs: Option<usize>,
    /// The daemon's own ceiling. Only ever set absurdly low in these tests, to
    /// make the guard fire without needing a real leak.
    pub daemon_max_rss_mb: Option<u64>,
}

impl Limits {
    fn to_toml(&self) -> String {
        let mut out = String::from("[limits]\n");
        if let Some(value) = self.idle_shutdown_secs {
            out.push_str(&format!("idle_shutdown_secs = {value}\n"));
        }
        if let Some(value) = self.max_rss_mb {
            out.push_str(&format!("max_rss_mb = {value}\n"));
        }
        if let Some(value) = self.memory_sample_ms {
            out.push_str(&format!("memory_sample_ms = {value}\n"));
        }
        if let Some(value) = self.max_instances {
            out.push_str(&format!("max_instances = {value}\n"));
        }
        if let Some(value) = self.max_open_docs {
            out.push_str(&format!("max_open_docs = {value}\n"));
        }
        if let Some(value) = self.daemon_max_rss_mb {
            out.push_str(&format!("daemon_max_rss_mb = {value}\n"));
        }
        out
    }
}

/// A tempdir, the paths inside it, and the daemon started there.
pub struct TestEnv {
    dir: tempfile::TempDir,
    home: PathBuf,
    socket: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
    log: PathBuf,
    daemon: Option<Child>,
    daemon_pid: Option<u32>,
}

impl std::fmt::Debug for TestEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestEnv")
            .field("socket", &self.socket)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl TestEnv {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        // The config lives at the default path. A daemon that `opencraylsp-mcp` starts
        // after a crash gets no `--config`, so this is the only way a revived
        // daemon sees the test's servers.
        let config = home.join(".config").join("opencraylsp").join("config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).expect("config dir");
        Self {
            socket: dir.path().join("opencraylsp.sock"),
            config,
            log: dir.path().join("opencraylsp.log"),
            home,
            workspace,
            dir,
            daemon: None,
            daemon_pid: None,
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn daemon_pid(&self) -> Option<u32> {
        self.daemon_pid
    }

    /// Writes the config the daemon should load.
    pub fn write_config(&self, servers: &[ServerSpec], limits: &Limits) {
        let mut text = limits.to_toml();
        for server in servers {
            text.push('\n');
            text.push_str(&server.to_toml());
        }
        std::fs::write(&self.config, text).expect("write config");
    }

    /// A command with the daemon-safe environment: `HOME` is the tempdir and
    /// the XDG variables are gone, so nothing can reach the real home.
    pub fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("XDG_STATE_HOME")
            .current_dir(&self.workspace);
        command
    }

    /// Starts the real `opencraylspd serve` and waits for its socket.
    pub fn start_daemon(&mut self) {
        assert!(
            self.daemon.is_none(),
            "start_daemon called twice on the same TestEnv"
        );
        let child = self
            .command(&binaries().opencraylspd)
            .args(["serve", "--socket"])
            .arg(&self.socket)
            .arg("--log-file")
            .arg(&self.log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn opencraylspd serve");
        self.daemon_pid = Some(child.id());
        self.daemon = Some(child);
        assert!(
            wait_until(
                || std::os::unix::net::UnixStream::connect(&self.socket).is_ok(),
                Duration::from_secs(10),
            ),
            "opencraylspd never listened on {}",
            self.socket.display()
        );
    }

    /// `opencraylspd status --json` against this env's socket.
    pub fn status(&self) -> Value {
        let output = self
            .command(&binaries().opencraylspd)
            .args(["status", "--json", "--socket"])
            .arg(&self.socket)
            .output()
            .expect("run opencraylspd status");
        assert!(
            output.status.success(),
            "opencraylspd status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("status --json is not JSON: {e}"))
    }

    /// The tail of the daemon's log, for failure messages.
    pub fn daemon_log_tail(&self, lines: usize) -> String {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        let start = all.len().saturating_sub(lines);
        all[start..].join("\n")
    }

    /// Asks the daemon to stop, then waits for it to be gone.
    pub fn stop_daemon(&mut self) {
        let _ = self
            .command(&binaries().opencraylspd)
            .args(["stop", "--socket"])
            .arg(&self.socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = wait_until(|| !self.socket.exists(), Duration::from_secs(10));
        self.reap();
    }

    /// Kills the process we started and waits for it (panic-safe).
    pub fn reap(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Asserts the daemon pid is gone; use at the end of a test.
    pub fn assert_daemon_gone(&mut self) {
        self.reap();
        if let Some(pid) = self.daemon_pid.take() {
            assert!(
                wait_until(
                    || !PathBuf::from(format!("/proc/{pid}")).exists(),
                    Duration::from_secs(5)
                ),
                "daemon pid {pid} is still alive after the test"
            );
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = self
            .command(&binaries().opencraylspd)
            .args(["stop", "--socket"])
            .arg(&self.socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        self.reap();
    }
}

/// Polls `condition` until it holds or `timeout` passes.
pub fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Fake stand-ins for the built-in presets, with the same root markers so
/// `auto` detection behaves like production. Overriding the preset names keeps
/// the real servers (rust-analyzer on PATH, …) out of the tests.
pub fn standard_servers() -> Vec<ServerSpec> {
    vec![
        ServerSpec::fake("rust-analyzer", &[("rs", "rust")]).root_marker("Cargo.toml"),
        ServerSpec::fake("gopls", &[("go", "go")]).root_marker("go.mod"),
        ServerSpec::fake("intelephense", &[("php", "php")]).root_marker("composer.json"),
        ServerSpec::fake(
            "typescript-language-server",
            &[("ts", "typescript"), ("js", "javascript")],
        )
        .root_marker("tsconfig.json"),
    ]
}
