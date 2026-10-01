#!/bin/sh
# Documentation checks for the files in docs/, README.md and friends:
#   1. every relative Markdown link points at a file that exists;
#   2. no personal absolute path such as /root/ leaks into the docs;
#   3. the docs are English (no CJK characters);
#   4. every `crates/...` path written as inline code exists (a bad rename
#      leaves these behind, and they are not links).
#
# Usage: check-docs.sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

files="README.md CONTRIBUTING.md CHANGELOG.md SECURITY.md"
for extra in docs/*.md; do
	[ -f "$extra" ] && files="$files $extra"
done

python3 - "$root" $files <<'PY'
import os
import re
import sys

root = sys.argv[1]
files = sys.argv[2:]
problems = []

code_path_re = re.compile(r'`(crates/[A-Za-z0-9_./-]+)`')
link_re = re.compile(r'\]\(([^)\s]+)\)')
cjk = re.compile(
    "[\u3000-\u303f\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\uff00-\uffef]"
)

for name in files:
    path = os.path.join(root, name)
    if not os.path.isfile(path):
        problems.append(f"{name}: listed but missing")
        continue
    text = open(path, encoding="utf-8").read()
    base = os.path.dirname(path)
    for target in link_re.findall(text):
        if target.startswith(("http://", "https://", "mailto:", "#", "//")):
            continue
        if "://" in target:
            continue
        target = target.split("#", 1)[0]
        if target == "":
            continue
        resolved = os.path.normpath(os.path.join(base, target)) if not target.startswith("/") \
            else os.path.join(root, target.lstrip("/"))
        if not os.path.exists(resolved):
            problems.append(f"{name}: broken relative link -> {target}")
    for target in code_path_re.findall(text):
        if not os.path.exists(os.path.join(root, target.rstrip("/"))):
            problems.append(f"{name}: inline path does not exist -> {target}")
    for lineno, line in enumerate(text.splitlines(), 1):
        if "/root/" in line:
            problems.append(f"{name}:{lineno}: personal absolute path '/root/'")
        if cjk.search(line):
            problems.append(f"{name}:{lineno}: non-English (CJK) text")

if problems:
    print("documentation check failed:", file=sys.stderr)
    for problem in problems:
        print(f"  {problem}", file=sys.stderr)
    sys.exit(1)
print("documentation check passed")
PY
