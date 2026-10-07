#!/bin/sh
# Tests for check-preflight-parity.sh: the real tree passes, and a CI command that
# preflight lacks fails loudly (a gate that cannot go red proves nothing).
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH= cd -- "$here/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fail=0
mk_tree() {
	dest=$1
	mkdir -p "$dest/scripts" "$dest/.github/workflows"
	cp "$repo/scripts/check-preflight-parity.sh" "$repo/scripts/preflight.sh" "$dest/scripts/"
	cp "$repo/.github/workflows/ci.yml" "$dest/.github/workflows/ci.yml"
}

mk_tree "$work/clean"
if sh "$work/clean/scripts/check-preflight-parity.sh" >/dev/null 2>&1; then
	echo "  ok: the real ci.yml and preflight.sh agree"
else
	echo "  FAIL: the real tree does not pass its own parity check" >&2
	fail=1
fi

mk_tree "$work/drift"
cat >>"$work/drift/.github/workflows/ci.yml" <<'YML'

      - name: a step preflight does not know
        run: scripts/a-brand-new-check.sh
YML
if sh "$work/drift/scripts/check-preflight-parity.sh" >"$work/out" 2>&1; then
	echo "  FAIL: a CI command missing from preflight was accepted" >&2
	fail=1
elif grep -q "a-brand-new-check.sh" "$work/out"; then
	echo "  ok: a CI command missing from preflight is named and refused"
else
	echo "  FAIL: refused, but without naming the missing command" >&2
	fail=1
fi

mk_tree "$work/block"
python3 - "$work/block/.github/workflows/ci.yml" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
open(p, "w").write(s + "\n      - name: block step\n        run: |\n          scripts/first-ok.sh\n          scripts/second-missing.sh\n")
PY
if sh "$work/block/scripts/check-preflight-parity.sh" >"$work/out2" 2>&1; then
	echo "  FAIL: a missing command inside a run: | block was accepted" >&2
	fail=1
elif grep -q "second-missing.sh" "$work/out2"; then
	echo "  ok: every line of a run: | block is checked"
else
	echo "  FAIL: block case refused without naming the command" >&2
	fail=1
fi

[ "$fail" -eq 0 ] || { echo "check-preflight-parity.test: FAILED" >&2; exit 1; }
echo "check-preflight-parity.test: ok"
