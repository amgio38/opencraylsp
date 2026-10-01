#!/usr/bin/env python3
"""Render the generated block of docs/TOOLS.md from a tools/list transcript.

Reads the JSONL produced by an `opencraylsp-mcp --embedded` session (initialize +
tools/list) and rewrites the text between the GENERATED markers. Hand-written
sections outside the markers are preserved.
"""
import json
import sys

BEGIN = "<!-- BEGIN GENERATED: tools (scripts/gen-tools-doc.sh) -->"
END = "<!-- END GENERATED: tools -->"


def load_tools(path: str):
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        message = json.loads(line)
        result = message.get("result")
        if isinstance(result, dict) and isinstance(result.get("tools"), list):
            return result["tools"]
    raise SystemExit(f"no tools/list result in {path}")


def render(tools) -> str:
    parts = [BEGIN, ""]
    parts.append(
        "The catalogue below is generated from `opencraylsp_tools::tool_defs()`; "
        "do not edit it by hand.")
    parts.append("")
    for tool in tools:
        parts.append(f"### `{tool['name']}`")
        parts.append("")
        parts.append(tool["description"].strip())
        parts.append("")
        annotations = tool.get("annotations") or {}
        if annotations.get("readOnlyHint"):
            parts.append("Read-only (`readOnlyHint: true`).")
            parts.append("")
        parts.append("Arguments (JSON Schema):")
        parts.append("")
        parts.append("```json")
        schema = tool.get("input_schema", tool.get("inputSchema"))
        parts.append(json.dumps(schema, indent=2, ensure_ascii=False))
        parts.append("```")
        parts.append("")
    parts.append(END)
    return "\n".join(parts)


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: render-tools-doc.py <transcript.jsonl> <out.md>")
    transcript, out = sys.argv[1], sys.argv[2]
    block = render(load_tools(transcript))
    try:
        text = open(out, encoding="utf-8").read()
    except FileNotFoundError:
        text = ""
    if BEGIN in text and END in text:
        before = text.split(BEGIN, 1)[0]
        after = text.split(END, 1)[1]
        text = before + block + after
    else:
        if text and not text.endswith("\n"):
            text += "\n"
        text += "\n" + block + "\n"
    with open(out, "w", encoding="utf-8") as handle:
        handle.write(text)


if __name__ == "__main__":
    main()
