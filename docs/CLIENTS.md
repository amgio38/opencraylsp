# Connecting a client

Install `opencraylspd` and `opencraylsp-mcp` first ([INSTALL.md](INSTALL.md)), then register
`opencraylsp-mcp` with your agent. It speaks MCP over stdio, so any MCP-capable agent
can launch it directly.
This page has a copy-pasteable snippet per client. Each snippet shows how to
load a subset of languages with `--languages`; see
[CONFIGURATION.md](CONFIGURATION.md#languages-and-aliases) for the names.

In the snippets below, `opencraylsp-mcp` must be on the client's `PATH`, or replaced
with an absolute path such as `/usr/local/bin/opencraylsp-mcp`. `--workspace` is
optional and defaults to the directory the server is started in. You do not
start the daemon: `opencraylsp-mcp` does it on first use.

## Language batching

The daemon starts a language server only when a connection has enabled it, so
give each agent only what it needs:

```sh
opencraylsp-mcp --languages rust          # Rust only
opencraylsp-mcp --languages ts,js         # TypeScript + JavaScript
opencraylsp-mcp --languages go,php        # Go + PHP
opencraylsp-mcp                           # auto-detect from project markers
```

## Claude Code

Verified against the Claude Code MCP documentation on 2026-09-30
(<https://docs.claude.com/en/docs/claude-code/mcp>).

Add a stdio server with the CLI (everything after `--` is the command to run):

```sh
claude mcp add opencraylsp -- opencraylsp-mcp --languages rust,go
```

The default scope is `local` (this project, stored in `~/.claude.json`). Use
`--scope user` for every project, or `--scope project` to write the shared
`.mcp.json`:

```sh
claude mcp add --scope project opencraylsp -- opencraylsp-mcp --languages ts,js
```

Project-scoped `.mcp.json` at the repository root, written by hand:

```json
{
  "mcpServers": {
    "opencraylsp": {
      "command": "opencraylsp-mcp",
      "args": ["--languages", "go,php"]
    }
  }
}
```

Claude Code asks for approval the first time it loads a project-scoped server.

## Cursor

Verified against the Cursor MCP documentation on 2026-09-30
(<https://cursor.com/docs/context/mcp>).

Global config `~/.cursor/mcp.json`, or project config `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "opencraylsp": {
      "type": "stdio",
      "command": "opencraylsp-mcp",
      "args": ["--languages", "rust"]
    }
  }
}
```

Change `args` for another batch, for example `["--languages", "ts,js"]` or
`["--languages", "go,php"]`.

## opencode

Verified against the opencode MCP documentation on 2026-09-30
(<https://opencode.ai/docs/mcp-servers/>).

opencode reads `opencode.jsonc` (global or per project) and expects a local
server's `command` as an array:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "opencraylsp": {
      "type": "local",
      "command": ["opencraylsp-mcp", "--languages", "rust,go"],
      "enabled": true
    }
  }
}
```

For another batch, replace the array, for example
`["opencraylsp-mcp", "--languages", "ts,js"]`.

## Other MCP clients

Any client that can launch an MCP server over stdio works the same way:
configure the command `opencraylsp-mcp` (optionally followed by
`--languages <list>`), and leave the transport as stdio. There is nothing
client-specific to configure beyond the key names your client uses for a local
server (`mcpServers` / `mcp`, or its own spelling).

## Embedded mode

If you cannot run a daemon (for example, a locked-down sandbox), pass
`--embedded` and the pool runs inside `opencraylsp-mcp` itself. This avoids the daemon
but loses sharing between agents, so prefer the default (daemon) mode:

```sh
opencraylsp-mcp --embedded --languages rust
```

## Verifying a connection

From a terminal, list the tools the server offers:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | opencraylsp-mcp --languages rust
```

You should see eleven `lsp_*` tools. If the reply is empty, run the same command
with `--embedded` to remove the daemon from the picture, then `opencraylspd doctor`.
