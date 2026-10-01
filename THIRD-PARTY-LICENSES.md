# Third-party licenses

`opencraylspd` and `opencraylsp-mcp` are MIT-licensed (see `LICENSE`). They ship, statically or
otherwise, the 114 Rust dependencies below, each under its own license.

The inventory is generated from `Cargo.lock` and kept in
[`licenses/deps.tsv`](licenses/deps.tsv) -- one row per crate, machine-readable,
so it can be checked rather than trusted. `scripts/check-licenses.sh` compares
that file against `Cargo.lock` and against the texts in `licenses/`, and fails
if a crate is missing, if a license has no text, or if the two disagree.

## What has to ship with a binary, and why

| License | Text | Required by |
| --- | --- | --- |
| Apache-2.0 | `licenses/Apache-2.0.txt` | `similar` is licensed **only** under Apache-2.0, with no MIT alternative. Apache-2.0 section 4(d) requires a copy of the license with any redistribution. |
| Unicode-3.0 | `licenses/Unicode-3.0.txt` | 19 crates, which require their license text to be included. It derives from Apache-2.0 and carries the same redistribution condition. |
| MIT | `licenses/MIT.txt` | the rest. The MIT text is the same for all of them; each crate's own copyright line is in its `LICENSE` in the published source. |

A redistributed binary is expected to carry `LICENSE`, `NOTICE`, this file and
the `licenses/` directory. `scripts/install.sh` installs exactly that, under
`$PREFIX/share/doc/opencraylsp`, and so does `make install`; the release archive
carries it so that a `curl | sh` install has it to install. The set is
`scripts/doc-files.sh`.

## The election, crate by crate

Where a crate offers a choice (`MIT OR Apache-2.0`), this project takes **MIT**:
one license across the whole tree, and the most permissive on offer. Two deserve
a note:

- `r-efi` declares `MIT OR Apache-2.0 OR LGPL-2.1-or-later`. The LGPL option is
  never taken -- MIT is offered and is what we use -- so a `[bans] deny` list
  naming "LGPL" added later is not a finding about this tree. `deny.toml` records
  it for exactly that reason.
- `bitflags` 1.3.2, reached through `lsp-types`, declares `MIT/Apache-2.0`. That
  is not valid SPDX -- `/` is not an operator -- so a strict parser reports the
  license as unknown. Upstream `bitflags` is dual MIT/Apache-2.0, so the election
  is the same one every other dual dependency makes. `deny.toml` records the
  discrepancy rather than adding a rule to swallow a bad expression; the real
  fix is an `lsp-types` bump.

## Dependencies

"Relied on" is the license this project elects. "Text required" lists every
license whose text has to accompany a binary, which is not the same set: an
expression like `(MIT OR Apache-2.0) AND Unicode-3.0` needs the Unicode-3.0
text even though the MIT half is what we take.

| License expression | Relied on | Crates | Text required |
| --- | --- | --- | --- |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | MIT | 1 | licenses/MIT.txt<br>licenses/Unicode-3.0.txt |
| `Apache-2.0` | Apache-2.0 | 1 | licenses/Apache-2.0.txt |
| `Apache-2.0 OR MIT` | MIT | 7 | licenses/MIT.txt |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | MIT | 3 | licenses/MIT.txt |
| `MIT` | MIT | 20 | licenses/MIT.txt |
| `MIT OR Apache-2.0` | MIT | 60 | licenses/MIT.txt |
| `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | MIT | 1 | licenses/MIT.txt |
| `MIT/Apache-2.0` | MIT/Apache-2.0 | 1 | licenses/MIT.txt |
| `Unicode-3.0` | Unicode-3.0 | 18 | licenses/Unicode-3.0.txt |
| `Unlicense OR MIT` | MIT | 2 | licenses/MIT.txt |

## Known gaps

- `crates/opencraylspd-e2e` builds a `fake-lsp` binary that is a test fixture, not part
  of a release. It is still listed, because it is in `Cargo.lock`.
- Two versions of `syn` and two of `bitflags` are in the tree; `deny.toml` keeps
  `multiple-versions = "warn"` and says why. Both copies of each are covered.
- Neither `scripts/install.sh` nor `make install` copies these files next to the
  binaries, and the release workflow does not bundle them. That is a packaging
  decision for the maintainers; this file states the obligation, it does not
  discharge it. **Closed:** the three now do. `install.sh` and `make install`
  put the set under `$PREFIX/share/doc/opencraylsp`, the release archive carries it in
  the same place, and `uninstall.sh` / `make uninstall` remove it. The set is
  the one in `scripts/doc-files.sh`, which all four read.
