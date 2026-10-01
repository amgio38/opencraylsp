#!/bin/sh
# Install the language servers for the languages you choose. Nothing is
# installed for a language you do not name.
#
#   sh scripts/install-lsp.sh php js           # install just these
#   sh scripts/install-lsp.sh                  # ask (needs a terminal)
#   sh scripts/install-lsp.sh --dry-run go     # print what would run
#   sh scripts/install-lsp.sh --list           # show the menu and exit
#
# Languages: rust, go, js (JavaScript/TypeScript), php, python.
#
# This installs only the language server, with the language's own package
# manager (rustup, go, npm). It never installs a toolchain or a system
# package: when the package manager is missing it says how to get it and moves
# on. A server already on the PATH is left alone.
set -eu

DRY_RUN=0
LANGS=

usage() {
	cat <<'EOF'
usage: install-lsp.sh [--dry-run] [--list] [LANGUAGE...]

  LANGUAGE   rust | go | js | php | python   (ts is an alias of js)
  --dry-run  print the commands without running them
  --list     print the language menu and exit
  -h, --help this help

With no LANGUAGE and a terminal attached, a menu asks which to install.
EOF
}

die() { echo "install-lsp.sh: error: $*" >&2; exit 1; }

case "$(uname -s 2>/dev/null || echo unknown)" in
	MINGW*|MSYS*|CYGWIN*|Windows_NT)
		die "native Windows is not supported (the daemon needs Unix sockets). Use WSL2 and run this inside it." ;;
esac

menu() {
	cat <<'EOF'
Select the programming languages of your projects:

  1) PHP          intelephense
  2) JS/TS        typescript-language-server
  3) GO           gopls
  4) RUST         rust-analyzer
  5) PYTHON       pyright

Only the language servers you pick are installed. The MCP server starts a
language server only when it detects that language in your workspace.
EOF
}

# name of a language -> canonical key, or empty
canon() {
	case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
		rust|rs|4) echo rust ;;
		go|golang|3) echo go ;;
		js|ts|javascript|typescript|js/ts|2) echo js ;;
		php|1) echo php ;;
		python|py|5) echo python ;;
		*) echo ;;
	esac
}

binary_of() {
	case "$1" in
		rust) echo rust-analyzer ;;
		go) echo gopls ;;
		js) echo typescript-language-server ;;
		php) echo intelephense ;;
		python) echo pyright-langserver ;;
	esac
}

# The tool that performs the install, and how to get it.
needs_of() {
	case "$1" in
		rust) echo "rustup|https://rustup.rs" ;;
		go) echo "go|https://go.dev/doc/install" ;;
		js|php|python) echo "npm|https://nodejs.org (or nvm)" ;;
	esac
}

run() {
	if [ "$DRY_RUN" -eq 1 ]; then
		echo "  [dry-run] $*"
	else
		echo "  + $*"
		"$@"
	fi
}

install_one() {
	lang=$1
	bin=$(binary_of "$lang")
	label=$lang
	if command -v "$bin" >/dev/null 2>&1; then
		echo "$label: $bin already installed ($(command -v "$bin")), skipping"
		return 0
	fi
	needs=$(needs_of "$lang")
	tool=${needs%%|*}
	hint=${needs#*|}
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "$label: skipped, '$tool' is not installed (get it from $hint)" >&2
		FAILED=1
		return 0
	fi
	echo "$label: installing $bin"
	case "$lang" in
		rust) run rustup component add rust-analyzer ;;
		go) run go install golang.org/x/tools/gopls@latest ;;
		# TypeScript 7 no longer bundles tsserver.js; the server needs a 5.x one.
		js) run npm install -g typescript@5 typescript-language-server ;;
		php) run npm install -g intelephense ;;
		python) run npm install -g pyright ;;
	esac || {
		echo "$label: install failed" >&2
		case "$tool" in
			npm)
				echo "       if npm reported a permission error, do not use sudo: install Node with a" >&2
				echo "       version manager such as nvm, or give npm a prefix you own:" >&2
				echo "           npm config set prefix \"\$HOME/.local\"   (and put \$HOME/.local/bin on PATH)" >&2
				;;
		esac
		FAILED=1
		return 0
	}
	# `go install` and `cargo`-style installs land outside the default PATH.
	if [ "$DRY_RUN" -eq 0 ] && ! command -v "$bin" >/dev/null 2>&1; then
		case "$lang" in
			go)
				gobin=$(go env GOBIN 2>/dev/null)
				[ -n "$gobin" ] || gobin=$(go env GOPATH 2>/dev/null)/bin
				echo "$label: $bin was installed in $gobin, which is not on your PATH." >&2
				echo "       add it:  export PATH=\"$gobin:\$PATH\"" >&2
				;;
			*) echo "$label: $bin is not on your PATH yet; open a new shell" >&2 ;;
		esac
	fi
}

FAILED=0

while [ $# -gt 0 ]; do
	case "$1" in
		--dry-run) DRY_RUN=1 ;;
		--list) menu; exit 0 ;;
		-h|--help) usage; exit 0 ;;
		-*) echo "install-lsp.sh: unknown argument: $1" >&2; usage >&2; exit 2 ;;
		*)
			c=$(canon "$1")
			[ -n "$c" ] || { echo "install-lsp.sh: unknown language: $1 (use rust, go, js, php, python)" >&2; exit 2; }
			LANGS="$LANGS $c"
			;;
	esac
	shift
done

if [ -z "$LANGS" ]; then
	if [ -t 0 ] && [ -t 1 ]; then
		menu
		printf '\nEnter numbers or names separated by spaces (empty to skip): '
		read -r answer || answer=
		for word in $answer; do
			c=$(canon "$word")
			if [ -n "$c" ]; then
				LANGS="$LANGS $c"
			else
				echo "ignoring unknown choice: $word" >&2
			fi
		done
	else
		menu
		echo
		echo "No language given and no terminal to ask on. Pick with, for example:"
		echo "    make install-lsp php js"
		exit 0
	fi
fi

[ -n "$LANGS" ] || { echo "No language selected; no language server installed."; exit 0; }

# De-duplicate, keeping order.
seen=
for l in $LANGS; do
	case " $seen " in *" $l "*) continue ;; esac
	seen="$seen $l"
	install_one "$l"
done

echo
echo "Check with: opencraylspd doctor"
[ "$FAILED" -eq 0 ] || exit 1
