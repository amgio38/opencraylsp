//! Locating the real product binaries for the e2e tests.
//!
//! `opencraylspd` and `opencraylsp-mcp` belong to other packages, so cargo does not define
//! `CARGO_BIN_EXE_*` for them here. They live next to the test binary, and the
//! suite **always rebuilds them once** before use: a stale binary from an
//! earlier build would silently test yesterday's product (that is how a T5
//! check was once misread), and `cargo build` is a cheap no-op when they are
//! current. `fake-lsp` is this crate's own bin, so `CARGO_BIN_EXE_fake-lsp` is
//! always available and always the same profile as the test.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Absolute paths to everything the suite spawns.
#[derive(Debug, Clone)]
pub struct Binaries {
    pub opencraylspd: PathBuf,
    pub opencraylsp_mcp: PathBuf,
    pub fake_lsp: PathBuf,
}

/// The binaries, built once if needed. Panics with the paths that were tried.
pub fn binaries() -> &'static Binaries {
    static BINARIES: OnceLock<Binaries> = OnceLock::new();
    BINARIES.get_or_init(|| {
        let dir = profile_dir();
        let opencraylspd = dir.join("opencraylspd");
        let opencraylsp_mcp = dir.join("opencraylsp-mcp");
        build_products(&dir);
        assert!(
            opencraylspd.is_file() && opencraylsp_mcp.is_file(),
            "the e2e suite needs the `opencraylspd` and `opencraylsp-mcp` binaries in {} (looked for {} and {}); \
             run `cargo build -p opencraylspd -p opencraylsp-mcp` first",
            dir.display(),
            opencraylspd.display(),
            opencraylsp_mcp.display()
        );
        Binaries {
            opencraylspd,
            opencraylsp_mcp,
            fake_lsp: PathBuf::from(env!("CARGO_BIN_EXE_fake-lsp")),
        }
    })
}

/// `<target>/<profile>`: the directory holding the product binaries.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe is always available");
    // .../<target>/<profile>/deps/<test binary>
    match exe.parent().and_then(Path::parent) {
        Some(dir) => dir.to_path_buf(),
        None => panic!(
            "cannot derive the target profile directory from {}; \
             the e2e suite expects to run from <target>/<profile>/deps/",
            exe.display()
        ),
    }
}

fn build_products(profile_dir: &Path) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target = profile_dir
        .parent()
        .expect("profile directory has a parent (the target root)");
    let mut command = Command::new(&cargo);
    command
        .arg("build")
        .arg("-p")
        .arg("opencraylspd")
        .arg("-p")
        .arg("opencraylsp-mcp")
        .arg("--target-dir")
        .arg(target);
    // Build the same profile the tests run in.
    if let Some(profile) = profile_dir.file_name().and_then(|name| name.to_str())
        && profile != "debug"
    {
        command.arg("--profile").arg(profile);
    }
    let status = command.status();
    match status {
        Ok(status) if status.success() => {}
        other => panic!(
            "could not build `opencraylspd`/`opencraylsp-mcp` for the e2e suite: `{cargo:?} build -p opencraylspd -p opencraylsp-mcp \
             --target-dir {}` returned {other:?}; run it by hand first",
            target.display()
        ),
    }
}
