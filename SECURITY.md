# Security

`opencraylspd` runs language servers on your machine on behalf of coding agents. This
page describes what it protects and what it does not.

## Trust model

Everything runs as **your user**. The daemon authenticates connections by uid
(see below), which means it trusts every process running as you. It is not a
sandbox between same-uid processes: a process that can reach the socket can ask
the daemon to open files that process could open itself, within the workspace
boundary it declares. The boundary exists to stop *tools* from wandering out of
a project by accident, not to contain a hostile program running as your user.

Language servers are spawned as your user with no extra sandboxing. Only
install and configure servers you trust, exactly as you would run them yourself.

## Untrusted repositories

A language server analyses the project it is pointed at, and for many languages
that means **running the project's own code**: rust-analyzer executes build
scripts and procedural macros, and the TypeScript server can load plugins named
in a workspace's configuration. Opening a repository you do not trust through
this daemon is therefore as risky as building it yourself. Do that in a
container or a throwaway user instead.

As a mitigation, language servers do not inherit environment variables whose
names look like credentials (anything containing `TOKEN`, `SECRET`,
`PASSWORD`, `PASSWD`, `CREDENTIAL`, `API_KEY`, `ACCESS_KEY` or `PRIVATE_KEY`,
and `SSH_AUTH_SOCK`). Variables set in a server's own `env` table in the config
file are passed as written. This reduces accidental exposure; it is not a
sandbox.

## The socket

- The runtime directory and socket are created with mode `0700` / `0600`.
- On each accepted connection the daemon checks `SO_PEERCRED` and closes the
  connection if the peer's uid is not the daemon's own.
- The client does the same in the other direction: after connecting and before
  sending anything it checks that the process serving the socket runs as the
  same user, and refuses with `[daemon_untrusted]` otherwise (or when either
  uid cannot be determined). A different user who pre-creates the socket
  directory therefore cannot impersonate your daemon.
- The socket directory must be a real directory (not a symlink or a file),
  owned by you and not writable by group or other; otherwise the daemon refuses
  to start.
- A single daemon owns the socket through an `flock` on `<socket>.lock`; a
  second `opencraylspd serve` exits quietly.

## Workspace boundary

- A connection declares a workspace directory. Every path a tool asks about is
  resolved lexically and by `canonicalize` (which follows symlinks) and must
  land inside the boundary (or an `allowed_roots` entry), otherwise the tool
  answers `[outside_workspace]`. A symlink inside the workspace that points
  outside is refused.
- Results that legitimately point outside the boundary (a standard-library
  source file, for example) are reported as coordinates and a path only; their
  contents are not read.

## Read-only guarantee

- No tool writes to your project. `lsp_rename_preview` computes a unified diff
  in memory and returns it; it never writes a file.
- The daemon answers a language server's `workspace/applyEdit` with
  `applied: false`; it never applies an edit.

## Reporting a vulnerability

Please report security issues privately to the maintainers (for example, a
private issue or security advisory on the project's repository) rather than in a
public issue. Include what you ran, what you expected, and what happened; a
minimal reproduction helps most.
