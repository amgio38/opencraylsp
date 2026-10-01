#!/bin/sh
# End-to-end tests for scripts/install.sh, with the network replaced.
#
# `curl` and `wget` are stubbed on PATH, so the real script runs -- argument
# parsing, the tar member allowlist, checksum verification, the pair check and
# the staging -- against a tarball built here. That is the only way to test the
# things that matter here, which are decisions the script makes about *failure*:
# an unverified or mismatched release asset must not end up in $BINDIR.
#
# Usage: install-sh.test.sh
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
installer="$here/install.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

fail=0
ok() { echo "ok: $1"; }
bad() {
	echo "FAIL: $1" >&2
	[ $# -lt 2 ] || echo "  $2" >&2
	fail=1
}

# --- a release tarball the stubs serve -------------------------------------
# Two programs that answer `--version` with the same string, which is what
# `verify_pair` requires, plus the licence subtree a real release ships.
stage="$work/asset"
mkdir -p "$stage"
for prog in opencraylspd opencraylsp-mcp; do
	cat > "$stage/$prog" <<EOF
#!/bin/sh
echo "$prog 0.1.0"
EOF
	chmod 0755 "$stage/$prog"
done
# The same shape .github/workflows/release.yml builds: the programs at the top
# level and the licence files under $OPENCRAYLSP_DOC_SUBDIR, taken from the same list.
# Using the real list and the real repository files is the point -- if the
# workflow and this test disagreed, the test would be testing a different
# archive than the one that ships.
doc_subdir=$(
	. "$here/doc-files.sh"
	printf '%s' "$OPENCRAYLSP_DOC_SUBDIR"
)
doc_files=$(
	. "$here/doc-files.sh"
	printf '%s\n' $OPENCRAYLSP_DOC_FILES
)
repo_root=$(CDPATH= cd -- "$here/.." && pwd)
for f in $doc_files; do
	install -D -m 0644 "$repo_root/$f" "$stage/$doc_subdir/$f"
done
tarball="$work/opencraylsp-x86_64-unknown-linux-musl.tar.gz"
asset_name=$(basename "$tarball")
build_tarball() { (cd "$stage" && tar -czf "$work/$asset_name" opencraylspd opencraylsp-mcp "$doc_subdir"); }
build_tarball
# What a real release publishes alongside the asset.
if command -v sha256sum >/dev/null 2>&1; then
	(cd "$work" && sha256sum "$(basename "$tarball")" > "$(basename "$tarball").sha256")
fi

# A stub that serves those two files for any URL, and can be told to fail the
# checksum half.
stub_dir="$work/bin"
mkdir -p "$stub_dir"
cat > "$stub_dir/curl" <<'EOF'
#!/bin/sh
# Stands in for `curl -fsSL --proto '=https' --tlsv1.2 --connect-timeout N
# --max-time N <url> -o <out>`. The flags come first, so the URL has to be found
# by shape rather than by position: it is the only argument naming the asset.
# Every call is recorded so a case can assert on the flags themselves and not
# only on the file that arrived.
if [ -n "${STUB_LOG:-}" ]; then
	printf '%s\n' "$*" >>"$STUB_LOG"
fi
out=
url=
prev=
for arg in "$@"; do
	case "$prev" in -o) out=$arg ;; esac
	case "$arg" in
	*.tar.gz | *.tar.gz.sha256) url=$arg ;;
	esac
	prev=$arg
done
[ -n "$url" ] || exit 2
case "$url" in
*.sha256)
	[ -n "${STUB_NO_CHECKSUM:-}" ] && exit 22
	if [ -n "${STUB_EMPTY_CHECKSUM:-}" ]; then
		: > "$out"
	else
		cp "$SERVE_DIR/$SERVE_SHA" "$out"
	fi
	;;
*)
	cp "$SERVE_DIR/$SERVE_ASSET" "$out"
	;;
esac
EOF
chmod 0755 "$stub_dir/curl"

# Run the installer against the stubs. Extra arguments are passed through.
run_install() {
	prefix=$1
	shift
	env -i \
		PATH="$stub_dir:$PATH" \
		HOME="$work/home" \
		SERVE_DIR="$work" \
		SERVE_ASSET="$(basename "$tarball")" \
		SERVE_SHA="$(basename "$tarball").sha256" \
		STUB_LOG="${STUB_LOG:-}" \
		${STUB_NO_CHECKSUM:+STUB_NO_CHECKSUM=1} \
		${STUB_EMPTY_CHECKSUM:+STUB_EMPTY_CHECKSUM=1} \
		${OPENCRAYLSP_REPO:+OPENCRAYLSP_REPO="$OPENCRAYLSP_REPO"} \
		sh "$installer" --prefix "$prefix" "$@" >"$work/out" 2>"$work/err" && echo 0 || echo $?
}

# The other half of the round trip. No network, so no stubs needed.
run_uninstall() {
	prefix=$1
	shift
	env -i \
		PATH="$stub_dir:$PATH" \
		HOME="$work/home" \
		sh "$here/uninstall.sh" --prefix "$prefix" "$@" >"$work/uout" 2>"$work/uerr" && echo 0 || echo $?
}

mkdir -p "$work/home"

# --- a verified release installs -------------------------------------------
code=$(run_install "$work/pfx-good")
if [ "$code" = 0 ] && [ -x "$work/pfx-good/bin/opencraylspd" ] && [ -x "$work/pfx-good/bin/opencraylsp-mcp" ]; then
	ok "a verified release installs both programs"
else
	bad "a verified release installs both programs" "exit $code: $(cat "$work/err")"
fi
# And it leaves nothing but the two programs behind -- no staging directory, no
# temporary name.
left=$(ls -A "$work/pfx-good/bin" | grep -v '^opencraylspd$' | grep -v '^opencraylsp-mcp$' | tr '\n' ' ')
if [ -z "$left" ]; then
	ok "\$BINDIR holds only the two installed programs afterwards"
else
	bad "\$BINDIR holds only the two installed programs afterwards" "left: $left"
fi
# Later cases overwrite $work/out, so keep this install's own output for the
# checks below.
cp "$work/out" "$work/out-good"

# --- a missing checksum stops the install ---------------------------
rm -rf "$work/pfx-nochecksum"
code=$(STUB_NO_CHECKSUM=1 run_install "$work/pfx-nochecksum")
if [ "$code" != 0 ] && [ ! -e "$work/pfx-nochecksum/bin/opencraylspd" ]; then
	ok "a release with no published checksum is not installed"
else
	bad "a release with no published checksum is not installed" "exit $code"
fi
if grep -q 'no checksum published' "$work/err"; then
	ok "the missing checksum is named in the error"
else
	bad "the missing checksum is named in the error" "$(cat "$work/err")"
fi

# --- no SHA-256 tool on the machine stops the install ---------------
# A PATH that holds every ordinary command except the two hash tools, so the
# check has nothing to find.
nosha_dir="$work/nosha-bin"
mkdir -p "$nosha_dir"
for f in /usr/bin/* /bin/*; do
	n=${f##*/}
	case "$n" in
	sha256sum | shasum) ;;
	*) [ -e "$nosha_dir/$n" ] || [ -L "$nosha_dir/$n" ] || ln -s "$f" "$nosha_dir/$n" ;;
	esac
done
rm -rf "$work/pfx-nosha"
if env -i PATH="$stub_dir:$nosha_dir" HOME="$work/home" SERVE_DIR="$work" \
	SERVE_ASSET="$(basename "$tarball")" SERVE_SHA="$(basename "$tarball").sha256" \
	sh "$installer" --prefix "$work/pfx-nosha" >"$work/out" 2>"$work/err"; then
	bad "a machine with no SHA-256 tool does not install" "installer exited 0"
elif [ -e "$work/pfx-nosha/bin/opencraylspd" ]; then
	bad "a machine with no SHA-256 tool does not install" "a binary was installed"
elif grep -q 'no SHA-256 tool' "$work/err"; then
	ok "a machine with no SHA-256 tool does not install, and the error names the tool"
else
	bad "a machine with no SHA-256 tool does not install" "$(cat "$work/err")"
fi

# --- ...unless the user says so ---------------------------------------------
rm -rf "$work/pfx-insecure"
code=$(STUB_NO_CHECKSUM=1 run_install "$work/pfx-insecure" --insecure)
if [ "$code" = 0 ] && [ -x "$work/pfx-insecure/bin/opencraylspd" ]; then
	ok "--insecure installs an unverified asset, and says so"
else
	bad "--insecure installs an unverified asset, and says so" "exit $code: $(cat "$work/err")"
fi
if grep -q 'unverified' "$work/err"; then
	ok "the --insecure install is announced"
else
	bad "the --insecure install is announced" "$(cat "$work/err")"
fi

# --- an empty checksum file stops the install ------------------------------
rm -rf "$work/pfx-empty"
code=$(STUB_EMPTY_CHECKSUM=1 run_install "$work/pfx-empty")
if [ "$code" != 0 ] && [ ! -e "$work/pfx-empty/bin/opencraylspd" ]; then
	ok "an empty published checksum is not installed"
else
	bad "an empty published checksum is not installed" "exit $code"
fi

# --- a corrupted asset is refused (the check is really run) ----------------
if command -v sha256sum >/dev/null 2>&1; then
	printf 'corrupted' >> "$work/$asset_name"
	code=$(run_install "$work/pfx-badsum")
	if [ "$code" != 0 ] && [ ! -e "$work/pfx-badsum/bin/opencraylspd" ]; then
		ok "a corrupted asset is refused on the checksum"
	else
		bad "a corrupted asset is refused on the checksum" "exit $code"
	fi
	if grep -q 'checksum mismatch' "$work/err"; then
		ok "the checksum mismatch is named in the error"
	else
		bad "the checksum mismatch is named in the error" "$(cat "$work/err")"
	fi
	# Put the good asset and its checksum back for the later cases.
	build_tarball
	(cd "$work" && sha256sum "$asset_name" > "$asset_name.sha256")
else
	echo "skip: no sha256sum on this machine, the corrupted-asset case cannot run"
fi

# --- a mismatched pair is refused ------------------------------------------
rm -rf "$work/stage2" "$work/pfx-mixed"
mkdir -p "$work/stage2"
# Different versions, which is what `verify_pair` exists to catch.
cat > "$work/stage2/opencraylspd" <<'EOF'
#!/bin/sh
echo "opencraylspd 9.9.9"
EOF
cat > "$work/stage2/opencraylsp-mcp" <<'EOF'
#!/bin/sh
echo "opencraylsp-mcp 0.1.0"
EOF
chmod 0755 "$work/stage2/opencraylspd" "$work/stage2/opencraylsp-mcp"
# Swap in a tarball whose two programs report different versions, keeping the
# published checksum in step with it so this tests the pair check and not the
# checksum check.
cp "$work/$asset_name" "$work/good-asset"
(cd "$work/stage2" && tar -czf "$work/$asset_name" opencraylspd opencraylsp-mcp) # programs only: the doc subtree is copied in below
# The doc subtree has to stay in this archive too, or the cases after this one
# would be testing an archive that no release has ever published.
for f in $doc_files; do
	install -D -m 0644 "$stage/$doc_subdir/$f" "$work/stage2/$doc_subdir/$f"
done
(cd "$work/stage2" && tar -czf "$work/$asset_name" opencraylspd opencraylsp-mcp "$doc_subdir")
if command -v sha256sum >/dev/null 2>&1; then
	(cd "$work" && sha256sum "$asset_name" > "$asset_name.sha256")
fi
code=$(run_install "$work/pfx-mixed")
if [ "$code" != 0 ] && [ ! -e "$work/pfx-mixed/bin/opencraylspd" ]; then
	ok "a pair that reports different versions is refused"
else
	bad "a pair that reports different versions is refused" "exit $code"
fi
if grep -q 'version mismatch' "$work/err"; then
	ok "the version mismatch is named in the error"
else
	bad "the version mismatch is named in the error" "$(cat "$work/err")"
fi
# Put the good asset back.
cp "$work/good-asset" "$work/$asset_name"
if command -v sha256sum >/dev/null 2>&1; then
	(cd "$work" && sha256sum "$asset_name" > "$asset_name.sha256")
fi

# --- the staging name cannot be predicted ---------------------------
# A staging name derived from the pid can be created in advance by anything that
# can write to $BINDIR, and `install` writes *through* a symlink, so the staging
# step could be aimed at any file the invoking user can write.
#
# The child's pid is not knowable here, and `pid_max` on this machine is in the
# millions -- covering it would take minutes of `ln -s`. So this sweeps the range
# the child is likely to land in and says plainly when it could not cover it. The
# *deterministic* guard for this property is in scripts/check-licenses.sh, which
# fails when the installer derives a staging name from `$$` or does not stage
# through `mktemp -d`; that one is what makes this a gate rather than a
# probability.
symlinked="$work/pfx-symlink"
mkdir -p "$symlinked/bin"
victim="$work/victim"
echo "original" > "$victim"
covered=0
n=1
while [ "$n" -le 20000 ]; do
	ln -sf "$victim" "$symlinked/bin/.opencraylsp.install.$n" 2>/dev/null && covered=$n
	n=$((n + 1))
done
code=$(run_install "$symlinked")
if [ "$(cat "$victim")" = "original" ]; then
	ok "installing writes through none of $covered pre-created staging symlinks"
else
	bad "installing writes through a pre-created staging symlink" \
		"$victim was overwritten, so the staging name is predictable"
fi
if [ "$code" = 0 ] && [ -x "$symlinked/bin/opencraylspd" ]; then
	ok "the install still succeeds with those symlinks present"
else
	bad "the install still succeeds with those symlinks present" "exit $code: $(cat "$work/err")"
fi
# --- the licence files ship with the programs ------------------------------
# A binary in $BINDIR is a redistribution, and Apache-2.0 section 4(a)/(d) wants
# the License and the NOTICE to travel with it. The set is the one
# scripts/doc-files.sh names, and the files are the repository's.
docdir="$work/pfx-good/$doc_subdir"
missing=
for f in $doc_files; do
	[ -f "$docdir/$f" ] || missing="$missing $f"
done
if [ -z "$missing" ]; then
	ok "the licence files are installed under \$PREFIX/$doc_subdir"
else
	bad "the licence files are installed under \$PREFIX/$doc_subdir" "missing:$missing"
fi
# Same bytes as the repository's. Checking only that a file exists would pass for
# a placeholder, a truncated copy or a file of the right name and nothing else.
mismatch=
for f in $doc_files; do
	cmp -s "$docdir/$f" "$repo_root/$f" || mismatch="$mismatch $f"
done
if [ -z "$mismatch" ]; then
	ok "the installed licence files are byte-identical to the repository's"
else
	bad "the installed licence files are byte-identical to the repository's" "differ:$mismatch"
fi
# And the person installing is told where to read them, rather than having to
# know the convention.
if grep -q "licence file(s) into $docdir" "$work/out-good"; then
	ok "the install says how many licence files went where"
else
	bad "the install says how many licence files went where" "$(cat "$work/out-good")"
fi
if [ -f "$docdir/licenses/Apache-2.0.txt" ] && [ -f "$docdir/NOTICE" ]; then
	ok "the Apache text and the NOTICE are both among them"
else
	bad "the Apache text and the NOTICE are both among them"
fi

# --- uninstall takes the licence files with it ----------------------------
# The round trip is the real statement: a prefix that is installed and then
# uninstalled has to be empty, not "empty apart from a licence directory nobody
# remembers installing".
rm -rf "$work/pfx-round"
code=$(run_install "$work/pfx-round")
if [ "$code" != 0 ]; then
	bad "install then uninstall leaves the prefix empty" "install exit $code: $(cat "$work/err")"
else
	ucode=$(run_uninstall "$work/pfx-round")
	# No file and no symlink of ours is left. $PREFIX/bin itself stays, because
	# other programs live there and a tool does not get to remove its parent.
	ours=$(find "$work/pfx-round" \( -type f -o -type l \) 2>/dev/null | wc -l | tr -d ' ')
	if [ "$ucode" = 0 ] && [ "$ours" = 0 ]; then
		ok "install then uninstall leaves no file of ours behind"
	else
		bad "install then uninstall leaves no file of ours behind" \
			"uninstall exit $ucode, $ours file(s) left"
	fi
	# The share/doc/opencraylsp subtree goes too, and takes the empty directories it
	# created with it rather than leaving a hollow share/ tree.
	if [ ! -e "$work/pfx-round/$doc_subdir" ] && [ ! -e "$work/pfx-round/share" ]; then
		ok "uninstall removes the licence directory and the empty share/ it made"
	else
		bad "uninstall removes the licence directory and the empty share/ it made" \
			"still there: $(find "$work/pfx-round/share" 2>/dev/null | tr '\n' ' ')"
	fi
	if [ ! -e "$work/pfx-round/$doc_subdir" ]; then
		ok "uninstall removes the licence directory"
	else
		bad "uninstall removes the licence directory"
	fi
fi
# And a prefix with no licence directory in it is not an error.
rm -rf "$work/pfx-nodoc"
mkdir -p "$work/pfx-nodoc/bin"
ucode=$(run_uninstall "$work/pfx-nodoc")
if [ "$ucode" = 0 ]; then
	ok "uninstall on a prefix with no licence directory is not a failure"
else
	bad "uninstall on a prefix with no licence directory is not a failure" \
		"exit $ucode: $(cat "$work/uerr")"
fi

# --- a prefix that would install one level too deep is refused -------------
# This script puts the programs in $PREFIX/bin, so a --prefix that already ends
# in bin used to build PREFIX/bin/bin. `--prefix /usr/bin` did exactly that, and
# under root it made a new directory in a system location without being asked.
#
# DESTDIR is passed so that, with the guard absent, the case would install into
# $work rather than into /usr/bin: a regression test for this bug must not
# reproduce the bug on the machine running it.
for deep_prefix in /usr/bin /usr/local/bin; do
	rm -rf "$work/guard-destdir" "$work/pfx-deep"
	code=$(run_install "$work/pfx-deep" --destdir "$work/guard-destdir" --prefix "$deep_prefix")
	if [ "$code" != 0 ] && grep -q 'would install into' "$work/err"; then
		ok "--prefix $deep_prefix is refused instead of creating ${deep_prefix}/bin"
	else
		bad "--prefix $deep_prefix is refused instead of creating ${deep_prefix}/bin" \
			"exit $code: $(cat "$work/err")"
	fi
	if [ -e "$deep_prefix/bin" ]; then
		bad "--prefix $deep_prefix created nothing outside the prefix" "$deep_prefix/bin exists"
	else
		ok "--prefix $deep_prefix created nothing outside the prefix"
	fi
done
# The other arm of the guard -- a prefix that resolves to /bin, /sbin, /usr/bin or
# /usr/sbin -- is a static rule rather than a case here, and scripts/check-licenses.sh
# asserts it. Testing it at runtime would mean an install into a system directory
# on the machine that ran the test, which is the bug itself.

# --- a prefix that is fine is still accepted -------------------------------
# The guards above are only worth having if the prefixes people actually use keep
# working, so this is the case that would notice them being too broad.
rm -rf "$work/pfx-ok-prefix"
code=$(run_install "$work/ignored" --prefix /home/parent --destdir "$work/pfx-ok-prefix")
if [ "$code" = 0 ] && [ -x "$work/pfx-ok-prefix/home/parent/bin/opencraylspd" ]; then
	ok "a prefix under \$HOME with a DESTDIR still installs"
else
	bad "a prefix under \$HOME with a DESTDIR still installs" "exit $code: $(cat "$work/err")"
fi

# --- the platform decides what happens, and unsupported ones say so --------
# `uname` is stubbed so every platform can be exercised from one machine. The
# release asset is only ever fetched on Linux x86_64; everywhere else the script
# must say why and go to the source build, and native Windows must be refused
# before anything is downloaded or written.
uname_dir="$work/uname-bin"
mkdir -p "$uname_dir"
cat > "$uname_dir/uname" <<'EOF'
#!/bin/sh
case "$1" in
-s) echo "${FAKE_UNAME_S:-Linux}" ;;
-m) echo "${FAKE_UNAME_M:-x86_64}" ;;
*) echo "${FAKE_UNAME_S:-Linux}" ;;
esac
EOF
chmod 0755 "$uname_dir/uname"
# No cargo, git or compiler on this PATH, so the source build stops at its first
# check instead of really building: /usr/bin and /bin carry the core tools and
# not a Rust toolchain.
run_install_on() {
	fake_s=$1 fake_m=$2 prefix=$3
	shift 3
	env -i \
		PATH="$uname_dir:$stub_dir:/usr/bin:/bin" \
		HOME="$work/home" \
		FAKE_UNAME_S="$fake_s" FAKE_UNAME_M="$fake_m" \
		SERVE_DIR="$work" \
		SERVE_ASSET="${SERVE_ASSET_AS:-$(basename "$tarball")}" \
		SERVE_SHA="${SERVE_ASSET_AS:-$(basename "$tarball")}.sha256" \
		STUB_LOG="${STUB_LOG:-}" \
		sh "$installer" --prefix "$prefix" "$@" >"$work/out" 2>"$work/err" && echo 0 || echo $?
}

for win in MINGW64_NT-10.0 MSYS_NT-10.0 CYGWIN_NT-10.0 Windows_NT; do
	rm -rf "$work/pfx-win"
	: > "$work/stub.log"
	code=$(STUB_LOG="$work/stub.log" run_install_on "$win" x86_64 "$work/pfx-win")
	if [ "$code" != 0 ] && grep -q 'WSL2' "$work/err" && [ ! -e "$work/pfx-win" ] && [ ! -s "$work/stub.log" ]; then
		ok "native Windows ($win) is refused with a WSL2 hint, before any download or write"
	else
		bad "native Windows ($win) is refused with a WSL2 hint" "exit $code: $(cat "$work/err")"
	fi
done

for plat in "Linux armv7l" "Linux i686" "Darwin arm64" "Darwin x86_64"; do
	set -- $plat
	rm -rf "$work/pfx-src"
	: > "$work/stub.log"
	code=$(STUB_LOG="$work/stub.log" run_install_on "$1" "$2" "$work/pfx-src")
	if [ "$code" != 0 ] && grep -q 'building from source' "$work/err" && grep -q 'cargo not found' "$work/err" && [ ! -s "$work/stub.log" ]; then
		ok "$plat has no prebuilt binary: it says so, downloads nothing and goes to the source build"
	else
		bad "$plat has no prebuilt binary: it says so and goes to the source build" "exit $code: $(cat "$work/err")"
	fi
done

rm -rf "$work/pfx-x64"
code=$(run_install_on Linux amd64 "$work/pfx-x64")
if [ "$code" = 0 ] && [ -x "$work/pfx-x64/bin/opencraylspd" ]; then
	ok "Linux amd64 (the other spelling of x86_64) gets the prebuilt binary"
else
	bad "Linux amd64 gets the prebuilt binary" "exit $code: $(cat "$work/err")"
fi

# Linux aarch64 now has its own prebuilt archive: the installer must ask for
# *that* asset, not the x86-64 one.
arm_asset=opencraylsp-aarch64-unknown-linux-musl.tar.gz
cp "$work/$asset_name" "$work/$arm_asset"
(cd "$work" && sha256sum "$arm_asset" > "$arm_asset.sha256")
for arm in aarch64 arm64; do
	rm -rf "$work/pfx-arm"
	: > "$work/stub.log"
	code=$(SERVE_ASSET_AS="$arm_asset" STUB_LOG="$work/stub.log" run_install_on Linux "$arm" "$work/pfx-arm")
	if [ "$code" = 0 ] && [ -x "$work/pfx-arm/bin/opencraylspd" ] \
		&& grep -q 'opencraylsp-aarch64-unknown-linux-musl\.tar\.gz' "$work/stub.log" \
		&& ! grep -q 'x86_64' "$work/stub.log"; then
		ok "Linux $arm downloads the aarch64 archive, not the x86-64 one"
	else
		bad "Linux $arm downloads the aarch64 archive" "exit $code: $(cat "$work/err"); log: $(cat "$work/stub.log")"
	fi
done

# --- inputs that end up in a URL or a clone are validated -------------------
for badver in '../../evil' '-x' 'v1/../2' 'a b' 'v1;rm'; do
	code=$(run_install "$work/pfx-badver" --version "$badver")
	if [ "$code" = 2 ] && [ ! -e "$work/pfx-badver" ]; then
		ok "--version '$badver' is rejected before anything happens"
	else
		bad "--version '$badver' is rejected" "exit $code: $(cat "$work/err")"
	fi
done
code=$(run_install "$work/pfx-goodver" --version v0.20260929.1)
if [ "$code" = 0 ]; then
	ok "a real release tag is accepted"
else
	bad "a real release tag is accepted" "exit $code: $(cat "$work/err")"
fi
for badrepo in 'noslash' '../x' 'a/b/c' '-o/x' 'a/b c'; do
	code=$(OPENCRAYLSP_REPO="$badrepo" run_install "$work/pfx-badrepo")
	if [ "$code" = 2 ]; then
		ok "OPENCRAYLSP_REPO='$badrepo' is rejected"
	else
		bad "OPENCRAYLSP_REPO='$badrepo' is rejected" "exit $code: $(cat "$work/err")"
	fi
done

# --- no HOME and no prefix: a sentence, not "parameter not set" -------------
code=$(env -i PATH="$stub_dir:$PATH" sh "$installer" >"$work/out" 2>"$work/err" && echo 0 || echo $?)
if [ "$code" != 0 ] && grep -q 'HOME is not set' "$work/err"; then
	ok "with no HOME and no --prefix the error says what to do"
else
	bad "with no HOME and no --prefix the error says what to do" "exit $code: $(cat "$work/err")"
fi

# --- a prefix this user cannot write is explained (not run as root) ----------
if [ "$(id -u)" != 0 ]; then
	mkdir -p "$work/ro-prefix"
	chmod 0555 "$work/ro-prefix"
	code=$(run_install "$work/ro-prefix")
	if [ "$code" != 0 ] && grep -q 'cannot write to' "$work/err" && grep -q -- '--prefix' "$work/err"; then
		ok "an unwritable prefix is explained and a way out is named"
	else
		bad "an unwritable prefix is explained" "exit $code: $(cat "$work/err")"
	fi
	chmod 0755 "$work/ro-prefix"
fi

# --- nothing is left in the temporary directory ------------------------------
mkdir -p "$work/tmpdir"
rm -rf "$work/pfx-clean"
code=$(env -i PATH="$stub_dir:$PATH" HOME="$work/home" TMPDIR="$work/tmpdir" \
	SERVE_DIR="$work" SERVE_ASSET="$(basename "$tarball")" SERVE_SHA="$(basename "$tarball").sha256" \
	sh "$installer" --prefix "$work/pfx-clean" >"$work/out" 2>"$work/err" && echo 0 || echo $?)
leftover=$(ls -A "$work/tmpdir" | tr '\n' ' ')
if [ "$code" = 0 ] && [ -z "$leftover" ]; then
	ok "a successful install removes its downloads and scratch space"
else
	bad "a successful install removes its downloads and scratch space" "exit $code, left in TMPDIR: $leftover"
fi
# And a failed one too.
code=$(env -i PATH="$stub_dir:$PATH" HOME="$work/home" TMPDIR="$work/tmpdir" STUB_NO_CHECKSUM=1 \
	SERVE_DIR="$work" SERVE_ASSET="$(basename "$tarball")" SERVE_SHA="$(basename "$tarball").sha256" \
	sh "$installer" --prefix "$work/pfx-clean2" >"$work/out" 2>"$work/err" && echo 0 || echo $?)
leftover=$(ls -A "$work/tmpdir" | tr '\n' ' ')
if [ "$code" != 0 ] && [ -z "$leftover" ]; then
	ok "a failed install removes its scratch space too"
else
	bad "a failed install removes its scratch space too" "exit $code, left in TMPDIR: $leftover"
fi

# --- `curl | sh`: piped in, with options, and survives being cut off ---------
rm -rf "$work/pfx-pipe"
code=$(env -i PATH="$stub_dir:$PATH" HOME="$work/home" \
	SERVE_DIR="$work" SERVE_ASSET="$(basename "$tarball")" SERVE_SHA="$(basename "$tarball").sha256" \
	sh -s -- --prefix "$work/pfx-pipe" <"$installer" >"$work/out" 2>"$work/err" && echo 0 || echo $?)
if [ "$code" = 0 ] && [ -x "$work/pfx-pipe/bin/opencraylspd" ]; then
	ok "the script runs when piped into sh, and options pass through 'sh -s --'"
else
	bad "the script runs when piped into sh" "exit $code: $(cat "$work/err")"
fi
# A download cut off at any point must do nothing at all: main() is only called
# on the last line, so a prefix of the script is an unfinished function.
size=$(wc -c <"$installer")
cut_ok=1
for fraction in 10 25 50 75 90 99; do
	cut=$((size * fraction / 100))
	rm -rf "$work/pfx-cut"
	head -c "$cut" "$installer" >"$work/cut.sh"
	env -i PATH="$stub_dir:$PATH" HOME="$work/home" \
		SERVE_DIR="$work" SERVE_ASSET="$(basename "$tarball")" SERVE_SHA="$(basename "$tarball").sha256" \
		sh -s -- --prefix "$work/pfx-cut" <"$work/cut.sh" >"$work/out" 2>"$work/err" || true
	if [ -e "$work/pfx-cut" ] || [ -s "$work/out" ]; then
		cut_ok=0
		bad "a script cut off at ${fraction}% does nothing" "created $work/pfx-cut or printed: $(head -c 200 "$work/out")"
	fi
done
[ "$cut_ok" = 1 ] && ok "a script cut off part-way executes nothing (6 cut points)"

# --- a running daemon is told to restart, never restarted for the user -------
rm -rf "$work/pfx-restart"
code=$(run_install "$work/pfx-restart")
if [ "$code" = 0 ] && grep -q 'opencraylspd restart' "$work/out"; then
	ok "when a daemon answers, the installer says to restart it"
else
	bad "when a daemon answers, the installer says to restart it" "exit $code: $(cat "$work/out")"
fi
if grep -q 'export PATH=' "$work/out"; then
	ok "a prefix off PATH gets the exact export line to add"
else
	bad "a prefix off PATH gets the exact export line" "$(cat "$work/out")"
fi

# --- a hostile archive is refused -------------------------------------------
# GNU tar strips '../' and a leading '/' when it *creates* a member, so the two
# archives below cannot be built with tar(1) and are written directly. That is the
# shape a hostile release would serve; the release workflow cannot produce either.
raw_tar() {
	python3 - "$1" "$work/$asset_name" "${2:-file}" <<'PY'
import io
import sys
import tarfile

member = sys.argv[1]
kind = sys.argv[3]

with tarfile.open(sys.argv[2], "w:gz") as tf:
    for name in ("opencraylspd", "opencraylsp-mcp"):
        data = ("#!/bin/sh\necho '%s 0.1.0'\n" % name).encode()
        info = tarfile.TarInfo(name)
        info.size = len(data)
        info.mode = 0o755
        tf.addfile(info, io.BytesIO(data))
    info = tarfile.TarInfo(member)
    if kind == "symlink":
        info.type = tarfile.SYMTYPE
        info.linkname = "/etc/passwd"
        tf.addfile(info)
    else:
        data = b"not a licence\n"
        info.size = len(data)
        tf.addfile(info, io.BytesIO(data))
PY
}

if command -v python3 >/dev/null 2>&1; then
	# Keep the published checksum in step, so each of these tests the member
	# rules rather than the checksum rule that already has its own cases.
	restash_checksum() {
		(cd "$work" && sha256sum "$asset_name" >"$asset_name.sha256")
	}

	raw_tar "share/doc/opencraylsp/../../../escape"
	restash_checksum
	rm -rf "$work/pfx-traverse"
	code=$(run_install "$work/pfx-traverse")
	if [ "$code" != 0 ] && grep -q "refusing a member with" "$work/err"; then
		ok "an archive member that climbs out of the licence directory is refused"
	else
		bad "an archive member that climbs out of the licence directory is refused" \
			"exit $code: $(cat "$work/err")"
	fi
	if [ ! -e "$work/pfx-traverse/bin/opencraylspd" ]; then
		ok "nothing is installed from an archive that tried to climb out"
	else
		bad "nothing is installed from an archive that tried to climb out"
	fi

	raw_tar "bin/evil"
	restash_checksum
	rm -rf "$work/pfx-stray"
	code=$(run_install "$work/pfx-stray")
	if [ "$code" != 0 ] && grep -q "unexpected member" "$work/err"; then
		ok "an archive member outside the programs and the licence directory is refused"
	else
		bad "an archive member outside the programs and the licence directory is refused" \
			"exit $code: $(cat "$work/err")"
	fi

	# A licence "file" that is a symlink: `install -m 0644` would copy whatever
	# it points at, so this is a licence read from somewhere else entirely.
	raw_tar "share/doc/opencraylsp/LICENSE" symlink
	restash_checksum
	rm -rf "$work/pfx-symlinkdoc"
	code=$(run_install "$work/pfx-symlinkdoc")
	if [ "$code" != 0 ] && grep -q "is a symlink" "$work/err"; then
		ok "a licence file that is a symlink is refused"
	else
		bad "a licence file that is a symlink is refused" "exit $code: $(cat "$work/err")"
	fi
	# The refusal has to come before the programs are put in place. An archive
	# that installs the binaries and then complains about the licence has still
	# produced a redistributed binary with no licence next to it.
	if [ ! -e "$work/pfx-symlinkdoc/bin/opencraylspd" ]; then
		ok "a refused licence stops the install before the programs are placed"
	else
		bad "a refused licence stops the install before the programs are placed" \
			"$work/pfx-symlinkdoc/bin/opencraylspd exists"
	fi

	build_tarball
	restash_checksum
else
	echo "skip: no python3 on this machine, the hostile-archive cases cannot run"
fi

# --- the download is pinned to https, and to a TLS floor -------------------
# A redirect from an https release URL to another protocol, or a downgrade to a
# TLS version with a known weakness, is a property of the far end of the
# connection. curl's defaults do not rule either out.
STUB_LOG="$work/curl.log"
: >"$STUB_LOG"
rm -rf "$work/pfx-curl"
code=$(run_install "$work/pfx-curl")
first_call=$(head -n 1 "$STUB_LOG" 2>/dev/null || true)
if [ "$code" != 0 ]; then
	bad "curl is told to stay on https and not below TLS 1.2" "exit $code: $(cat "$work/err")"
elif printf '%s' "$first_call" | grep -q -- '--proto =https' &&
	printf '%s' "$first_call" | grep -q -- '--tlsv1.2'; then
	ok "curl is told to stay on https and not below TLS 1.2"
else
	bad "curl is told to stay on https and not below TLS 1.2" "curl was called as: $first_call"
fi
STUB_LOG=""

# --- `make install-docs` and `make uninstall-docs` ------------------------
# The Makefile's half of the same round trip. These have their own targets
# precisely so this can run them: `make install` builds the whole workspace in
# release mode first, and a test that cannot run the code it is testing is not
# a test of it.
if command -v make >/dev/null 2>&1; then
	rm -rf "$work/mkpfx"
	if make -C "$repo_root" --no-print-directory install-docs PREFIX="$work/mkpfx" >"$work/mkout" 2>&1; then
		mk_missing=
		for f in $doc_files; do
			cmp -s "$work/mkpfx/$doc_subdir/$f" "$repo_root/$f" || mk_missing="$mk_missing $f"
		done
		if [ -z "$mk_missing" ]; then
			ok "make install-docs puts the same licence set under the prefix share/doc path"
		else
			bad "make install-docs puts the same licence set under the prefix share/doc path" \
				"wrong or missing:$mk_missing"
		fi
	else
		bad "make install-docs puts the same licence set under the prefix share/doc path" \
			"$(cat "$work/mkout")"
	fi
	if make -C "$repo_root" --no-print-directory uninstall-docs PREFIX="$work/mkpfx" >"$work/mkout" 2>&1 &&
		[ ! -e "$work/mkpfx/$doc_subdir" ] && [ ! -e "$work/mkpfx/share" ]; then
		ok "make uninstall-docs removes it and the empty share/ it made"
	else
		bad "make uninstall-docs removes it and the empty share/ it made" "$(cat "$work/mkout")"
	fi
	# A list naming a file the checkout does not have must stop the install
	# rather than ship a binary with a partial licence set.
	rm -rf "$work/mkpfx2"
	if make -C "$repo_root" --no-print-directory install-docs PREFIX="$work/mkpfx2" DOC_FILES="LICENSE licenses/does-not-exist.txt" >"$work/mkout" 2>&1; then
		bad "make install-docs fails when the list names a missing file"
	else
		ok "make install-docs fails when the list names a missing file"
	fi
	if grep -q 'not in this checkout' "$work/mkout"; then
		ok "the missing file is named in the error"
	else
		bad "the missing file is named in the error" "$(cat "$work/mkout")"
	fi
	# And a prefix one level too deep is refused here too, for the same reason
	# as in install.sh: this appends /bin itself.
	if make -C "$repo_root" --no-print-directory install PREFIX="$work/mkdeep/bin" >"$work/mkout" 2>&1; then
		bad "make install refuses a PREFIX that ends in bin" "it did not refuse"
	else
		ok "make install refuses a PREFIX that ends in bin"
	fi
	if grep -q 'ends in bin or sbin' "$work/mkout"; then
		ok "the PREFIX guard says what was wrong with it"
	else
		bad "the PREFIX guard says what was wrong with it" "$(cat "$work/mkout")"
	fi
else
	echo "skip: no make on this machine, the Makefile licence cases cannot run"
fi

# --- the flags are understood ----------------------------------------------
if sh "$installer" --help | grep -q -- '--insecure'; then
	ok "--help documents --insecure"
else
	bad "--help documents --insecure"
fi
if sh "$installer" --nope >/dev/null 2>&1; then
	bad "an unknown flag is rejected"
else
	ok "an unknown flag is rejected"
fi
if sh "$installer" --prefix >/dev/null 2>&1; then
	bad "a flag with a missing value is rejected"
else
	ok "a flag with a missing value is rejected"
fi

if [ "$fail" -ne 0 ]; then
	echo "install-sh.test: FAILED" >&2
	exit 1
fi
echo "install-sh.test: all cases passed"
