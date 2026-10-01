# openCraylsp — one language-server pool for all your coding agents

Coding agents get real semantic tools — definitions, references, call
hierarchies, diagnostics — by driving real language servers. Those servers are
expensive: a single rust-analyzer can hold several gigabytes. `opencraylspd` runs one
daemon per machine and pools them, so three agents in the same project share
**one** rust-analyzer instead of starting three, and idle or runaway servers are
reclaimed automatically.

**What you get:** `opencraylsp-mcp`, a stdio MCP server exposing eleven read-only
`lsp_*` tools, backed by `opencraylspd`, a shared daemon that starts language servers on
demand and keeps them within memory and idle limits.

## Install in 30 seconds

```sh
curl -fsSL https://raw.githubusercontent.com/amgio38/opencraylsp/main/scripts/install.sh | sh
claude mcp add opencraylsp -- opencraylsp-mcp --languages auto
```

### Platforms

| Platform | How |
| --- | --- |
| Linux x86-64, aarch64 | The command above downloads a prebuilt static binary. No Rust needed. |
| Other Linux CPUs, macOS | The same command builds from source (needs Rust 1.89+ and a C compiler; on macOS run `xcode-select --install` first). |
| Windows | Run it inside **WSL2** (below). A native Windows build is not available: the daemon talks over Unix sockets. |

### Windows (WSL2)

From PowerShell, once (this installs Ubuntu and asks for a reboot):

```powershell
wsl --install -d Ubuntu
```

Then open the **Ubuntu** app and install exactly as on Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/amgio38/opencraylsp/main/scripts/install.sh | sh
```

Run your coding agent inside WSL as well, and keep your projects on the Linux side
(`~/projects`, not `/mnt/c/...`): file watching and language servers are much faster
there, and the paths the tools report then match what the agent sees.

The first line installs `opencraylspd` and `opencraylsp-mcp` into `~/.local/bin`, the second
registers the server with Claude Code. For Cursor, opencode, other clients, system-wide installs and
language-server prerequisites, see [docs/INSTALL.md](docs/INSTALL.md) and
[docs/CLIENTS.md](docs/CLIENTS.md).

`opencraylsp-mcp` starts connecting to `opencraylspd` in the background as soon as it runs, and
starts the daemon itself when none is running, so the first tool call finds it
ready. `initialize`, `ping` and `tools/list` answer without waiting for that
connection; only a real `tools/call` waits for it (and reports
`[daemon_unavailable]` if the daemon cannot be reached). Check the daemon any
time with:

```sh
opencraylspd status
opencraylspd doctor
```

## How it works

```
agent A ──stdio── opencraylsp-mcp ─┐
agent B ──stdio── opencraylsp-mcp ─┼── unix socket ── opencraylspd ──┬── rust-analyzer (project X)
agent C ──stdio── opencraylsp-mcp ─┘                         ├── gopls         (project Y)
                                                     └── ...
```

Each connection declares the languages it wants; the daemon starts a server only
when a connection has enabled it, and reuses an instance already serving the
same `(server, project root)`. When nothing asks for an instance for
`idle_shutdown_secs`, it is shut down.

## Choosing languages

```sh
opencraylsp-mcp --languages rust          # only Rust
opencraylsp-mcp --languages ts,js         # TypeScript + JavaScript (one server)
opencraylsp-mcp --languages go,php        # Go + PHP
opencraylsp-mcp                           # auto: detect from project markers
opencraylsp-mcp --languages all           # every configured language
```

`--languages` beats `OPENCRAYLSP_LANGUAGES`. Names are comma-separated and
case-insensitive; `auto` (the default) detects languages from project files
(`Cargo.toml`, `go.mod`, `tsconfig.json`, …). A tool called on a file whose
language this connection did not enable answers `[language_disabled]` with the
enabled list instead of silently doing nothing.

## Tools

| Tool | Purpose |
| --- | --- |
| `lsp_status` | What the daemon and its instances are doing. |
| `lsp_find_symbol` | Find symbols by name; `path` scopes it to a file or directory, exact name matches rank first. |
| `lsp_definition` | Jump to the definition of a symbol. |
| `lsp_references` | Every use of a symbol, grouped by file. |
| `lsp_hover` | The type and documentation of a symbol. |
| `lsp_implementations` | Types that implement a trait or interface. |
| `lsp_outline` | The symbols a file declares, nested by scope. |
| `lsp_callers` | Functions that call this one, as a tree. |
| `lsp_callees` | Functions this one calls, as a tree. |
| `lsp_diagnostics` | Compiler and linter errors for a file. |
| `lsp_rename_preview` | A unified diff for renaming a symbol (never writes files). |

Every tool is read-only; `lsp_rename_preview` returns a diff for you to apply.
Full arguments and outputs are in [docs/TOOLS.md](docs/TOOLS.md).

## Resource control

Tunable in `~/.config/opencraylsp/config.toml`; see
[docs/CONFIGURATION.md](docs/CONFIGURATION.md) for the whole file.

| Setting | Default | Effect |
| --- | --- | --- |
| `max_instances` | 8 | Most language-server instances alive at once. |
| `max_rss_mb` | 8192 | Per-instance memory ceiling (whole process tree). |
| `idle_shutdown_secs` | 900 | Reclaim an instance after this long without a request. |
| `max_open_docs` | 256 | Open documents kept per instance (LRU). |
| `request_timeout_ms` | 30000 | Per-request timeout. |
| `startup_timeout_ms` | 60000 | How long a server may take to start. |
| `max_restarts` | 3 | Crashes before a server is refused. |

## FAQ

**Do I need to run `opencraylspd` myself?** No. The first `opencraylsp-mcp` starts it. Use
`opencraylspd serve` only to watch one in the foreground.

**Where is the log?** `$XDG_STATE_HOME/opencraylsp/opencraylsp.log`, else
`~/.local/state/opencraylsp/opencraylsp.log`. Most `[daemon_unavailable]` messages already
include its tail.

**A tool answered `[indexing]`.** The server is still building its index; wait a
few seconds and retry. First run on a large project is the slow one.

**Which marker did I just get?** All of them are listed with causes and fixes in
[docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md).

**Can I skip the daemon?** Yes, `opencraylsp-mcp --embedded` runs the pool in-process.
You lose sharing between agents, so prefer the daemon.

## Repository layout

| Crate | Role |
| --- | --- |
| `opencraylspd` | The daemon and its CLI (`serve`, `status`, `stop`, `restart`, `doctor`, `version`). |
| `opencraylsp-mcp` | The MCP stdio server, with an in-process `--embedded` mode. |
| `opencraylsp-core` | Language-server transport, instances, document sync and the pool. |
| `opencraylsp-proto` | Wire types shared by the daemon, the client and the tools. |
| `opencraylsp-tools` | The `lsp_*` tool catalogue, symbol resolution and formatting. |
| `opencraylsp-client` | The daemon connection, autospawn and reconnect logic. |
| `opencraylspd-e2e` | End-to-end tests: real `opencraylspd` + real `opencraylsp-mcp` + a fake language server. |

Build and test with `make ci` (fmt, clippy, tests) and `make docs-check`
(link and generated-doc checks). See [CONTRIBUTING.md](CONTRIBUTING.md) for the
conventions and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the crates
fit together and why.

## License

MIT. See [LICENSE](LICENSE) and [SECURITY.md](SECURITY.md).

Dependencies are under their own licenses: see
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md) for the crate-by-crate
inventory and [NOTICE](NOTICE) for the Apache-2.0 attribution. The license texts
those require are in [`licenses/`](licenses/). `deny.toml` is the policy and
`make deny` is the gate -- CI runs it on every push.

An installed copy carries all of it: `install.sh` and `make install` put this set
under `$PREFIX/share/doc/opencraylsp`, the release archive carries it so there is
something to install, and both uninstall paths remove it.

The crates in this workspace are marked `publish = false` in their
`Cargo.toml`. That is deliberate: they are released as the `opencraylspd` and `opencraylsp-mcp`
binaries, not as a library for others to depend on, and the protocol between
them moves together. It also means `cargo install opencraylspd` from crates.io will not
work and is not intended to -- use the installer above, or build from a checkout
with `cargo build --release -p opencraylspd -p opencraylsp-mcp`.
