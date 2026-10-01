#!/bin/sh
# Regenerate the generated block of docs/TOOLS.md from opencraylsp_tools::tool_defs(),
# by asking a real opencraylsp-mcp (--embedded) for tools/list.
#
# Usage: gen-tools-doc.sh [OUTPUT]   (default: docs/TOOLS.md)
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
out=${1:-$root/docs/TOOLS.md}
jobs=${JOBS:-14}
target=${CARGO_TARGET_DIR:-$root/target}
export CARGO_TARGET_DIR="$target"

cargo build -p opencraylsp-mcp -j "$jobs" >/dev/null

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/ws"

bin="$target/debug/opencraylsp-mcp"
[ -x "$bin" ] || bin="$target/release/opencraylsp-mcp"

printf '%s\n' \
	'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"gen-tools-doc","version":"0"}}}' \
	'{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' \
	'{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
	| "$bin" --embedded --workspace "$work/ws" > "$work/transcript.jsonl" 2>/dev/null

python3 "$root/scripts/render-tools-doc.py" "$work/transcript.jsonl" "$out"
echo "wrote $out"
