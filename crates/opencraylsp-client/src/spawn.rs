//! Starting `opencraylspd` when nobody else has.
//!
//! Two rules keep this from starting a second daemon:
//!
//! * `<socket>.lock` is the arbiter. If it can be locked, no daemon owns the
//!   socket and this client may start one. If the lock is held, a daemon is
//!   starting or already running, and starting another would only race it.
//! * The started daemon gets its own process group, so it survives this client
//!   exiting and its lifetime is not tied to our terminal.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::ClientOptions;

/// Environment variable naming the daemon binary, ahead of every other guess.
pub const BIN_ENV: &str = "OPENCRAYLSP_BIN";

/// The daemon's default log file, matching `opencraylspd`'s own rule:
/// `$XDG_STATE_HOME/opencraylsp/opencraylsp.log`, else `$HOME/.local/state/opencraylsp/opencraylsp.log`.
///
/// Used to explain a failed connect with the daemon's last words, since a
/// daemon the client starts has its stderr discarded.
pub(crate) fn default_log_path() -> Option<PathBuf> {
    default_log_path_from(&|key| std::env::var(key).ok())
}

/// [`default_log_path`] with the environment injected, so it can be tested.
fn default_log_path_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let non_empty = |key: &str| env(key).filter(|value| !value.is_empty());
    if let Some(state) = non_empty("XDG_STATE_HOME") {
        return Some(
            Path::new(&state)
                .join("opencraylsp")
                .join("opencraylsp.log"),
        );
    }
    non_empty("HOME").map(|home| {
        Path::new(&home)
            .join(".local")
            .join("state")
            .join("opencraylsp")
            .join("opencraylsp.log")
    })
}

/// Most bytes read from the end of a log file to build a tail.
///
/// The daemon appends to this file and never rotates it, so its size is
/// attacker-adjacent: a language server that crashes in a loop can grow it
/// without bound. Reading the whole thing on every failed connect turns that
/// into the *client's* memory spike, so only this much is ever read.
const LOG_TAIL_WINDOW_BYTES: u64 = 64 * 1024;

/// The last `lines` non-empty lines of `path`, joined with `" | "`.
pub(crate) fn log_tail(path: &Path, lines: usize) -> Option<String> {
    let window = log_tail_window(path)?;
    let mut tail: Vec<&str> = window
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(lines)
        .collect();
    if tail.is_empty() {
        return None;
    }
    tail.reverse();
    Some(tail.join(" | "))
}

/// Reads at most the last [`LOG_TAIL_WINDOW_BYTES`] of `path`.
///
/// Seeking to `len - window` can land mid-line when the file is larger than
/// the window; that first fragment is dropped so the caller never reports half
/// a line as if it were the server's last word.
fn log_tail_window(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return Some(String::new());
    }
    let start = len.saturating_sub(LOG_TAIL_WINDOW_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0u8; (len - start) as usize];
    file.read_exact(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Some(if start == 0 {
        text
    } else {
        match text.find('\n') {
            // Cut the partial first line; keep everything after it.
            Some(at) => text[at + 1..].to_owned(),
            // One line longer than the window: there is no complete line to
            // report, and returning it whole would defeat the point of the cap.
            None => String::new(),
        }
    })
}

/// Finds the `opencraylspd` binary: explicit option, then `OPENCRAYLSP_BIN`, then a sibling of
/// our own executable, then `PATH`.
///
/// Returns `None` rather than a guess: spawning something that is not the
/// daemon would be worse than telling the user we could not find it.
pub fn resolve_daemon_bin(options: &ClientOptions) -> Option<PathBuf> {
    resolve_with(
        options,
        &|key| std::env::var(key).ok(),
        &std::env::current_exe().ok(),
    )
}

/// [`resolve_daemon_bin`] with the environment and the running executable
/// injected, so every branch can be tested without touching the process
/// environment (mirrors `opencraylsp_proto::paths::socket_path_from`).
fn resolve_with(
    options: &ClientOptions,
    env: &dyn Fn(&str) -> Option<String>,
    current_exe: &Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(bin) = options
        .daemon_bin
        .as_ref()
        .filter(|b| !b.as_os_str().is_empty())
    {
        return Some(bin.clone());
    }
    if let Some(from_env) = env(BIN_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(from_env));
    }
    if let Some(sibling) = current_exe.as_ref().and_then(|exe| sibling_binary(exe)) {
        return Some(sibling);
    }
    which(env, "opencraylspd")
}

/// `opencraylspd` next to `exe`, if it is there.
fn sibling_binary(exe: &Path) -> Option<PathBuf> {
    let sibling = exe.parent()?.join("opencraylspd");
    sibling.is_file().then_some(sibling)
}

/// The smallest `PATH` lookup that avoids pulling in a dependency.
fn which(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<PathBuf> {
    let path = env("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Starts `opencraylspd serve --socket <path>` detached from us.
///
/// The lock is taken and released purely as a probe: the daemon takes it again
/// for its own lifetime. Releasing it before spawning is what lets the daemon
/// win the race.
pub(crate) fn spawn_daemon(options: &ClientOptions) -> Result<u32, String> {
    spawn_daemon_with_env(options, &[])
}

/// [`spawn_daemon`], with extra environment for the daemon.
///
/// The extra entries are what the daemon would inherit from this process; the
/// `RUST_LOG` removal below applies to them exactly as it does to the ambient
/// environment, which is what makes the behaviour testable without changing
/// this process's own environment.
#[cfg_attr(not(test), inline)]
pub(crate) fn spawn_daemon_with_env(
    options: &ClientOptions,
    env: &[(&str, &str)],
) -> Result<u32, String> {
    if daemon_holds_lock(&options.socket)? {
        tracing::debug!("another opencraylspd owns the socket; not starting one");
        return Err("the socket is locked by another daemon".to_owned());
    }
    let Some(bin) = resolve_daemon_bin(options) else {
        return Err(format!(
            "cannot find the opencraylspd binary; set {BIN_ENV} or pass --embedded"
        ));
    };
    // `opencraylspd` was resolved by name, so a writable directory early on
    // `PATH` would choose what runs as this user. Refuse a binary somebody
    // else owns or that group/other may replace.
    let trust = opencraylsp_proto::trust::program_trust(&bin);
    if trust != opencraylsp_proto::trust::Trust::Owned {
        return Err(trust.explain(&bin));
    }
    spawn_binary_with_env(options, &bin, env)
}

/// Starts `bin serve --socket <path>`, detached and with its stdio discarded.
///
/// `env` is extra environment for the child — in production empty, since the
/// child inherits this process's. It exists so a test can prove what the daemon
/// is *not* given without changing this process's own environment, which would
/// be `unsafe` under the crate's `forbid(unsafe_code)`.
fn spawn_binary_with_env(
    options: &ClientOptions,
    bin: &Path,
    env: &[(&str, &str)],
) -> Result<u32, String> {
    // stdio to null: a daemon that writes to an inherited terminal would
    // scribble over the MCP client's stdout, which belongs to the protocol.
    let mut command = Command::new(bin);
    command.arg("serve").arg("--socket").arg(&options.socket);
    // A daemon started by the client must read the same config the user meant;
    // only passed when the user asked for one.
    if let Some(config) = options.daemon_config.as_ref() {
        command.arg("--config").arg(config);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    // Last, so nothing above can reintroduce it. The MCP client runs
    // inside the harness, where `RUST_LOG` is usually set for *it* — often to
    // `trace`. Inheriting it would turn the long-lived daemon into a
    // per-request logger writing to a file nobody reads, for the life of the
    // workspace. The daemon has its own `--log-file` and its own `info`
    // default, so the client's verbosity is not inherited.
    command.env_remove("RUST_LOG");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Only Unix, so this is always available; keeps the daemon out of our
    // process group so a Ctrl-C aimed at the harness leaves it alone.
    set_process_group(&mut command);
    let mut child = command
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
    let pid = child.id().unwrap_or_default();
    if let Some(observe) = options.spawn_observer.as_ref() {
        observe(pid);
    }
    // Wait on the child so it is reaped. Without this the daemon would sit in
    // the process table as a zombie once it exits, holding its pid and
    // making "is it really gone?" impossible to answer.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(pid)
}

/// Whether `<socket>.lock` is currently held by a live daemon, or why the lock
/// file could not be inspected at all.
fn daemon_holds_lock(socket: &Path) -> Result<bool, String> {
    let lock_path = opencraylsp_proto::paths::lock_path(socket);
    // On a fresh machine the socket's directory does not exist yet, so the
    // lock file cannot be opened. That is "nobody owns it", not "somebody
    // does": create the directory the way the daemon would (0700).
    if let Some(dir) = lock_path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::os::unix::fs::DirBuilderExt::mode(
            std::fs::DirBuilder::new().recursive(true),
            0o700,
        )
        .create(dir);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot inspect the lock file {}: {e}", lock_path.display()))?;
    // Only "somebody holds it" means a daemon is running. Every other error
    // (a filesystem without locking, a read-only mount, a full disk) is NOT
    // evidence of a live daemon: treating those as "locked" makes the client
    // decide a daemon is already there and never start one, on a machine where
    // no daemon can ever run. The reason travels out so the user sees the real
    // cause instead of a misleading "another daemon owns the socket".
    lock_is_held_by_a_daemon(&lock_path, file.try_lock())
}

/// Classifies one `try_lock` outcome. Split out so the decision — the whole
/// point of the lock check — can be pinned by a test without having to provoke a real
/// filesystem into producing each kind of error.
fn lock_is_held_by_a_daemon(
    lock_path: &Path,
    outcome: std::result::Result<(), std::fs::TryLockError>,
) -> Result<bool, String> {
    match outcome {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(err)) => Err(format!(
            "cannot test the lock on {}: {err}",
            lock_path.display()
        )),
    }
}

/// Puts the daemon in its own process group so a Ctrl-C aimed at the harness
/// does not take the daemon - which is meant to outlive us - with it.
fn set_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn options() -> ClientOptions {
        ClientOptions::default_for_tests()
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn a_missing_socket_directory_does_not_look_like_a_running_daemon() {
        // First run on a fresh machine: nothing exists yet, and the lock file
        // cannot be opened until its directory does.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fresh").join("opencraylsp.sock");
        assert!(!daemon_holds_lock(&socket).expect("the lock is inspectable"));
        assert!(socket.parent().unwrap().is_dir());
    }

    #[test]
    fn the_explicit_binary_wins_over_everything() {
        let mut opts = options();
        opts.daemon_bin = Some(PathBuf::from("/opt/explicit-opencraylspd"));
        assert_eq!(
            resolve_with(
                &opts,
                &env_of(&[("OPENCRAYLSP_BIN", "/env/opencraylspd")]),
                &None
            ),
            Some(PathBuf::from("/opt/explicit-opencraylspd"))
        );
    }

    #[test]
    fn an_empty_explicit_binary_is_ignored() {
        let mut opts = options();
        opts.daemon_bin = Some(PathBuf::new());
        assert_eq!(
            resolve_with(
                &opts,
                &env_of(&[("OPENCRAYLSP_BIN", "/env/opencraylspd")]),
                &None
            ),
            Some(PathBuf::from("/env/opencraylspd"))
        );
    }

    #[test]
    fn the_environment_variable_is_second() {
        assert_eq!(
            resolve_with(
                &options(),
                &env_of(&[("OPENCRAYLSP_BIN", "/env/opencraylspd")]),
                &None
            ),
            Some(PathBuf::from("/env/opencraylspd"))
        );
        // An empty value counts as unset.
        assert_eq!(
            resolve_with(&options(), &env_of(&[("OPENCRAYLSP_BIN", "")]), &None),
            None
        );
    }

    #[test]
    fn a_sibling_of_our_executable_is_third() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("opencraylsp-mcp");
        std::fs::write(&exe, b"fake").unwrap();
        std::fs::write(dir.path().join("opencraylspd"), b"fake").unwrap();
        assert_eq!(
            resolve_with(&options(), &env_of(&[]), &Some(exe.clone())),
            Some(dir.path().join("opencraylspd"))
        );
        // Without the sibling file there is nothing to use.
        std::fs::remove_file(dir.path().join("opencraylspd")).unwrap();
        assert_eq!(resolve_with(&options(), &env_of(&[]), &Some(exe)), None);
    }

    #[test]
    fn path_is_the_last_resort() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("opencraylspd"), b"fake").unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        assert_eq!(
            resolve_with(
                &options(),
                &env_of(&[("PATH", path.as_str())]),
                &Some(PathBuf::from("/nonexistent/opencraylsp-mcp")),
            ),
            Some(dir.path().join("opencraylspd"))
        );
        // No PATH at all, nothing found.
        assert_eq!(resolve_with(&options(), &env_of(&[]), &None), None);
    }

    #[tokio::test]
    async fn spawning_a_missing_binary_reports_the_path() {
        let mut opts = options();
        opts.daemon_bin = Some(PathBuf::from("/nonexistent/opencraylspd"));
        let err =
            spawn_binary_with_env(&opts, Path::new("/nonexistent/opencraylspd"), &[]).unwrap_err();
        assert!(err.contains("/nonexistent/opencraylspd"), "{err}");
    }

    #[test]
    fn a_lock_we_can_take_means_no_daemon_owns_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        assert!(!daemon_holds_lock(&socket).expect("the lock is inspectable"));

        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(opencraylsp_proto::paths::lock_path(&socket))
            .unwrap();
        held.try_lock().unwrap();
        assert!(daemon_holds_lock(&socket).expect("the lock is inspectable"));
    }

    // ---- only WouldBlock means "a daemon owns the socket" ----

    /// An *unrelated* lock failure must be reported, not mistaken for a
    /// running daemon.
    ///
    /// The old code answered `file.try_lock().is_err()`, so every error — a
    /// filesystem without locking, a read-only mount, an exhausted fd table —
    /// became "another daemon owns the socket". The client then never tried to
    /// start a daemon and the user saw a message that named the wrong cause.
    /// `TryLockError` has a dedicated `WouldBlock` variant precisely so this
    /// can be told apart.
    #[test]
    fn a_lock_error_that_is_not_would_block_is_reported_not_guessed() {
        // The decision itself is a pure function of one `TryLockError`, so it
        // is pinned directly here. Going through the real filesystem cannot
        // reach it: any lock error the OS produces on a normal filesystem is
        // `WouldBlock`, and a file that cannot be opened at all fails one step
        // earlier, when the lock file is opened.
        assert!(
            !lock_is_held_by_a_daemon(Path::new("/l"), Ok(())).unwrap(),
            "a free lock means nobody"
        );
        assert!(
            lock_is_held_by_a_daemon(Path::new("/l"), Err(std::fs::TryLockError::WouldBlock))
                .unwrap(),
            "a contended lock means a live daemon"
        );
        // Any other failure is NOT evidence of a daemon: the old code turned
        // it into `true`, so the client decided one was already running and
        // never started one, with a message naming the wrong cause.
        let other = lock_is_held_by_a_daemon(
            Path::new("/l"),
            Err(std::fs::TryLockError::Error(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no locking here",
            ))),
        );
        assert!(
            other.is_err(),
            "an unsupported filesystem must not be reported as a running daemon"
        );
        assert!(
            other.unwrap_err().contains("lock"),
            "the reason must name it"
        );
    }

    /// A lock file that cannot even be opened is reported, not guessed at.
    #[test]
    fn an_unopenable_lock_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        // A directory where the lock file should be: opening it for writing
        // fails, and that must surface as an `Err` naming the lock.
        std::fs::create_dir_all(opencraylsp_proto::paths::lock_path(&socket)).unwrap();
        let outcome = daemon_holds_lock(&socket);
        assert!(outcome.is_err(), "got {outcome:?}");
        assert!(outcome.unwrap_err().contains("lock"));
    }

    /// The ordinary cases stay as they were: a free lock means nobody owns the
    /// socket, a held one means a daemon is there. The lock check must not regress this.
    #[test]
    fn a_free_lock_is_not_a_daemon_and_a_held_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        assert!(
            !daemon_holds_lock(&socket).expect("a fresh lock is inspectable"),
            "a free lock must not be reported as a running daemon"
        );
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(opencraylsp_proto::paths::lock_path(&socket))
            .unwrap();
        held.try_lock().unwrap();
        assert!(
            daemon_holds_lock(&socket).expect("the lock is inspectable"),
            "a held lock must be reported as a running daemon"
        );
    }

    // ---- the log tail reads only a window ----

    #[test]
    fn the_tail_keeps_the_last_non_empty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("opencraylsp.log");
        std::fs::write(&log, b"first\n\nsecond\nthird\n").unwrap();
        let tail = log_tail(&log, 2).expect("a tail");
        assert_eq!(tail, "second | third");
    }

    /// A log larger than the window must not be read whole, and the
    /// tail must still come from its end.
    ///
    /// The daemon appends to this file and never rotates it, so its size is
    /// bounded only by how long it has been running and how noisy the server
    /// is. Reading all of it on every failed connect made the *client* the
    /// thing that ran out of memory.
    #[test]
    fn a_huge_log_is_tailed_from_its_end_without_reading_it_all() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("opencraylsp.log");
        // 4 MiB of filler, well past the 64 KiB window.
        let mut text = String::with_capacity(4 * 1024 * 1024 + 64);
        for n in 0..40_000 {
            text.push_str(&format!("filler line {n} aaaaaaaaaaaaaaaaaaaaaaaa\n"));
        }
        let total = text.len() as u64;
        assert!(total > LOG_TAIL_WINDOW_BYTES * 4);
        text.push_str("the real last line\n");
        std::fs::write(&log, text.as_bytes()).unwrap();

        // The *read size* is the property under test, not the tail content:
        // reading the whole file still produces the right last lines, so
        // asserting on the tail alone would pass on the unfixed code.
        let window = log_tail_window(&log).expect("a window");
        assert!(
            window.len() as u64 <= LOG_TAIL_WINDOW_BYTES,
            "only the window may be read, got {} bytes of a {total}-byte file",
            window.len(),
        );
        let tail = log_tail(&log, 3).expect("a tail");
        assert!(
            tail.contains("the real last line"),
            "the tail must come from the end of the file: {tail}"
        );
        assert!(
            !tail.contains("filler line 0 "),
            "the beginning of the log must not be in a 3-line tail"
        );
    }

    /// Seeking to `len - window` lands mid-line; that partial first line must
    /// be dropped rather than reported as if the server had said it.
    #[test]
    fn a_window_starting_mid_line_drops_the_partial_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("opencraylsp.log");
        let mut text = "x".repeat(LOG_TAIL_WINDOW_BYTES as usize + 5_000);
        text.push_str("\nkeep this one\nand this one\n");
        std::fs::write(&log, &text).unwrap();
        let window = log_tail_window(&log).expect("a window");
        assert!(
            !window.starts_with('x'),
            "the truncated first line must be dropped: {:?}",
            &window[..window.len().min(20)]
        );
        assert!(window.starts_with("keep this one"));
    }

    /// A log that is one enormous line has no complete line inside the
    /// window; reporting a fragment would defeat the cap, so there is no tail.
    #[test]
    fn a_single_line_longer_than_the_window_yields_no_tail() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("opencraylsp.log");
        std::fs::write(&log, vec![b'z'; LOG_TAIL_WINDOW_BYTES as usize + 1]).unwrap();
        assert_eq!(log_tail(&log, 5), None);
    }

    /// An empty or missing log must degrade quietly, as before.
    #[test]
    fn an_empty_or_missing_log_has_no_tail() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.log");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(log_tail(&empty, 5), None);
        assert_eq!(log_tail(&dir.path().join("nope.log"), 5), None);
    }

    // ---- an untrusted daemon binary is not started ----

    #[tokio::test]
    async fn a_world_writable_daemon_binary_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("opencraylspd");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o777)).unwrap();

        let mut opts = options();
        // The socket must live in the tempdir: `ClientOptions::default_for_tests`
        // leaves it empty, and `lock_path("")` resolves to `.lock` *in the
        // current directory* — which dropped a stray file into the crate's
        // source tree every time this test ran.
        opts.socket = dir.path().join("opencraylsp.sock");
        opts.daemon_bin = Some(bin);
        let err = spawn_daemon(&opts).expect_err("an untrusted daemon must be refused");
        assert!(
            err.contains("chmod"),
            "the refusal must name the fix: {err}"
        );
        assert!(
            !std::path::Path::new(".lock").exists(),
            "the probe must not write a lock file into the current directory"
        );
    }

    #[tokio::test]
    async fn an_owned_daemon_binary_passes_the_trust_check() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let bin = dir.path().join("opencraylspd");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut opts = options();
        opts.socket = socket.clone();
        opts.daemon_bin = Some(bin);
        // The trust check passes, so the failure now comes from the program
        // itself (a shell script is not a daemon), not from ownership.
        let outcome = spawn_daemon(&opts);
        if let Err(err) = outcome {
            assert!(
                !err.contains("chmod"),
                "a trustworthy binary must not be refused for trust: {err}"
            );
        }
    }

    // ---- the daemon does not inherit the client's RUST_LOG ----

    /// The MCP client runs inside the harness, where `RUST_LOG` is usually set
    /// for *it* — often to `trace`. Inheriting it would turn the long-lived
    /// daemon into a per-request logger writing to a file nobody reads, for the
    /// life of the workspace.
    ///
    /// The variable is set on the *stub's* own command environment rather than on
    /// this process: changing it here is `unsafe` under the crate's
    /// `forbid(unsafe_code)`, and re-executing a child test harness raced the
    /// parent (measurably flaky under `-j`). What is under test is that
    /// `spawn_binary` strips the variable it finds in the environment it is
    /// given, so the stub reports what actually reached it.
    #[tokio::test]
    async fn a_spawned_daemon_does_not_inherit_rust_log() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("env.txt");
        let stub = dir.path().join("opencraylspd");
        std::fs::write(
            &stub,
            "#!/bin/sh\nif [ -z \"${RUST_LOG+x}\" ]; then printf unset > \
             ${OPENCRAYLSP_RECORD}; else printf \"set:%s\" \"$RUST_LOG\" > ${OPENCRAYLSP_RECORD}; fi\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut opts = options();
        opts.socket = dir.path().join("opencraylsp.sock");
        opts.daemon_bin = Some(stub);
        // The child would inherit these; `spawn_binary` must remove RUST_LOG
        // while leaving the rest of the environment alone.
        let record_for_stub = record.display().to_string();
        let spawned = spawn_daemon_with_env(
            &opts,
            &[
                ("RUST_LOG", "trace"),
                ("OPENCRAYLSP_RECORD", &record_for_stub),
            ],
        );
        assert!(spawned.is_ok(), "the trusted stub must start: {spawned:?}");

        let mut seen = String::new();
        for _ in 0..250 {
            if let Ok(text) = std::fs::read_to_string(&record) {
                seen = text;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            seen, "unset",
            "the daemon must not inherit the client's RUST_LOG, it saw {seen:?}"
        );
    }
}
