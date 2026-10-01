#!/bin/sh
# Fail if docs/TOOLS.md has drifted from opencraylsp_tools::tool_defs().
# Usage: check-tools-doc.sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# Regenerate into a copy of the committed file, so only the generated block can
# differ; the hand-written sections are carried along untouched.
generated=$(mktemp)
trap 'rm -f "$generated"' EXIT
cp "$root/docs/TOOLS.md" "$generated"

sh "$root/scripts/gen-tools-doc.sh" "$generated" >/dev/null

if ! diff -u "$root/docs/TOOLS.md" "$generated" > /tmp/check-tools-doc-diff.$$; then
	echo "docs/TOOLS.md is out of sync with opencraylsp_tools::tool_defs()." >&2
	echo "Run scripts/gen-tools-doc.sh and commit the result." >&2
	cat /tmp/check-tools-doc-diff.$$ >&2
	rm -f /tmp/check-tools-doc-diff.$$
	exit 1
fi
rm -f /tmp/check-tools-doc-diff.$$
echo "docs/TOOLS.md is in sync with tool_defs()"
