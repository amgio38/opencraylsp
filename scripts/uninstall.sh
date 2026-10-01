#!/bin/sh
# Remove the programs installed by scripts/install.sh (or `make install`).
#
#   sh scripts/uninstall.sh
#   sh scripts/uninstall.sh --prefix /usr/local
#
# Removes the two programs and the licence files under $PREFIX/share/doc/opencraylsp,
# and, when the prefix carries a `cargo install` registry (a prefix that was the
# target of `cargo install --root`), tells cargo to drop those entries too. The
# daemon's socket, its log under $XDG_STATE_HOME/opencraylsp/ and your config under
# $XDG_CONFIG_HOME/opencraylsp/ are left alone. Stop the daemon first with `opencraylspd stop`.
#
# Environment: PREFIX, DESTDIR.
set -eu

PREFIX=${PREFIX:-}
DESTDIR=${DESTDIR:-}

usage() {
	cat <<'EOF'
usage: uninstall.sh [options]

  --prefix DIR    prefix the files were installed under (default: $PREFIX or ~/.local)
  --destdir DIR   staging prefix prepended to --prefix, for packagers
  -h, --help      show this help

Environment: PREFIX, DESTDIR.
EOF
}

need_value() {
	[ "$1" -ge 2 ] || { echo "uninstall.sh: error: $2 needs a value" >&2; usage >&2; exit 2; }
}

# `--prefix ''` reached this for as long as the script has existed, and the
# function was never defined: `set -e` turned it into a bare
# "usage_die: not found" and exit 127, with no idea which argument was meant.
usage_die() {
	echo "uninstall.sh: error: $*" >&2
	usage >&2
	exit 2
}

main() {
	PREFIX_GIVEN=0
	while [ $# -gt 0 ]; do
		case "$1" in
			--prefix) need_value $# "$1"; PREFIX=$2; PREFIX_GIVEN=1; shift 2 ;;
			--prefix=*) PREFIX=${1#*=}; PREFIX_GIVEN=1; shift ;;
			--destdir) need_value $# "$1"; DESTDIR=$2; shift 2 ;;
			--destdir=*) DESTDIR=${1#*=}; shift ;;
			-h|--help) usage; exit 0 ;;
			*) echo "uninstall.sh: unknown argument: $1" >&2; usage >&2; exit 2 ;;
		esac
	done
	if [ -z "$PREFIX" ] && [ "$PREFIX_GIVEN" -eq 0 ]; then
		[ -n "${HOME:-}" ] || { echo "uninstall.sh: error: HOME is not set, so there is no default prefix; pass --prefix DIR" >&2; exit 2; }
		PREFIX=$HOME/.local
	fi
	# Same reason as install.sh's copy: a prefix that already ends in bin or sbin
	# would name a directory the programs were never installed into.
	case "$PREFIX" in
		/bin|/sbin|*/bin|*/sbin)
			echo "uninstall.sh: error: --prefix $PREFIX ends in bin or sbin, which is not where the" >&2
			echo "              programs were installed; pass the parent directory instead" >&2
			exit 2 ;;
	esac

	case "$PREFIX" in
		'~'|'~/'*)
			[ -n "${HOME:-}" ] || { echo "uninstall.sh: error: HOME is not set, so ~ cannot be expanded; pass an absolute --prefix" >&2; exit 2; }
			PREFIX=$(printf '%s' "$HOME${PREFIX#\~}") ;;
	esac

	[ -n "$PREFIX" ] || usage_die "--prefix must not be empty"

	BINDIR=$(printf '%s%s/bin' "$DESTDIR" "$PREFIX" | tr -s /)
	ROOT="$DESTDIR$PREFIX"
	DOCDIR=$(printf '%s/share/doc/opencraylsp' "$ROOT" | tr -s /)

	# A prefix that was the target of `cargo install --root` keeps opencraylspd/opencraylsp-mcp in
	# its registry; `cargo uninstall` removes the programs and the entries.
	cargo_ran=0
	if { [ -f "$ROOT/.crates2.json" ] || [ -f "$ROOT/.crates.toml" ]; } && command -v cargo >/dev/null 2>&1; then
		cargo_ran=1
		CARGO_INSTALL_ROOT="$ROOT" cargo uninstall opencraylspd opencraylsp-mcp >/dev/null 2>&1 || true
	fi

	removed=0
	for name in opencraylspd opencraylsp-mcp; do
		if [ -e "$BINDIR/$name" ]; then
			rm -f "$BINDIR/$name"
			echo "uninstall.sh: removed $BINDIR/$name"
			removed=$((removed + 1))
		fi
	done

	# The licence files are ours and only ours: install.sh creates this directory and
	# puts nothing else in it. Refuse to remove it rather than follow it -- `rm -rf`
	# on a path built from user input is not something to do without a check, and
	# the check here is that the last three components are the constant ones.
	case "$DOCDIR" in
		/share/doc/opencraylsp)
			echo "uninstall.sh: refusing to remove $DOCDIR" >&2
			exit 1 ;;
	esac
	if [ -d "$DOCDIR" ]; then
		if [ -L "$DOCDIR" ]; then
			echo "uninstall.sh: $DOCDIR is a symlink; not removing it" >&2
		else
			rm -rf "$DOCDIR"
			# Take the directories this created, but only while they are empty:
			# `rmdir` refuses a directory that holds anything, so a prefix someone
			# else has put files in keeps them and keeps its own share/doc.
			rmdir "$(dirname "$DOCDIR")" 2>/dev/null || true
			rmdir "$(dirname "$(dirname "$DOCDIR")")" 2>/dev/null || true
			echo "uninstall.sh: removed $DOCDIR"
			removed=$((removed + 1))
		fi
	fi

	# Without cargo, the registry files can only be pointed at, not rewritten:
	# deleting them wholesale would drop every other crate installed in the root.
	if [ "$cargo_ran" -eq 0 ] && ! command -v cargo >/dev/null 2>&1; then
		if [ -f "$ROOT/.crates2.json" ] || [ -f "$ROOT/.crates.toml" ]; then
			echo "uninstall.sh: note: $ROOT/.crates{.toml,2.json} still lists installed" >&2
			echo "              crates; cargo is not installed, so remove the opencraylspd/opencraylsp-mcp" >&2
			echo "              entries by hand" >&2
		fi
	fi

	if [ "$removed" -eq 0 ] && [ "$cargo_ran" -eq 0 ]; then
		echo "uninstall.sh: nothing to remove under $BINDIR"
	fi
}

main "$@"
