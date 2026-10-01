//! `opencraylspd doctor`: is each configured language server installed, what version
//! is it, and which languages will this workspace auto-enable?
//!
//! It never starts a language server to analyse anything: the only process it
//! runs is `<command> --version`, under a short timeout.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use opencraylsp_client::DaemonClient;
use opencraylsp_core::{LspConfig, ServerConfig, languages};
use serde::{Deserialize, Serialize};

use super::{DoctorArgs, block_on, probe_options, resolve_socket};

/// How long `<command> --version` gets before it is killed. Generous on
/// purpose: pyright answers in a few seconds on a slow machine, and a false
/// `<timeout>` is worse than a slow `doctor`.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// What `doctor` learned about one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerReport {
    pub name: String,
    pub installed: bool,
    /// Absolute path of the resolved executable, when found.
    pub path: Option<PathBuf>,
    /// First line of `--version` output; `<timeout>` if it never answered.
    pub version: Option<String>,
    pub version_timed_out: bool,
    pub languages: Vec<String>,
    pub extensions: Vec<String>,
    /// Install command, when the server is missing and it is a known preset.
    pub hint: Option<String>,
}

/// Everything `doctor` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub servers: Vec<ServerReport>,
    pub socket: PathBuf,
    pub daemon_running: bool,
    pub config_path: Option<PathBuf>,
    pub config_exists: bool,
    pub workspace: PathBuf,
    /// Languages the workspace's project markers will enable (`auto`).
    pub auto_languages: Vec<String>,
}

pub fn run(args: DoctorArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let socket = resolve_socket(args.socket);
    let config_path = opencraylsp_core::config::default_config_path();
    let config = match LspConfig::load(None) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(err, "error: cannot load the config: {error}");
            return ExitCode::from(2);
        }
    };
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path_env = std::env::var_os("PATH");
    block_on(async move {
        let report = build_report(
            &config,
            &socket,
            path_env.as_deref(),
            config_path.as_deref(),
            &workspace,
            VERSION_TIMEOUT,
        )
        .await;
        if args.json {
            if let Err(error) = serde_json::to_writer_pretty(&mut *out, &report) {
                let _ = writeln!(err, "error: cannot write the report: {error}");
                return ExitCode::from(1);
            }
            let _ = writeln!(out);
        } else if render(&report, out).is_err() {
            return ExitCode::from(1);
        }
        ExitCode::SUCCESS
    })
}

/// Gathers the report. `path_env`, `config_path` and `timeout` are injected so
/// tests can use tempdir scripts and a short timeout.
pub async fn build_report(
    config: &LspConfig,
    socket: &Path,
    path_env: Option<&OsStr>,
    config_path: Option<&Path>,
    workspace: &Path,
    timeout: Duration,
) -> DoctorReport {
    let mut servers = Vec::new();
    for (name, server) in &config.servers {
        let path = find_command(&server.command, path_env);
        let (version, version_timed_out) = match &path {
            Some(path) => match command_version(path, timeout).await {
                Probe::Version(version) => (Some(version), false),
                Probe::TimedOut => (Some("<timeout>".to_owned()), true),
                Probe::Unusable => (None, false),
            },
            None => (None, false),
        };
        let installed = path.is_some();
        servers.push(ServerReport {
            name: name.clone(),
            installed,
            path,
            version,
            version_timed_out,
            languages: languages::server_languages(server).into_iter().collect(),
            extensions: server.extensions.keys().cloned().collect(),
            hint: if installed {
                None
            } else {
                install_hint(name, server)
            },
        });
    }
    let daemon_running = socket.exists()
        && DaemonClient::connect(probe_options(socket.to_owned()))
            .await
            .is_ok();
    DoctorReport {
        servers,
        socket: socket.to_owned(),
        daemon_running,
        config_exists: config_path.map(|path| path.exists()).unwrap_or(false),
        config_path: config_path.map(Path::to_owned),
        auto_languages: languages::detect(workspace, &config.servers)
            .into_iter()
            .collect(),
        workspace: workspace.to_owned(),
    }
}

/// The path of `command`: absolute as given, otherwise looked up on `PATH`.
fn find_command(command: &str, path_env: Option<&OsStr>) -> Option<PathBuf> {
    let candidate = Path::new(command);
    if candidate.is_absolute() {
        return candidate.is_file().then(|| candidate.to_owned());
    }
    let path = path_env?;
    std::env::split_paths(path)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

enum Probe {
    Version(String),
    TimedOut,
    Unusable,
}

/// All version probes share one gate: `doctor` can be asked about several
/// servers at once, and a machine under load will refuse to fork if every
/// probe spawns at the same instant. Serialising them costs nothing (each
/// takes milliseconds) and keeps the result about the server, not the load.
fn probe_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Runs `<command> --version` without a shell and kills it on timeout.
async fn command_version(program: &Path, timeout: Duration) -> Probe {
    let _gate = probe_gate().lock().await;
    let mut last = Probe::Unusable;
    for attempt in 0..3 {
        last = probe_once(program, timeout).await;
        // A failed fork under load is transient; a server that is genuinely
        // not runnable stays unusable across the retries.
        if !matches!(last, Probe::Unusable) {
            return last;
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    last
}

/// Most bytes of a probe's stdout kept.
///
/// `Command::output()` buffers a child's *entire* output before returning, so
/// a misbehaving or hostile `--version` — one that prints forever — is
/// collected in full before the timeout can stop it. The only thing needed is
/// the first non-empty line, so the child's stdout is taken as a stream and
/// cut off well before that.
const PROBE_STDOUT_LIMIT: usize = 8 * 1024;

/// Reads at most [`PROBE_STDOUT_LIMIT`] of the child's stdout and returns its
/// first non-empty line.
///
/// Read line by line rather than `read_to_end`: only the first line is ever
/// needed, and `read_to_end` waits for EOF — which a child that prints forever
/// never sends. Reading incrementally means the answer is available as soon as
/// the first line is, and the cap bounds a child whose *first* line is
/// pathologically long.
async fn first_stdout_line(child: &mut tokio::process::Child) -> Option<String> {
    let stdout = child.stdout.take()?;
    first_line_of(stdout).await
}

/// [`first_stdout_line`] over any reader, so the read bound can be tested
/// without a child process.
async fn first_line_of<R: tokio::io::AsyncRead + Unpin>(stdout: R) -> Option<String> {
    use tokio::io::AsyncBufReadExt;
    // The limit is on the *read*, not on the line. `lines()` would buffer one
    // enormous line in full before this function could refuse it, which is the
    // very thing the cap exists to prevent, so the stream itself is bounded and
    // a line that still does not fit is dropped whole rather than half-kept.
    let capped = tokio::io::AsyncReadExt::take(stdout, PROBE_STDOUT_LIMIT as u64);
    let mut reader = tokio::io::BufReader::new(capped);
    let mut budget = PROBE_STDOUT_LIMIT;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        if line.len() > budget {
            return None;
        }
        budget -= line.len();
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
        if budget == 0 {
            return None;
        }
    }
}

async fn probe_once(program: &Path, timeout: Duration) -> Probe {
    let mut command = tokio::process::Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Probe::Unusable,
    };
    let probe = tokio::time::timeout(timeout, async {
        // Read the bounded head of stdout first: `wait()` is what reports a
        // clean exit, and reading first means a huge output cannot be buffered.
        let line = first_stdout_line(&mut child).await;
        let status = child.wait().await;
        match status {
            // Only a clean exit with a non-empty stdout line is a version. A
            // server that does not support `--version` (pyright,
            // intelephense) prints an error or a usage message; that text is
            // not a version.
            Ok(status) if status.success() => line.map_or(Probe::Unusable, Probe::Version),
            // A server that does not support `--version` exits non-zero after
            // printing a usage or error message; that text is not a version, so
            // a clean exit really is required. The bound is paid for elsewhere:
            // the line was already read before `wait()`, so a chatty server
            // never had its whole output buffered.
            _ => Probe::Unusable,
        }
    })
    .await;
    // The child is dropped either way; `kill_on_drop` stops one that ignored
    // the timeout and is still printing.
    match probe {
        Ok(probe) => probe,
        Err(_) => Probe::TimedOut,
    }
}

/// The documented install hint for a missing preset.
fn install_hint(name: &str, _server: &ServerConfig) -> Option<String> {
    let hint = match name {
        "rust-analyzer" => {
            "rustup component add rust-analyzer  (must match the toolchain pinned in rust-toolchain.toml)"
        }
        "gopls" => "go install golang.org/x/tools/gopls@latest",
        "intelephense" => "npm i -g intelephense",
        "typescript-language-server" => {
            "npm i -g typescript typescript-language-server  (needs TS 5.x tsserver; a global TS 7 has no tsserver.js, so point initialization_options.tsserver.path at it)"
        }
        "pyright-langserver" => "npm i -g pyright",
        _ => return None,
    };
    Some(hint.to_owned())
}

fn render(report: &DoctorReport, out: &mut dyn Write) -> std::io::Result<()> {
    for server in &report.servers {
        let path = server
            .path
            .as_ref()
            .map_or_else(|| "-".to_owned(), |path| path.display().to_string());
        let version = match (&server.version, server.installed) {
            (Some(version), _) => version.clone(),
            (None, true) => "unknown".to_owned(),
            (None, false) => "-".to_owned(),
        };
        writeln!(
            out,
            "{:<26} {:<10} {:<28} {:<18} languages: {}  extensions: {}",
            server.name,
            if server.installed {
                "installed"
            } else {
                "missing"
            },
            path,
            version,
            if server.languages.is_empty() {
                "-".to_owned()
            } else {
                server.languages.join(",")
            },
            server.extensions.join(",")
        )?;
        if let Some(hint) = &server.hint {
            writeln!(out, "  install: {hint}")?;
        }
    }
    writeln!(
        out,
        "socket: {}  (daemon: {})",
        report.socket.display(),
        if report.daemon_running {
            "running"
        } else {
            "not running"
        }
    )?;
    writeln!(
        out,
        "config: {}  ({})",
        report
            .config_path
            .as_ref()
            .map_or_else(|| "none".to_owned(), |path| path.display().to_string()),
        if report.config_exists {
            "present"
        } else {
            "missing"
        }
    )?;
    writeln!(
        out,
        "workspace: {}  auto-detected languages: {}",
        report.workspace.display(),
        if report.auto_languages.is_empty() {
            "none".to_owned()
        } else {
            report.auto_languages.join(", ")
        }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn server(command: &str, extensions: &[(&str, &str)]) -> ServerConfig {
        ServerConfig {
            command: command.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            extensions: extensions
                .iter()
                .map(|(ext, lang)| ((*ext).to_owned(), (*lang).to_owned()))
                .collect(),
            root_markers: Vec::new(),
            workspace: None,
            initialization_options: None,
            settings: None,
        }
    }

    fn config_with(servers: Vec<(&str, ServerConfig)>) -> LspConfig {
        LspConfig {
            servers: servers
                .into_iter()
                .map(|(name, server)| (name.to_owned(), server))
                .collect(),
            ..LspConfig::default()
        }
    }

    #[tokio::test]
    async fn an_installed_server_reports_its_version() {
        let dir = tempfile::tempdir().unwrap();
        script(dir.path(), "fake-ls", "#!/bin/sh\necho 'fake-ls 1.2.3'\n");
        let config = config_with(vec![("fake", server("fake-ls", &[("rs", "rust")]))]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        let entry = &report.servers[0];
        assert!(entry.installed, "{entry:?}");
        assert_eq!(entry.path, Some(dir.path().join("fake-ls")));
        assert_eq!(entry.version.as_deref(), Some("fake-ls 1.2.3"));
        assert!(!entry.version_timed_out);
        assert_eq!(entry.extensions, vec!["rs".to_owned()]);
    }

    #[tokio::test]
    async fn a_missing_server_is_not_installed_and_shows_no_version() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(vec![(
            "custom",
            server("definitely-not-here", &[("x", "x")]),
        )]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_millis(200),
        )
        .await;
        let entry = &report.servers[0];
        assert!(!entry.installed);
        assert_eq!(entry.path, None);
        assert_eq!(entry.version, None);
    }

    #[tokio::test]
    async fn a_hanging_version_is_killed_and_marked() {
        let dir = tempfile::tempdir().unwrap();
        // `exec` so the shell is replaced and killing the child kills `sleep`.
        script(dir.path(), "hang-ls", "#!/bin/sh\nexec sleep 999\n");
        let config = config_with(vec![("hang", server("hang-ls", &[("rs", "rust")]))]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_millis(150),
        )
        .await;
        let entry = &report.servers[0];
        assert!(entry.installed);
        assert!(entry.version_timed_out);
        assert_eq!(entry.version.as_deref(), Some("<timeout>"));
    }

    #[tokio::test]
    async fn an_absolute_command_is_used_without_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "abs-ls", "#!/bin/sh\necho abs 9\n");
        let config = config_with(vec![(
            "abs",
            server(path.to_str().unwrap(), &[("rs", "rust")]),
        )]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            None,
            None,
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        assert!(report.servers[0].installed);
        assert_eq!(report.servers[0].version.as_deref(), Some("abs 9"));
    }

    #[tokio::test]
    async fn presets_that_are_missing_come_with_an_install_hint() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(vec![(
            "rust-analyzer",
            server("definitely-not-rust-analyzer", &[("rs", "rust")]),
        )]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_millis(200),
        )
        .await;
        let hint = report.servers[0].hint.as_deref().unwrap();
        assert!(
            hint.contains("rustup component add rust-analyzer"),
            "{hint}"
        );
    }

    #[tokio::test]
    async fn auto_languages_come_from_the_workspace_markers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let config = config_with(vec![(
            "rust-analyzer",
            ServerConfig {
                root_markers: vec!["Cargo.toml".to_owned()],
                ..server("definitely-not-rust-analyzer", &[("rs", "rust")])
            },
        )]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(report.auto_languages, vec!["rust".to_owned()]);
    }

    #[test]
    fn the_json_form_round_trips() {
        let report = DoctorReport {
            servers: vec![ServerReport {
                name: "rust-analyzer".into(),
                installed: false,
                path: None,
                version: None,
                version_timed_out: false,
                languages: vec!["rust".into()],
                extensions: vec!["rs".into()],
                hint: Some("rustup component add rust-analyzer".into()),
            }],
            socket: PathBuf::from("/tmp/opencraylsp.sock"),
            daemon_running: false,
            config_path: Some(PathBuf::from("/home/u/.config/opencraylsp/config.toml")),
            config_exists: false,
            workspace: PathBuf::from("/ws"),
            auto_languages: vec!["rust".into()],
        };
        let text = serde_json::to_string(&report).unwrap();
        let parsed: DoctorReport = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, report);

        let mut out = Vec::new();
        render(&report, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("rust-analyzer"), "{text}");
        assert!(
            text.contains("install: rustup component add rust-analyzer"),
            "{text}"
        );
        assert!(text.contains("auto-detected languages: rust"), "{text}");
    }

    // Keeps `dir_os` used; documents that PATH values are plain paths.
    #[test]
    fn path_injection_uses_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "probe", "#!/bin/sh\necho probe\n");
        assert_eq!(
            find_command("probe", Some(dir.path().as_os_str())),
            Some(path)
        );
    }

    #[test]
    fn each_known_preset_has_an_install_hint() {
        for name in [
            "rust-analyzer",
            "gopls",
            "intelephense",
            "typescript-language-server",
            "pyright-langserver",
        ] {
            assert!(
                install_hint(name, &server("x", &[])).is_some(),
                "missing hint for {name}"
            );
        }
        assert!(install_hint("custom", &server("x", &[])).is_none());
    }

    #[tokio::test]
    async fn an_absolute_command_that_does_not_exist_is_not_installed() {
        let config = config_with(vec![("abs", server("/definitely/not/here", &[]))]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            None,
            None,
            Path::new("/"),
            Duration::from_millis(50),
        )
        .await;
        assert!(!report.servers[0].installed);
        assert_eq!(report.servers[0].path, None);
    }

    #[tokio::test]
    async fn a_command_that_cannot_run_has_no_version() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken-ls");
        std::fs::write(&broken, b"this is not an executable").unwrap();
        let config = config_with(vec![(
            "broken",
            server(broken.to_str().unwrap(), &[("rs", "rust")]),
        )]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            None,
            None,
            dir.path(),
            Duration::from_millis(200),
        )
        .await;
        let entry = &report.servers[0];
        assert!(entry.installed);
        assert_eq!(entry.version, None);
        assert!(!entry.version_timed_out);
    }

    #[tokio::test]
    async fn a_server_that_exits_nonzero_has_no_version() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "err-ls",
            "#!/bin/sh\necho 'error: bad flag'\nexit 2\n",
        );
        let config = config_with(vec![("err", server("err-ls", &[("rs", "rust")]))]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        let entry = &report.servers[0];
        assert!(entry.installed);
        assert_eq!(entry.version, None, "stderr/error text is not a version");
        assert!(!entry.version_timed_out);
    }

    #[tokio::test]
    async fn only_stderr_is_not_a_version() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "stderr-ls",
            "#!/bin/sh\necho 'oops' 1>&2\nexit 0\n",
        );
        let config = config_with(vec![("stderr", server("stderr-ls", &[("rs", "rust")]))]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(report.servers[0].version, None);
    }

    #[tokio::test]
    async fn multi_line_output_uses_the_first_line() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "multi-ls",
            "#!/bin/sh\necho 'first-ls 1.0'\necho 'second line'\n",
        );
        let config = config_with(vec![("multi", server("multi-ls", &[("rs", "rust")]))]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(report.servers[0].version.as_deref(), Some("first-ls 1.0"));
    }

    /// How much of a probe's stdout may be kept.
    ///
    /// `Command::output()` buffers a child's *entire* output before returning,
    /// so the timeout cannot act until the child is killed and all of it is
    /// already in memory — and `.lines()` on that buffer hands back a first
    /// "line" of whatever size the child chose to print without a newline.
    /// The probe therefore reports a megabyte-scale "version" string. Reading
    /// one line at a time under a byte budget means an oversized first line is
    /// dropped instead.
    #[tokio::test]
    async fn an_oversized_probe_line_is_not_reported_as_a_version() {
        let dir = tempfile::tempdir().unwrap();
        // One enormous line: 256 KiB with no newline at all.
        script(
            dir.path(),
            "verbose-ls",
            "#!/bin/sh\nawk 'BEGIN { s = \"x\"; for (i = 0; i < 262144; i++) s = s \"x\"; print s }'\n",
        );
        let config = config_with(vec![("verbose", server("verbose-ls", &[("rs", "rust")]))]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_secs(10),
        )
        .await;

        // The property: whatever was kept is bounded, so a chatty server cannot
        // make the doctor carry its output.
        let kept = report.servers[0].version.as_deref().unwrap_or("");
        assert!(
            kept.len() <= PROBE_STDOUT_LIMIT,
            "kept {} bytes of probe output, over the {PROBE_STDOUT_LIMIT}-byte budget",
            kept.len()
        );
    }

    /// The cap is on the read, not only on the line kept. A reader that never
    /// ends and never sends a newline must stop being read at the cap; checking the
    /// line afterwards would have buffered all of it first.
    #[tokio::test]
    async fn the_probe_stops_reading_at_the_cap() {
        use std::pin::Pin;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll};

        struct Endless(Arc<AtomicUsize>);
        impl tokio::io::AsyncRead for Endless {
            fn poll_read(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                // Ends after 1 MiB, so a regression that removes the cap makes this
                // test fail on the byte count instead of reading without bound.
                if self.0.load(Ordering::Relaxed) >= 1024 * 1024 {
                    return Poll::Ready(Ok(()));
                }
                let n = buf.remaining();
                buf.put_slice(&vec![b'a'; n]);
                self.0.fetch_add(n, Ordering::Relaxed);
                Poll::Ready(Ok(()))
            }
        }

        let served = Arc::new(AtomicUsize::new(0));
        let answer = tokio::time::timeout(
            Duration::from_secs(5),
            first_line_of(Endless(served.clone())),
        )
        .await
        .expect("an endless reader must not hold the probe");
        // Whatever is kept stays within the budget; what matters here is how much
        // was read.
        assert!(answer.is_none_or(|line| line.len() <= PROBE_STDOUT_LIMIT));
        let read = served.load(Ordering::Relaxed);
        // The cap plus at most one extra buffer fill from the BufReader.
        assert!(
            read <= PROBE_STDOUT_LIMIT * 2,
            "read {read} bytes from a reader capped at {PROBE_STDOUT_LIMIT}"
        );
    }

    /// A normal probe still reports its version, so the bound did not cost the
    /// feature.
    /// A normal probe still reports its version, so the bound did not cost the
    /// feature.
    #[tokio::test]
    async fn an_ordinary_probe_still_reports_its_version() {
        let dir = tempfile::tempdir().unwrap();
        script(dir.path(), "good-ls", "#!/bin/sh\necho 'good-ls 2.1'\n");
        let config = config_with(vec![("good", server("good-ls", &[("rs", "rust")]))]);
        let report = build_report(
            &config,
            Path::new("/no/socket"),
            Some(dir.path().as_os_str()),
            None,
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(report.servers[0].version.as_deref(), Some("good-ls 2.1"));
    }
}
