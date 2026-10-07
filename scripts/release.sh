#!/usr/bin/env bash
# The one way a release is cut. Order matters, and every step must pass before the
# next one runs:
#
#   1. preflight        the same checks CI runs, on the exact tree being released
#   2. push main        and WAIT for CI on that commit to finish green
#   3. tag              v<Cargo version>, created only after step 2 is green
#   4. push the tag     and WAIT for the release and audit workflows to finish green
#
# A tag is never created on a commit whose CI has not passed, so a published
# release always points at a green commit. A tag is never moved or deleted: if a
# step fails after the tag is public, the fix is a NEW version.
#
#   bash scripts/release.sh            # prints the plan and runs preflight only
#   bash scripts/release.sh --publish  # does all four steps (pushes to origin)
#
# Needs: git, gh (authenticated as an account that may push to origin).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PUBLISH=0
case "${1:-}" in
	--publish) PUBLISH=1 ;;
	'') ;;
	*) echo "usage: release.sh [--publish]" >&2; exit 2 ;;
esac

version="$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml | head -1)"
[ -n "$version" ] || { echo "release: no workspace version in Cargo.toml" >&2; exit 1; }
tag="v$version"

branch="$(git branch --show-current)"
[ "$branch" = "main" ] || { echo "release: releases are cut from main, not '$branch'" >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
	echo "release: tag $tag already exists; a tag is never reused. Bump the version." >&2
	exit 1
fi
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
	echo "release: tag $tag already exists on origin; a tag is never reused. Bump the version." >&2
	exit 1
fi
# CHANGELOG sections are headed with the crate version, e.g. `## [0.20260929.1] - 2026-10-01`.
grep -q "^## \[${version}\]" CHANGELOG.md ||
	{ echo "release: CHANGELOG.md has no '## [$version]' section; cut it in the same commit as the bump" >&2; exit 1; }

echo "release plan for $tag at $(git rev-parse --short HEAD):"
echo "  1. preflight"
echo "  2. push main, wait for CI green"
echo "  3. create tag $tag"
echo "  4. push the tag, wait for release and audit green"

bash scripts/preflight.sh

if [ "$PUBLISH" -ne 1 ]; then
	echo
	echo "release: preflight passed. Re-run with --publish to push and tag."
	exit 0
fi

command -v gh >/dev/null 2>&1 || { echo "release: gh is required for --publish" >&2; exit 1; }

# Waits for every workflow run attached to <sha>, optionally restricted to one event,
# and fails unless each one concluded 'success'. A run that does not exist yet is
# waited for (GitHub takes a few seconds to create it after a push).
wait_green() {
	local sha="$1" label="$2" ref="${3:-}" deadline=$((SECONDS + 1800)) json total pending bad
	echo "release: waiting for $label on ${sha:0:7} ..."
	while [ "$SECONDS" -lt "$deadline" ]; do
		json="$(gh run list --commit "$sha" --limit 20 --json workflowName,status,conclusion,headBranch 2>/dev/null || echo '[]')"
		if [ -n "$ref" ]; then
			json="$(printf '%s' "$json" | python3 -c "import sys,json; ref=sys.argv[1]; print(json.dumps([r for r in json.load(sys.stdin) if r['headBranch']==ref]))" "$ref")"
		fi
		total="$(printf '%s' "$json" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')"
		pending="$(printf '%s' "$json" | python3 -c "import sys,json; print(sum(1 for r in json.load(sys.stdin) if r['status']!='completed'))")"
		bad="$(printf '%s' "$json" | python3 -c "import sys,json; print(sum(1 for r in json.load(sys.stdin) if r['status']=='completed' and r['conclusion']!='success'))")"
		if [ "$bad" -gt 0 ]; then
			echo "release: $label FAILED:" >&2
			printf '%s' "$json" | python3 -c "import sys,json; [print('  ',r['workflowName'],r['conclusion']) for r in json.load(sys.stdin) if r['status']=='completed' and r['conclusion']!='success']" >&2
			return 1
		fi
		if [ "$total" -gt 0 ] && [ "$pending" -eq 0 ]; then
			echo "release: $label green ($total workflow run(s))"
			return 0
		fi
		sleep 15
	done
	echo "release: timed out waiting for $label" >&2
	return 1
}

head_sha="$(git rev-parse HEAD)"
git push origin main
wait_green "$head_sha" "CI on main" main

git tag -a "$tag" -m "opencraylsp $version"
git push origin "$tag"
wait_green "$head_sha" "release and audit for $tag" "$tag"

echo "release: $tag published and green."
