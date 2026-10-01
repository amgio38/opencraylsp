#!/bin/sh
# Licence and release-hygiene checks that need no network and no cargo-deny:
#
#   1. every dependency in Cargo.lock is accounted for in
#      THIRD-PARTY-LICENSES.md, and every license text that
#      THIRD-PARTY-LICENSES.md relies on is present under licenses/;
#   2. a dependency whose license cannot be parsed cannot slip through;
#   3. `cargo deny` is actually wired into `make ci` and into CI -- an
#      unenforced policy file is not a policy;
#   4. scripts/install.sh and the Makefile stage through mktemp rather than a
#      name derived from the pid, and install.sh cannot skip a checksum in
#      silence.
#
# Usage: check-licenses.sh [--root DIR]
#
# Exits non-zero with a list of problems; prints nothing else.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ "${1:-}" = "--root" ]; then
	root=$(CDPATH= cd -- "$2" && pwd)
fi
cd "$root"

problems=0
note() { echo "check-licenses: $*" >&2; problems=$((problems + 1)); }

# --- 1 + 2: the inventory agrees with Cargo.lock, and every license that the
# inventory relies on has its text on disk ------------------------------------
inventory=licenses/deps.tsv
if [ ! -f THIRD-PARTY-LICENSES.md ]; then
	note "THIRD-PARTY-LICENSES.md is missing: nothing records the licenses we ship under"
fi
if [ ! -f "$inventory" ]; then
	note "$inventory is missing: no machine-readable dependency inventory to check"
elif [ ! -f Cargo.lock ]; then
	note "Cargo.lock is missing: cannot check the dependency inventory"
else
	python3 - "$inventory" Cargo.lock licenses <<'PY2'
import os
import re
import sys

tsv_path, lock_path, licenses_dir = sys.argv[1], sys.argv[2], sys.argv[3]
problems = []

# --- what Cargo.lock says --------------------------------------------------
lock = open(lock_path, encoding="utf-8").read()
own = set(re.findall(
    r'^name = "(opencraylspd|opencraylsp-mcp|opencraylsp-core|opencraylsp-client|opencraylsp-proto|opencraylsp-tools|opencraylspd-e2e)"$',
    lock, re.M))
in_lock = set()
for block in lock.split("[[package]]"):
    name = re.search(r'^name = "([^"]+)"', block, re.M)
    version = re.search(r'^version = "([^"]+)"', block, re.M)
    if name and version and name.group(1) not in own:
        in_lock.add(f"{name.group(1)} {version.group(1)}")

# --- what the inventory says ----------------------------------------------
listed = {}
for lineno, line in enumerate(open(tsv_path, encoding="utf-8"), 1):
    if line.startswith("#") or not line.strip():
        continue
    fields = line.rstrip("\n").split("\t")
    if len(fields) != 5:
        problems.append(f"{tsv_path}:{lineno}: expected 5 tab-separated fields, got {len(fields)}")
        continue
    name, version, expression, elected, texts = fields
    listed[f"{name} {version}"] = (expression, elected, texts)

missing = sorted(in_lock - set(listed))
if missing:
    problems.append(
        f"{len(missing)} dependencies in Cargo.lock are absent from {tsv_path}: "
        + ", ".join(missing[:8]) + (" ..." if len(missing) > 8 else ""))
stale = sorted(set(listed) - in_lock)
if stale:
    problems.append(
        f"{len(stale)} entries in {tsv_path} are not in Cargo.lock: "
        + ", ".join(stale[:8]) + (" ..." if len(stale) > 8 else ""))

# --- every text the inventory claims, checked for real ---------------------
MARKERS = {
    "licenses/MIT.txt": "Permission is hereby granted, free of charge",
    "licenses/Apache-2.0.txt": "Apache License",
    "licenses/Unicode-3.0.txt": "UNICODE LICENSE V3",
}
for rel in sorted(MARKERS):
    path = os.path.join(licenses_dir, os.path.basename(rel))
    if not os.path.isfile(path):
        problems.append(f"{rel} is missing: the license text has to ship with a binary")
    elif MARKERS[rel] not in open(path, encoding="utf-8", errors="replace").read():
        problems.append(f"{rel} does not contain the text it claims to be")

# --- a license nobody has a text for is a hole, not a detail ---------------
for pair, (expression, elected, texts) in sorted(listed.items()):
    for rel in [t for t in texts.split(",") if t]:
        if rel not in MARKERS:
            problems.append(
                f"{tsv_path}: {pair} is licensed `{expression}`, and `{elected}` has no "
                f"text in licenses/ (recorded as {rel})")
    if expression in ("unknown", "NOASSERTION"):
        problems.append(f"{tsv_path}: {pair} has no license this project can rely on")

# --- the two known oddities must stay documented --------------------------
if any(expression == "MIT/Apache-2.0" for expression, _, _ in listed.values()):
    if "not valid SPDX" not in open("deny.toml", encoding="utf-8").read():
        problems.append(
            "a dependency declares the invalid `MIT/Apache-2.0` expression but deny.toml "
            "no longer explains it")
if any("LGPL" in expression for expression, _, _ in listed.values()):
    if "LGPL" not in open("deny.toml", encoding="utf-8").read():
        problems.append(
            "a dependency offers LGPL but deny.toml no longer says the option is not taken")

# --- the project's own files ----------------------------------------------
for required in ("LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.md"):
    if not os.path.isfile(required):
        problems.append(f"{required} is missing: a redistribution has to carry it")
if os.path.isfile("LICENSE") and "MIT License" not in open("LICENSE", encoding="utf-8").read():
    problems.append("LICENSE does not look like the MIT License")

for problem in problems:
    print("check-licenses: " + problem, file=sys.stderr)
sys.exit(1 if problems else 0)
PY2
	[ $? -eq 0 ] || problems=$((problems + 1))
fi

# --- 3: the policy is enforced, not just written ---------------------------
# Not "a deny target exists" -- a target nobody runs is exactly the finding.
# The `ci` line has to name it.
ci_line=$(grep -E '^ci[ :]*(fmt-check|\$)' Makefile 2>/dev/null | head -1)
if [ -z "$ci_line" ]; then
	note "the Makefile has no 'ci' target to check"
elif ! printf '%s' "$ci_line" | grep -Eq '(^|[[:space:]])deny([[:space:]]|$)'; then
	note "the Makefile 'ci' target does not run 'deny': deny.toml is not enforced"
fi
if ! grep -q 'cargo deny\|cargo-deny' .github/workflows/ci.yml 2>/dev/null; then
	note ".github/workflows/ci.yml never runs cargo deny: a bad licence or a yanked crate would merge"
fi

# --- 3b: the two known license oddities stay documented ---------------------
# Both are facts about a dependency, both are invisible once you stop looking,
# and both look like ordinary findings later ("unknown license", "LGPL in the
# tree") to whoever reads cargo-deny output first. So their presence is a rule,
# not a comment.
if grep -q 'MIT/Apache-2.0' licenses/deps.tsv 2>/dev/null &&
	! grep -q 'not valid SPDX' deny.toml; then
	note "a dependency declares the invalid 'MIT/Apache-2.0' expression and deny.toml no longer explains why"
fi
if grep -q 'LGPL' licenses/deps.tsv 2>/dev/null && ! grep -q 'LGPL' deny.toml; then
	note "a dependency offers an LGPL option and deny.toml no longer says the option is not taken"
fi
if ! grep -q 'multiple-versions' deny.toml; then
	note "deny.toml no longer states the multiple-versions policy"
fi

# --- 3c: the crates being unpublished is stated where a reader looks -------
# Every crate here is `publish = false`. That is a deliberate release decision,
# and a reader who does not find it written down concludes `cargo install opencraylspd`
# is broken rather than intended.
if ! grep -q 'publish = false' README.md; then
	note "README.md does not say the crates are publish = false and so are not on crates.io"
fi

# --- 4: install.sh / Makefile staging and checksum behaviour --------------
installer=scripts/install.sh
if [ -f "$installer" ]; then
	if ! sh -n "$installer" 2>/dev/null; then
		note "$installer is not valid POSIX sh"
	fi
	# A staging name derived from the pid can be predicted, and `install` writes
	# through a symlink left there.
	if grep -nE '\.opencraylsp(-mcp|d)?\.install\.\$\$|\.opencraylsp(-mcp|d)?\.install\.[0-9]+' "$installer" >/dev/null; then
		note "$installer stages through a name derived from the pid; use mktemp"
	fi
	if ! grep -q 'mktemp -d "\$BINDIR' "$installer"; then
		note "$installer does not stage through 'mktemp -d' inside \$BINDIR"
	fi
	# Failing open: a note on stderr and then carry on installing anyway. Matched
	# on code lines only -- the prose above `verify_checksum` names the old
	# behaviour, and a check that flags its own explanation is not a check.
	if grep -v '^[[:space:]]*#' "$installer" \
		| grep -q 'skipping checksum\|skipping verification'; then
		note "$installer can still skip checksum verification and install anyway"
	fi
	# The three ways verification used to be lost, each of which must now stop
	# the install rather than warn about it.
	for refusal in 'no checksum published' 'published checksum' 'no SHA-256 tool'; do
		if ! grep -q "die .*$refusal" "$installer"; then
			note "$installer does not die when '$refusal'"
		fi
	done
	if ! grep -q -- '--insecure' "$installer"; then
		note "$installer has no opt-out flag, so refusing a missing checksum is not usable"
	fi
fi

if grep -nE 'a="\$\$d/\.opencraylsp\.install\.\$\$\$"' Makefile >/dev/null; then
	note "the Makefile install recipe stages through a name derived from the pid; use mktemp"
fi
# A `#` inside a backslash-continued recipe line comments out the rest of the
# command in the shell, which is how the staging fix could look applied and not
# be.
if awk '/^[ \t]*#/{inrecipe=1} inrecipe && /\\$/{next} inrecipe && !/^[ \t]*#/{inrecipe=0}' Makefile \
	| grep -q '^[[:space:]]*#'; then
	note "the Makefile has a comment inside a backslash-continued recipe line"
fi

# --- 5: the licence actually ships with the programs ------------------------
# The obligation is recorded elsewhere. This is the part where it is met: a binary in
# $BINDIR is a redistribution, and Apache-2.0 section 4 wants the License and
# the NOTICE beside it. A licence that only exists in the repository is not
# available to anyone who has the binary.
#
# The three paths that put a binary somewhere are install.sh, `make install` and
# the release workflow, and all three have to carry the files. install.sh and
# `make install` are covered at runtime by scripts/install-sh.test.sh; the
# workflow and the Makefile's structure are not, so they are asserted here.
list=scripts/doc-files.sh
if [ ! -f "$list" ]; then
	note "$list is missing: nothing says which files have to ship with the binary"
else
	if ! sh -n "$list" 2>/dev/null; then
		note "$list is not valid POSIX sh"
	fi
	# The list is newline-separated; make it one line before matching words
	# against it, or " LICENSE" never matches "LICENSE\nNOTICE".
	doc_files=$(. "$list" 2>/dev/null && printf '%s' "$OPENCRAYLSP_DOC_FILES" | tr '\n' ' ')
	doc_subdir=$(. "$list" 2>/dev/null && printf '%s' "$OPENCRAYLSP_DOC_SUBDIR")
	if [ -z "$doc_files" ]; then
		note "$list names no licence files"
	fi
	if [ "$doc_subdir" != 'share/doc/opencraylsp' ]; then
		note "$list puts the licence in '$doc_subdir' rather than share/doc/opencraylsp"
	fi
	# Everything the inventory says needs a text has to be in the list, or it
	# ships nowhere.
	for required in LICENSE NOTICE THIRD-PARTY-LICENSES.md; do
		case " $doc_files " in
		*" $required "*) ;;
		*) note "$required is not in $list, so it would not travel with a binary" ;;
		esac
	done
	# And a name in the list that is not in the repository would fail every
	# install at run time instead of here.
	for f in $doc_files; do
		if [ ! -f "$f" ]; then
			note "$f is listed in $list but is not in this checkout"
		fi
	done
fi

# The release archive is what a `curl | sh` install actually receives, so if the
# workflow does not put the licence in it then the installer has nothing to
# install and the whole path is theatre.
wf=.github/workflows/release.yml
if [ -f "$wf" ]; then
	if ! grep -q 'OPENCRAYLSP_DOC_FILES' "$wf"; then
		note "$wf does not build the licence set, so a release asset ships a binary with no licence"
	fi
	if ! grep -q 'OPENCRAYLSP_DOC_SUBDIR' "$wf"; then
		note "$wf does not put the licence under \$OPENCRAYLSP_DOC_SUBDIR, where install.sh looks for it"
	fi
	# The archive must still contain the two programs: the test tarball is built
	# from the same list, and a workflow that forgot them would pass this file.
	if ! grep -q 'opencraylspd opencraylsp-mcp' "$wf"; then
		note "$wf no longer tars opencraylspd and opencraylsp-mcp"
	fi
fi

if [ -f Makefile ]; then
	# Both halves: the target has to exist *and* the caller has to call it.
	# Grepping for the name alone would be satisfied by the definition sitting
	# unused in the file, which is the shape of an unenforced policy file.
	for pair in 'install-docs' 'uninstall-docs'; do
		if ! grep -qE "^$pair:" Makefile; then
			note "the Makefile has no '$pair' target"
		fi
		if ! grep -qE "\\\$\(MAKE\).*--no-print-directory[[:space:]]+$pair( |\$)" Makefile; then
			note "no Makefile recipe calls '$pair', so it is a target nobody runs"
		fi
	done
	if ! grep -q 'OPENCRAYLSP_DOC_FILES' Makefile; then
		note "the Makefile does not read the licence list, so it can drift from scripts/doc-files.sh"
	fi
	# A licence file the checkout does not have must stop the install rather
	# than leave a binary beside a partial set.
	if ! grep -q 'not in this checkout' Makefile; then
		note "the Makefile installs licence files without checking the list exists here"
	fi
fi

if [ -f scripts/uninstall.sh ]; then
	if ! grep -q 'share/doc/opencraylsp' scripts/uninstall.sh; then
		note "scripts/uninstall.sh does not know about the licence directory"
	fi
	# `rm -rf` on a path built from user input needs the check that says so.
	if ! grep -q 'refusing to remove' scripts/uninstall.sh; then
		note "scripts/uninstall.sh removes the licence directory without refusing the case where it is /"
	fi
fi

# --- 6: the installer's network and prefix handling ------------------------
# A few properties that scripts/install-sh.test.sh cannot run, because testing
# them for real would mean doing the thing: an arm that refuses to install into
# /usr/bin, and a clone URL, cannot be exercised without a git remote and a
# system directory.
if [ -f "$installer" ]; then
	for guarded in '/bin|/sbin|/usr/bin|/usr/sbin' '*/bin|*/sbin'; do
		if ! grep -qF "$guarded" "$installer"; then
			note "$installer no longer refuses '$guarded'"
		fi
	done
	# A --version beginning with a dash is read by git as an option unless the
	# value is attached, and it comes from the caller's environment.
	# (The curl and clone rules are in section 6, with the comment-line
	# exclusion they need.)
	# A redirect to another protocol, or a downgrade below TLS 1.2, is decided
	# at the far end of the connection.
	#
	# Code lines only. The prose above `fetch` names the flags it explains, and a
	# check that matches its own explanation reports the fix applied when the
	# flags are gone -- which is what happened the first time this was written.
	if grep -v '^[[:space:]]*#' "$installer" | grep -q -- "--proto '=https'"; then
		:
	else
		note "$installer does not pin curl to https, so a redirect off https is followed silently"
	fi
	if grep -v '^[[:space:]]*#' "$installer" | grep -q -- '--tlsv1.2'; then
		:
	else
		note "$installer does not set a TLS floor for curl"
	fi
	# The same for the clone: a --version starting with a dash is read by git as
	# an option unless the value is attached, and it comes from the caller's
	# environment. Comment lines are excluded for the same reason.
	if grep -v '^[[:space:]]*#' "$installer" | grep -qE 'git clone[^|]*--branch[[:space:]]+"\$VERSION"'; then
		note "$installer passes --branch a separate value, so a version starting with a dash is read as an option"
	fi
	# The licence has to be judged before the programs are placed, or a refusal
	# leaves a redistributed binary with nothing beside it.
	if ! grep -q 'check_doc_files "\$tmp/\$DOC_SUBDIR"' "$installer"; then
		note "$installer installs the programs before checking the licence files"
	fi
fi

if [ "$problems" -ne 0 ]; then
	echo "check-licenses: $problems problem(s)" >&2
	exit 1
fi
echo "license check passed"
