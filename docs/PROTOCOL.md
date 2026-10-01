# Daemon protocol v1

`opencraylsp-mcp` talks to `opencraylspd` over a Unix domain socket. This document is the
external contract; the daemon's implementation lives in `opencraylspd` and the shared
types in `opencraylsp-proto`.

## Transport

- A Unix domain socket. The default path is
  `$XDG_RUNTIME_DIR/opencraylsp/opencraylsp.sock`, or `/tmp/opencraylsp-<uid>/opencraylsp.sock` when
  `XDG_RUNTIME_DIR` is unset. Override with `--socket` or `OPENCRAYLSP_SOCKET`.
  When you override it, the socket's parent directory must already exist, be
  owned by your user, and have mode `0700`; `opencraylspd serve` refuses to start
  otherwise (it exits 1 and writes the reason to its log file, not to stderr,
  because a daemon started by a client has no terminal).
- One UTF-8 JSON-RPC 2.0 message per line. A single line carries no unescaped
  newline.
- One line is at most **4 MiB**. A longer line is answered with `-32700` and the
  connection is closed. That limit is the socket's, and it applies in both
  directions. The separate stdio hop between your harness and `opencraylsp-mcp`
  has its own, larger limit of **8 MiB** for a request line; the answers are
  capped much lower than either — a tool returns at most 256 KiB of text — so a
  reply always fits the 4 MiB socket line.
- Several requests may be in flight on one connection; replies may arrive in any
  order and are matched by `id`.

The socket directory is created `0700` and the socket `0600`, and the daemon
checks the connecting uid with `SO_PEERCRED`. See [SECURITY.md](../SECURITY.md).

## Handshake

`hello` must be the first request on a connection. Any other method before it is
answered with `-32002 not_initialized`. `hello` binds the connection to a
workspace and declares the languages it wants:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "hello",
  "params": {
    "protocol": 1,
    "client": { "name": "opencraylsp-mcp", "version": "0.1.0" },
    "workspace": "/home/user/project",
    "languages": ["rust", "go"]
  }
}
```

`languages` is raw user input (`rust`, `ts`, `all`, …); the daemon normalizes
aliases and validates names. Omitting it, or sending an empty list, means
`auto` (detect from the workspace's project markers). The reply reports the
languages that are actually enabled:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocol": 1,
    "daemon_version": "0.1.0",
    "pid": 12345,
    "languages": ["rust", "go"],
    "language_mode": "declared"
  }
}
```

## Methods

| Method | Params | Result |
| --- | --- | --- |
| `hello` | `{protocol, client: {name, version}, workspace, languages?}` | `{protocol, daemon_version, pid, languages, language_mode}` |
| `tools/list` | `{}` | `{tools: [ToolDef]}` |
| `tools/call` | `{name, arguments}` | `ToolOutput {text, is_error}` |
| `status` | `{}` | `StatusReport` |
| `shutdown` | `{}` | `{}`, then the daemon shuts down gracefully |
| `$/cancel` (notification) | `{id}` | — |

`ToolDef` is `{name, description, input_schema, annotations?}`.
`ToolOutput` is `{text, is_error}`. A tool failure is **not** a JSON-RPC error:
it is a successful result with `is_error: true` whose `text` starts with a
bracketed code (see [TOOLS.md](TOOLS.md#error-and-status-codes)). JSON-RPC
errors are only for protocol problems.

`StatusReport`:

```json
{
  "daemon": { "version": "0.1.0", "pid": 12345, "uptime_secs": 42,
              "rss_bytes": 1048576, "clients": 2 },
  "limits": { "max_instances": 8, "max_rss_mb": 8192,
              "idle_shutdown_secs": 900, "max_open_docs": 256 },
  "enabled_languages": ["rust"],
  "language_mode": "declared",
  "not_installed": [],
  "instances": [
    { "server": "rust-analyzer", "root": "/home/user/project", "state": "ready",
      "pid": 12346, "rss_bytes": 536870912, "idle_secs": 3, "restarts": 0,
      "memory_restarts": 0, "open_docs": 4, "indexing": null }
  ]
}
```

`state` is one of `starting`, `indexing`, `ready`, `restarting`, `failed`,
`stopped`.

The two `rss_bytes` fields measure different things. The daemon's is the daemon
process alone, judged against `daemon.max_rss_mb`; an instance's is the whole
process tree under that server (its proc-macro helpers included), judged against
`limits.max_rss_mb`. A daemon supervising a multi-gigabyte rust-analyzer
therefore reports a small `daemon.rss_bytes` next to large instance figures.

## Errors

| Code | Name | When |
| --- | --- | --- |
| `-32700` | parse error | A line was not valid JSON, or exceeded the socket's 4 MiB. |
| `-32600` | invalid request | Not a JSON-RPC 2.0 request object. |
| `-32601` | method not found | Unknown method. |
| `-32602` | invalid params | Malformed params for the method. |
| `-32001` | protocol_mismatch | The client speaks a different protocol version. `data: {supported: [1]}`. |
| `-32002` | not_initialized | A method other than `hello` was sent before `hello`. |
| `-32003` | workspace_invalid | The workspace path does not exist or is not a directory. |
| `-32004` | shutting_down | The daemon is going away; reconnect. |
| `-32005` | unknown_language | `hello.languages` named a language the daemon does not know. `data: {valid: [...]}`. |

Closing a connection cancels all of its in-flight requests; the language-server
instances stay up for other clients.

## Example transcript

`initialize` is MCP; below is the daemon side of one session (`>` client, `<` daemon):

```
> {"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol":1,"client":{"name":"opencraylsp-mcp","version":"0.1.0"},"workspace":"/home/user/app","languages":["rust"]}}
< {"jsonrpc":"2.0","id":1,"result":{"protocol":1,"daemon_version":"0.1.0","pid":12345,"languages":["rust"],"language_mode":"declared"}}
> {"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
< {"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"lsp_status","description":"...","input_schema":{"type":"object"}}]}}
> {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lsp_hover","arguments":{"symbol":"parse_config"}}}
< {"jsonrpc":"2.0","id":3,"result":{"text":"fn parse_config(path: &Path) -> Config","is_error":false}}
> {"jsonrpc":"2.0","id":4,"method":"status","params":{}}
< {"jsonrpc":"2.0","id":4,"result":{"daemon":{"version":"0.1.0","pid":12345,"uptime_secs":42,"rss_bytes":1048576,"clients":1},"limits":{"max_instances":8,"max_rss_mb":8192,"idle_shutdown_secs":900,"max_open_docs":256},"enabled_languages":["rust"],"language_mode":"declared","not_installed":[],"instances":[]}}
> {"jsonrpc":"2.0","id":5,"method":"shutdown","params":{}}
< {"jsonrpc":"2.0","id":5,"result":{}}
```

A cancellation is a notification and has no reply:

```
> {"jsonrpc":"2.0","method":"$/cancel","params":{"id":3}}
```

## Lifecycle

- The client starts `opencraylspd serve` when the socket cannot be reached and no other
  daemon holds the lock, retrying with 50/100/200/400/800/1600 ms back-off up to
  8 s. It never silently falls back to an in-process pool.
- A daemon holds an `flock` on `<socket>.lock` for its lifetime; a second
  `opencraylspd serve` exits quietly.
- On `shutdown` (or SIGTERM) the daemon stops accepting, cancels in-flight
  requests, shuts every instance down (each within 5 s, then killed), removes
  the socket, and exits.
- If the daemon dies mid-request, the client answers `[daemon_unavailable]
  connection lost` and reconnects (once) on the next call.
