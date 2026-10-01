//! The daemon's own runaway guard: sample this process's resident memory on the
//! pool's memory-sampling tick and shut down cleanly when it is over the
//! ceiling.
//!
//! Why the daemon needs its own ceiling: `max_rss_mb` governs the *language
//! servers*, and a client restarting one of those does not touch the daemon. A
//! leak in the daemon itself — a growing pending map, a cache nobody trims — is
//! invisible to that limit, so without this the daemon grows until the machine
//! stops answering.
//!
//! Why the trip counter is on disk: the daemon that goes over the limit is
//! *restarted* by its client, so it is a brand new process with no memory of how
//! often it has already done this. Counting in memory would restart-loop
//! forever: each new daemon leaks, exceeds, exits, and the client dutifully
//! starts another. A small file beside the socket is the only state that
//! survives the process, and it is the only state that can stop the loop.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How many over-limit exits are allowed inside [`OVER_LIMIT_WINDOW`].
///
/// Two, so the *third* one inside the hour is refused — which is what the
/// requirement says: an over-limit daemon that has already restarted twice is
/// not going to behave on the third try, and each restart costs the user a
/// warm cache and a fresh index.
///
/// Note this is one less than the per-instance memory guard's
/// `MEMORY_RESTARTS_PER_WINDOW`, which allows three restarts before refusing
/// the fourth. The two numbers answer different questions: a language server
/// is restarted *in place* by the same daemon, so a retry is cheap, whereas the
/// daemon's own exit is a full cold start that a client has to pay for.
pub const RESTART_BUDGET: usize = 2;

/// The sliding window those exits are counted over.
pub const OVER_LIMIT_WINDOW: Duration = Duration::from_secs(3600);

/// The file recording when the daemon last exited over its ceiling.
///
/// Beside the socket: it must live where two daemons on the same machine would
/// agree, and it must not survive as a stale marker in a directory nobody
/// cleans. `<socket>.rss-over-limit` is derived from the socket, so a daemon on
/// a custom socket keeps its own history instead of sharing the default one's.
pub fn stamp_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(".rss-over-limit");
    PathBuf::from(name)
}

/// What the recorded history says about right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Under the ceiling, or the history is empty: shutting down is allowed and
    /// the exit is recorded.
    Record,
    /// Too many exits in the window: do not shut down. The daemon keeps serving
    /// and reports the condition instead, because a restart loop helps nobody.
    Refuse { exits: usize },
}

/// Decides whether an over-limit exit is allowed, from the exits recorded in
/// `stamp` (oldest first) at time `now`.
///
/// Entries outside the window are dropped rather than counted, so a daemon that
/// has been healthy for an hour starts with a clean count again.
pub fn verdict(stamp: &[u64], now: SystemTime) -> Verdict {
    // The clock is read through `now` so a test can place the window anywhere;
    // a clock before the epoch has nothing to measure against and is treated as
    // no history rather than as a refusal.
    let Ok(epoch) = now.duration_since(SystemTime::UNIX_EPOCH) else {
        return Verdict::Record;
    };
    let recent: Vec<u64> = stamp
        .iter()
        .copied()
        .filter(|at| epoch.saturating_sub(Duration::from_secs(*at)) < OVER_LIMIT_WINDOW)
        .collect();
    if recent.len() > RESTART_BUDGET {
        Verdict::Refuse {
            exits: recent.len(),
        }
    } else {
        Verdict::Record
    }
}

/// How often the refusal is allowed to say so, so a daemon that stays over the
/// ceiling for an hour does not fill its own log with one line per sample.
pub const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Whether enough time has passed to log the refusal again.
pub fn may_log_refusal(last: Option<SystemTime>, now: SystemTime) -> bool {
    match last {
        None => true,
        Some(at) => now
            .duration_since(at)
            .map(|since| since >= REFUSAL_LOG_INTERVAL)
            .unwrap_or(true),
    }
}

/// Appends `now` to `path` and keeps the file to the last few entries.
///
/// The file is written with mode 0600: it names when a daemon gave up, and
/// lives in the socket's directory, which is already 0700 — but the mode is set
/// explicitly rather than inherited from the process umask.
///
/// Entries older than the window are dropped on the way in, so the file cannot
/// grow without bound on a machine that has been up for months.
pub fn record_exit(path: &Path, now: SystemTime) -> std::io::Result<()> {
    let stamp = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut kept: Vec<u64> = read_stamp(path)
        .into_iter()
        .filter(|at| {
            now.duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.saturating_sub(Duration::from_secs(*at)) < OVER_LIMIT_WINDOW)
                .unwrap_or(false)
        })
        .collect();
    kept.push(stamp);
    if kept.len() > RESTART_BUDGET + 1 {
        let excess = kept.len() - (RESTART_BUDGET + 1);
        kept.drain(..excess);
    }
    let text = kept
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{text}\n"))?;
    restrict(path);
    Ok(())
}

/// Reads the recorded exit times, oldest first.
///
/// A file that cannot be read, or that holds something other than numbers, is
/// not an emergency: it is treated as no history at all. Refusing to start
/// because a marker file is corrupt would turn a nuisance into an outage, and
/// the window is short enough that a lost history costs at most a few extra
/// restarts.
pub fn read_stamp(path: &Path) -> Vec<u64> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.trim().parse::<u64>().ok())
        .collect()
}

/// Mode 0600 on the stamp file, best effort: a filesystem that refuses to say
/// (Windows, some network mounts) must not stop the daemon from exiting.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn secs_ago(n: u64) -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            - n
    }

    /// The first two exits are recorded; the third is refused. That is the whole
    /// point of the file: the third daemon must not also exit, or the client
    /// restarts it forever.
    #[test]
    fn the_third_exit_inside_the_window_is_refused() {
        let now = SystemTime::now();
        assert_eq!(verdict(&[], now), Verdict::Record);
        assert_eq!(verdict(&[secs_ago(10)], now), Verdict::Record);
        // The second exit is the last one allowed; the third is the refusal.
        assert_eq!(verdict(&[secs_ago(20), secs_ago(10)], now), Verdict::Record);
        assert_eq!(
            verdict(&[secs_ago(30), secs_ago(20), secs_ago(10)], now),
            Verdict::Refuse { exits: 3 },
            "two restarts have already been paid for"
        );
    }

    /// The count is a sliding window, not a lifetime total: after the hour the
    /// daemon starts over, so a machine that leaked once an afternoon ago gets
    /// its full budget of restarts again.
    #[test]
    fn an_hour_later_the_budget_is_restored() {
        let now = SystemTime::now();
        let old = [
            secs_ago(OVER_LIMIT_WINDOW.as_secs() + 60),
            secs_ago(OVER_LIMIT_WINDOW.as_secs() + 30),
            secs_ago(OVER_LIMIT_WINDOW.as_secs() + 10),
        ];
        assert_eq!(
            verdict(&old, now),
            Verdict::Record,
            "entries older than the window must not count"
        );
    }

    /// A stamp file that was truncated by a crash, or written by an older
    /// version, must not stop the daemon from exiting — that would trade a
    /// restart for a permanent refusal.
    #[test]
    fn a_damaged_stamp_file_reads_as_no_history() {
        let dir = temp();
        let path = dir.path().join("stamp");
        std::fs::write(&path, "not a number\n\n42\n").expect("write");
        assert_eq!(read_stamp(&path), vec![42], "garbage lines are dropped");

        std::fs::write(&path, "").expect("write");
        assert_eq!(read_stamp(&path), Vec::<u64>::new());

        // A file that is not there at all is the normal first-run case.
        assert_eq!(read_stamp(&dir.path().join("absent")), Vec::<u64>::new());
        assert_eq!(
            verdict(&read_stamp(&dir.path().join("absent")), SystemTime::now()),
            Verdict::Record
        );
    }

    /// Recording accumulates across daemon lifetimes and stops growing.
    #[test]
    fn recording_accumulates_across_restarts_and_is_bounded() {
        let dir = temp();
        let path = dir.path().join("stamp");
        for _ in 0..5 {
            record_exit(&path, SystemTime::now()).expect("record");
        }
        let entries = read_stamp(&path);
        assert_eq!(
            entries.len(),
            RESTART_BUDGET + 1,
            "the file must not grow past the window's worth of entries"
        );
        assert_eq!(
            verdict(&entries, SystemTime::now()),
            Verdict::Refuse { exits: 3 }
        );
    }

    /// The stamp sits beside the socket, so a daemon on a custom socket keeps
    /// its own history rather than sharing the default daemon's.
    #[test]
    fn the_stamp_is_derived_from_the_socket() {
        assert_eq!(
            stamp_path(Path::new("/run/user/7/opencraylsp/opencraylsp.sock")),
            PathBuf::from("/run/user/7/opencraylsp/opencraylsp.sock.rss-over-limit")
        );
    }

    /// The file names when a daemon gave up; it is written 0600 rather than left
    /// to the umask.
    #[cfg(unix)]
    #[test]
    fn the_stamp_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp();
        let path = dir.path().join("stamp");
        record_exit(&path, SystemTime::now()).expect("record");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the stamp must be readable only by its owner");
    }

    /// A refusal is logged at most once a minute, so a daemon that stays over
    /// the ceiling does not write a line per sample forever.
    #[test]
    fn a_refusal_is_logged_at_most_once_a_minute() {
        let now = SystemTime::now();
        assert!(
            may_log_refusal(None, now),
            "the first refusal always speaks"
        );
        let just_now = now
            .checked_sub(Duration::from_secs(5))
            .expect("earlier time");
        assert!(!may_log_refusal(Some(just_now), now));
        let long_ago = now
            .checked_sub(REFUSAL_LOG_INTERVAL + Duration::from_secs(1))
            .expect("earlier time");
        assert!(may_log_refusal(Some(long_ago), now));
    }
}
