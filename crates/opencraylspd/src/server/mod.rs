//! The `serve` command: claim the socket, accept clients, shut down cleanly.
//!
//! Layout: [`lifecycle`] owns the single-instance lock and socket file,
//! [`conn`] speaks the protocol on one connection, [`runner`] is the seam to
//! the tools, and this module wires them together and handles signals.

pub mod conn;
pub mod lifecycle;
pub mod logging;
pub mod runner;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use opencraylsp_core::daemon_guard;
use opencraylsp_core::{DaemonGuard, LspConfig, Pool, PoolOptions};
use opencraylsp_proto::paths::{current_uid, default_socket_path};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use conn::ServerState;
use lifecycle::LifecycleError;
use runner::{LspTools, ToolRunner};

/// How long connections get to finish after an ordinary shutdown begins.
const CONNECTION_DRAIN: Duration = Duration::from_secs(2);

/// How long connections get to finish after the daemon's own memory ceiling is
/// hit.
///
/// Deliberately far longer than [`CONNECTION_DRAIN`]. An operator asking the
/// daemon to stop wants it gone in a moment; a daemon being *replaced* because
/// it grew too large should not cut a half-finished answer in half to save
/// three seconds, especially since a client is about to reconnect to the
/// replacement anyway. Thirty seconds is the request timeout, so anything still
/// running at that point is stuck rather than slow.
const OVER_LIMIT_DRAIN: Duration = Duration::from_secs(30);

/// How long the language servers get to exit before the daemon stops waiting.
const POOL_SHUTDOWN: Duration = Duration::from_secs(10);

/// Command-line options of `opencraylspd serve`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ServeArgs {
    /// Socket path (default: `$XDG_RUNTIME_DIR/opencraylsp/opencraylsp.sock`).
    #[arg(long, env = "OPENCRAYLSP_SOCKET")]
    pub socket: Option<PathBuf>,
    /// Config file (default: `~/.config/opencraylsp/config.toml`).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Log file (default: `~/.local/state/opencraylsp/opencraylsp.log`).
    #[arg(long)]
    pub log_file: Option<PathBuf>,
}

/// Why `serve` ended abnormally.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
}

/// Entry point of `opencraylspd serve`.
pub fn run(args: ServeArgs) -> ExitCode {
    logging::init(args.log_file.as_deref());
    let config = match LspConfig::load(args.config.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            tracing::error!(error = %err, "cannot load the config");
            return ExitCode::from(2);
        }
    };
    let socket = args.socket.unwrap_or_else(default_socket_path);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(error = %err, "cannot start the async runtime");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(async move {
        let shutdown = CancellationToken::new();
        spawn_signal_handler(shutdown.clone());
        // Without a uid the daemon cannot decide which connections to
        // trust, so it refuses to serve rather than guessing uid 0.
        let uid = match current_uid() {
            Some(uid) => uid,
            None => {
                tracing::error!(
                    "cannot determine the current user id; \
                     refusing to serve without it (set OPENCRAYLSP_SOCKET to a path you own)"
                );
                return ExitCode::from(2);
            }
        };
        let outcome = serve(&socket, config, Arc::new(LspTools), shutdown, uid).await;
        match outcome {
            Ok(()) => ExitCode::SUCCESS,
            Err(ServeError::Lifecycle(LifecycleError::AlreadyRunning(path))) => {
                tracing::info!(path = %path.display(), "opencraylspd is already running");
                ExitCode::SUCCESS
            }
            Err(err) => {
                tracing::error!(error = %err, "opencraylspd could not start");
                ExitCode::from(1)
            }
        }
    })
}

/// Cancels `shutdown` on SIGTERM or SIGINT.
fn spawn_signal_handler(shutdown: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::warn!("cannot install signal handlers; stop the daemon with `opencraylspd stop`");
        return;
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received"),
            _ = int.recv() => tracing::info!("SIGINT received"),
        }
        shutdown.cancel();
    });
}

/// Runs the daemon on `socket` until `shutdown` is cancelled (by a signal, the
/// `shutdown` request, or a test). Returns after every language server has
/// been asked to exit and the socket file is gone.
pub async fn serve(
    socket: &Path,
    config: LspConfig,
    runner: Arc<dyn ToolRunner>,
    shutdown: CancellationToken,
    expected_uid: u32,
) -> Result<(), ServeError> {
    serve_with_options(
        socket,
        config,
        runner,
        shutdown,
        expected_uid,
        Default::default(),
    )
    .await
}

/// [`serve`] with the pool's options, so a test can shorten the drains and
/// sample a scripted memory figure.
pub async fn serve_with_options(
    socket: &Path,
    config: LspConfig,
    runner: Arc<dyn ToolRunner>,
    shutdown: CancellationToken,
    expected_uid: u32,
    options: PoolOptions,
) -> Result<(), ServeError> {
    let ownership = lifecycle::claim(socket)?;
    // The guard's shutdown is the same token the signal handler uses, so an
    // over-limit daemon leaves by exactly the path a `SIGTERM` takes: stop
    // accepting, drain, shut the servers down, remove the socket. There is no
    // second exit path to keep in step with this one.
    let guard = DaemonGuard::new(daemon_guard::stamp_path(socket), {
        let shutdown = shutdown.clone();
        move |_, _| shutdown.cancel()
    });
    let pool = Pool::with_options(
        Arc::new(config),
        PoolOptions {
            daemon_guard: Some(guard),
            ..options
        },
    );
    pool.spawn_maintenance();
    let state = Arc::new(ServerState {
        pool: pool.clone(),
        runner,
        shutdown,
        shutting_down: AtomicBool::new(false),
        expected_uid,
    });
    tracing::info!(socket = %socket.display(), "opencraylspd listening");
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = state.shutdown.cancelled() => break,
            accepted = ownership.listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(conn::handle_connection(stream, state.clone()));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    // An over-limit exit waits far longer for in-flight work than an operator's
    // `stop` does, because this daemon is being replaced rather than retired.
    let over_limit = pool.daemon_rss_over_limit();
    let drain_budget = if over_limit {
        OVER_LIMIT_DRAIN
    } else {
        CONNECTION_DRAIN
    };
    if over_limit {
        tracing::warn!(
            seconds = drain_budget.as_secs(),
            "draining in-flight requests before exiting over the memory ceiling"
        );
    }
    tracing::info!("opencraylspd shutting down");
    state
        .shutting_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(drain_budget, drain).await.is_err() {
        tracing::warn!("connections did not finish in time; aborting them");
        connections.abort_all();
    }
    if tokio::time::timeout(POOL_SHUTDOWN, pool.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("language servers did not exit in time");
    }
    ownership.cleanup();
    tracing::info!("opencraylspd stopped");
    Ok(())
}

#[cfg(test)]
mod tests;
