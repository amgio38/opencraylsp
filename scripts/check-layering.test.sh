#!/bin/sh
# Tests for check-layering.sh: a clean tree passes, and each rule fails loudly.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
checker="$here/check-layering.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fail=0
expect_pass() {
	name=$1 root=$2
	if out=$(sh "$checker" --root "$root" 2>&1); then
		echo "ok: $name"
	else
		echo "FAIL: $name: expected pass, got: $out" >&2
		fail=1
	fi
}
expect_fail_containing() {
	name=$1 root=$2 needle=$3
	if out=$(sh "$checker" --root "$root" 2>&1); then
		echo "FAIL: $name: expected failure, got pass" >&2
		fail=1
	elif printf '%s' "$out" | grep -qF "$needle"; then
		echo "ok: $name"
	else
		echo "FAIL: $name: message did not mention '$needle': $out" >&2
		fail=1
	fi
}

mk() { mkdir -p "$work/$1/crates/$2"; }

# --- a clean tree -----------------------------------------------------------
clean="$work/clean"
mk clean opencraylsp-proto
cat > "$clean/crates/opencraylsp-proto/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-proto"
EOF
mk clean opencraylsp-core
cat > "$clean/crates/opencraylsp-core/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-core"
[dependencies]
opencraylsp-proto = { workspace = true }
EOF
mk clean opencraylsp-tools
cat > "$clean/crates/opencraylsp-tools/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-tools"
[dependencies]
opencraylsp-core = { workspace = true }
EOF
mk clean opencraylsp-client
cat > "$clean/crates/opencraylsp-client/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-client"
[dependencies]
opencraylsp-proto = { workspace = true }
EOF
mk clean opencraylsp-mcp
cat > "$clean/crates/opencraylsp-mcp/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-mcp"
[dependencies]
opencraylsp-client = { workspace = true }
opencraylsp-tools = { workspace = true }
EOF
mk clean opencraylspd
cat > "$clean/crates/opencraylspd/Cargo.toml" <<'EOF'
[package]
name = "opencraylspd"
[dependencies]
opencraylsp-core = { workspace = true }
opencraylsp-client = { workspace = true }
EOF
expect_pass "clean tree" "$clean"

# --- R1: opencraylsp-core -> opencraylsp-client --------------------------------------------
r1="$work/r1"
mk r1 opencraylsp-proto; printf '[package]\nname = "opencraylsp-proto"\n' > "$r1/crates/opencraylsp-proto/Cargo.toml"
mk r1 opencraylsp-core
cat > "$r1/crates/opencraylsp-core/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-core"
[dependencies]
opencraylsp-proto = { workspace = true }
opencraylsp-client = { workspace = true }
EOF
mk r1 opencraylsp-client; printf '[package]\nname = "opencraylsp-client"\n' > "$r1/crates/opencraylsp-client/Cargo.toml"
expect_fail_containing "R1 opencraylsp-core -> opencraylsp-client" "$r1" "opencraylsp-core depends on opencraylsp-client"

# --- R2: opencraylsp-proto -> opencraylsp-core ---------------------------------------------
r2="$work/r2"
mk r2 opencraylsp-proto
cat > "$r2/crates/opencraylsp-proto/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-proto"
[dependencies]
opencraylsp-core = { workspace = true }
EOF
mk r2 opencraylsp-core; printf '[package]\nname = "opencraylsp-core"\n' > "$r2/crates/opencraylsp-core/Cargo.toml"
expect_fail_containing "R2 opencraylsp-proto -> opencraylsp-core" "$r2" "opencraylsp-proto depends on opencraylsp-core"

# --- R3: a path dependency outside the workspace ---------------------------
r3="$work/r3"
mk r3 opencraylsp-core
cat > "$r3/crates/opencraylsp-core/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-core"
[dependencies]
external-thing = { path = "/opt/external-thing" }
EOF
expect_fail_containing "R3 path dependency outside the workspace" "$r3" "outside the workspace"

# --- R3: a git dependency ---------------------------------------------------
r3git="$work/r3git"
mk r3git opencraylsp-core
cat > "$r3git/crates/opencraylsp-core/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-core"
[dependencies]
external-thing = { git = "https://github.com/example/external-thing" }
EOF
expect_fail_containing "R3 git dependency" "$r3git" "via git dependency"

# --- R3: a path dependency on a crate of this workspace is fine -------------
r3ok="$work/r3ok"
mk r3ok opencraylsp-proto
printf '[package]\nname = "opencraylsp-proto"\n' > "$r3ok/crates/opencraylsp-proto/Cargo.toml"
mk r3ok opencraylsp-core
cat > "$r3ok/crates/opencraylsp-core/Cargo.toml" <<'EOF'
[package]
name = "opencraylsp-core"
[dependencies]
opencraylsp-proto = { path = "../opencraylsp-proto" }
EOF
expect_pass "R3 path dependency inside the workspace" "$r3ok"

if [ "$fail" -ne 0 ]; then
	echo "check-layering.test.sh: FAILED" >&2
	exit 1
fi
echo "check-layering.test.sh: all cases passed"
