#!/usr/bin/env bash
# Pre-push gate: runs, locally, what CI runs, in the order CI runs it.
#
# Why this exists: an open-source project cannot keep going red on `main`. The rule
# this script encodes is simple: nothing is pushed until the same commands CI will
# run have passed on the exact tree being pushed. "I ran the relevant checks" is how
# a red CI gets published (a version bump once shipped with a golden transcript still
# holding the old version string; the full test suite would have caught it).
#
#   bash scripts/preflight.sh      # everything CI runs
#   make preflight
#
# Machine caps: the defaults keep a shared host usable; override with
# CARGO_BUILD_JOBS / RUST_TEST_THREADS.
#
# Keep the steps below in the same order as .github/workflows/ci.yml; a step added to
# CI belongs here too (scripts/check-preflight-parity.sh fails when they drift).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

[ "$#" -eq 0 ] || { echo "usage: preflight.sh" >&2; exit 2; }

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-4}"

steps=0
step() {
	steps=$((steps + 1))
	printf '\n== [%s] %s\n' "$steps" "$1"
}

step "working tree is committed (what passes is what gets pushed)"
dirty="$(git -c core.fileMode=false status --porcelain --untracked-files=all || true)"
if [ -n "$dirty" ]; then
	echo "preflight: the tree has uncommitted or untracked files; commit or remove them first:" >&2
	printf '%s\n' "$dirty" | head -20 >&2
	exit 1
fi
echo "  clean"

step "lockfile and workspace version agree"
cargo metadata --locked --offline --format-version 1 >/dev/null
version="$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml | head -1)"
[ -n "$version" ] || { echo "preflight: no [workspace.package] version found" >&2; exit 1; }
echo "  workspace version $version"

step "cargo fmt --all -- --check"
cargo fmt --all -- --check

step "cargo clippy (default features)"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "cargo clippy (all features)"
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

# The full suite, not a selection: golden transcripts embed the daemon version, so a
# version bump that forgot to re-record them fails here instead of in CI.
step "cargo test"
cargo test --workspace --locked

step "cargo test (opencraylsp-mcp, all features)"
cargo test -p opencraylsp-mcp --all-features --locked

step "layering check"
scripts/check-layering.sh
scripts/check-layering.test.sh

step "license and release hygiene check"
scripts/check-licenses.sh

step "installer tests (network stubbed)"
scripts/install-sh.test.sh

step "cargo deny (licenses, bans, advisories)"
command -v cargo-deny >/dev/null 2>&1 ||
	{ echo "preflight: cargo-deny is not installed (cargo install cargo-deny --locked); CI runs it, so preflight must too" >&2; exit 1; }
cargo deny --all-features check licenses bans advisories sources

step "documentation check"
scripts/check-docs.sh

step "generated tools doc check"
scripts/check-tools-doc.sh

step "preflight matches CI"
scripts/check-preflight-parity.sh
scripts/check-preflight-parity.test.sh

printf '\npreflight: all %s steps passed for %s at %s\n' "$steps" "$version" "$(git rev-parse --short HEAD)"
