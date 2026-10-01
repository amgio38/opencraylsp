//! `opencraylspd`: the shared language-server daemon and its command line.
//!
//! Ownership: `server/` (the `serve` command) belongs to the core workstream,
//! `cli/` (status/stop/restart/doctor) to the MCP workstream. This file only
//! routes subcommands.

mod cli;
mod server;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Shown by `opencraylspd --help` after the subcommands.
const AFTER_HELP: &str = "\
EXAMPLES:
    opencraylspd serve                          run the daemon in the foreground
    opencraylspd status                         is a daemon up, and what is it running
    opencraylspd doctor                         which language servers are installed here
    opencraylspd restart                        stop the daemon and start a fresh one
    opencraylspd stop                           stop the daemon

`opencraylsp-mcp` starts a daemon on first use, so `serve` is only needed to watch one
in the foreground. Every command accepts `--socket PATH`; the default is
$OPENCRAYLSP_SOCKET, else $XDG_RUNTIME_DIR/opencraylsp/opencraylsp.sock, else /tmp/opencraylsp-<uid>/opencraylsp.sock.
The log lives in $XDG_STATE_HOME/opencraylsp/opencraylsp.log, else ~/.local/state/opencraylsp/opencraylsp.log.

Documentation: https://github.com/amgio38/opencraylsp";

#[derive(Debug, Parser)]
#[command(
    name = "opencraylspd",
    version,
    about = "Shared language-server daemon",
    long_about = "Shared language-server daemon.\n\nOne opencraylspd per machine pools the \
                  language servers its clients need: instances are shared by \
                  (server, project root), reclaimed when idle, and restarted when a \
                  process tree crosses the memory ceiling.",
    after_help = AFTER_HELP
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Serve(server::ServeArgs),
    /// Show daemon and language-server status.
    Status(cli::StatusArgs),
    /// Stop the daemon.
    Stop(cli::StopArgs),
    /// Stop the daemon (if running) and start a fresh one.
    Restart(cli::RestartArgs),
    /// Check which language servers are installed.
    Doctor(cli::DoctorArgs),
    /// Print the version and protocol revision.
    Version,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match args.command {
        Command::Serve(args) => server::run(args),
        Command::Status(args) => cli::status(args),
        Command::Stop(args) => cli::stop(args),
        Command::Restart(args) => cli::restart(args),
        Command::Doctor(args) => cli::doctor(args),
        Command::Version => cli::version(),
    }
}
