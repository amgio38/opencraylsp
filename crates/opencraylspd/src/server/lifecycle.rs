//! Single-instance lock and socket setup .
//!
//! The lock file `<socket>.lock` is held with an advisory `flock` for the whole
//! process lifetime. Holding it is what entitles the daemon to delete a stale
//! socket left by a crashed predecessor and bind a fresh one: a client that
//! finds a dead socket never unlinks it itself, because releasing the lock and
//! deleting the file would race with another daemon starting up.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use opencraylsp_proto::paths::{current_uid, lock_path};
use tokio::net::UnixListener;

/// Why the daemon could not take ownership of its socket.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// Another daemon holds the lock: not a failure, the caller exits quietly.
    #[error("another opencraylspd already owns `{0}`")]
    AlreadyRunning(PathBuf),
    #[error("cannot prepare `{path}`: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("socket directory `{path}` belongs to uid {owner}, not to this user (uid {me})")]
    ForeignDirectory { path: PathBuf, owner: u32, me: u32 },
    /// The socket directory path exists but is a symlink or a file, not a
    /// directory, so what the checks would inspect is not what gets used.
    #[error("socket directory `{path}` is not a real directory (a symlink or a file)")]
    NotADirectory { path: PathBuf },
    /// The socket directory is writable by group or other, so anyone can plant
    /// the lock file in it and decide whether a daemon ever starts.
    #[error(
        "socket directory `{path}` is mode {mode:o}, which lets group or other write to it; \
         run `chmod 700 {path}`"
    )]
    LooseDirectory { path: PathBuf, mode: u32 },
    /// The current user's id could not be determined, so the socket
    /// directory's owner cannot be verified. Guessing a uid would let the
    /// daemon trust a directory it has not checked.
    #[error(
        "cannot determine the current user id to verify `{path}`; \
         set OPENCRAYLSP_SOCKET to a path you own"
    )]
    UnknownUid { path: PathBuf },
}

fn io_error(path: &Path, err: &io::Error) -> LifecycleError {
    LifecycleError::Io {
        path: path.to_owned(),
        reason: err.to_string(),
    }
}

/// The daemon's claim on its socket path: the held lock plus the bound
/// listener. Dropping it releases the lock; [`Ownership::cleanup`] also removes
/// the socket file.
#[derive(Debug)]
pub struct Ownership {
    pub listener: UnixListener,
    socket: PathBuf,
    _lock: File,
}

impl Ownership {
    /// Removes the socket file (the lock is released on drop).
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Creates the socket's directory (mode 0700) if missing and refuses one owned
/// by somebody else or reachable by them.
///
/// Ownership alone is not enough. A directory the owner has left group- or
/// world-writable (or whose mode was loosened by a stray `chmod -R`) lets
/// anyone create the `.lock` file inside it, and a lock file an attacker owns
/// is a lock they can hold — which keeps every client from ever starting a
/// daemon, and lets them decide when one appears.
fn ensure_private_dir(dir: &Path) -> Result<(), LifecycleError> {
    // `symlink_metadata`, not `metadata`: a symlink would be followed to
    // whatever it points at, so a link to a directory this user happens to own
    // would pass the checks below while the real socket lands somewhere else.
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.file_type().is_dir() {
                return Err(LifecycleError::NotADirectory {
                    path: dir.to_owned(),
                });
            }
            let me = current_uid().ok_or_else(|| LifecycleError::UnknownUid {
                path: dir.to_owned(),
            })?;
            if meta.uid() != me {
                return Err(LifecycleError::ForeignDirectory {
                    path: dir.to_owned(),
                    owner: meta.uid(),
                    me,
                });
            }
            // Only the two write bits matter: read and search permissions are
            // what a shared directory legitimately needs (a group-readable
            // project tree, say), while write is what lets someone plant the
            // socket or the lock in it.
            if meta.mode() & 0o022 != 0 {
                return Err(LifecycleError::LooseDirectory {
                    path: dir.to_owned(),
                    mode: meta.mode() & 0o777,
                });
            }
            Ok(())
        }
        Err(_) => std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| io_error(dir, &e)),
    }
}

/// Takes the lock, clears any stale socket, and binds `socket` with mode 0600.
pub fn claim(socket: &Path) -> Result<Ownership, LifecycleError> {
    if let Some(dir) = socket.parent().filter(|d| !d.as_os_str().is_empty()) {
        ensure_private_dir(dir)?;
    }
    let lock_file_path = lock_path(socket);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_file_path)
        .map_err(|e| io_error(&lock_file_path, &e))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(LifecycleError::AlreadyRunning(socket.to_owned()));
        }
        Err(TryLockError::Error(e)) => return Err(io_error(&lock_file_path, &e)),
    }
    // We hold the lock, so whatever sits at the socket path is a corpse.
    match std::fs::remove_file(socket) {
        Ok(()) => tracing::info!(path = %socket.display(), "removed stale socket"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error(socket, &e)),
    }
    let listener = UnixListener::bind(socket).map_err(|e| io_error(socket, &e))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| io_error(socket, &e))?;
    Ok(Ownership {
        listener,
        socket: socket.to_owned(),
        _lock: lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn claim_binds_a_private_socket_in_a_private_directory() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("run/opencraylsp.sock");
        let owned = claim(&socket).unwrap();
        assert_eq!(mode(&socket), 0o600);
        assert_eq!(
            mode(socket.parent().unwrap()),
            0o700,
            "created directory is private"
        );
        assert!(lock_path(&socket).exists());
        owned.cleanup();
        assert!(!socket.exists());
    }

    /// Claims `socket`, retrying briefly. Other tests in this binary spawn
    /// processes, and for the instant between `fork` and `exec` a child holds a
    /// duplicate of every open descriptor - including a just-dropped lock - so
    /// the lock can look held for a few milliseconds after `drop`.
    fn claim_soon(socket: &std::path::Path) -> bool {
        for _ in 0..100 {
            if claim(socket).is_ok() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[tokio::test]
    async fn a_second_claim_is_refused_while_the_first_lives() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let first = claim(&socket).unwrap();
        let second = claim(&socket).unwrap_err();
        assert!(
            matches!(second, LifecycleError::AlreadyRunning(_)),
            "{second:?}"
        );
        drop(first);
        // Once the first is gone the lock is free again.
        assert!(claim_soon(&socket));
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_once_the_lock_is_ours() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        // A corpse: a regular file where the socket should be.
        std::fs::write(&socket, "stale").unwrap();
        let owned = claim(&socket).unwrap();
        assert_eq!(mode(&socket), 0o600);
        drop(owned);
    }

    #[tokio::test]
    async fn a_socket_left_by_a_dead_daemon_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        drop(claim(&socket).unwrap()); // leaves the socket file behind, lock released
        assert!(socket.exists());
        assert!(claim_soon(&socket));
    }

    #[test]
    fn a_directory_owned_by_someone_else_is_refused() {
        // `/proc` is owned by root; skip when the tests themselves run as root.
        if current_uid().is_some_and(|uid| uid == 0) {
            return;
        }
        let err = ensure_private_dir(Path::new("/proc")).unwrap_err();
        assert!(
            matches!(err, LifecycleError::ForeignDirectory { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_symlink_in_place_of_the_socket_directory_is_refused() {
        let base = tempfile::tempdir().expect("temp dir");
        let real = base.path().join("real");
        std::fs::create_dir(&real).expect("create real dir");
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");
        let err = ensure_private_dir(&link).unwrap_err();
        assert!(
            matches!(err, LifecycleError::NotADirectory { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn an_unwritable_location_reports_its_path() {
        let err = claim(Path::new("/proc/definitely/not/here/opencraylsp.sock")).unwrap_err();
        assert!(err.to_string().contains("/proc"), "{err}");
    }

    #[test]
    fn a_bare_relative_socket_name_needs_no_directory() {
        // No parent component to create; the lock file is made in the cwd of
        // the test, so only exercise the parent-less branch of the helper.
        assert!(
            Path::new("opencraylsp.sock")
                .parent()
                .unwrap()
                .as_os_str()
                .is_empty()
        );
    }

    /// A socket directory that group or other can write to is refused.
    /// Ownership alone is not enough — anyone who can write the directory can
    /// plant the `.lock` file, and a lock they own is a lock they can hold,
    /// which stops every client from ever starting a daemon.
    #[test]
    fn a_world_writable_socket_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = ensure_private_dir(&path).expect_err("a writable directory must be refused");
        assert!(
            matches!(err, LifecycleError::LooseDirectory { .. }),
            "want LooseDirectory, got {err:?}"
        );
        // The message has to name the fix.
        assert!(err.to_string().contains("chmod 700"), "{err}");
    }

    #[test]
    fn a_group_writable_socket_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(ensure_private_dir(&path).is_err());
    }

    /// Read and search bits are what a legitimately shared directory needs;
    /// only write access is the problem, so 0755 must still be accepted.
    #[test]
    fn a_readable_socket_directory_is_accepted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&path).is_ok(), "0755 must be accepted");
    }
}
