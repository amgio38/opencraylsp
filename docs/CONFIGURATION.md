# Configuration

`opencraylspd` reads one TOML file. Unknown fields are rejected with the offending line,
so a typo fails loudly instead of being ignored.

## Where the file lives

| Source | Location |
| --- | --- |
| `--config <path>` | Explicit; must exist. On `opencraylspd serve` and on `opencraylsp-mcp` (which forwards it to a daemon it starts). |
| `$XDG_CONFIG_HOME/opencraylsp/config.toml` | Used when the variable is set and non-empty. |
| `~/.config/opencraylsp/config.toml` | Default. |
| `OPENCRAYLSP_CONFIG` | Environment fallback for `opencraylsp-mcp --config`. |

A missing default file simply means "built-in presets and defaults". A missing
`--config` file is an error.

## `[limits]`

| Field | Default | Meaning |
| --- | --- | --- |
| `max_instances` | `8` | Most language-server instances alive at once. A new request first reclaims the least-recently-used idle instance; if all are busy it waits up to 10 s, then answers `[capacity]`. |
| `max_rss_mb` | `8192` | Per-instance memory ceiling for the whole process tree. Sampled every `memory_sample_ms`; over the limit the instance is drained and restarted. While a server is indexing the ceiling doubles. Three restarts within an hour mark it `failed` and refuse to run it again. The default was sized from a measured rust-analyzer run on a large workspace (peak 7.5 GiB while indexing, 5.1 GiB steady), so the margin over the peak is small and relies on the doubled ceiling during indexing; raise it for very large projects and lower it on small machines. |
| `idle_shutdown_secs` | `900` | An instance with no request for this long is shut down. `0` disables idle reclaim. |
| `max_open_docs` | `256` | Open documents kept per instance, evicted least-recently-used. |
| `request_timeout_ms` | `30000` | Per-request timeout. |
| `startup_timeout_ms` | `60000` | How long a server may take to start. |
| `startup_grace_ms` | `3000` | After start, a server that has reported no progress yet still counts as indexing. `0` disables the grace period. |
| `max_restarts` | `3` | Crashes before a server is refused. |
| `diagnostics_settle_ms` | `1500` | Quiet period required after a `publishDiagnostics` before the diagnostics answer is trusted. |
| `diagnostics_timeout_ms` | `20000` | Upper bound for waiting on diagnostics. |
| `memory_sample_ms` | `5000` | How often memory is sampled. |
| `daemon_max_rss_mb` | `512` | Ceiling on the daemon's **own** resident memory — the daemon process alone, with no language servers included — sampled on the same `memory_sample_ms` tick. Over the limit the daemon stops accepting new connections and requests, gives in-flight requests up to 30 s to finish, shuts every language server down, removes its socket and exits 0; the next client request starts a fresh daemon. This is a runaway guard, not a working budget — an idle daemon measures around 6.7 MB, so 512 MiB only trips on a leak. It exists because `max_rss_mb` governs the language servers, and a leak inside the daemon is invisible to it. Raise it if a legitimate workload is larger; lower it to catch a leak earlier. |
| `max_results` | `100` | Cap on listed references and symbols; the rest is summarized as a count. |

The two ceilings measure different processes on purpose. `max_rss_mb` reads an
instance's whole process tree (rust-analyzer's proc-macro servers are its
children); `daemon_max_rss_mb` reads the daemon by itself. Adding the servers
to the daemon's figure would charge the same memory to two ceilings at once and
would make one rust-analyzer indexing a large workspace — several GiB, with a
daemon weighing about ten MiB — look like a runaway daemon.

### The daemon restarting itself

Because the client starts a new daemon on demand, a daemon that exits over its
own ceiling is immediately replaced by a fresh one — which would leak, exceed
the ceiling and exit again, forever. To stop that, each over-limit exit is
recorded in `<socket>.rss-over-limit` (mode `0600`, beside the socket), and the
count is a sliding one-hour window.

Two over-limit exits in an hour are allowed; the **third** is refused. A refused
daemon keeps serving, logs the refusal at most once a minute, and says so in
`lsp_status`. An hour after the last recorded exit the budget is full again. A
damaged or unreadable stamp file is treated as no history rather than as a
refusal, because trading a restart for a permanent refusal would be the worse
failure.

## Top-level fields

| Field | Default | Meaning |
| --- | --- | --- |
| `allowed_roots` | `[]` | Extra directories, besides the workspace boundary, whose files may be opened. |
| `warmup` | `false` | Start discovered `(server, project root)` pairs in the background so indexing is underway before the first question. |
| `watch_interval_ms` | `3000` | How often running servers are told about files changed on disk (`workspace/didChangeWatchedFiles`). `0` disables the watcher. |
| `warmup_max_depth` | `4` | How deep below the boundary warm-up looks for project root markers. A request that names no file (`lsp_find_symbol` with no `path`) looks the same way when it has to pick a project; see [A request with no file](TOOLS.md#a-request-with-no-file). |
| `warmup_max_instances` | `16` | Upper bound on servers warm-up starts. |
| `warmup_exclude` | `[]` | Extra directory names skipped by warm-up and the watcher, on top of the built-ins (hidden directories, `node_modules`, `target`, `vendor`, `dist`, `build`). |

## `[[server]]` entries

Each `[[server]]` adds or overrides a language server. A user entry with the same
`name` as a built-in preset replaces that preset wholesale.

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | yes | Server name; also what `lsp_status` shows. |
| `command` | yes | Executable. Looked up on `PATH` when not absolute. |
| `args` | no | Arguments passed to `command`. |
| `env` | no | Extra environment for the child process. Values set here are passed as written; credential-looking variables inherited from the daemon (`*TOKEN*`, `*SECRET*`, `*PASSWORD*`, `SSH_AUTH_SOCK`, ...) are not passed unless listed here. See SECURITY.md. |
| `extensions` | yes | File extension (without the dot) to LSP `languageId`, e.g. `rs = "rust"`. |
| `root_markers` | no | Files whose presence marks a project root (`Cargo.toml`, `go.mod`, …). The topmost directory at or below the boundary wins. Empty means "the boundary itself". |
| `workspace` | no | Overrides the workspace boundary for this server only. |
| `initialization_options` | no | Sent verbatim as LSP `initializationOptions`. |
| `settings` | no | Returned for `workspace/configuration` requests. |

## Built-in presets

A preset is available when its `command` is found on `PATH`.

| Name | Command | Args | Extensions | Root markers |
| --- | --- | --- | --- | --- |
| `rust-analyzer` | `rust-analyzer` | | `rs` → `rust` | `Cargo.toml` |
| `gopls` | `gopls` | | `go` → `go` | `go.work`, `go.mod` |
| `intelephense` | `intelephense` | `--stdio` | `php` → `php` | `composer.json` |
| `typescript-language-server` | `typescript-language-server` | `--stdio` | `ts`, `mts`, `cts` → `typescript`; `tsx` → `typescriptreact`; `js`, `mjs`, `cjs` → `javascript`; `jsx` → `javascriptreact` | `tsconfig.json`, `jsconfig.json`, `package.json` |
| `pyright-langserver` | `pyright-langserver` | `--stdio` | `py`, `pyi` → `python` | `pyproject.toml`, `setup.py`, `requirements.txt` |

`rust-analyzer` is preset with `initialization_options = { files = { watcher =
"server" } }`, so its own watcher keeps the index fresh for edits made by other
tools.

## Overriding a preset

The examples below change one language each. `opencraylspd doctor` reports whether each
command is installed and prints an install hint when it is not.

### Rust

```toml
[[server]]
name = "rust-analyzer"
command = "rust-analyzer"
root_markers = ["Cargo.toml"]
[server.extensions]
rs = "rust"
[server.initialization_options.files]
watcher = "server"
```

### Go

```toml
[[server]]
name = "gopls"
command = "gopls"
root_markers = ["go.work", "go.mod"]
[server.extensions]
go = "go"
```

### Python

```toml
[[server]]
name = "pyright-langserver"
command = "pyright-langserver"
args = ["--stdio"]
root_markers = ["pyproject.toml", "setup.py", "requirements.txt"]
[server.extensions]
py = "python"
pyi = "python"
```

### PHP

```toml
[[server]]
name = "intelephense"
command = "intelephense"
args = ["--stdio"]
root_markers = ["composer.json"]
[server.extensions]
php = "php"
```

### TypeScript / JavaScript

A global TypeScript 7 installation has no bundled `tsserver.js`, so point the
server at a 5.x one explicitly:

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

## A complete example

```toml
[limits]
idle_shutdown_secs = 300
max_instances = 4
max_rss_mb = 4096

warmup = false
allowed_roots = ["/opt/shared/vendor"]

[[server]]
name = "rust-analyzer"
command = "rust-analyzer"
root_markers = ["Cargo.toml"]
[server.extensions]
rs = "rust"
```

## Languages and aliases

A connection enables languages with `opencraylsp-mcp --languages <list>`,
`OPENCRAYLSP_LANGUAGES`, or the default `auto` (detect from project markers). `all`
enables every language a configured server provides.

| Canonical | Aliases |
| --- | --- |
| `rust` | `rs` |
| `go` | `golang` |
| `php` | |
| `typescript` | `ts` |
| `javascript` | `js` |
| `python` | `py` |
| `typescript` + `javascript` | `tsjs`, `ts/js`, `js/ts`, `node`, `nodejs` |

`auto` and `all` are also accepted. Names are case-insensitive and
comma-separated; an unknown name makes `opencraylsp-mcp` exit with code 2 and a message
listing the valid names.

`opencraylsp-mcp` checks the names against the config **it** can see (`--config`, else
the default path) before connecting, so a typo is rejected without starting a
daemon. A name that exists only in the config of an already-running daemon —
for example one started with a different `--config` — is rejected there even
though that daemon would accept it; point the client at the same config file.
