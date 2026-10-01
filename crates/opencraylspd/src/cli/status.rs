//! `opencraylspd status`: what the daemon is doing, or that it is not running.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use opencraylsp_client::{ClientError, DaemonClient};
use opencraylsp_proto::{InstanceState, LanguageMode, StatusReport};

use super::{StatusArgs, block_on, probe_options, resolve_socket};

pub fn run(args: StatusArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let socket = resolve_socket(args.socket);
    block_on(report(socket, args.json, out, err))
}

async fn report(socket: PathBuf, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    // A missing socket file means nobody is listening; do not spend the probe
    // deadline discovering what the filesystem already says.
    if !socket.exists() {
        return not_running(json, &socket, out, err);
    }
    let client = match DaemonClient::connect(probe_options(socket.clone())).await {
        Ok(client) => client,
        Err(ClientError::Unavailable { .. }) => return not_running(json, &socket, out, err),
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return ExitCode::from(1);
        }
    };
    let report = match client.status().await {
        Ok(report) => report,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return ExitCode::from(1);
        }
    };
    if json {
        if let Err(error) = serde_json::to_writer_pretty(&mut *out, &report) {
            let _ = writeln!(err, "error: cannot write the report: {error}");
            return ExitCode::from(1);
        }
        let _ = writeln!(out);
    } else if write_table(out, &report).is_err() {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// The daemon is absent. With `--json` the caller still gets JSON on stdout
/// (so a script's parser does not choke on prose) and the sentence goes to
/// stderr; the exit code is non-zero either way.
fn not_running(
    json: bool,
    socket: &std::path::Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if json {
        let value = serde_json::json!({
            "running": false,
            "socket": socket.display().to_string(),
        });
        if let Err(error) = serde_json::to_writer_pretty(&mut *out, &value) {
            let _ = writeln!(err, "error: cannot write the report: {error}");
            return ExitCode::from(1);
        }
        let _ = writeln!(out);
        let _ = writeln!(err, "opencraylspd is not running");
    } else {
        let _ = writeln!(out, "opencraylspd is not running");
    }
    ExitCode::from(1)
}

/// One line for the daemon and one per instance, as `lsp_status` shows it.
fn write_table(out: &mut dyn Write, report: &StatusReport) -> std::io::Result<()> {
    writeln!(
        out,
        "daemon  pid {}  version {}  uptime {}s  rss {}  clients {}{}",
        report.daemon.pid,
        report.daemon.version,
        report.daemon.uptime_secs,
        daemon_rss(report.daemon.rss_bytes, report.daemon.max_rss_mb),
        report.daemon.clients,
        if report.daemon.rss_over_limit {
            "  [rss_over_limit: over memory ceiling, refusing to restart]"
        } else {
            ""
        }
    )?;
    writeln!(
        out,
        "languages  {} ({}){}",
        if report.enabled_languages.is_empty() {
            "-".to_owned()
        } else {
            report.enabled_languages.join(", ")
        },
        mode(report.language_mode),
        if report.not_installed.is_empty() {
            String::new()
        } else {
            format!("  not installed: {}", report.not_installed.join(", "))
        }
    )?;
    for instance in &report.instances {
        writeln!(
            out,
            "server  {}  root {}  state {}  rss {}  idle {}s  open_docs {}  restarts {}  memory_restarts {}",
            instance.server,
            instance.root,
            state(instance.state),
            rss(instance.rss_bytes),
            instance.idle_secs,
            instance.open_docs,
            instance.restarts,
            instance.memory_restarts
        )?;
    }
    Ok(())
}

fn rss(bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) => format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)),
        None => "-".to_owned(),
    }
}

/// The daemon's own memory against its own ceiling: `12.0/512.0 MB`.
///
/// A bare figure answers nothing on its own — 12 MB is unremarkable at a 512 MB
/// ceiling and alarming at a 16 MB one. A daemon too old to report a ceiling
/// (`None`) keeps the plain form rather than showing an empty ratio, so a
/// version-skewed answer degrades to what it used to be instead of to noise.
fn daemon_rss(bytes: Option<u64>, max_mb: Option<u64>) -> String {
    let Some(limit) = max_mb else {
        return rss(bytes);
    };
    let used = rss(bytes);
    // Each side keeps its own unit: `12.0 MB/512 MB` says what the reader is
    // looking at, where `12.0/512.0 MB` leaves it to be inferred.
    let ceiling = format!("{limit} MB");
    if used == "-" {
        format!("-/{ceiling}")
    } else {
        format!("{used}/{ceiling}")
    }
}

fn state(state: InstanceState) -> &'static str {
    match state {
        InstanceState::Starting => "starting",
        InstanceState::Indexing => "indexing",
        InstanceState::Ready => "ready",
        InstanceState::Restarting => "restarting",
        InstanceState::Failed => "failed",
        InstanceState::Stopped => "stopped",
    }
}

fn mode(mode: LanguageMode) -> &'static str {
    match mode {
        LanguageMode::Auto => "auto",
        LanguageMode::Declared => "declared",
        LanguageMode::All => "all",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencraylsp_proto::{DaemonInfo, InstanceInfo, Limits};
    use std::path::Path;

    fn sample_report() -> StatusReport {
        StatusReport {
            daemon: DaemonInfo {
                version: "0.1.0".into(),
                pid: 4242,
                uptime_secs: 12,
                rss_bytes: Some(12 * 1024 * 1024),
                clients: 2,
                // 12 MB of 512: the table is expected to say so.
                max_rss_mb: Some(512),
                rss_over_limit: false,
            },
            limits: Limits {
                max_instances: 8,
                max_rss_mb: 6144,
                idle_shutdown_secs: 900,
                max_open_docs: 256,
            },
            enabled_languages: vec!["rust".into(), "go".into()],
            language_mode: LanguageMode::Declared,
            not_installed: vec!["go".into()],
            instances: vec![InstanceInfo {
                server: "rust-analyzer".into(),
                root: "/ws".into(),
                state: InstanceState::Ready,
                pid: Some(7),
                rss_bytes: Some(120 * 1024 * 1024),
                idle_secs: 3,
                restarts: 0,
                memory_restarts: 0,
                open_docs: 2,
                indexing: None,
            }],
        }
    }

    #[test]
    fn the_table_names_every_designed_column() {
        let mut out = Vec::new();
        write_table(&mut out, &sample_report()).unwrap();
        let text = String::from_utf8(out).unwrap();
        for needle in [
            "pid 4242",
            "12.0 MB",
            "rust-analyzer",
            "root /ws",
            "state ready",
            "120.0 MB",
            "idle 3s",
            "open_docs 2",
            "restarts 0",
            "memory_restarts 0",
            "not installed: go",
            "declared",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    /// The daemon's own memory is printed against its own ceiling. A bare
    /// "12.0 MB" gives an operator nothing to judge it by; "12.0/512.0 MB" says
    /// the daemon is using 2% of what it is allowed, which is the question
    /// `rss` is actually asked.
    #[test]
    fn the_daemon_line_shows_its_memory_against_its_ceiling() {
        let mut out = Vec::new();
        write_table(&mut out, &sample_report()).unwrap();
        let text = String::from_utf8(out).unwrap();
        let daemon_line = text.lines().next().expect("a daemon line");
        assert!(
            daemon_line.contains("rss 12.0 MB/512 MB"),
            "the ceiling must be on the daemon line: {daemon_line}"
        );
    }

    /// A daemon that has refused to restart says so, even though it is serving.
    /// Without the note a daemon in that state looks exactly like a healthy one.
    #[test]
    fn a_daemon_that_refused_to_restart_says_so() {
        let mut report = sample_report();
        report.daemon.rss_over_limit = true;
        let mut out = Vec::new();
        write_table(&mut out, &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("over memory ceiling"),
            "the refusal must be visible: {text}"
        );
        assert!(text.contains("rss_over_limit"), "and named for scripts");
    }

    /// A daemon old enough not to know its own ceiling still prints cleanly,
    /// rather than as an empty or nonsensical ratio.
    #[test]
    fn a_daemon_without_a_ceiling_still_prints_its_memory() {
        let mut report = sample_report();
        report.daemon.max_rss_mb = None;
        let mut out = Vec::new();
        write_table(&mut out, &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        // The daemon line only: the instance lines carry their own slashes (a
        // root path, a `root /ws`), so the whole table is not the subject here.
        let daemon_line = text.lines().next().expect("a daemon line");
        assert!(
            daemon_line.contains("rss 12.0 MB"),
            "an absent ceiling must not produce a ratio: {daemon_line}"
        );
        assert!(
            !daemon_line.contains('/'),
            "nor a stray slash on the daemon line: {daemon_line}"
        );
    }

    #[test]
    fn a_status_report_survives_a_json_round_trip() {
        let original = sample_report();
        let text = serde_json::to_string(&original).unwrap();
        let parsed: StatusReport = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn a_missing_socket_is_reported_as_not_running() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = block_on(async {
            report(
                Path::new("/definitely/not/a/socket").to_owned(),
                false,
                &mut out,
                &mut err,
            )
            .await
        });
        assert_eq!(code, ExitCode::from(1));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("opencraylspd is not running")
        );
    }

    /// A daemon that answers `hello` with the given result and then goes away.
    fn fake_hello(socket: &Path, hello: serde_json::Value) {
        use std::io::{BufRead as _, Write as _};
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    line.clear();
                    continue;
                };
                if value["method"] == "hello" {
                    let response = serde_json::json!({"jsonrpc": "2.0", "id": value["id"].clone(), "result": hello});
                    let mut write = stream.try_clone().unwrap();
                    let _ = writeln!(write, "{response}");
                    return;
                }
                line.clear();
            }
        });
    }

    #[test]
    fn a_protocol_mismatch_is_exit_1_with_a_restart_hint() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        fake_hello(
            &socket,
            serde_json::json!({"protocol": 99, "daemon_version": "x", "pid": 1,
                               "languages": [], "language_mode": "auto"}),
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = block_on(report(socket, false, &mut out, &mut err));
        assert_eq!(code, ExitCode::from(1));
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("protocol mismatch"), "{err}");
        assert!(err.contains("opencraylspd restart"), "{err}");
    }

    #[test]
    fn an_empty_report_still_renders() {
        let mut report = sample_report();
        report.instances.clear();
        report.enabled_languages.clear();
        report.not_installed.clear();
        let mut out = Vec::new();
        write_table(&mut out, &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("languages  -"), "{text}");
    }

    /// A daemon that answers `hello` and `status` (or errors on `status`).
    fn fake_daemon(socket: &Path, status: serde_json::Value, status_error: bool) {
        use std::io::{BufRead as _, Write as _};
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut write = stream;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    line.clear();
                    continue;
                };
                let id = value["id"].clone();
                let response = match value["method"].as_str().unwrap_or_default() {
                    "hello" => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"protocol": 1, "daemon_version": "0.1.0-fake",
                                   "pid": 1, "languages": [], "language_mode": "auto"},
                    }),
                    "status" if status_error => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "unknown method `status`"},
                    }),
                    "status" => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": status}),
                    _ => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                };
                let _ = writeln!(write, "{response}");
                line.clear();
            }
        });
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("no"))
        }
    }

    #[test]
    fn a_status_method_error_is_exit_1() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        fake_daemon(&socket, serde_json::Value::Null, true);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = block_on(report(socket, false, &mut out, &mut err));
        assert_eq!(code, ExitCode::from(1));
        assert!(String::from_utf8(err).unwrap().contains("unknown method"));
    }

    #[test]
    fn a_writer_that_fails_is_exit_1_for_json_and_table() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let status = serde_json::to_value(sample_report()).unwrap();
        fake_daemon(&socket, status, false);
        let mut err = Vec::new();
        let code = block_on(report(socket.clone(), true, &mut FailingWriter, &mut err));
        assert_eq!(code, ExitCode::from(1));

        let mut err = Vec::new();
        let code = block_on(report(socket, false, &mut FailingWriter, &mut err));
        assert_eq!(code, ExitCode::from(1));
    }

    #[test]
    fn every_state_and_mode_has_a_name() {
        use opencraylsp_proto::InstanceState::{
            Failed, Indexing, Ready, Restarting, Starting, Stopped,
        };
        assert_eq!(state(Starting), "starting");
        assert_eq!(state(Indexing), "indexing");
        assert_eq!(state(Ready), "ready");
        assert_eq!(state(Restarting), "restarting");
        assert_eq!(state(Failed), "failed");
        assert_eq!(state(Stopped), "stopped");
        assert_eq!(mode(LanguageMode::Auto), "auto");
        assert_eq!(mode(LanguageMode::Declared), "declared");
        assert_eq!(mode(LanguageMode::All), "all");
    }
}
