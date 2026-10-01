//! Daemon logging: a file (the daemon has no terminal), never stdout.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

/// `$XDG_STATE_HOME/opencraylsp/opencraylsp.log`, else `$HOME/.local/state/opencraylsp/opencraylsp.log`.
pub fn default_log_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let non_empty = |key: &str| env(key).filter(|v| !v.is_empty());
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

/// The log is rotated once it passes this size.
///
/// The daemon appends here forever and nothing else ever truncates it: a
/// chatty server, a crash loop, or a long-lived daemon in a chatty environment
/// grows the file without bound until the disk fills. Rotating on startup is
/// enough — a daemon that is restarted periodically (which is what happens
/// after a crash or an upgrade) keeps only one log's worth of history, and a
/// daemon that runs for months in one session was never the growing case.
const LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Moves `path` aside to `<path>.1` when it is over the size cap.
///
/// Only one previous file is kept: the point is to bound the disk, not to
/// archive. The rename is best-effort — if it fails the daemon logs to the
/// existing file rather than refusing to start.
fn rotate_if_large(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= LOG_MAX_BYTES {
        return;
    }
    let previous = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.1"),
        None => "1".to_owned(),
    });
    let _ = std::fs::remove_file(&previous);
    if let Err(err) = std::fs::rename(path, &previous) {
        tracing::debug!(error = %err, "could not rotate the log; continuing with the current file");
    }
}

/// Opens the log for appending, creating its directory (0700) and the file
/// (0600) if needed: the log can hold server stderr and panic messages, so it
/// stays private to this user.
fn open_log(path: &Path) -> Option<std::fs::File> {
    if let Some(dir) = path.parent() {
        let _ = std::os::unix::fs::DirBuilderExt::mode(
            std::fs::DirBuilder::new().recursive(true),
            0o700,
        )
        .create(dir);
    }
    rotate_if_large(path);
    std::os::unix::fs::OpenOptionsExt::mode(
        std::fs::OpenOptions::new().create(true).append(true),
        0o600,
    )
    .open(path)
    .ok()
}

/// Installs the global subscriber. Falls back to stderr when the log file
/// cannot be opened, so a broken log path never stops the daemon. Calling it
/// twice is harmless (the second call is ignored).
pub fn init(explicit: Option<&Path>) {
    let path = explicit
        .map(Path::to_owned)
        .or_else(|| default_log_path(&|key| std::env::var(key).ok()));
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file = path.and_then(|path| open_log(&path));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    let _ = match file {
        Some(file) => builder.with_writer(Mutex::new(file)).try_init(),
        None => builder.with_writer(std::io::stderr).try_init(),
    };
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(install_panic_hook);
}

/// Routes panics into the log. The daemon is detached from any terminal
/// (stderr is null when a client spawns it), so without this a panic leaves no
/// trace at all.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        tracing::error!(%location, %message, "panic");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_log_and_its_directory_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("opencraylsp.log");
        drop(open_log(&path).expect("log opens"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    fn env(pairs: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn log_path_prefers_xdg_state_then_home() {
        assert_eq!(
            default_log_path(&env(vec![("XDG_STATE_HOME", "/s"), ("HOME", "/h")])),
            Some(PathBuf::from("/s/opencraylsp/opencraylsp.log"))
        );
        assert_eq!(
            default_log_path(&env(vec![("HOME", "/h")])),
            Some(PathBuf::from("/h/.local/state/opencraylsp/opencraylsp.log"))
        );
        assert_eq!(
            default_log_path(&env(vec![("XDG_STATE_HOME", ""), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.local/state/opencraylsp/opencraylsp.log"))
        );
        assert_eq!(default_log_path(&env(vec![])), None);
    }

    #[test]
    fn init_tolerates_an_unwritable_path_and_repeat_calls() {
        init(Some(Path::new(
            "/proc/definitely/not/writable/opencraylsp.log",
        )));
        let dir = tempfile::tempdir().unwrap();
        init(Some(&dir.path().join("x.log")));
    }

    /// A log past the size cap is moved aside on open, so the file the
    /// daemon appends to stays bounded and only one previous log is kept.
    #[test]
    fn an_oversized_log_is_rotated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencraylsp.log");
        std::fs::write(&path, vec![b'x'; (LOG_MAX_BYTES + 1024) as usize]).unwrap();

        drop(open_log(&path).expect("log opens"));

        let fresh = std::fs::metadata(&path).unwrap().len();
        assert!(
            fresh <= LOG_MAX_BYTES,
            "the live log must not be the oversized one, it is {fresh} bytes"
        );
        let previous = dir.path().join("opencraylsp.log.1");
        assert!(
            previous.exists(),
            "the old log must be kept aside as opencraylsp.log.1"
        );
        assert_eq!(
            std::fs::metadata(&previous).unwrap().len(),
            LOG_MAX_BYTES + 1024
        );
    }

    /// A log under the cap is left exactly where it is: rotation must not throw
    /// away history on every start.
    #[test]
    fn a_small_log_is_appended_to_not_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencraylsp.log");
        std::fs::write(&path, b"earlier entry\n").unwrap();

        drop(open_log(&path).expect("log opens"));

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "earlier entry\n",
            "an under-cap log must keep its contents"
        );
        assert!(!dir.path().join("opencraylsp.log.1").exists());
    }

    /// Only one generation is kept: a second oversized start replaces the
    /// previous one rather than stacking copies forever.
    #[test]
    fn rotation_keeps_exactly_one_previous_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencraylsp.log");
        for _ in 0..3 {
            std::fs::write(&path, vec![b'x'; (LOG_MAX_BYTES + 1) as usize]).unwrap();
            drop(open_log(&path).expect("log opens"));
        }
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries.contains(&"opencraylsp.log.1".to_owned()),
            "{entries:?}"
        );
        assert!(
            !entries.iter().any(|name| name.ends_with(".1.1")),
            "rotation must not accumulate generations: {entries:?}"
        );
    }
}
