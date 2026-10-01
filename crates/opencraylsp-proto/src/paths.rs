//! Where the daemon's socket and lock file live. Both the daemon
//! and every client resolve the path through this module so they always agree.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Environment variable overriding the socket path.
pub const SOCKET_ENV: &str = "OPENCRAYLSP_SOCKET";

/// The socket path from the process environment.
///
/// The uid fallback arm is only reached when neither `OPENCRAYLSP_SOCKET` nor
/// `XDG_RUNTIME_DIR` is set *and* the uid cannot be determined; in that case
/// the path is left empty rather than naming a shared `/tmp/opencraylsp-0` location.
/// Callers that need a real path should use [`require_socket_path`].
pub fn default_socket_path() -> PathBuf {
    socket_path_from(&|key| std::env::var(key).ok(), current_uid())
}

/// [`default_socket_path`] that refuses to hand back a shared location.
pub fn require_socket_path() -> Result<PathBuf, String> {
    let from_env = &|key: &str| std::env::var(key).ok();
    if let Some(explicit) = from_env(SOCKET_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    if let Some(runtime) = from_env("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return Ok(Path::new(&runtime)
            .join("opencraylsp")
            .join("opencraylsp.sock"));
    }
    let uid = require_current_uid()?;
    Ok(PathBuf::from(format!("/tmp/opencraylsp-{uid}")).join("opencraylsp.sock"))
}

/// Same as [`default_socket_path`] with the environment and uid injected.
///
/// Order: `OPENCRAYLSP_SOCKET` > `$XDG_RUNTIME_DIR/opencraylsp/opencraylsp.sock` >
/// `/tmp/opencraylsp-<uid>/opencraylsp.sock`. Empty values count as unset.
pub fn socket_path_from(env: &dyn Fn(&str) -> Option<String>, uid: Option<u32>) -> PathBuf {
    let non_empty = |key: &str| env(key).filter(|v| !v.is_empty());
    if let Some(explicit) = non_empty(SOCKET_ENV) {
        return PathBuf::from(explicit);
    }
    if let Some(runtime) = non_empty("XDG_RUNTIME_DIR") {
        return Path::new(&runtime)
            .join("opencraylsp")
            .join("opencraylsp.sock");
    }
    // No uid means no safe per-user location: return an empty path rather than
    // a shared one. `require_socket_path` is the entry point that refuses.
    match uid {
        Some(uid) => PathBuf::from(format!("/tmp/opencraylsp-{uid}")).join("opencraylsp.sock"),
        None => PathBuf::new(),
    }
}

/// The lock file that guards the single daemon instance for `socket`.
///
/// An empty socket has no parent to name a lock relative to, and appending the
/// suffix would produce `.lock` *in the current directory* — a file with no
/// relation to any daemon. Callers that probe the lock (a client deciding
/// whether to start one) would then create that stray file on every run.
/// Such a socket is refused here instead, so a caller must give a real path.
pub fn lock_path(socket: &Path) -> PathBuf {
    if socket.as_os_str().is_empty() {
        return PathBuf::from("<no-socket>/opencraylsp.sock.lock");
    }
    let mut name = socket.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// The uid of the current process, without `unsafe` or extra crates: the
/// owner of `/proc/self` on Linux, the owner of `$HOME` elsewhere.
///
/// Returns `None` when neither can be read, rather than guessing `0`.
pub fn current_uid() -> Option<u32> {
    std::fs::metadata("/proc/self")
        .or_else(|_| std::fs::metadata(std::env::var_os("HOME").unwrap_or_default()))
        .ok()
        .map(|m| m.uid())
}

/// [`current_uid`], or an error naming what could not be determined.
///
/// The old fallback was `unwrap_or(0)`. On a system where `/proc` is not
/// mounted and `$HOME` is unset, that produced uid 0, so the socket path became
/// `/tmp/opencraylsp-0/opencraylsp.sock` — one path shared by *every* user on the machine.
/// Whatever the socket directory's permissions, naming a shared path is not a
/// safe default: it makes the socket a fixed, guessable target and lets one
/// user's daemon squat on another's. Refusing to start is honest; guessing
/// root is not.
pub fn require_current_uid() -> Result<u32, String> {
    current_uid().ok_or_else(|| {
        "cannot determine the current user id (neither /proc/self nor $HOME is readable); \
         set OPENCRAYLSP_SOCKET explicitly or run where the user's identity is discoverable"
            .to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn explicit_env_wins() {
        let env = env_of(&[
            ("OPENCRAYLSP_SOCKET", "/x/y.sock"),
            ("XDG_RUNTIME_DIR", "/run/u"),
        ]);
        assert_eq!(socket_path_from(&env, Some(1)), PathBuf::from("/x/y.sock"));
    }

    #[test]
    fn xdg_runtime_dir_is_next() {
        let env = env_of(&[("XDG_RUNTIME_DIR", "/run/user/7")]);
        assert_eq!(
            socket_path_from(&env, Some(7)),
            PathBuf::from("/run/user/7/opencraylsp/opencraylsp.sock")
        );
    }

    #[test]
    fn falls_back_to_tmp_with_uid() {
        assert_eq!(
            socket_path_from(&env_of(&[]), Some(1234)),
            PathBuf::from("/tmp/opencraylsp-1234/opencraylsp.sock")
        );
    }

    /// With no uid there is no safe per-user location. The old code
    /// defaulted to uid 0 and produced `/tmp/opencraylsp-0/opencraylsp.sock` — one path
    /// shared by every user on the machine, and a fixed target to squat on.
    #[test]
    fn an_unknown_uid_yields_no_path_rather_than_the_root_one() {
        assert_eq!(
            socket_path_from(&env_of(&[]), None),
            PathBuf::new(),
            "an undeterminable uid must not fall back to /tmp/opencraylsp-0"
        );
        // An explicit socket is still honoured without any uid.
        let env = env_of(&[("OPENCRAYLSP_SOCKET", "/x/y.sock")]);
        assert_eq!(socket_path_from(&env, None), PathBuf::from("/x/y.sock"));
        let env = env_of(&[("XDG_RUNTIME_DIR", "/run/user/9")]);
        assert_eq!(
            socket_path_from(&env, None),
            PathBuf::from("/run/user/9/opencraylsp/opencraylsp.sock")
        );
    }

    #[test]
    fn current_uid_is_stable() {
        assert_eq!(current_uid(), current_uid());
        // On a normal Linux box the uid is discoverable; it must never be
        // reported as absent there.
        assert!(current_uid().is_some(), "the uid should be discoverable");
    }

    #[test]
    fn empty_values_count_as_unset() {
        let env = env_of(&[("OPENCRAYLSP_SOCKET", ""), ("XDG_RUNTIME_DIR", "")]);
        assert_eq!(
            socket_path_from(&env, Some(5)),
            PathBuf::from("/tmp/opencraylsp-5/opencraylsp.sock")
        );
    }

    #[test]
    fn lock_path_appends_suffix() {
        assert_eq!(
            lock_path(Path::new("/a/opencraylsp.sock")),
            PathBuf::from("/a/opencraylsp.sock.lock")
        );
    }

    /// An empty socket must not yield a lock file in the current directory:
    /// `"".lock` is `.lock`, and a client probing the lock would create a
    /// stray file in whatever directory it happened to run from (this really
    /// happened: a test left `crates/opencraylsp-client/.lock` behind).
    #[test]
    fn an_empty_socket_has_no_lock_path_in_the_current_directory() {
        let lock = lock_path(Path::new(""));
        assert!(
            !lock.as_os_str().is_empty() && lock != Path::new(".lock"),
            "an empty socket must not resolve to `.lock`, got {lock:?}"
        );
    }
}
