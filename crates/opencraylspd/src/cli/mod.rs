//! Client-side subcommands: `status`, `stop`, `restart`, `doctor` and
//! `version`. `serve` lives in `server/`.
//!
//! All output goes through injected writers rather than `print!`/`println!`,
//! which the workspace forbids so stdout stays meaningful for the daemon's
//! callers. The daemon's real socket is never contacted by a test: every
//! command takes `--socket`.

pub mod doctor;
pub mod lifecycle;
pub mod status;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;
use opencraylsp_client::ClientOptions;
use opencraylsp_proto::paths::default_socket_path;

/// How long `status`/`stop` try the socket before calling the daemon absent.
///
/// `opencraylsp-client`'s default is eight seconds, which is right when starting or
/// reconnecting a daemon and wrong for a command that only reports: a plain
/// `status` with no daemon must not stall.
const PROBE_DEADLINE: Duration = Duration::from_millis(500);

/// How long `stop`/`restart` wait for the daemon to let go of its socket.
const RELEASE_DEADLINE: Duration = Duration::from_secs(10);

/// `opencraylspd status [--json] [--socket p]`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Print the raw `StatusReport` as JSON.
    #[arg(long)]
    pub json: bool,
    /// Socket path (default: the standard runtime path).
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// `opencraylspd stop [--socket p]`.
#[derive(Debug, Args)]
pub struct StopArgs {
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// `opencraylspd restart [--socket p]`.
#[derive(Debug, Args)]
pub struct RestartArgs {
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// `opencraylspd doctor [--json] [--socket p]`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

pub fn status(args: StatusArgs) -> ExitCode {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = status::run(args, &mut out, &mut err);
    let _ = out.flush();
    code
}

pub fn stop(args: StopArgs) -> ExitCode {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = lifecycle::run_stop(args, &mut out, &mut err);
    let _ = out.flush();
    code
}

pub fn restart(args: RestartArgs) -> ExitCode {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = lifecycle::run_restart(args, &mut out, &mut err);
    let _ = out.flush();
    code
}

pub fn doctor(args: DoctorArgs) -> ExitCode {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = doctor::run(args, &mut out, &mut err);
    let _ = out.flush();
    code
}

/// `opencraylspd version`: the daemon version and the protocol revision it speaks.
pub fn version() -> ExitCode {
    let mut out = std::io::stdout();
    let text = format!(
        "opencraylspd {} (protocol {})\n",
        env!("CARGO_PKG_VERSION"),
        opencraylsp_proto::PROTOCOL_VERSION
    );
    let code = match out.write_all(text.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    };
    let _ = out.flush();
    code
}

/// Options for commands that only look: never start a daemon, fail fast.
pub(crate) fn probe_options(socket: PathBuf) -> ClientOptions {
    let mut options = ClientOptions::defaults();
    options.socket = socket;
    options.spawn = false;
    options.connect_deadline = PROBE_DEADLINE;
    options
}

/// Options for `restart`, which is allowed to start a daemon.
pub(crate) fn spawn_options(socket: PathBuf) -> ClientOptions {
    let mut options = ClientOptions::defaults();
    options.socket = socket;
    options.spawn = true;
    options
}

/// Runs one async command on a fresh runtime, reporting a runtime failure.
pub(crate) fn block_on<F: std::future::Future<Output = ExitCode>>(future: F) -> ExitCode {
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(future),
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "error: could not start the runtime: {error}"
            );
            ExitCode::from(2)
        }
    }
}

pub(crate) fn resolve_socket(socket: Option<PathBuf>) -> PathBuf {
    socket.unwrap_or_else(default_socket_path)
}

pub(crate) const RELEASE: Duration = RELEASE_DEADLINE;
