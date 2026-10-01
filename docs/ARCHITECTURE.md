# Architecture

This document explains how `opencraylspd` is put together and, more importantly, *why*
it is put together that way. It is written for someone who wants to change the
code, not for someone who wants to use it: for usage, see the
[README](../README.md), [docs/TOOLS.md](TOOLS.md) and
[docs/CONFIGURATION.md](CONFIGURATION.md).

The normative behaviour lives in the code and its tests. Where this document and
the code disagree, the code wins — please open an issue.

## 1. The problem

Language servers are the best available answer to "where is this symbol
defined, who calls it, does this file compile", but they are also heavy: a
single `rust-analyzer` on a mid-sized Rust workspace settles around 5 GiB of
resident memory and takes minutes to index. A machine that runs several coding
agents would start one server per agent, per project, and exhaust itself long
before it ran out of work to do.

`opencraylspd` exists to make *N* agents share *one* set of language servers, with the
resource policy applied in one place. It is deliberately split in two:

* `opencraylspd` — a per-machine daemon. It is the only process that owns language
  server processes.
* `opencraylsp-mcp` — a stateless MCP stdio server. It speaks MCP to the agent and the
  daemon protocol to `opencraylspd`, and starts a daemon if one is not running.

Agents therefore never talk to a language server directly, and never need to
know that one is being shared.

```
agent A ──stdio── opencraylsp-mcp ─┐
agent B ──stdio── opencraylsp-mcp ─┼── unix socket ── opencraylspd ──┬── rust-analyzer (project X)
agent C ──stdio── opencraylsp-mcp ─┘                         ├── gopls        (project Y)
                                                     └── ...
```

The top line is a real simplification: there is no broker, no registry and no
supervisor above the daemon. `opencraylsp-mcp` connects, and if the socket is not there
it spawns the daemon and retries (see [Failure semantics](#8-failure-semantics)).
A single daemon per machine is enforced by an exclusive lock on the socket path,
not by configuration.

## 2. Components

| Crate | Responsibility |
| --- | --- |
| `opencraylsp-proto` | Wire types shared by everything: daemon protocol messages, `ToolDef`, `ToolOutput`, `StatusReport`. No I/O, no policy. |
| `opencraylsp-core` | Language server transport, instances, document sync, configuration and root discovery, the instance pool, the `LspBackend` trait, and an in-memory mock backend. |
| `opencraylsp-tools` | The `lsp_*` tool catalogue: argument schemas, symbol resolution, position handling, and output formatting. |
| `opencraylsp-client` | The daemon connection: socket discovery, autospawn, reconnect, and the `DaemonHost` implementation of `ToolHost`. |
| `opencraylsp-mcp` | The MCP stdio server binary, including the in-process `--embedded` mode. |
| `opencraylspd` | The daemon binary (`src/server/**`) and its CLI (`src/cli/**`: `serve`, `status`, `stop`, `restart`, `doctor`, `version`). |
| `opencraylspd-e2e` | End-to-end tests: real daemon, real `opencraylsp-mcp`, and a scripted fake language server. |

Dependencies only ever point in one direction:

| Crate | Depends on |
| --- | --- |
| `opencraylsp-proto` | nothing else in the workspace |
| `opencraylsp-core` | `opencraylsp-proto` |
| `opencraylsp-tools` | `opencraylsp-proto`, `opencraylsp-core` |
| `opencraylsp-client` | `opencraylsp-proto` |
| `opencraylsp-mcp` | `opencraylsp-proto`, `opencraylsp-client`, `opencraylsp-core`, `opencraylsp-tools` |
| `opencraylspd` | `opencraylsp-proto`, `opencraylsp-core`, `opencraylsp-tools`, `opencraylsp-client` |

The invariant that matters is that `opencraylsp-core` and `opencraylsp-tools` never depend on
`opencraylsp-client`, `opencraylsp-mcp` or `opencraylspd`; `scripts/check-layering.sh` enforces it, and
carries a failing fixture so the check itself cannot rot. The layering is what
makes the `--embedded` mode possible: `opencraylsp-mcp` can run the pool and the tools
in-process without pulling in the daemon.

Two boundaries are worth naming, because they are where most changes belong:

* `LspBackend` (`opencraylsp-core/src/backend.rs`) — the tool layer's view of a server.
  `opencraylsp-tools` is written entirely against this trait, so its tests run against
  the mock backend and stay fast and hermetic.
* `ToolHost` (`opencraylsp-proto`) — the MCP layer's view of a backend. Implemented by
  `DaemonHost` (over the socket) and `EmbeddedHost` (in-process).

The tool layer is deliberately ignorant of *how* a request reaches a server: it
resolves a symbol, calls the backend, and formats the answer. Everything about
sockets, pooling, retries and leases lives below it.

## 3. Connections and methods

The daemon speaks newline-delimited JSON-RPC 2.0 over a Unix domain socket: one
message per line, UTF-8, no unescaped newlines, with a 4 MiB line limit
(exceeding it is a parse error and the connection is closed, because a peer that
does that is not going to recover by our guessing). A connection may have many
requests in flight and replies may come back out of order; they are matched by
`id`. Tool calls are the slow ones and dominate, so head-of-line blocking would
be a real cost.

| Method | Purpose |
| --- | --- |
| `hello` | First request on every connection: protocol version, client identity, workspace directory, optional language list. |
| `tools/list` | The tool catalogue, as `ToolDef`s. |
| `tools/call` | Run one tool; answers with `ToolOutput`. |
| `status` | Daemon and instance status (`StatusReport`). |
| `shutdown` | Graceful stop; no further requests are accepted. |
| `$/cancel` | Notification cancelling one in-flight request by `id`. |

`hello` must be first; anything else gets `-32002 not_initialized`. Protocol
level errors and their codes are listed in [docs/PROTOCOL.md](PROTOCOL.md).

The important distinction, and a frequent source of confusion for new
contributors:

* **Protocol errors** are JSON-RPC `error` objects. They mean the *connection*
  is being used wrongly: bad JSON, unknown method, wrong version, workspace that
  does not exist.
* **Tool failures** are `ToolOutput { is_error: true }` with a machine-readable
  `[code]` first line. They mean the tool ran and could not answer — the symbol
  is ambiguous, the server is still indexing, the path is outside the workspace.

A tool that fails must never look like a protocol failure: an agent should be
able to retry, or ask the user, without its MCP client tearing the session down.
The catalogue of `[code]` markers is in [docs/TOOLS.md](TOOLS.md).

## 4. Instances, concurrency and leases

An **instance** is one language server process, identified by the pair
`(server, root)`. `root` is found by walking up from the file being asked
about, taking the highest directory that still contains one of the server's
root markers (`Cargo.toml`, `go.mod`, …), and never walking above the
connection's workspace boundary. Two agents asking about the same project get
the same key, and therefore the same process; two agents asking about different
projects do not.

The pool owns all instances. One `Pool` serves every connection, and each
request takes a **lease** on the entry it needs. The lease is the mechanism that
keeps "this instance is going away" from racing "this request wants that
instance":

* `Pool::acquire` increments the entry's in-flight count and returns a `Lease`,
  all under the instance-table lock.
* Retirement — an idle sweep, a memory restart, a shutdown — flips the entry's
  `retiring` flag under the same lock, and only when no lease is out.
* Dropping the lease decrements the count and stamps `last_used`, which is what
  the idle sweeper and the LRU eviction read.

Without that handshake two bugs appear immediately: a request talks to a server
the sweeper is in the middle of killing, or a second instance for the same key
is started while the first is still exiting (which is exactly the "two
rust-analyzers for one project" failure this project exists to prevent).

Because leases are held for the duration of a request, one slow request does not
block others: calls to the same instance run concurrently, and calls to
different instances are independent. `docs/CONFIGURATION.md` lists the knobs.

Instance lifecycle states are reported verbatim in `lsp_status`:
`starting`, `indexing`, `ready`, `restarting`, `failed`, `stopped`.

## 5. Resource policy

Limits live in the config file and are enforced in one place, so that an agent
cannot accidentally opt out of them.

| Setting | Default | Behaviour when hit |
| --- | --- | --- |
| `max_instances` | 8 | Reclaim the least-recently-used *idle* instance; if every instance is in use, wait up to 10 s for a slot, then answer `[capacity]`. |
| `max_rss_mb` | 8192 | Measured every 5 s over the whole process tree. Over the limit: let in-flight requests drain (up to 30 s), restart the server, answer `[memory_restart]`. While indexing the threshold is doubled, because indexing is a transient spike. Three restarts within an hour, then the instance is `failed` and further calls are refused with an explanation. |
| `idle_shutdown_secs` | 900 | An instance with no requests for this long is shut down and its memory returned; the next call starts it again. |
| `max_open_docs` | 256 | Open the document, then `didClose` the least-recently-used one. |
| `request_timeout_ms` | 30000 | `[timeout]`. |
| `startup_timeout_ms` | 60000 | A server that has not finished initialising is `failed`. |
| `max_restarts` | 3 | Crashes before the instance is refused (`[server_failed]`) until the daemon restarts. |
| `watch_interval_ms` | 3000 | How often the watcher re-snapshots the tree; `0` disables it. |
| `memory_sample_ms` | 5000 | Sampling interval for the memory guard. |

Two honest caveats are built in rather than papered over:

* Memory measurement reads `/proc/<pid>/status` and walks the process tree. On
  platforms without `/proc`, the guard is **disabled** and `rss_bytes` is
  `null`. Reporting a made-up number would be worse than reporting none.
* Reclaiming an instance is not free: the next request pays a cold start, and
  will be told `[indexing]` rather than being handed a stale or empty answer.

## 6. Indexing honesty

A language server that is still building its index will answer `null` or an
empty array to a perfectly valid question. Reporting that as "no definitions
found" is the single most misleading thing this kind of tool can do, so the
daemon tracks indexing explicitly:

* The client advertises `window.workDoneProgress = true` during `initialize`.
  Without it, some servers never send progress at all — a subtle failure mode
  whose symptom is a permanent "not indexing" reading.
* Every `$/progress` work-done token is tracked. While any token is running, the
  instance is `indexing`; when they all end, it is `ready` — but only after a
  500 ms settle window, because real indexers pass through several stages with
  short gaps between them, and a request landing in a gap would see a
  half-built index.
* A server that has never reported progress is treated as starting for the
  first 3 s after launch, rather than as an instantly-ready one.
* A token that has not moved for 120 s is treated as stale, so that a server bug
  cannot wedge the state machine forever.

The rule the tool layer applies: **if the result is empty and the instance is
indexing, answer `[indexing]`; never answer empty.** A non-empty result is
returned normally, with a note that indexing is still going on so the caller
knows it may be incomplete.

Diagnostics get the same treatment. `lsp_diagnostics` syncs the file, sends
`didSave`, and then waits for a `publishDiagnostics` for *that version*,
followed by 1500 ms of quiet, with a 20 s cap. If no publish for the current
version ever arrives it says so — it does not report "0 errors", because "no
diagnostics yet" and "no problems" are different facts.

## 7. Document synchronisation

**Disk is the source of truth.** There is no unsaved-buffer tracking: an agent
that wants a change reflected must write the file. This keeps the daemon
stateless with respect to editors, which is what makes it shareable between
agents that have no idea the others exist.

Before a request, for each document already open on the instance:

1. Compare `(mtime, size)`. If both match the last known values, nothing to do.
2. Otherwise hash the contents. A file whose mtime is very recent (under ~2 s)
   is re-hashed rather than trusted, because a same-size edit inside the
   filesystem's timestamp granularity would otherwise slip through — a
   correctness detail that only shows up under fast edit loops, which is
   exactly what agents do.
3. Changed content becomes a full-text `didChange` with `version + 1`; a
   deleted file becomes `didClose`.

Files that were never opened are handled by the watcher: it snapshots the tree
on an interval, diffs against the previous snapshot, and sends
`workspace/didChangeWatchedFiles` for what moved.

Open documents belong to the *instance*, not to a connection. There is no
per-client reference counting: the LRU cap and file deletion are what end a
document's life. That is a deliberate simplification — reference counting would
mean that a crashed agent leaks documents forever, while an LRU cap degrades
gracefully.

## 8. Failure semantics

| Situation | Behaviour |
| --- | --- |
| `opencraylsp-mcp` cannot reach the socket | Read the lock to decide whether a daemon exists. If not, spawn one (`opencraylspd serve`) with stdio detached and retry with backoff (50/100/200/400/800/1600 ms, 8 s total). On failure: `[daemon_unavailable]`. It does **not** silently fall back to `--embedded`, because that would start a second set of language servers behind the user's back. |
| The socket file exists but nothing accepts | Treat it as stale. If the lock can be taken, start a daemon. The client never unlinks the socket: it could delete the *new* daemon's socket while that daemon is starting. The daemon itself removes a stale socket after it holds the lock, just before binding. |
| The daemon dies mid-request | That request gets `[daemon_unavailable] connection lost`; the next call reconnects or respawns. |
| Protocol version mismatch | `[daemon_unavailable] protocol mismatch`, naming both versions and suggesting a restart. |
| A client disconnects | Its in-flight requests are cancelled. Instances stay up — other clients may be using them. |
| The daemon is asked to stop | Stop accepting, cancel in-flight requests, shut down every instance in parallel (10 s budget for the pool, then kill), remove the socket, exit. |
| A language server crashes | Restart on the next request, up to `max_restarts`; then `[server_failed]` until the daemon restarts. |
| A tool handler panics | The panic is caught and answered as `[internal_error]`, and the daemon stays up. One bad tool call must not take the pool down with it. |

The common thread: **failures are reported where they happened, with a code the
caller can act on, and never by quietly doing something else.** A tool answer
that is wrong but confident is worse than one that says it does not know.

## 9. Security model

The threat model is "several agents on one user's machine", not "a hostile
tenant". The guarantees reflect that, and they are enforced, not merely
intended:

* **Same user only.** The socket directory is `0700` and the socket is `0600`.
  After `accept`, `SO_PEERCRED` is checked and a peer whose uid is not the
  daemon's uid is disconnected.
* **Paths stay inside the boundary.** Every path is canonicalised (symlinks
  resolved) and must land inside the connection's workspace, or inside a
  configured `allowed_roots`. Anything else is `[outside_workspace]`. Results
  *outside* the boundary — standard library sources, for instance — are
  reported as coordinates and a path only; their contents are never read.
* **Read-only by construction.** `lsp_rename_preview` turns a `WorkspaceEdit`
  into a unified diff and writes nothing. There is a test that fails if any
  non-test code in `opencraylsp-tools` so much as opens a file for writing.
* **`workspace/applyEdit` is refused** (`applied: false`). A language server
  cannot edit the project behind the agent's back.
* **One daemon per socket**, via an exclusive `flock` on `<socket>.lock`; a
  second daemon exits 0 rather than fighting for the socket.
* **No `unsafe`.** It is forbidden at the workspace level, so it cannot be
  reintroduced by accident.

`opencraylsp-mcp --embedded` exists for testing and for environments where a daemon is
not wanted. It moves the trust boundary: the pool runs inside the agent's
process. It is not the default, and nothing falls back to it automatically.

## 10. Language selection

Each connection declares the languages it wants; the daemon enables exactly
those. The set is called `E`, and it is enforced in the pool, not merely
filtered in the tool layer:

* Canonical names: `rust`, `go`, `php`, `typescript`, `javascript`, `python`.
  `typescript` and `javascript` are two languages served by one server
  (`typescript-language-server`), differing only in file extensions; enabling
  both still yields one instance, because instances are keyed by `(server,
  root)`.
* Aliases are accepted case-insensitively: `rs`, `golang`, `ts`, `js`, `py`,
  `tsjs`/`node`.
* `--languages rust,go` wins over `OPENCRAYLSP_LANGUAGES`. `auto` (the default, also
  the empty string) enables every language with a project marker inside the
  workspace (at most two levels deep, ignoring hidden directories and vendored
  trees). `all` enables everything the configuration provides.
* Enabling a language that is not installed is not an error: the call answers
  `[server_not_installed]` with an install hint, which is more useful than
  pretending the language does not exist.
* A file whose language is not in `E` is refused with `[language_disabled]`,
  naming the enabled set. Asking for an unknown language name fails the
  connection with `-32005 unknown_language` and the valid list, rather than
  being ignored.
* Instances remain shared regardless of who enabled what. If one connection
  enables Rust and another does not, the second one's Rust calls are refused,
  but the two still share the single `rust-analyzer` the first one needs.

The "why" is straightforward: an agent editing a Rust project should not be the
reason a Go server is holding a gigabyte of memory. Language selection is how a
user expresses that, per harness, without a central configuration file being
edited every time.

## 11. Design trade-offs

**A daemon, not a library.** Sharing language servers requires a process that
outlives any single agent and can arbitrate. Doing it in-process would mean
either one agent's crash kills everyone's server, or each agent gets its own.

**A Unix socket with a hand-rolled protocol, not an HTTP server.** The peers are
agents on the same machine; a local socket gives peer credentials, filesystem
permissions and no port allocation. Newline-delimited JSON keeps the wire
debuggable with `cat`, and a 4 MiB line cap keeps a broken peer from eating
memory.

**Symbol names first, positions second.** Models are bad at producing line and
column numbers and good at producing names. Every positioning tool accepts a
symbol name and resolves it, and it says `[ambiguous]` with candidates rather
than guessing when a name matches several things. Positions are still accepted,
because sometimes the caller really does have one.

**Tool errors as tool output.** A failed lookup is a normal result of asking a
question, not a broken connection. Keeping them separate is what lets an agent
retry an `[indexing]` answer in a loop without its MCP session falling over.

**Never answer empty while indexing.** Covered above, and worth repeating: it is
the failure mode that wastes the most of a user's time, because the answer is
plausible and wrong.

**No silent fallback to embedded.** If the daemon is unreachable, saying so is
correct; quietly starting a private pool would mean two sets of language servers
and two different views of the project.

**Disk as truth.** Unsaved-buffer sync would make the daemon depend on each
editor's notion of a buffer. Requiring a write is a small imposition on the
agent and removes an entire class of disagreement.

**Leases rather than a global lock.** A single lock around the instance table
would serialise every request in the daemon; leases hold the lock only long
enough to hand out a reference, and give the sweeper a precise moment at which
an instance is provably unused.

**Failures refuse rather than degrade.** A server that has exceeded its memory
ceiling three times is marked `failed` and calls are refused with an
explanation, instead of being restarted forever in a loop that never completes
any work.

## 12. Where to look

| Question | Start at |
| --- | --- |
| How do I add a tool? | `crates/opencraylsp-tools/src/tools.rs`, then `crates/opencraylsp-tools/src/format.rs`; the catalogue is generated into [docs/TOOLS.md](TOOLS.md) by `scripts/gen-tools-doc.sh`. |
| How does a request reach a server? | `crates/opencraylsp-core/src/manager.rs` (per-connection view) then `crates/opencraylsp-core/src/pool.rs` (leases, policy). |
| How is the wire formatted? | `crates/opencraylsp-proto/src/rpc.rs`, [docs/PROTOCOL.md](PROTOCOL.md). |
| Why did my server not start? | `crates/opencraylsp-core/src/config.rs` (presets, discovery), `crates/opencraylspd/src/cli/doctor.rs`. |
| How do I run the whole thing for real? | [CONTRIBUTING.md](../CONTRIBUTING.md); the end-to-end suite is `crates/opencraylspd-e2e`. |

## License

MIT — see [LICENSE](../LICENSE) and [SECURITY.md](../SECURITY.md).
