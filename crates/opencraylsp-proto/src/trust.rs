//! Is this program safe to execute as this user?
//!
//! The client starts `opencraylspd` from `PATH`, and the daemon starts the configured
//! language servers. Both are resolved by name, so a directory early on `PATH`
//! (or a writable entry in it) decides what code runs with the user's
//! privileges — the same mistake as sourcing a shell profile from a shared
//! directory.
//!
//! The check is deliberately narrow and advisory-friendly: it refuses the
//! clearly dangerous cases (group- or world-writable, or owned by somebody else)
//! and says which one it found, so the message can tell the user what to fix.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Why a program was refused, or that it is fine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// The program is owned by this user and not writable by anyone else.
    Owned,
    /// Somebody else owns it.
    ForeignOwner { uid: u32 },
    /// Group or other may write it, so anyone can replace what runs.
    GroupOrWorldWritable { mode: u32 },
    /// It could not be inspected; the caller decides whether that is fatal.
    Unreadable { reason: String },
}

impl Trust {
    /// A sentence the user can act on, naming the fix.
    pub fn explain(&self, path: &Path) -> String {
        match self {
            Trust::Owned => format!("`{}` is owned by this user", path.display()),
            Trust::ForeignOwner { uid } => format!(
                "`{}` is owned by uid {uid}, not by this user; refusing to run a \
                 program another account controls (install it under your own \
                 prefix, or point the config at your copy)",
                path.display()
            ),
            Trust::GroupOrWorldWritable { mode } => format!(
                "`{}` is mode {:o}, so group or other can replace it; refusing to \
                 run a program anyone can swap out (run `chmod 755 {}`)",
                path.display(),
                mode & 0o777,
                path.display()
            ),
            Trust::Unreadable { reason } => {
                format!("cannot inspect `{}`: {reason}", path.display())
            }
        }
    }
}

/// Inspects `path` and reports whether it is safe to execute.
///
/// Symlinks are followed, because what matters is the file that actually runs:
/// a link in a trusted directory pointing at somebody else's binary is exactly
/// the case this is meant to catch.
pub fn program_trust(path: &Path) -> Trust {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            return Trust::Unreadable {
                reason: err.to_string(),
            };
        }
    };
    // The ownership half needs a uid to compare against. When the current uid
    // is undiscoverable only the permission half can be judged.
    if let Some(me) = current_uid()
        && meta.uid() != me
    {
        return Trust::ForeignOwner { uid: meta.uid() };
    }
    if meta.mode() & 0o022 != 0 {
        return Trust::GroupOrWorldWritable {
            mode: meta.mode() & 0o777,
        };
    }
    Trust::Owned
}

/// Whether `path` may be executed: true only when [`program_trust`] says
/// [`Trust::Owned`]. An unreadable path is not trusted.
pub fn is_trustworthy_program(path: &Path) -> bool {
    program_trust(path) == Trust::Owned
}

/// The uid of the current process, or `None` when it cannot be determined.
///
/// `/proc/self` on Linux, else the owner of `$HOME`.
fn current_uid() -> Option<u32> {
    std::fs::metadata("/proc/self")
        .or_else(|_| std::fs::metadata(std::env::var_os("HOME").unwrap_or_default()))
        .ok()
        .map(|m| m.uid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_exec(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn an_owned_0755_binary_is_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("opencraylspd");
        write_exec(&bin, "#!/bin/sh\n");
        assert_eq!(program_trust(&bin), Trust::Owned);
        assert!(is_trustworthy_program(&bin));
    }

    #[test]
    fn a_world_writable_binary_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("opencraylspd");
        write_exec(&bin, "#!/bin/sh\n");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            program_trust(&bin),
            Trust::GroupOrWorldWritable { .. }
        ));
        assert!(!is_trustworthy_program(&bin));
    }

    #[test]
    fn a_group_writable_binary_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("opencraylspd");
        write_exec(&bin, "#!/bin/sh\n");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(!is_trustworthy_program(&bin));
    }

    #[test]
    fn a_symlink_is_judged_by_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-opencraylspd");
        write_exec(&real, "#!/bin/sh\n");
        let link = dir.path().join("opencraylspd");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // The link points at a trustworthy file, so it is trustworthy — what
        // runs is the target, not the name.
        assert_eq!(program_trust(&link), Trust::Owned);
    }

    #[test]
    fn a_missing_binary_is_unreadable_not_trusted() {
        assert!(matches!(
            program_trust(Path::new("/definitely/not/here/opencraylspd")),
            Trust::Unreadable { .. }
        ));
        assert!(!is_trustworthy_program(Path::new(
            "/definitely/not/here/opencraylspd"
        )));
    }

    #[test]
    fn the_explanation_names_the_problem_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("opencraylspd");
        write_exec(&bin, "#!/bin/sh\n");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o777)).unwrap();
        let text = Trust::GroupOrWorldWritable { mode: 0o777 }.explain(&bin);
        assert!(text.contains("chmod"), "{text}");
        assert!(text.contains(&bin.display().to_string()), "{text}");
    }
}
