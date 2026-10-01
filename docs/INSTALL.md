# Installing opencraylspd and opencraylsp-mcp

`opencraylspd` is the daemon; `opencraylsp-mcp` is the MCP server your agent launches. You
install both, you start neither by hand: `opencraylsp-mcp` starts `opencraylspd` on first use.

Install in this order:

1. the two programs (this page);
2. the language servers for the languages you will work in
   ([below](#language-servers));
3. the client connection ([docs/CLIENTS.md](CLIENTS.md)).

## Requirements

Supported platforms:

| Platform | How it installs |
| --- | --- |
| Linux x86-64 and aarch64 | A prebuilt static binary. No Rust needed. |
| Linux on other CPUs | Built from source with cargo (below). |
| macOS (Intel and Apple Silicon) | Built from source with cargo. It compiles for `aarch64-apple-darwin`; the maintainers have not run it on a Mac, and the daemon's memory guard is off there because it reads `/proc`. |
| Windows | Through **WSL2** (see [Windows](#windows-wsl2)). A native Windows build is not available: the daemon uses Unix sockets. |

| Need | When |
| --- | --- |
| Rust 1.89 or newer, and a C compiler (`cc`, `gcc` or `clang`) | Only when building from source. On macOS the compiler comes from `xcode-select --install`. `rustc --version` reports the toolchain (this repository pins 1.95.0 in `rust-toolchain.toml`). |
| `curl` or `wget` | For the one-line installer's download. |
| `tar`, `install`, `mktemp`, `sed`, `sort`, `head`, `tr`, `dirname`, and `sha256sum` or `shasum` | Standard on Linux and macOS; the installer names any that are missing. |
| Node.js, Go, PHP or a Rust toolchain | Only for the language servers you choose. |

Nothing here needs root, and the installer never uses `sudo`: the default prefix
is `~/.local`. If you pick a prefix you cannot write to, it says so and stops.

## One-line install

```sh
curl -fsSL https://raw.githubusercontent.com/amgio38/opencraylsp/main/scripts/install.sh | sh
```

Pass options through the pipe with `sh -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/amgio38/opencraylsp/main/scripts/install.sh | sh -s -- --prefix /opt/lsp
```

If you would rather read it before running it, download it first:

```sh
curl -fsSLO https://raw.githubusercontent.com/amgio38/opencraylsp/main/scripts/install.sh
less install.sh
sh install.sh
```

The script detects the platform. On Linux x86-64 and aarch64 it downloads the
static musl binary for that CPU from the latest GitHub Release, verifies its `sha256`, and installs
`opencraylspd` and `opencraylsp-mcp` into `~/.local/bin`. Everywhere else, or when
there is no release asset, it runs `cargo install --locked --path` from the
current checkout, or from a shallow clone when run outside one.

Safe to pipe: the whole script is one function that runs on its last line, so a
download that is cut off part-way does nothing. It never reads standard input,
validates `--version` and `OPENCRAYLSP_REPO` before they reach a URL, installs
the pair atomically (a failure leaves the old install untouched), and cleans up
after itself. It does not restart a running daemon; if one is running it tells
you to run `opencraylspd restart` when it suits you.

The checksum is fetched from the same release as the archive, so it detects a
corrupted or truncated download, not a compromised release. If that matters to
you, build from source from a tag you have reviewed (`--from-source`).

Useful flags (see `sh scripts/install.sh --help`):

```sh
sh scripts/install.sh --prefix /usr/local     # install elsewhere
sh scripts/install.sh --version v0.20260929.1        # a specific release tag
sh scripts/install.sh --from-source           # always build with cargo
DESTDIR=/tmp/stage sh scripts/install.sh      # stage for a package
```

It ends by printing the one client command an agent needs:

```sh
claude mcp add opencraylsp -- opencraylsp-mcp --languages auto
```

## Windows (WSL2)

The programs are Linux programs; on Windows they run inside WSL2, which is a real
Linux kernel and needs no virtual machine to manage.

1. In PowerShell (run as administrator the first time), install a distribution and
   reboot when asked:

   ```powershell
   wsl --install -d Ubuntu
   ```

2. Open the **Ubuntu** app from the Start menu and create your Linux user.
3. Install exactly as on Linux, inside that Ubuntu shell:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/amgio38/opencraylsp/main/scripts/install.sh | sh
   ```

4. Run your coding agent inside WSL too (for example Claude Code installed in the
   Ubuntu shell), and register the server there with `claude mcp add opencraylsp --
   opencraylsp-mcp --languages auto`.

Two things worth knowing:

- **Keep projects on the Linux filesystem** (`~/projects`), not under `/mnt/c/...`.
  Reading Windows files through `/mnt/c` is slow, and language servers index a lot.
- **Use WSL2, not WSL1.** WSL1 translates system calls instead of running a Linux
  kernel; `wsl -l -v` shows the version, and `wsl --set-version Ubuntu 2` upgrades it.

Running the Windows build of an agent against the WSL install (a Windows program
launching `wsl opencraylsp-mcp`) is not covered here: paths differ between the two
sides (`C:\work` against `/mnt/c/work`) and the tools do not translate them.

## From a checkout

```sh
git clone https://github.com/amgio38/opencraylsp
cd opencraylsp
make install                 # builds release binaries, installs to ~/.local/bin
```

`make install` respects the usual variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `PREFIX` | `$(HOME)/.local` | Install prefix; binaries go to `$PREFIX/bin`. |
| `BINDIR` | `$(PREFIX)/bin` | Exact destination directory. |
| `DESTDIR` | empty | Staging prefix prepended to `BINDIR`, for packagers. |

```sh
make install PREFIX=/usr/local                 # system-wide (may need sudo)
make install DESTDIR=/tmp/stage                # /tmp/stage$HOME/.local/bin/…
```

Equivalent with cargo directly:

```sh
cargo install --locked --path crates/opencraylspd
cargo install --locked --path crates/opencraylsp-mcp
```

`cargo install` writes to `~/.cargo/bin`; use `--root DIR` to change that.

## Prebuilt release binaries

Each `v*` tag publishes one archive per CPU, `opencraylsp-x86_64-unknown-linux-musl.tar.gz`
and `opencraylsp-aarch64-unknown-linux-musl.tar.gz` (each containing `opencraylspd` and
`opencraylsp-mcp`, statically linked), and a `.sha256` for each. Pick the one for
`uname -m` (`x86_64` or `aarch64`):

```sh
T=x86_64-unknown-linux-musl        # or aarch64-unknown-linux-musl
curl -fsSLO "https://github.com/amgio38/opencraylsp/releases/latest/download/opencraylsp-$T.tar.gz"
curl -fsSLO "https://github.com/amgio38/opencraylsp/releases/latest/download/opencraylsp-$T.tar.gz.sha256"
sha256sum -c "opencraylsp-$T.tar.gz.sha256"
mkdir -p ~/.local/bin && tar -xzf "opencraylsp-$T.tar.gz" -C ~/.local/bin opencraylspd opencraylsp-mcp
```

The installer does the same thing with more care: it downloads with connect and
total timeouts, checks the archive's `sha256`, unpacks only the two program
files, and installs them as a pair (staged, verified to run and to report the
same version, then moved into place, so a failure leaves your old install
untouched). The checksum protects against a corrupted transfer; it is published
by the same release, so it is not a signature against a compromised upstream.

To build the same static binaries yourself (needs the musl target):

```sh
rustup target add x86_64-unknown-linux-musl
make release-static          # writes dist/opencraylspd and dist/opencraylsp-mcp
```

## Language servers

`opencraylsp-mcp` only speaks LSP; the semantic work is done by a real language server.
Install only the ones you need and make sure each command is on the `PATH` of
the process that starts `opencraylsp-mcp`. `opencraylspd doctor` checks all of them and prints
the same install hints.

Nothing is installed for a language you do not choose. From a checkout:

```sh
make install                # the two programs; on a terminal, then a language menu
make install php js         # the programs plus the PHP and JS/TS servers
make install-lsp go rust    # language servers only, no rebuild
sh scripts/install-lsp.sh --dry-run php    # show the command without running it
```

Languages: `rust`, `go`, `js` (`ts` is an alias), `php`, `python`. The script
uses the language's own package manager (`rustup`, `go`, `npm`); it never
installs a toolchain, skips a server that is already on the `PATH`, and says
where to get a missing package manager.

Only the servers for languages found in your workspace are started. With
`opencraylsp-mcp --languages auto` (the default), the daemon detects languages from
project markers and starts a server on demand, so an installed but unused
server costs nothing.

| Language | Server | Install |
| --- | --- | --- |
| Rust | `rust-analyzer` | `rustup component add rust-analyzer` |
| Go | `gopls` | `go install golang.org/x/tools/gopls@latest` |
| Python | `pyright-langserver` | `npm i -g pyright` |
| PHP | `intelephense` | `npm i -g intelephense` |
| TypeScript / JavaScript | `typescript-language-server` | `npm i -g typescript typescript-language-server` |

Notes:

- **rust-analyzer** must match the toolchain your project builds with. If you
  use a `rust-toolchain.toml`, add the component to that toolchain:
  `rustup component add rust-analyzer --toolchain 1.95.0`.
- **TypeScript** needs a 5.x `tsserver.js`. A global TypeScript 7 install no
  longer bundles one, so point the server at a 5.x copy (see
  [CONFIGURATION.md](CONFIGURATION.md#typescript--javascript)); otherwise
  requests fail with `[server_failed]`.
- **pyright** and **intelephense** may not report a version the way `doctor`
  expects; a server that answers `--version` too slowly is shown as
  `<timeout>` (the probe gives it 10 seconds), and one whose output is not a
  version is shown as `unknown`. Either is normal for a server that is
  installed.

## Verify

```sh
opencraylspd doctor          # servers found, versions, socket, config, auto languages
opencraylspd version         # daemon version and protocol revision
```

Then connect a client: [docs/CLIENTS.md](CLIENTS.md). If a tool answers with a
bracketed marker such as `[server_not_installed]`, see
[docs/TROUBLESHOOTING.md](TROUBLESHOOTING.md).

## Updating and uninstalling

Re-run the installer (or `make install`) to update. Remove the programs with
either:

```sh
make uninstall                        # from a checkout
sh scripts/uninstall.sh               # from anywhere
```

Uninstalling removes only `opencraylspd` and `opencraylsp-mcp`. If the prefix was the target of
`cargo install --root` (a prefix carrying `.crates.toml`/`.crates2.json`), the
script also asks cargo to drop those registry entries; if you installed them
into the default cargo root with `cargo install`, remove them with
`cargo uninstall opencraylspd opencraylsp-mcp`. Your configuration, the daemon's socket and its
log are left in place; see
[What lives where](TROUBLESHOOTING.md#what-lives-where) to remove those too.
