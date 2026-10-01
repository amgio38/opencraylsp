//! The real `opencraylsp-mcp` binary, given an argument that is not valid UTF-8.
//!
//! This has to run the *binary*, not the test harness. An earlier version of
//! this check re-executed the test executable itself, which meant it was
//! exercising libtest's argument handling rather than the program's — and
//! `std::env::args()` panicking on non-UTF-8 input is a property of `std`,
//! not of this crate, so the check passed even with the panic reintroduced.
//! It also printed `panicked at .../env.rs` on every run, which looks like a
//! real failure to anyone reading the output.
//!
//! `CARGO_BIN_EXE_opencraylsp-mcp` is set by cargo to the binary built for this test,
//! so this starts the program a user would start.

use std::os::unix::ffi::OsStringExt;
use std::process::Command;

/// Runs the real binary with one non-UTF-8 argument and returns its outcome.
fn run_with_non_utf8_arg() -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencraylsp-mcp"));
    // An unknown flag, so the program rejects it *after* reading its arguments.
    // The point is that reaching argument parsing at all is what used to panic.
    command.arg(std::ffi::OsString::from_vec(vec![b'-', b'-', 0xFF, 0xFE]));
    command.output().expect("opencraylsp-mcp should run")
}

/// A non-UTF-8 argument must not kill the binary with a panic.
///
/// `std::env::args()` panics outright when any argument is not valid UTF-8, and
/// a workspace path with unusual bytes is ordinary rather than a mistake. The
/// program reads its arguments with `args_os` for exactly this reason.
#[test]
fn a_non_utf8_argument_does_not_panic_the_binary() {
    let output = run_with_non_utf8_arg();
    let stderr = String::from_utf8_lossy(&output.stderr);

    // A Rust panic prints this and exits 101. Asserting on the message is what
    // makes the check meaningful: `status.code().is_some()` is satisfied by a
    // panicking exit too, which is exactly the weakness this replaces.
    assert!(
        !stderr.contains("panicked"),
        "opencraylsp-mcp panicked on a non-UTF-8 argument:\n{stderr}"
    );
    assert_ne!(
        output.status.code(),
        Some(101),
        "exit code 101 is a Rust panic; opencraylsp-mcp must not panic:\n{stderr}"
    );

    // And it must have behaved like a program: the argument reached the parser
    // and was rejected as an unknown flag (clap exits 2 on a parse error).
    assert_eq!(
        output.status.code(),
        Some(2),
        "the unknown flag should be rejected, not crash the binary:\n{stderr}"
    );
}

/// The non-UTF-8 argument must be reported rather than silently mangled, so a
/// user whose path really does contain unusual bytes learns why it did not work.
#[test]
fn a_non_utf8_argument_is_reported_not_swallowed() {
    let output = run_with_non_utf8_arg();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("UTF-8"),
        "the lossy conversion should be reported to the user:\n{stderr}"
    );
}
