# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `scripts/preflight.sh` (`make preflight`): runs, before any push, what CI runs, in CI
  order, and refuses a tree with uncommitted files. `scripts/check-preflight-parity.sh`
  (with its own test) fails in preflight and in CI when `ci.yml` runs a command that
  preflight does not.
- `scripts/release.sh`: the one way to cut a release. It runs preflight, pushes `main`,
  waits for CI to finish green, and only then creates and pushes the tag and waits for
  the release workflow. It refuses to reuse a tag. `docs/RELEASING.md` documents the flow,
  including re-recording the golden MCP transcript after a version bump.

### Changed

- The release workflow now refuses a tag that differs from the crate version, or a
  version with no `CHANGELOG.md` section, before building anything.

## [0.20260929.1] - 2026-10-01

Release name: **V0.20260929.001**. From this version the project is numbered
`V0.<yyyymmdd>.<nnn>`; Cargo's semver cannot carry a leading zero, so the crate
version is `0.20260929.1`.

### Security

- The client now checks, before sending anything, that the process serving the
  socket runs as the same user, and refuses with `[daemon_untrusted]` otherwise.
  Until now only the daemon verified its clients, so another local user who
  pre-created the socket directory could impersonate the daemon.
- The socket directory must be a real directory: a symlink or a file in its
  place is refused.
- The type-alias check in `lsp_implementations` reads a source line from disk.
  It now only reads a regular file inside the workspace and no larger than the
  shared read cap, as every other read does.
- Language servers no longer inherit environment variables whose names look like
  credentials (`*TOKEN*`, `*SECRET*`, `*PASSWORD*`, `*API_KEY*`, ..., and
  `SSH_AUTH_SOCK`). Variables set in a server's `env` config table are kept.
  `SECURITY.md` now states plainly that opening an untrusted repository runs its
  build scripts and plugins.

### Fixed

- `lsp_find_symbol`: `path` scopes the answer to a file or a directory. A
  directory used to be treated as a file and answered `[no_server]`.
- `lsp_find_symbol`: an unknown `kind` is an error that lists the 26 legal
  names, instead of an empty answer indistinguishable from "no such symbols".
- `lsp_implementations` on a type alias says that an alias has no
  implementations of its own and names the type to ask about, instead of
  "no implementation was found".

### Changed

- `lsp_find_symbol` ranks exact name matches first, then name prefixes, then the
  rest, and the header says how many of each are shown.
- The repository URL is now `https://github.com/amgio38/opencraylsp`.
- Comments and documentation no longer carry internal tracking references.

## [0.1.1] - 2026-10-01

### Fixed

- A request's write to a language server's stdin is now bounded by the request's
  own deadline. Only notifications were bounded; a server that stopped reading
  held `send_request_within`, and so `shutdown`, past any deadline.
- `lsp_rename_preview` truncated a file's edits beyond the per-file ceiling
  without saying so, although the code comment promised the count would be
  printed. It now ends the preview with how many edits were not decoded.
- A `null` answer to call-hierarchy `incomingCalls`/`outgoingCalls` is the
  spec's spelling of "no callers", but had been reported as an invalid response.
  It is now an empty answer; other non-list shapes still fail.
- The regression test for encoding failures (a response that cannot be encoded still gets an error
  frame and the queue behind it still goes out) did not exist; the hook it needed
  had no caller. It does now.

### Changed

- `lsp_find_symbol` lists matches under `vendor/` and `node_modules/` after the
  project's own, instead of letting them fill the limit first. They are still
  listed.
- A `lsp_hover` or `lsp_references` miss from a `symbol` now says the name was
  resolved to its declaration and suggests a `path`+`line`+`column` at a use,
  since some servers (intelephense) answer nothing at a declaration.
- `lsp_implementations` on a function or method answers `[not_found]` with the
  reason (nothing to implement; point at the type or interface) instead of the
  server's raw `[rpc_error]`.
- A `null` rename answer now says it means either nothing renameable is there or
  the server does not implement rename, instead of "did not say why".

### Fixed

- The daemon's own memory ceiling measured the wrong process: it read the
  resident memory of the whole process tree under the daemon, so the language
  servers it supervises were charged to `limits.daemon_max_rss_mb` as well as
  to their own `limits.max_rss_mb`. One rust-analyzer indexing a large
  workspace (measured: two instances holding 4.7 GiB and 4.0 GiB, daemon
  process 9.85 MB) made a healthy daemon report `rss 8.3 GB / 512 MB` and trip
  the restart circuit breaker. The daemon's guard and `status` now read this
  process alone, which is what the setting has always described; the
  per-instance guard still reads each server's whole tree.
- Disambiguating a name took two attempts: the `[ambiguous]` answer said to
  call again with `path`, `line` and `column`, while passing those beside the
  `symbol` from the failed call was rejected as mutually exclusive, so the
  retry had to drop `symbol` — a step the message never mentioned. Each
  candidate now carries the exact arguments to retry with, and a complete
  `path`+`line`+`column` is honoured even when `symbol` is still present, so the
  retry works whether or not the model remembered to remove the name.

### Fixed

- `lsp_rename_preview` answered a position with nothing at it by repeating the
  language server's `InvalidParams` ("No references found at position"), which
  names neither the position nor the likely cause. It is now `[not_renamable]`
  with the position printed back and the identifiers on that line listed with
  their columns, so a wrong `column` can be fixed from the answer. A different
  `InvalidParams` — one that says only `invalid params` — stays an `rpc_error`,
  because there the server really is complaining about the arguments.
- A position-targeted tool that found nothing said only that it found nothing,
  leaving the caller to retry the same call with another number and, because the
  cause is almost always a `column` beside the identifier rather than on it, to
  do the same thing twice. The miss now lists the identifiers on that line with
  their columns — in the same 1-based Unicode scalar counting the tools use
  everywhere, so a line of CJK text is not off by two per character — capped at
  eight, and omitted for a line with no identifiers, a line over 2000 characters,
  and any file outside the workspace. `lsp_definition`, `lsp_references`,
  `lsp_hover`, `lsp_implementations`, `lsp_callers`, `lsp_callees` and
  `lsp_rename_preview` all answer this way, from one function.
- A `column` past the end of its line was clamped to the line's end, so the
  server was asked about a position the caller never meant. It is now refused,
  with the line's real length stated; a column one past the end is still a
  position, because that is where a caret sits at the end of a line.
- A request that names no file (`lsp_find_symbol` with no `path`) used the
  workspace boundary as the language-server root even when the boundary was not
  a project. In a workspace holding a project one directory down — a monorepo, or
  a directory of side-by-side checkouts — that started a second server which
  indexed nothing while holding gigabytes: measured on a real session, one
  rust-analyzer on the boundary at 4.5 GiB while every query went to the project
  below it. The boundary is now used only when it really is a project (or the
  server has no `root_markers`); otherwise the most recently used instance is
  reused, or the single project below the boundary, or the request is refused
  with `[no_project]` listing the candidates rather than answered about a
  project the caller never asked about.

## [0.1.0] - 2026-09-30

Initial release.

### Added

- `opencraylspd`: a shared daemon that pools language-server instances across clients,
  with a Unix-socket JSON-RPC protocol v1, single-instance locking, per-request
  cancellation and graceful shutdown.
- `opencraylspd serve`, `status`, `stop`, `restart`, `doctor` and `version`.
- `opencraylsp-mcp`: an MCP stdio server exposing eleven read-only `lsp_*` tools, with
  `--workspace`, `--languages`, `--socket`, `--config` and `--embedded`.
- Automatic daemon startup and one-shot reconnect in `opencraylsp-client` when the
  daemon is unreachable or restarts.
- Per-connection language selection (`rust`, `go`, `php`, `typescript`,
  `javascript`, `python`, `all`, `auto`) with shared instances keyed by
  `(server, project root)`.
- Resource management: idle reclaim, per-instance memory ceilings with graceful
  restart and a circuit breaker, and a cap on instances and open documents.
- Built-in server presets for rust-analyzer, gopls, pyright, TypeScript and
  intelephense, overridable from `~/.config/opencraylsp/config.toml`.
- Static release builds via `make release-static`, documentation under
  `docs/`, and `scripts/check-layering.sh` for the crate dependency direction.

### Fixed (found by the pre-release review and acceptance runs)

- A tool handler that panicked left its request unanswered forever; it now
  answers `[internal_error]` and the panic is written to the log.
- First run on a fresh machine: the client could not open the daemon's lock
  file when the socket directory did not exist yet, never started the daemon,
  and `opencraylsp-mcp` stalled for 8 seconds per request.
- `workspace/symbol` on TypeScript answered "No Project" until a file was open;
  a probe file of the server's language is now opened first.
- `lsp_definition` by name reported a miss on servers that answer nothing for
  a declaration (intelephense); it now falls back to the symbol's own location.
- A start cancelled during `initialize` left a half-started server behind.
- `workspace/applyEdit` requests from a server are answered `applied: false`.
- Requests to a silent daemon are bounded by a client-side deadline.
- File changes are forwarded to servers in batches of at most 1000.
- Blocking file and `/proc` reads moved off the async workers.

### Known limitations

- Positions sent to a server are always UTF-16 columns. Servers are only
  offered UTF-16, so this matters only for a server that ignores the
  negotiation and answers with another encoding.
- Document synchronisation checks every open document's modification time on
  each request for a given server instance (up to `max_open_docs`, 256 by
  default); requests to one instance are serialised by that check.
