# Releasing

One command cuts a release, and it enforces the order:

```sh
bash scripts/release.sh            # plan + preflight only, pushes nothing
bash scripts/release.sh --publish  # preflight, push main, wait for CI green, tag, push tag, wait for the release workflow green
```

[`scripts/release.sh`](../scripts/release.sh) refuses to run off `main`, refuses a tag that
already exists locally or on `origin`, refuses a version with no `CHANGELOG.md` section,
and creates the tag **only after CI on the pushed commit has finished green**. A published
release therefore always points at a green commit. The release workflow repeats the tag
check on its side: a tag that differs from the crate version is refused before anything is
built.

## Before it: the commit that opens a release

1. Set the version in the root `Cargo.toml`: the `[workspace.package]` version **and** the
   four `opencraylsp-*` path-dependency pins in `[workspace.dependencies]`. Then refresh the
   lockfile with `cargo update --workspace --offline`.
2. Cut the `CHANGELOG.md` section, headed `## [<version>] - <date>`, in the same commit.
3. **Re-record the golden MCP transcript.** It contains the daemon version, so a bump
   without this turns CI red:
   `UPDATE_GOLDEN=1 cargo test -p opencraylspd-e2e --test e2e t10_the_mcp_session_matches_the_golden_transcript`,
   then read the diff: it must be the version string and nothing else.
4. Commit, then run `bash scripts/release.sh`.

## Rules that never bend

- **Nothing is pushed before [`scripts/preflight.sh`](../scripts/preflight.sh) passes** on the
  exact tree being pushed (`make preflight`). It runs what CI runs, in the same order:
  format, clippy with default and all features, the **whole** workspace test suite, the
  layering, licence, installer, `cargo deny`, docs and generated-doc checks. "I ran the
  relevant checks" is how a red CI gets published.
- `scripts/check-preflight-parity.sh` fails, in preflight and in CI, when `ci.yml` runs a
  command that `preflight.sh` does not, so the two lists cannot drift apart unnoticed.
- A tag is never moved, reused or deleted. If anything fails after the tag is public, fix
  forward with a new version.
- The static builds and what each archive contains are described in
  [`CONTRIBUTING.md`](../CONTRIBUTING.md#building-and-testing); the install path is in
  [`INSTALL.md`](INSTALL.md).
