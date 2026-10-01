# Tools

`opencraylsp-mcp` exposes eleven read-only `lsp_*` tools. They answer semantic
questions about a workspace by driving real language servers (rust-analyzer,
gopls, pyright, typescript-language-server, intelephense) through the `opencraylspd`
daemon. Every tool is type-aware and cross-file, so it beats a text search for
definitions, references and call relationships.

## Calling a tool

Tools are called through MCP. The shape is always:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/call",
  "params": { "name": "lsp_status", "arguments": {} }
}
```

The reply is an MCP tool result: `{"content":[{"type":"text","text":"..."}],
"isError": false}`. A failing tool is not a protocol error; it is a successful
result with `isError: true` whose text starts with a bracketed code (see
[Error and status codes](#error-and-status-codes)).

## Targeting a symbol

Tools that need a location accept the same *target* arguments at the top level:

| Argument | Meaning |
| --- | --- |
| `symbol` | A name, optionally qualified: `parse`, `Foo::bar`, `pkg.Foo`. Resolved through the language server's symbol index. |
| `path` | A file, absolute or relative to the workspace root. On its own it means "restrict to this file". |
| `line` | 1-based line. Give it with `column`, and not with `symbol`. |
| `column` | 1-based character column on that line. |

Give either `symbol`, or `path` + `line` + `column`. When a name matches more
than one symbol the tool returns `[ambiguous]` with the candidates instead of
guessing. Each candidate is printed with the exact `path`, `line` and `column`
to retry with, so the disambiguating call can be pasted as it stands. A
complete `path` + `line` + `column` is used as the position even if `symbol` is
still in the arguments (the name is then ignored); a *partial* position beside a
name — a line with no path, a line with no column — is still rejected.

### A request with no file

`lsp_find_symbol` and a `symbol` search with no `path` have no file to walk up
from, so the project root has to be chosen another way. A language server serves
one project, so this is a real decision rather than a detail:

- The workspace boundary is the root when the boundary *is* a project, or when
  the server has no `root_markers` at all (it works file by file).
- Otherwise the most recently used instance of that server is reused, which is
  what makes a second question in the same project cost nothing.
- Otherwise, if the workspace holds exactly one project of that language, that
  project is used.
- Otherwise the answer is `[no_project]`: either no project was found, or the
  candidates are listed with a `path` to retry with. A boundary that is not a
  project is never used as a root, because a server started there indexes
  nothing while still holding gigabytes.

Nested projects stay separate — one instance per project root — because that is
what a language server can actually serve.

### When a position finds nothing

A position-targeted tool that comes back empty is nearly always aimed one
character to the left or right of the identifier. So the miss carries the line:

```
[not_found] no definition was found: the language server returned nothing in rust-analyzer
Identifiers on line 981: let@102 found@106 fts_normalised@110 raw@127
```

`name@column` uses the same 1-based Unicode scalar counting as every other
position in this document, so a line holding CJK text or an emoji gives the
column the tools themselves accept. At most eight identifiers are listed and the
rest are counted; a line with none, a line longer than 2000 characters, and a
file outside the workspace get no hint at all, because reading any of them would
either bury the miss or walk around the boundary.

One call covers `lsp_definition`, `lsp_references`, `lsp_hover`,
`lsp_implementations`, `lsp_callers`, `lsp_callees` and `lsp_rename_preview`
(`[not_renamable]` there) — all from one function, so the hint is never present
in some answers and missing from others.

A `column` past the end of its line is refused outright, with the line's real
length, rather than clamped to the line's end; a column *one* past the end is a
position (that is where a caret sits at the end of a line).

## The tools

<!-- BEGIN GENERATED: tools (scripts/gen-tools-doc.sh) -->

The catalogue below is generated from `opencraylsp_tools::tool_defs()`; do not edit it by hand.

### `lsp_status`

Report the language servers running for this workspace: version, pid, uptime, memory, connected clients, and each instance's state and indexing progress. Call it before blaming the tools — it separates "no server is running" from "the server is still indexing", which grep can never tell you. Example: lsp_status(). When a server shows [indexing], wait a few seconds and retry rather than giving up on LSP.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {},
  "required": [],
  "type": "object"
}
```

### `lsp_find_symbol`

Find symbols by name across the workspace with a type-aware index. Better than grep because it matches declarations, not text: a comment or a string containing the name is not a hit, and each result carries file, line and kind. Example: lsp_find_symbol(query="parse"). If you get [indexing], retry in a few seconds. If you get [language_disabled], that language was not enabled for this connection; do not retry — use another tool.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "kind": {
      "description": "One LSP SymbolKind name (`function`, `struct`, `enum`, ...), case-insensitive.",
      "type": "string"
    },
    "language": {
      "description": "Restrict to one enabled language, e.g. `rust`, `go`.",
      "type": "string"
    },
    "limit": {
      "description": "Most symbols to list; default 50, maximum 200.",
      "maximum": 200,
      "minimum": 1,
      "type": "integer"
    },
    "path": {
      "description": "A file or directory that scopes the answer to what is inside it.",
      "type": "string"
    },
    "query": {
      "description": "Name to search for; fuzzy, so `parse` also finds `parse_args`.",
      "type": "string"
    }
  },
  "required": [
    "query"
  ],
  "type": "object"
}
```

### `lsp_definition`

Jump from a symbol to where it is defined, following the import that grep cannot: grep finds every file mentioning the name, the server finds the one this use means. Example: lsp_definition(symbol="LspConfig") — prefer symbol over path+line+column, you should not have to count characters. If the answer is [indexing], retry in a few seconds.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "column": {
      "description": "1-based character on that line; give it with line, not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "line": {
      "description": "1-based line; give it with column, and not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "path": {
      "description": "File to look in; with line and column it is the position.",
      "type": "string"
    },
    "symbol": {
      "description": "Symbol name; may be qualified, e.g. `Foo::bar` or `pkg.Foo`.",
      "type": "string"
    }
  },
  "required": [],
  "type": "object"
}
```

### `lsp_references`

List every use of a symbol, grouped by file, as only the compiler can: comments and strings are excluded, and uses across files and crates are found. This is the tool to reach for instead of grep before a rename or a deletion. Example: lsp_references(symbol="LspConfig::new"). If the answer is [indexing], retry shortly.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "column": {
      "description": "1-based character on that line; give it with line, not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "include_declaration": {
      "description": "Also list the declaration itself; default false.",
      "type": "boolean"
    },
    "limit": {
      "description": "Most references to list; default 100.",
      "maximum": 500,
      "minimum": 1,
      "type": "integer"
    },
    "line": {
      "description": "1-based line; give it with column, and not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "path": {
      "description": "File to look in; with line and column it is the position.",
      "type": "string"
    },
    "symbol": {
      "description": "Symbol name; may be qualified, e.g. `Foo::bar` or `pkg.Foo`.",
      "type": "string"
    }
  },
  "required": [],
  "type": "object"
}
```

### `lsp_hover`

Show the type and documentation of a symbol — the compiler's view, not the raw text grep would return: resolved signature, parameter types, doc comment. Example: lsp_hover(symbol="Config::load"). If you get [indexing], wait a few seconds and retry.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "column": {
      "description": "1-based character on that line; give it with line, not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "line": {
      "description": "1-based line; give it with column, and not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "path": {
      "description": "File to look in; with line and column it is the position.",
      "type": "string"
    },
    "symbol": {
      "description": "Symbol name; may be qualified, e.g. `Foo::bar` or `pkg.Foo`.",
      "type": "string"
    }
  },
  "required": [],
  "type": "object"
}
```

### `lsp_implementations`

Find the types that implement a trait or interface, or the methods that override one. grep cannot answer this: the spelling differs per language and the answer crosses files. Example: lsp_implementations(symbol="LspBackend"). If you get [indexing], retry in a few seconds.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "column": {
      "description": "1-based character on that line; give it with line, not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "line": {
      "description": "1-based line; give it with column, and not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "path": {
      "description": "File to look in; with line and column it is the position.",
      "type": "string"
    },
    "symbol": {
      "description": "Symbol name; may be qualified, e.g. `Foo::bar` or `pkg.Foo`.",
      "type": "string"
    }
  },
  "required": [],
  "type": "object"
}
```

### `lsp_outline`

List the symbols a file declares, nested by scope, with kinds and line numbers. Cheaper than reading the file and more accurate than grepping `fn`/`func`/`def`: it is the language's own parse tree. Example: lsp_outline(path="src/lib.rs"). If you get [indexing], retry in a few seconds.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "path": {
      "description": "File to outline, absolute or relative to the workspace root.",
      "type": "string"
    }
  },
  "required": [
    "path"
  ],
  "type": "object"
}
```

### `lsp_callers`

List the functions that call this one, as an indented tree — the question grep answers badly, since a comment naming it is not a caller. Follows `depth` levels up (1-3; default 1); each line is `name  path:line:column`, and a place already shown says `(see above)`. Use it before changing a signature. Example: lsp_callers(symbol="LspConfig::new"). If you get [indexing], retry in a few seconds.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "column": {
      "description": "1-based character on that line; give it with line, not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "depth": {
      "description": "How many levels to follow, 1 to 3; default 1.",
      "maximum": 3,
      "minimum": 1,
      "type": "integer"
    },
    "line": {
      "description": "1-based line; give it with column, and not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "path": {
      "description": "File to look in; with line and column it is the position.",
      "type": "string"
    },
    "symbol": {
      "description": "Symbol name; may be qualified, e.g. `Foo::bar` or `pkg.Foo`.",
      "type": "string"
    }
  },
  "required": [],
  "type": "object"
}
```

### `lsp_callees`

List the functions this one calls, as the language server resolved them and as an indented tree — calls, not the text grep would return. Follows `depth` levels down (1-3; default 1); each line is `name  path:line:column`. Use it to see what a function touches before you change it. Example: lsp_callees(symbol="main"). If you get [indexing], retry in a few seconds.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "column": {
      "description": "1-based character on that line; give it with line, not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "depth": {
      "description": "How many levels to follow, 1 to 3; default 1.",
      "maximum": 3,
      "minimum": 1,
      "type": "integer"
    },
    "line": {
      "description": "1-based line; give it with column, and not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "path": {
      "description": "File to look in; with line and column it is the position.",
      "type": "string"
    },
    "symbol": {
      "description": "Symbol name; may be qualified, e.g. `Foo::bar` or `pkg.Foo`.",
      "type": "string"
    }
  },
  "required": [],
  "type": "object"
}
```

### `lsp_diagnostics`

Report the compiler and linter errors and warnings for a file, with line and column. Use it right after you edit a file to check it still compiles — the server's verdict, not your guess. Example: lsp_diagnostics(path="src/lib.rs"). If it says the diagnostics are not known yet, ask again instead of assuming the file is clean.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "path": {
      "description": "File to diagnose, absolute or relative to the workspace root.",
      "type": "string"
    }
  },
  "required": [
    "path"
  ],
  "type": "object"
}
```

### `lsp_rename_preview`

Preview the whole-project edit for renaming a symbol, as a unified diff — nothing on disk changes. Better than grep+sed: exact references, no comments or strings, every affected file shown before you touch it. Example: lsp_rename_preview(symbol="LspConfig::new", new_name="build"). If you get [indexing], retry in a few seconds: a rename now could miss references.

Read-only (`readOnlyHint: true`).

Arguments (JSON Schema):

```json
{
  "additionalProperties": false,
  "properties": {
    "column": {
      "description": "1-based character on that line; give it with line, not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "line": {
      "description": "1-based line; give it with column, and not with symbol.",
      "minimum": 1,
      "type": "integer"
    },
    "new_name": {
      "description": "The new name: one identifier, no spaces, at most 200 characters.",
      "type": "string"
    },
    "path": {
      "description": "File to look in; with line and column it is the position.",
      "type": "string"
    },
    "symbol": {
      "description": "Symbol name; may be qualified, e.g. `Foo::bar` or `pkg.Foo`.",
      "type": "string"
    }
  },
  "required": [
    "new_name"
  ],
  "type": "object"
}
```

<!-- END GENERATED: tools -->

## Error and status codes

A tool result whose first line starts with `[code]` is machine-readable. With
the exception of `ambiguous` and `not_found`, the codes below are tool errors
(`isError: true`).

| Code | `isError` | Meaning |
| --- | --- | --- |
| `indexing` | true | The server is still indexing and the result is empty or untrustworthy; the message names the server and suggests retrying in a few seconds. |
| `ambiguous` | false | The name matched several candidates; the message lists them, each with the `path`+`line`+`column` to retry with. |
| `not_found` | false | The server clearly answered "nothing", and it is not indexing. |
| `no_server` | true | No configured server handles that file extension. |
| `server_not_installed` | true | The server command was not found; the message includes an install hint. |
| `server_failed` | true | The server used up its restart budget. |
| `timeout` | true | The request exceeded `request_timeout_ms`. |
| `outside_workspace` | true | The path is outside this connection's workspace boundary. |
| `capacity` | true | The instance limit is reached and nothing could be reclaimed. |
| `memory_restart` | true | The server was just restarted for using too much memory; retry. |
| `daemon_unavailable` | true | `opencraylspd` could not be reached; produced by `opencraylsp-client`. |
| `invalid_args` | true | Missing or mutually exclusive arguments. |
| `language_disabled` | true | The language exists but this connection did not enable it; the message lists the enabled languages and how to change `--languages`. |
| `no_project` | true | The request names no file, and the workspace boundary is not itself a project, so there was nothing to route it to. The message either says no project of that language was found under the boundary, or lists the projects it did find, each with a `path` to retry with. |
| `cancelled` | true | The request was cancelled. |
| `unsupported` | true | The server does not implement that LSP method (`-32601`). |
| `not_renamable` | true | There is no renameable symbol at that position. The message prints the position back and, when the file can be read, the identifiers on that line with their columns — a `column` beside an identifier rather than on it is the usual cause. A language server's own `InvalidParams` (“No references found at position” and similar) is answered this way; an `InvalidParams` that says only `invalid params` stays an `rpc_error`, because then the server really is complaining about the arguments. |
| `not_implemented` | true | The tool is still being built (development only). |
| `rpc_error` | true | The server returned a JSON-RPC error other than `-32601`; includes the server, code and message. |
| `io_error` | true | Reading a file or the transport failed. |
| `invalid_response` | true | The server's reply did not fit the expected shape. |
| `internal_error` | true | An internal invariant broke; please report it. |

## Relation to `opencraylspd`

The tools never talk to a language server directly. `opencraylsp-mcp` forwards each
call to `opencraylspd`, which owns the language-server processes and pools them across
clients; see [PROTOCOL.md](PROTOCOL.md) and [CONFIGURATION.md](CONFIGURATION.md).
