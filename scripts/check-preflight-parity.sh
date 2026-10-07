#!/bin/sh
# Fails when .github/workflows/ci.yml runs a command that scripts/preflight.sh does
# not. preflight is only worth anything if it is the same list CI runs: a step added
# to CI and not here is exactly how a push goes red after a green local run.
#
# Every `run:` command in ci.yml (single-line, or a `run: |` block, one command per
# line) must appear in preflight.sh, spelled the same, ignoring `cd`-free whitespace
# and a leading `./`. Steps that use an action (`uses:`) are out of scope: preflight
# calls the tool the action installs directly.
#
# Usage: check-preflight-parity.sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

python3 - .github/workflows/ci.yml scripts/preflight.sh <<'PY'
import re
import sys

ci_path, pre_path = sys.argv[1], sys.argv[2]


def norm(cmd):
    cmd = cmd.strip()
    cmd = re.sub(r"^\./", "", cmd)
    return re.sub(r"\s+", " ", cmd)


ci_cmds = []
lines = open(ci_path, encoding="utf-8").read().splitlines()
i = 0
while i < len(lines):
    m = re.match(r"^(\s*)(?:- )?run:\s*(.*)$", lines[i])
    if not m:
        i += 1
        continue
    indent, rest = len(m.group(1)), m.group(2).strip()
    if rest in ("|", ">", "|-", ">-"):
        i += 1
        while i < len(lines) and (not lines[i].strip() or len(lines[i]) - len(lines[i].lstrip()) > indent):
            if lines[i].strip() and not lines[i].strip().startswith("#"):
                ci_cmds.append(norm(lines[i]))
            i += 1
        continue
    if rest and not rest.startswith("#"):
        ci_cmds.append(norm(rest))
    i += 1

pre = open(pre_path, encoding="utf-8").read()
pre_norm = {norm(l) for l in pre.splitlines()}
missing = [c for c in ci_cmds if c not in pre_norm]
if not ci_cmds:
    sys.exit("check-preflight-parity: found no run: commands in ci.yml; the parser is out of date")
if missing:
    print("check-preflight-parity: ci.yml runs commands that scripts/preflight.sh does not:", file=sys.stderr)
    for c in missing:
        print("  " + c, file=sys.stderr)
    print("add them to scripts/preflight.sh in the same order, or CI will go red after a green local run.", file=sys.stderr)
    sys.exit(1)
print("check-preflight-parity: ok (%d CI commands, all in preflight)" % len(ci_cmds))
PY
