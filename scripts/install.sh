#!/bin/sh
# One-line installer for opencraylspd and opencraylsp-mcp.
#
#   curl -fsSL https://raw.githubusercontent.com/amgio38/opencraylsp/main/scripts/install.sh | sh
#   sh scripts/install.sh --prefix "$HOME/.local"
#
# Pass options through the pipe with `sh -s --`:
#
#   curl -fsSL https://raw.githubusercontent.com/amgio38/opencraylsp/main/scripts/install.sh | sh -s -- --prefix /opt/lsp
#
# Platforms:
#   Linux x86_64   prebuilt static binary (no Rust needed);
#   Linux aarch64  prebuilt static binary (no Rust needed);
#   Linux, others  build from source (Rust 1.89+ and a C compiler);
#   macOS          build from source (Rust 1.89+ and the Xcode command line tools);
#   Windows        not supported natively -- use WSL2 and run this inside it.
#
# Resolution order on Linux x86_64:
#   1. a prebuilt release asset (static musl, no Rust);
#   2. otherwise `cargo install --locked --path` from the current checkout, or
#      from a shallow clone when there is no checkout.
#
# The whole script is wrapped in main() and only runs on its last line, so a
# download that is cut off part-way parses as an incomplete function and does
# nothing, instead of executing a truncated script. That matters for `curl | sh`.
# Nothing here reads standard input (it is the script itself when piped), and
# child programs that might are given /dev/null.
#
# Both binaries are installed as a pair: they are staged first, checked that
# they run and report the same version, and only then moved into place. A
# failure leaves the previous installation untouched.
#
# Installing never edits a client's configuration, never uses sudo, and never
# restarts a running daemon; the last lines printed say what to do next.
#
# The licence files go to $PREFIX/share/doc/opencraylsp. A binary in $BINDIR is a
# redistribution, and the licences of what is bundled in it have to travel with
# it, so this installer puts them there rather than leaving them in the
# repository where a person who only has the binary cannot read them.
#
# Environment: PREFIX, DESTDIR, OPENCRAYLSP_VERSION, OPENCRAYLSP_REPO, OPENCRAYLSP_INSECURE.
set -eu

# Defaults that do not depend on the arguments. Anything that reads $HOME waits
# for main(), where an unset HOME can be reported in words.
FROM_SOURCE=0
INSECURE=${OPENCRAYLSP_INSECURE:-0}
tmp=
stage=

# Where the licence files go under $PREFIX. Kept as a constant here rather than
# read from scripts/doc-files.sh because this script is normally run as
# `curl ... | sh`: whatever directory it was fetched into is not somewhere a
# sibling file can be relied upon to be. The release workflow, the Makefile and
# the license check all do read that file, and those run in a checkout.
DOC_SUBDIR='share/doc/opencraylsp'

usage() {
	cat <<'EOF'
usage: install.sh [options]

  --prefix DIR    install under DIR/bin, and the licence files under
                  DIR/share/doc/opencraylsp (default: $PREFIX or ~/.local)
  --destdir DIR   staging prefix prepended to --prefix, for packagers
  --version TAG   release tag to fetch (default: the newest release)
  --from-source   skip release assets and build with cargo
  --insecure      install a release asset with no checksum, if that is all
                  there is. Off by default: see below.
  -h, --help      show this help

A release asset is only installed after its published SHA-256 matches. If no
checksum is published, or no SHA-256 tool is installed, the installer stops
rather than installing an unverified binary -- `sha256sum` is not present on a
stock macOS, so failing open would have skipped verification for most of the
platform this script is used from. Pass --insecure (or OPENCRAYLSP_INSECURE=1) when
you are reproducing a build yourself and accept the risk.

A prefix that ends in /bin or /sbin is refused: this script adds /bin itself, so
that asks for a directory one level too deep. A prefix that would put the
programs in /bin, /sbin, /usr/bin or /usr/sbin is refused as well -- use
--prefix /usr/local, or DESTDIR if you are building a package that is meant to
install there.

Platforms: Linux x86_64 and aarch64 install a prebuilt static binary. Linux on
other CPUs and macOS build from source with cargo (Rust 1.89+ and a C
compiler). Native Windows is not supported; use WSL2 and run this inside it.

Environment: PREFIX, DESTDIR, OPENCRAYLSP_VERSION, OPENCRAYLSP_REPO, OPENCRAYLSP_INSECURE.
EOF
}

die() { echo "install.sh: error: $*" >&2; exit 1; }
usage_die() { echo "install.sh: error: $*" >&2; usage >&2; exit 2; }

# A flag that takes a value must be given one, or the shell's own
# "parameter not set" would be the error message.
need_value() {
	[ "$1" -ge 2 ] || { echo "install.sh: error: $2 needs a value" >&2; usage >&2; exit 2; }
}

# One cleanup for everything this run creates, so no code path can replace the
# trap and leave the downloads behind.
cleanup() {
	[ -z "$stage" ] || rm -rf "$stage"
	[ -z "$tmp" ] || rm -rf "$tmp"
}

parse_args() {
	while [ $# -gt 0 ]; do
		case "$1" in
			--prefix) need_value $# "$1"; PREFIX=$2; PREFIX_GIVEN=1; shift 2 ;;
			--prefix=*) PREFIX=${1#*=}; PREFIX_GIVEN=1; shift ;;
			--destdir) need_value $# "$1"; DESTDIR=$2; shift 2 ;;
			--destdir=*) DESTDIR=${1#*=}; shift ;;
			--version) need_value $# "$1"; VERSION=$2; shift 2 ;;
			--version=*) VERSION=${1#*=}; shift ;;
			--from-source) FROM_SOURCE=1; shift ;;
			--insecure) INSECURE=1; shift ;;
			-h|--help) usage; exit 0 ;;
			*) echo "install.sh: unknown argument: $1" >&2; usage >&2; exit 2 ;;
		esac
	done
}

# The release tag and the repository name end up in URLs and in `git clone`, and
# both can come from the caller's environment. Allow only what a tag or a
# GitHub owner/name can contain; in particular nothing that starts with a dash
# and no slash or dot-dot that could walk the URL somewhere else.
validate_inputs() {
	if [ -n "$VERSION" ]; then
		case "$VERSION" in
			-*|*[!A-Za-z0-9._+-]*|*..*) usage_die "--version '$VERSION' is not a release tag (letters, digits, . _ + - only)" ;;
		esac
	fi
	case "$REPO" in
		*/*/*|/*|*/|-*|*[!A-Za-z0-9._/-]*|*..*|"") usage_die "OPENCRAYLSP_REPO '$REPO' is not of the form owner/name" ;;
		*/*) ;;
		*) usage_die "OPENCRAYLSP_REPO '$REPO' is not of the form owner/name" ;;
	esac
}

# What this machine is, and therefore whether a prebuilt binary exists for it.
# Sets OS_NAME, PLATFORM_LABEL and TARGET (empty when there is no prebuilt
# binary). Refuses the platforms the daemon cannot run on at all.
detect_platform() {
	OS_NAME=$(uname -s 2>/dev/null || echo unknown)
	arch=$(uname -m 2>/dev/null || echo unknown)
	PLATFORM_LABEL="$OS_NAME $arch"
	TARGET=
	case "$OS_NAME" in
		Linux)
			case "$arch" in
				x86_64|amd64) TARGET=x86_64-unknown-linux-musl ;;
				aarch64|arm64) TARGET=aarch64-unknown-linux-musl ;;
			esac ;;
		Darwin) ;;
		MINGW*|MSYS*|CYGWIN*|Windows_NT)
			die "native Windows is not supported (the daemon needs Unix sockets). Install WSL2, open a Linux shell, and run this installer there." ;;
		*)
			echo "install.sh: note: $OS_NAME is not a tested platform; trying a source build" >&2 ;;
	esac
}

# The programs this script itself leans on, named up front instead of failing
# half-way with "command not found".
require_tools() {
	missing=
	for t in uname tar mktemp install sed sort head tr dirname; do
		command -v "$t" >/dev/null 2>&1 || missing="$missing $t"
	done
	[ -z "$missing" ] || die "missing required tools:$missing"
}

have_downloader() {
	command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1
}

# Whether this user can create $1: true when it exists and is writable, or when
# its nearest existing parent is. Reports the problem in words, because a bare
# "Permission denied" from `install -d` does not say what to do about it.
require_writable() {
	d=$1
	while [ ! -e "$d" ]; do
		parent=$(dirname -- "$d")
		[ "$parent" != "$d" ] || break
		d=$parent
	done
	if [ ! -d "$d" ] || [ ! -w "$d" ]; then
		die "cannot write to $d (needed for $1). Install somewhere you own with --prefix \"\$HOME/.local\", or re-run with the privileges that directory needs."
	fi
}

# Download URL DEST with connect and total timeouts, so a black-holed network
# fails in bounded time instead of hanging the installer forever.
#
# --proto '=https' refuses a redirect to any other protocol, and --tlsv1.2 sets
# a floor on the negotiated version. Both are about the far end of the connection
# rather than this script, which is why they are here and not left to curl's
# defaults: a redirect from an https release URL to http:// would otherwise be
# followed silently, and the checksum would be computed over whatever arrived.
# A curl too old to know these flags stops the install instead of quietly doing
# without them, which is the same trade the checksum rule makes.
fetch() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --proto '=https' --tlsv1.2 --connect-timeout 10 --max-time 300 --retry 2 "$1" -o "$2"
	elif command -v wget >/dev/null 2>&1; then
		wget -q --https-only --connect-timeout=10 --timeout=300 --tries=3 -O "$2" "$1"
	else
		return 1
	fi
}

# The SHA-256 tool this system has, or empty when it has none.
#
# `sha256sum` is GNU coreutils; macOS ships `shasum` instead, so a check that
# only looks for the first one silently passes on most of the platforms this
# installer is used from.
sha256_tool() {
	if command -v sha256sum >/dev/null 2>&1; then
		echo sha256sum
	elif command -v shasum >/dev/null 2>&1; then
		echo "shasum -a 256"
	else
		echo ""
	fi
}

# Refuses to install a release asset that was not verified.
#
# Every path out of here used to print a line and carry on, which made the
# installer's only integrity guarantee optional: no published checksum, an empty
# checksum file, or a system without `sha256sum` each ended with the same
# "skipping verification" note and an unverified binary in $BINDIR. The note was
# on stderr and easy to scroll past, and there was no way to ask for the check.
# It is now opt-out, and it says which of the three things was missing.
verify_checksum() {
	base=$1 asset=$2
	if [ "$INSECURE" = 1 ]; then
		echo "install.sh: --insecure given; installing $asset unverified" >&2
		return 0
	fi
	tool=$(sha256_tool)
	if [ -z "$tool" ]; then
		die "no SHA-256 tool found (need sha256sum or shasum); refusing to install \
$asset unverified -- install coreutils, or pass --insecure if you accept the risk"
	fi
	if ! fetch "$base/$asset.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
		die "no checksum published for $asset; refusing to install it unverified -- \
pass --insecure if you accept the risk"
	fi
	if [ ! -s "$tmp/$asset.sha256" ]; then
		die "the published checksum for $asset is empty; refusing to install it \
unverified -- pass --insecure if you accept the risk"
	fi
	# `shasum -a 256 -c` and `sha256sum -c` take the same arguments.
	# shellcheck disable=SC2086
	(cd "$tmp" && $tool -c "$asset.sha256") >/dev/null ||
		die "checksum mismatch for $asset; refusing to install it"
}

# The version a staged binary reports, or empty when it will not run.
binary_version() {
	line=$("$1" --version 2>/dev/null | head -n 1) || true
	printf '%s' "${line#* }"
}

# Both programs speak the same wire protocol, so a mixed pair fails at the
# first call. Reports and returns non-zero so the caller can clean up its
# staged files and leave the old installation alone.
verify_pair() {
	opencraylspd_version=$(binary_version "$1")
	opencraylsp_mcp_version=$(binary_version "$2")
	if [ -z "$opencraylspd_version" ]; then
		echo "install.sh: error: $1 is not runnable (no \`--version\` output)" >&2
		return 1
	fi
	if [ -z "$opencraylsp_mcp_version" ]; then
		echo "install.sh: error: $2 is not runnable (no \`--version\` output)" >&2
		return 1
	fi
	if [ "$opencraylspd_version" != "$opencraylsp_mcp_version" ]; then
		echo "install.sh: error: version mismatch: opencraylspd $opencraylspd_version vs opencraylsp-mcp $opencraylsp_mcp_version; refusing to install a mixed pair" >&2
		return 1
	fi
}

# Moves a verified pair into $BINDIR. The programs are staged under temporary
# names in the destination directory and only then renamed over the old ones,
# so a failure before the renames leaves the existing installation untouched.
install_pair() {
	install -d "$BINDIR"
	# A private directory, not a predictable name in $BINDIR. `$$` is the pid:
	# anything that can write to $BINDIR can name it in advance and leave a
	# symlink there, and `install -m 0755` writes through a symlink -- so the
	# staging step could be aimed at any file the invoking user can write.
	# mktemp creates the directory with 0700 and fails rather than reuse it.
	stage=$(mktemp -d "$BINDIR/.opencraylsp.install.XXXXXX") || die "cannot create a staging directory in $BINDIR"
	opencraylspd_tmp="$stage/opencraylspd"
	mcp_tmp="$stage/opencraylsp-mcp"
	install -m 0755 "$1" "$opencraylspd_tmp"
	install -m 0755 "$2" "$mcp_tmp"
	if ! verify_pair "$opencraylspd_tmp" "$mcp_tmp"; then
		exit 1
	fi
	mv -f "$opencraylspd_tmp" "$BINDIR/opencraylspd"
	mv -f "$mcp_tmp" "$BINDIR/opencraylsp-mcp"
	rm -rf "$stage"
	stage=
	echo "install.sh: installed opencraylspd and opencraylsp-mcp ($opencraylspd_version) into $BINDIR"
}

# Checks that every named path is a plain file this installer is willing to
# copy. $1 is the directory the paths are relative to, $2 names where they came
# from; the rest are relative paths.
#
# Separate from the copying so a release archive can be judged *before* anything
# is installed: a refusal that arrived after the programs were in $BINDIR would
# leave a prefix holding a redistributed binary with no licence beside it, which
# is the outcome this whole change exists to prevent.
check_doc_files() {
	src=$1 what=$2
	shift 2
	for rel in "$@"; do
		case "$rel" in
		/*|*..*) die "refusing to install $what as a licence file: $rel" ;;
		esac
		# `install -m 0644` copies whatever a symlink points at, so a licence
		# "file" that is a link is a licence read from somewhere else.
		[ -L "$src/$rel" ] && die "refusing to install $what as a licence file: $rel is a symlink"
		[ -f "$src/$rel" ] || die "refusing to install $what as a licence file: $rel is not a regular file"
	done
}

install_doc_files() {
	src=$1 what=$2
	shift 2
	[ $# -gt 0 ] || return 0
	check_doc_files "$src" "$what" "$@"
	install -d "$DOCDIR"
	for rel in "$@"; do
		install -d "$DOCDIR/$(dirname "$rel")"
		install -m 0644 "$src/$rel" "$DOCDIR/$rel"
	done
	echo "install.sh: installed $# licence file(s) into $DOCDIR"
}

install_from_release() {
	[ "$FROM_SOURCE" -eq 0 ] || return 1
	if [ -z "$TARGET" ]; then
		echo "install.sh: no prebuilt binary for $PLATFORM_LABEL; building from source" >&2
		return 1
	fi
	have_downloader || die "neither curl nor wget is installed; install one of them, or run from a checkout with --from-source"

	if [ -n "$VERSION" ]; then
		base="https://github.com/$REPO/releases/download/$VERSION"
	else
		base="https://github.com/$REPO/releases/latest/download"
	fi
	asset="opencraylsp-$TARGET.tar.gz"

	echo "install.sh: fetching $base/$asset"
	fetch "$base/$asset" "$tmp/$asset" || return 1
	verify_checksum "$base" "$asset"
	# The archive carries the two programs and, under one directory, the licence
	# files that have to travel with a redistributed binary.
	#
	# The two programs are still an exact allowlist, and anything that is neither
	# one of them nor under $DOC_SUBDIR is refused. The licence subtree is
	# checked by shape rather than by name, because this script cannot know which
	# files a future release adds and cannot read a list to find out: it is
	# normally piped straight out of curl. What it does have to guarantee is that
	# every member is a plain relative path with no traversal, so the archive
	# cannot write anywhere else -- which is the only thing the allowlist was
	# ever protecting.
	tar -tzf "$tmp/$asset" | sed 's|^\./||' | LC_ALL=C sort > "$tmp/members"
	programs=
	doc_members=
	while IFS= read -r m; do
		[ -n "$m" ] || continue
		case "$m" in
		opencraylspd|opencraylsp-mcp) programs="$programs $m" ;;
		"$DOC_SUBDIR"/*)
			case "/$m/" in
			*/../*) die "refusing a member with '..' in $asset: $m" ;;
			esac
			# tar records the directories it walked as members ending in "/".
			# They carry no content and `tar -x` recreates the parents of the
			# files anyway, so they are accepted and otherwise ignored.
			case "$m" in
			*/) ;;
			*) doc_members="$doc_members $m" ;;
			esac ;;
		*) die "unexpected member in $asset: $m" ;;
		esac
	done < "$tmp/members"
	# Sorted above, so this is an order-independent exact allowlist: the two
	# programs, and nothing else at the top level.
	[ "$programs" = " opencraylsp-mcp opencraylspd" ] || die "unexpected programs in $asset: $programs"
	# shellcheck disable=SC2086
	tar --no-same-owner --no-same-permissions -xzf "$tmp/$asset" -C "$tmp" opencraylspd opencraylsp-mcp $doc_members \
		|| die "cannot unpack $asset"
	[ -f "$tmp/opencraylspd" ] && [ -f "$tmp/opencraylsp-mcp" ] || die "release asset did not contain opencraylspd and opencraylsp-mcp"
	# The archive's own member list, with the one directory prefix removed. Word
	# splitting here means a member name with a space in it is refused as two
	# files that do not exist, which is the right way round for this list.
	doc_rel=
	for m in $doc_members; do
		doc_rel="$doc_rel ${m#"$DOC_SUBDIR"/}"
	done
	# Judged before anything is installed, so a refusal cannot leave a binary in
	# $BINDIR with no licence beside it.
	check_doc_files "$tmp/$DOC_SUBDIR" "$asset" $doc_rel
	install_pair "$tmp/opencraylspd" "$tmp/opencraylsp-mcp"
	install_doc_files "$tmp/$DOC_SUBDIR" "$asset" $doc_rel
}

install_from_source() {
	command -v cargo >/dev/null 2>&1 || die "cargo not found; building from source needs Rust 1.89 or newer (https://rustup.rs). Prebuilt binaries exist only for Linux x86_64 and aarch64."
	# rustc cannot link without a C toolchain, and a fresh machine often has none.
	if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
		case "$OS_NAME" in
			Darwin) die "no C compiler found; run 'xcode-select --install' and try again" ;;
			*) die "no C compiler found; install one (for example 'apt install build-essential' or 'dnf install gcc') and try again" ;;
		esac
	fi
	repo=$PWD
	if [ ! -f "$repo/Cargo.toml" ] || [ ! -d "$repo/crates/opencraylspd" ] || [ ! -d "$repo/crates/opencraylsp-mcp" ]; then
		command -v git >/dev/null 2>&1 || die "not in a checkout and git is not installed"
		repo="$tmp/src"
		echo "install.sh: cloning https://github.com/$REPO"
		if [ -n "$VERSION" ]; then
			# --branch=VALUE, not --branch VALUE: a version beginning with a
			# dash is read by git as an option rather than as the name to
			# check out, and $VERSION comes from the caller's environment.
			git clone --depth 1 --branch="$VERSION" "https://github.com/$REPO" "$repo" </dev/null >&2 ||
				die "cannot clone $VERSION from https://github.com/$REPO"
		else
			git clone --depth 1 "https://github.com/$REPO" "$repo" </dev/null >&2 ||
				die "cannot clone https://github.com/$REPO"
		fi
	elif [ -n "$VERSION" ]; then
		echo "install.sh: ignoring --version $VERSION: building the local checkout" >&2
	fi
	echo "install.sh: building with cargo (this can take a few minutes)"
	# Build into a private root first: the reported binaries are checked and
	# then installed as a pair, and no cargo bookkeeping is left in $PREFIX.
	build_root="$tmp/cargo-root"
	cargo install --quiet --locked --path "$repo/crates/opencraylspd" --root "$build_root" </dev/null
	cargo install --quiet --locked --path "$repo/crates/opencraylsp-mcp" --root "$build_root" </dev/null
	[ -x "$build_root/bin/opencraylspd" ] || die "cargo did not produce $build_root/bin/opencraylspd"
	[ -x "$build_root/bin/opencraylsp-mcp" ] || die "cargo did not produce $build_root/bin/opencraylsp-mcp"
	install_pair "$build_root/bin/opencraylspd" "$build_root/bin/opencraylsp-mcp"
	# A checkout has the licence files at its root rather than under $DOC_SUBDIR,
	# and it is also the one place scripts/doc-files.sh can be read from. A file
	# added to that list without being added to the repository fails here rather
	# than quietly not shipping.
	if [ -f "$repo/scripts/doc-files.sh" ]; then
		# shellcheck source=/dev/null
		. "$repo/scripts/doc-files.sh"
		# shellcheck disable=SC2086
		install_doc_files "$repo" "the checkout" $OPENCRAYLSP_DOC_FILES
	else
		echo "install.sh: note: no scripts/doc-files.sh in $repo, so the licence files were not installed" >&2
	fi
}

report_next_step() {
	case ":$PATH:" in
		*":$BINDIR:"*) ;;
		*)
			echo "install.sh: note: $BINDIR is not on your PATH. Add it, for example:"
			echo
			echo "    export PATH=\"$BINDIR:\$PATH\""
			echo
			echo "and put that line in your shell's startup file (~/.profile, ~/.bashrc or ~/.zshrc)."
			;;
	esac
	# An already-running daemon keeps serving the old version from memory; the
	# new files only apply to the next one. It is not restarted here because
	# other agents may be using it right now.
	if [ -z "$DESTDIR" ] && "$BINDIR/opencraylspd" status >/dev/null 2>&1; then
		echo
		echo "install.sh: a daemon is running with the previous version. Run"
		echo "    opencraylspd restart"
		echo "when no agent is mid-request, to start using the new one."
	fi
	echo
	echo "Next: add the MCP server to your agent, for example Claude Code:"
	echo
	echo "    claude mcp add opencraylsp -- opencraylsp-mcp --languages auto"
	echo
	echo "See https://github.com/$REPO/blob/main/docs/CLIENTS.md for Cursor, opencode"
	echo "and the other clients, and https://github.com/$REPO/blob/main/docs/INSTALL.md"
	echo "for the language servers to install (rust-analyzer, gopls, pyright, ...)."
	echo
	echo "The licence for what was just installed is in $DOCDIR."
}

main() {
	REPO=${OPENCRAYLSP_REPO:-amgio38/opencraylsp}
	DESTDIR=${DESTDIR:-}
	VERSION=${OPENCRAYLSP_VERSION:-}
	PREFIX=${PREFIX:-}
	PREFIX_GIVEN=0
	parse_args "$@"
	if [ -z "$PREFIX" ] && [ "$PREFIX_GIVEN" -eq 0 ]; then
		[ -n "${HOME:-}" ] || die "HOME is not set, so there is no default install location; pass --prefix DIR"
		PREFIX=$HOME/.local
	fi
	validate_inputs

	# Expand a leading ~ so `--prefix '~/.local'` works even when quoted.
	case "$PREFIX" in
		'~'|'~/'*)
			[ -n "${HOME:-}" ] || die "HOME is not set, so ~ cannot be expanded; pass an absolute --prefix"
			PREFIX=$(printf '%s' "$HOME${PREFIX#\~}") ;;
	esac
	[ -n "$PREFIX" ] || usage_die "--prefix must not be empty"

	# $DESTDIR$PREFIX/bin, with any doubled slash tidied up.
	BINDIR=$(printf '%s%s/bin' "$DESTDIR" "$PREFIX" | tr -s /)
	DOCDIR=$(printf '%s%s/%s' "$DESTDIR" "$PREFIX" "$DOC_SUBDIR" | tr -s /)

	# This script puts the programs in $PREFIX/bin, so a --prefix that already ends
	# in bin or sbin asks for one level too deep. `--prefix /usr/bin` did exactly
	# that: it created /usr/bin/bin, and under root that is a new directory in a
	# system location, made without being asked for anything of the sort.
	case "$PREFIX" in
		/bin|/sbin|*/bin|*/sbin)
			usage_die "--prefix $PREFIX would install into $BINDIR; pass the parent directory instead" ;;
	esac

	# Overwriting a distribution's own binaries is not what any of these prefixes is
	# for. DESTDIR has already been prepended, so a packager staging into /tmp is
	# unaffected and can still build a package meant to install under /usr/bin.
	case "$BINDIR" in
		/bin|/sbin|/usr/bin|/usr/sbin)
			usage_die "refusing to install into $BINDIR (use --prefix /usr/local, or --destdir when packaging)" ;;
	esac

	require_tools
	detect_platform
	require_writable "$BINDIR"
	require_writable "$DOCDIR"

	tmp=$(mktemp -d "${TMPDIR:-/tmp}/opencraylsp-install.XXXXXX") || die "cannot create a temporary directory in ${TMPDIR:-/tmp}"
	trap cleanup EXIT INT TERM

	if ! install_from_release; then
		if [ "$FROM_SOURCE" -eq 0 ] && [ -n "$TARGET" ]; then
			echo "install.sh: no usable release asset; falling back to cargo" >&2
		fi
		install_from_source
	fi

	report_next_step
}

main "$@"
