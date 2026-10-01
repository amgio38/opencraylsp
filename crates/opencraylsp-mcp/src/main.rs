//! `opencraylsp-mcp`: MCP stdio server.
//!
//! stdout belongs to the protocol and nothing else: replies are written by the
//! protocol layer's single writer, and all logging goes to stderr through
//! `tracing`.
//!
//! Two backends: the default talks to `opencraylspd` through `opencraylsp-client` (connecting
//! lazily, so a harness still starts when no daemon is up yet); `--embedded`
//! runs the pool in this process.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use opencraylsp_client::ClientOptions;
use opencraylsp_mcp::daemon_host::LazyDaemonHost;
use opencraylsp_mcp::mcp::McpServer;
use opencraylsp_mcp::options::{self, Cli, Options};
use opencraylsp_proto::ToolHost;

fn main() -> ExitCode {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    // `env::args()` panics outright when any argument is not valid UTF-8,
    // and an argument can be — a path to a workspace, a `--config` — without
    // anyone choosing that. `args_os` cannot panic, so the raw arguments are
    // taken here and only turned into text afterwards, where a bad one is a
    // message rather than a crash.
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let program = raw
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or("opencraylsp-mcp")
        .to_owned();
    let mut lossy = false;
    let args: Vec<String> = raw
        .iter()
        .skip(1)
        .map(|arg| match arg.to_str() {
            Some(text) => text.to_owned(),
            None => {
                lossy = true;
                arg.to_string_lossy().into_owned()
            }
        })
        .collect();
    if lossy {
        let _ = writeln!(
            err,
            "warning: some arguments are not valid UTF-8 and were read lossily; \
             paths with unusual bytes may not resolve"
        );
    }
    let env = |key: &str| std::env::var(key).ok();
    run(&program, &args, &env, &mut out, &mut err)
}

/// The command line, split out so tests can drive it with captured streams and
/// an injected environment.
fn run(
    program: &str,
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let cli = match Cli::try_parse_from(
        std::iter::once(program.to_owned()).chain(args.iter().cloned()),
    ) {
        Ok(cli) => cli,
        Err(error) => {
            // clap routes `--help`/`--version` to stdout with a success code;
            // real parse errors go to stderr with exit 2.
            let to_stdout = !error.use_stderr();
            let _ = if to_stdout {
                write!(out, "{error}")
            } else {
                write!(err, "{error}")
            };
            return if to_stdout {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
        }
    };

    let options = match options::resolve(&cli, env) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            return ExitCode::from(2);
        }
    };

    if cli.fake_host {
        #[cfg(feature = "test-fake-host")]
        {
            init_tracing();
            let host: Arc<dyn ToolHost> =
                opencraylsp_mcp::fake_host::FakeHost::with_default_tools();
            let runtime = match runtime() {
                Ok(runtime) => runtime,
                Err(error) => return runtime_failed(&error, err),
            };
            return serve(host, &runtime, opencraylsp_mcp::mcp::default_eof_grace());
        }
        #[cfg(not(feature = "test-fake-host"))]
        {
            let _ = writeln!(
                err,
                "error: this build has no `--fake-host` (development only)"
            );
            return ExitCode::from(2);
        }
    }

    if options.embedded {
        #[cfg(feature = "embedded")]
        {
            return run_embedded(&options, err);
        }
        #[cfg(not(feature = "embedded"))]
        {
            let _ = writeln!(err, "error: this build has no `--embedded` support");
            return ExitCode::from(2);
        }
    }

    run_daemon(&options, err)
}

/// The default backend: connect to (or start) the daemon and serve MCP.
fn run_daemon(options: &Options, err: &mut dyn Write) -> ExitCode {
    // Validate the language flags locally, before any daemon is contacted: a
    // name no configured server can serve must be a fast exit 2, and the
    // stdio loop must never wait on a daemon to learn it.
    if let Err((input, valid)) = validate_languages(options) {
        return report_unknown_language(&input, &valid, err);
    }
    init_tracing();
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(error) => return runtime_failed(&error, err),
    };
    let host = LazyDaemonHost::new(client_options(options));
    // Connect in the background. `initialize`, `ping` and `tools/list` answer
    // immediately; the first `tools/call` joins this dial (bounded by the
    // client's connect deadline) and reports `[daemon_unavailable]` if it
    // could not connect.
    //
    // The EOF grace is `default_eof_grace()` rather than the connect deadline:
    // it has to outlast every in-flight request, and a dial is only the first
    // part of one. Tying the two together used to make a call that answered a
    // hair after the deadline lose its reply to `cancel_all`.
    let background = Arc::clone(&host);
    runtime.spawn(async move {
        match background.connect_now().await {
            Ok(()) => tracing::debug!("connected to opencraylspd"),
            // "No daemon yet" is not fatal: the loop still serves `tools/list`,
            // and a later call retries the connection on demand.
            Err(error) => {
                tracing::warn!(%error, "no daemon yet; entering the MCP loop and retrying on demand");
            }
        }
    });
    serve(host, &runtime, opencraylsp_mcp::mcp::default_eof_grace())
}

/// Checks the requested languages against what a server could ever serve,
/// without contacting the daemon.
///
/// The daemon owns alias resolution, but an impossible name is knowable here:
/// the built-ins plus any language the config file adds. Keeping the check
/// local means no connection is opened just to reject a typo, and the harness
/// is never left waiting on a daemon that has not started.
fn validate_languages(options: &Options) -> Result<(), (String, Vec<String>)> {
    let Some(languages) = options.languages.as_deref() else {
        return Ok(());
    };
    let extra = local_languages(options.config.as_deref());
    opencraylsp_core::languages::normalize(Some(languages), &extra)
        .map(|_| ())
        .map_err(|unknown| (unknown.input, unknown.valid))
}

/// The languages the config file adds on top of the built-ins.
///
/// A config this client cannot read is the daemon's to report: validating
/// against the built-ins alone is still better than not validating at all.
fn local_languages(config: Option<&std::path::Path>) -> std::collections::BTreeSet<String> {
    match opencraylsp_core::LspConfig::load(config) {
        Ok(config) => opencraylsp_core::languages::known_languages(&config.servers),
        Err(_) => std::collections::BTreeSet::new(),
    }
}

/// The `--embedded` backend: pool plus tools in this process.
#[cfg(feature = "embedded")]
fn run_embedded(options: &Options, err: &mut dyn Write) -> ExitCode {
    init_tracing();
    let config = match opencraylsp_core::LspConfig::load(options.config.as_deref()) {
        Ok(config) => Arc::new(config),
        Err(error) => {
            let _ = writeln!(err, "error: cannot load the config: {error}");
            return ExitCode::from(2);
        }
    };
    let extra = opencraylsp_core::languages::known_languages(&config.servers);
    let selection =
        match opencraylsp_core::languages::normalize(options.languages.as_deref(), &extra) {
            Ok(selection) => selection,
            Err(unknown) => return report_unknown_language(&unknown.input, &unknown.valid, err),
        };
    let pool = opencraylsp_core::Pool::new(config);
    let backend = pool.bind(options.workspace.clone(), selection);
    let host = Arc::new(opencraylsp_mcp::embedded::EmbeddedHost::from_pool(backend));
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(error) => return runtime_failed(&error, err),
    };
    let code = serve(
        host.clone(),
        &runtime,
        opencraylsp_mcp::mcp::default_eof_grace(),
    );
    runtime.block_on(host.shutdown());
    code
}

fn client_options(options: &Options) -> ClientOptions {
    let mut client = ClientOptions::defaults();
    client.socket = options.socket.clone();
    client.workspace = options.workspace.clone();
    client.languages = options.languages.clone();
    client.daemon_config = options.config.clone();
    client.spawn = true;
    client
}

fn report_unknown_language(input: &str, valid: &[String], err: &mut dyn Write) -> ExitCode {
    let _ = writeln!(err, "{}", options::unknown_language_message(input, valid));
    ExitCode::from(2)
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}

fn runtime_failed(error: &std::io::Error, err: &mut dyn Write) -> ExitCode {
    let _ = writeln!(err, "error: could not start the async runtime: {error}");
    ExitCode::from(2)
}

fn serve(
    host: Arc<dyn ToolHost>,
    runtime: &tokio::runtime::Runtime,
    eof_grace: std::time::Duration,
) -> ExitCode {
    let server = McpServer::new(host, env!("CARGO_PKG_VERSION")).with_eof_grace(eof_grace);
    match runtime.block_on(server.serve(tokio::io::stdin(), tokio::io::stdout())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "stdio loop failed");
            ExitCode::FAILURE
        }
    }
}

/// Tracing goes to stderr only; stdout carries protocol messages alone. Safe to
/// call more than once (tests do): the first call wins.
fn init_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(args: &[&str]) -> (ExitCode, String, String) {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run("opencraylsp-mcp", &args, &|_| None, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("utf8 stdout"),
            String::from_utf8(err).expect("utf8 stderr"),
        )
    }

    #[test]
    fn help_lists_the_languages_and_an_example() {
        let (code, out, _) = run_args(&["--help"]);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.contains("LANGUAGES"), "{out}");
        assert!(out.contains("(rs)"), "{out}");
        assert!(out.contains("claude mcp add opencraylsp"), "{out}");
    }

    #[test]
    fn version_goes_to_stdout() {
        let (code, out, _) = run_args(&["--version"]);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.contains("opencraylsp-mcp"), "{out}");
    }

    #[test]
    fn an_unknown_flag_is_rejected_with_exit_2() {
        let (code, _, err) = run_args(&["--nope"]);
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("--nope"), "{err}");
    }

    #[test]
    fn a_missing_workspace_is_rejected_with_exit_2() {
        let (code, _, err) = run_args(&["--workspace", "/definitely/not/here"]);
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("workspace"), "{err}");
    }

    #[test]
    fn a_config_flag_is_accepted_for_the_daemon_path() {
        // `--config` also applies without `--embedded`; the failure here is the
        // bad workspace, not the flag.
        let (code, _, err) = run_args(&[
            "--config",
            "/tmp/opencraylsp.toml",
            "--workspace",
            "/definitely/not/here",
        ]);
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("workspace"), "{err}");
    }

    #[cfg(not(feature = "test-fake-host"))]
    #[test]
    fn a_build_without_the_fake_host_says_so() {
        let (code, _, err) = run_args(&["--fake-host"]);
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("--fake-host"), "{err}");
    }

    #[cfg(not(feature = "embedded"))]
    #[test]
    fn a_build_without_embedded_says_so() {
        let (code, _, err) = run_args(&["--embedded"]);
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("--embedded"), "{err}");
    }
}
