# Troubleshooting

Every tool answer that starts with a bracketed marker such as
`[server_not_installed]` is machine-readable: the first token names the cause.
This page lists them all, what produces them, and what to do.

Start with the two commands that answer most questions:

```sh
opencraylspd status      # is the daemon up, what instances exist, what are they doing
opencraylspd doctor      # is each language server installed, and which languages auto-enable here
```

`opencraylspd status --json` and `opencraylspd doctor --json` are the versions for scripts:
their stdout is JSON. When no daemon is running, `opencraylspd status --json` prints
`{"running": false, …}` and exits non-zero, with the sentence on stderr.

`opencraylsp-mcp` starts connecting to the daemon in the background as soon as it runs
(and starts `opencraylspd` itself when none is running). `initialize`, `ping` and
`tools/list` answer without waiting for that connection; only a real
`tools/call` waits for it, and it is where `[daemon_unavailable]` appears if no
daemon answers.

## What lives where

| Thing | Location |
| --- | --- |
| Programs | `~/.local/bin/opencraylspd`, `~/.local/bin/opencraylsp-mcp` (see [INSTALL.md](INSTALL.md)) |
| Config | `$XDG_CONFIG_HOME/opencraylsp/config.toml`, else `~/.config/opencraylsp/config.toml` |
| Socket | `$OPENCRAYLSP_SOCKET`, else `$XDG_RUNTIME_DIR/opencraylsp/opencraylsp.sock`, else `/tmp/opencraylsp-<uid>/opencraylsp.sock` |
| Lock | the socket path plus `.lock` |
| Daemon log | `$XDG_STATE_HOME/opencraylsp/opencraylsp.log`, else `~/.local/state/opencraylsp/opencraylsp.log` |

Only one file is written for you to read: the log. Configuration is read from
the path above; nothing rewrites it.

## Reading the log

```sh
tail -n 50 "${XDG_STATE_HOME:-$HOME/.local/state}/opencraylsp/opencraylsp.log"
```

The daemon logs there, not to a terminal, because it is normally started in the
background by the first `opencraylsp-mcp`. When `opencraylsp-mcp` cannot reach a daemon it
started, the `[daemon_unavailable]` message includes the log path and the last
lines, so the usual case needs no manual `tail`.

To watch a daemon in the foreground instead, with logs on stderr:

```sh
opencraylspd serve
```

## `opencraylspd doctor`

For every configured server `doctor` prints installed/missing, the resolved
path, the first line of `--version` (or `unknown` when the server has no usable
version output, and `<timeout>` when it did not answer in time), the languages
and extensions it serves, and an install hint when it is missing. It then prints
the socket, whether a daemon is running, the config path and whether it exists,
the workspace, and the languages `auto` would enable there.

`doctor` never starts a language server: the only process it runs is
`<command> --version`, under a 10-second timeout. A server shown as `unknown`
(but `installed`) is fine; so is a slow one shown as `<timeout>`.

## Tool markers

The first line of any tool answer is `[marker] one English sentence`. The
following are the markers this project defines.

### Errors

| Marker | Cause | What to do |
| --- | --- | --- |
| `[no_server]` | No configured server serves the file, or the named server is unknown. | Check the extension is covered by a `[[server]]` (`opencraylspd doctor`) and enable its language. |
| `[server_not_installed]` | The server for the file's language is not on `PATH`. | Install it — `opencraylspd doctor` prints the hint — and restart the agent so it inherits the new `PATH`. |
| `[server_failed]` | The server crashed or failed to start; after `max_restarts` within an hour it is refused. | Read the log; run the server's own command by hand. Fix the cause, then `opencraylspd restart`. |
| `[timeout]` | The request outlived `request_timeout_ms`, or the daemon did not answer within the client's deadline (the daemon may be stuck). | Retry once; if it repeats, `opencraylspd status` to see the instance, then `opencraylspd restart`. |
| `[cancelled]` | The client cancelled the request (for example the agent aborted). | Retry if the answer is still wanted. |
| `[outside_workspace]` | The path is outside the workspace boundary and any `allowed_roots`. | Restart the agent from the project root, pass `--workspace`, or add the directory to `allowed_roots`. |
| `[unsupported]` | The server answered JSON-RPC `MethodNotFound` for this call. | Use another server for that language, or another tool; retrying the same server will not help. |
| `[rpc_error]` | Any other JSON-RPC error from the language server. | The sentence carries the server's own message; often a project or configuration problem inside the server. |
| `[indexing]` | The server is still building its index, so an empty result would be a lie. | Wait a few seconds and retry. Large Rust projects can take minutes on first run. |
| `[capacity]` | `max_instances` are all busy and a waiting request hit the limit. | Retry shortly; raise `max_instances`, or lower `idle_shutdown_secs` so idle servers are reclaimed sooner. |
| `[memory_restart]` | An instance crossed `max_rss_mb` and was restarted. | Retry; if frequent, raise `max_rss_mb` or narrow the workspace. |
| `[language_disabled]` | The connection did not enable the language of this file. | Start `opencraylsp-mcp` with `--languages` including it (or `all`); the daemon itself does not restart. |
| `[io_error]` | Reading a file failed. | Check the path exists and is readable by the daemon's user. |
| `[daemon_unavailable]` | `opencraylsp-mcp` cannot reach or start `opencraylspd`. | `opencraylspd status`, then `opencraylspd serve` to watch it start. The message includes the log tail. |
| `[daemon_untrusted]` | The socket is served by a process that does not run as your user (or your uid could not be determined), so the client refused to talk to it. | Check who owns the socket directory with `ls -ld` on its parent; remove it, or set `OPENCRAYLSP_SOCKET` to a path you own. |
| `[invalid_args]` | A tool argument was missing or malformed. | Read the sentence; it names the argument. |
| `[invalid_response]` | The language server's reply could not be parsed. | Update the server; if it persists, report it with the tool call and the log. |
| `[not_renamable]` | `lsp_rename_preview` was asked about something that cannot be renamed (a keyword, macro expansion, …). | Rename from a constant or definition site, or rename by hand. |
| `[internal_error]` | A bug in this project: a handler panicked or the request was dropped. | Retry; if it repeats, please open an issue with the log lines around it. |
| `[not_implemented]` | A development-only code path. | Should never appear in a release build; report it if it does. |

### Not errors

These two are successful answers whose text begins with a marker, so an agent
can branch on them:

| Marker | Meaning | What to do |
| --- | --- | --- |
| `[not_found]` | The server looked and found nothing. | Check spelling; if the symbol is in an as-yet-unindexed file, retry after `[indexing]` clears. |
| `[ambiguous]` | The name matches several symbols and the tool will not guess. | Call again with `path`, `line` and `column` of the one you mean. |

## Common pitfalls

### TypeScript: `[server_failed]` or no results

`typescript-language-server` needs a 5.x `tsserver.js`. A global TypeScript 7
installation no longer ships one, so point the server at a 5.x copy:

```toml
[[server]]
name = "typescript-language-server"
command = "typescript-language-server"
args = ["--stdio"]
root_markers = ["tsconfig.json", "package.json"]
[server.extensions]
ts = "typescript"
js = "javascript"
[server.initialization_options]
tsserver = { path = "/usr/local/lib/node_modules/typescript/lib/tsserver.js" }
```

### PHP: some tools never work, and diagnostics can time out

`intelephense` does not implement call hierarchy, `textDocument/implementation`
or rename (its `initialize` answer has no `callHierarchyProvider`, and
`implementationProvider` and `renameProvider` are `false`). For PHP,
`lsp_callers`, `lsp_callees`, `lsp_implementations` and `lsp_rename_preview`
therefore cannot return results; the same tools work on Go, Rust and TypeScript.
This is the server's limit, not a fault of `opencraylspd`.

Two more intelephense habits to know about:

- `lsp_hover` and `lsp_references` given only a `symbol` are asked at the
  declaration, where intelephense answers with nothing. Pass `path`+`line`+`column`
  of a place that uses the symbol instead; the miss says so.
- `lsp_diagnostics` can time out on a large tree that includes `vendor/`: the
  server publishes diagnostics only after it has indexed. The answer says it is
  not a statement that the file is clean; ask again once indexing has finished.

### Rust: rust-analyzer is missing or the wrong version

`rustup component add rust-analyzer` installs it for the **default** toolchain.
If a project pins another toolchain and `rustup` has no default set,
`rust-analyzer` may not be on `PATH` at all. Either set a default
(`rustup default stable`) or add the component to the pinned toolchain
(`rustup component add rust-analyzer --toolchain 1.95.0`).

### The socket cannot be created

`opencraylspd serve` creates the socket's parent directory if it is missing, with mode
`0700` (every level it has to create). It refuses only a directory that already
exists and is owned by a **different user**; that case exits 1 with the reason
in the log, not on stderr. A world-writable directory such as `/tmp` is fine —
putting `--socket /tmp/opencraylsp.sock` there works, because the defenses are the
socket's own `0600` mode and the daemon's peer-uid check, not the directory.
Directory permissions are not the thing to debug here: if `opencraylspd` exits 1, read
the log for the reason (see [Reading the log](#reading-the-log)).

### A cold start looks like a hang

The first request against a large project waits for the server to index. Until
it reports ready, tools answer `[indexing]` instead of an empty result. Watch
progress with `opencraylspd status`; after the first run the index is cached by the
server and later starts are fast.

### Everything is slow after editing files outside the agent

The daemon tells running servers about changes on disk every
`watch_interval_ms`. Set `watch_interval_ms = 0` to disable it, or narrow the
workspace so fewer files are watched.

### Too much memory

Instances are capped at `max_rss_mb` and reclaimed after `idle_shutdown_secs`.
Lower both if a machine is shared; see
[CONFIGURATION.md](CONFIGURATION.md#limits) for every knob.
