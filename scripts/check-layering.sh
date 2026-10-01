#!/bin/sh
# Enforce the dependency rules of the workspace.
#
# Rules:
#   R1  opencraylsp-core and opencraylsp-tools must not depend on opencraylsp-client, opencraylsp-mcp or opencraylspd.
#   R2  opencraylsp-proto must not depend on any other workspace crate.
#   R3  every dependency must come from crates.io or from this workspace: a
#       `path` dependency may only point at another crate in this workspace,
#       and a `git` dependency is never allowed. Dependencies on an external
#       project make the build depend on a checkout that is not published.
#
# Reads Cargo.toml directly (no cargo invocation), so it works on a fixture
# tree too. Usage: check-layering.sh [--root DIR]
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
while [ $# -gt 0 ]; do
	case "$1" in
		--root) root=$2; shift 2 ;;
		-h|--help) echo "usage: check-layering.sh [--root DIR]"; exit 0 ;;
		*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done

crates_dir="$root/crates"
if [ ! -d "$crates_dir" ]; then
	echo "error: no crates/ directory under $root" >&2
	exit 2
fi

# Emits "<package>\t<dependent>\t<section>\t<kind>\t<value>" for every
# dependency edge. `<kind>` is `path`, `git`, or empty; `<value>` is the path or
# URL. The crate's own directory is appended as a sixth field so a relative
# path can be resolved without guessing.
edges() {
	for manifest in "$crates_dir"/*/Cargo.toml; do
		[ -f "$manifest" ] || continue
		awk '
			/^\[package\]/                    { section = "package"; next }
			/^\[dependencies\]/               { section = "dependencies"; next }
			/^\[dev-dependencies\]/           { section = "dev-dependencies"; next }
			/^\[build-dependencies\]/         { section = "build-dependencies"; next }
			/^\[/                             { section = ""; next }
			section == "package" && /^[[:space:]]*name[[:space:]]*=/ {
				name = $0; sub(/^[^=]*=[[:space:]]*/, "", name); gsub(/["[:space:]]/, "", name)
				next
			}
			(section == "dependencies" || section == "dev-dependencies" || section == "build-dependencies") \
				&& /^[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=/ {
				line = $0
				dep = line; sub(/[[:space:]]*=.*/, "", dep); gsub(/^[[:space:]]+/, "", dep)
				kind = ""; value = ""
				if (match(line, /path[[:space:]]*=[[:space:]]*"[^"]*"/)) {
					value = substr(line, RSTART, RLENGTH)
					sub(/^path[[:space:]]*=[[:space:]]*"/, "", value); sub(/"$/, "", value)
					kind = "path"
				} else if (match(line, /git[[:space:]]*=[[:space:]]*"[^"]*"/)) {
					value = substr(line, RSTART, RLENGTH)
					sub(/^git[[:space:]]*=[[:space:]]*"/, "", value); sub(/"$/, "", value)
					kind = "git"
				}
				crate_dir = FILENAME
				sub(/\/Cargo\.toml$/, "", crate_dir)
				printf "%s\t%s\t%s\t%s\t%s\t%s\n", name, dep, section, kind, value, crate_dir
			}
			END { }
		' "$manifest"
	done
}

# Workspace crate names, taken from each package's own `name`.
workspace_names() {
	for manifest in "$crates_dir"/*/Cargo.toml; do
		[ -f "$manifest" ] || continue
		awk -F'=' '/^\[package\]/{p=1;next} /^\[/{p=0} p && $1 ~ /^[[:space:]]*name[[:space:]]*$/ {gsub(/["[:space:]]/,"",$2); print $2}' "$manifest"
	done
}

violations=0
report() {
	violations=$((violations + 1))
	printf 'layering violation: %s\n' "$1" >&2
}

ws=$(workspace_names | tr '\n' ' ')
all_edges=$(edges)

# R1: core/tools must not reach the client/daemon layer.
for dependent in opencraylsp-core opencraylsp-tools; do
	for dep in opencraylsp-client opencraylsp-mcp opencraylspd; do
		if printf '%s\n' "$all_edges" | grep -q "^$dependent	$dep	"; then
			report "$dependent depends on $dep (rule R1: opencraylsp-core/opencraylsp-tools must not depend on opencraylsp-client/opencraylsp-mcp/opencraylspd)"
		fi
	done
done

# R2: opencraylsp-proto depends on nothing else in the workspace.
printf '%s\n' "$all_edges" | while IFS='	' read -r pkg dep section kind value crate_dir; do
	[ "$pkg" = "opencraylsp-proto" ] || continue
	case " $ws " in *" $dep "*) echo "opencraylsp-proto $dep $section" ;; esac
done > /tmp/check-layering-proto.$$
while read -r pkg dep section; do
	report "opencraylsp-proto depends on $dep (rule R2: opencraylsp-proto must not depend on other workspace crates)"
done < /tmp/check-layering-proto.$$
rm -f /tmp/check-layering-proto.$$

# R3: only crates.io or workspace crates.
printf '%s\n' "$all_edges" | while IFS='	' read -r pkg dep section kind value crate_dir; do
	[ -n "$dep" ] || continue
	case "$kind" in
		git)
			echo "$pkg -> $dep via git dependency $value ($section)" ;;
		path)
			case "$value" in
				/*) target=$value ;;
				*) target=$crate_dir/$value ;;
			esac
			resolved=$(realpath -m -- "$target" 2>/dev/null || printf '%s' "$target")
			case "$resolved" in
				"$crates_dir"/*)
					# Under crates/, but it still has to be a crate: a path
					# into an empty or foreign directory is not a workspace crate.
					[ -f "$resolved/Cargo.toml" ] || \
						echo "$pkg -> $dep via path dependency $value ($section, not a crate of this workspace)" ;;
				*) echo "$pkg -> $dep via path dependency $value ($section, outside the workspace)" ;;
			esac ;;
	esac
done > /tmp/check-layering-external.$$
while IFS= read -r line; do
	[ -n "$line" ] || continue
	report "$line (rule R3: dependencies must be crates.io packages or crates of this workspace, not a path or git dependency on an external project)"
done < /tmp/check-layering-external.$$
rm -f /tmp/check-layering-external.$$

if [ "$violations" -ne 0 ]; then
	echo "layering check failed with $violations violation(s)" >&2
	exit 1
fi
echo "layering check passed"
