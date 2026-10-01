## What and why

<!-- What does this change, and what problem does it solve? Link the issue if there is one. -->

## How it was checked

<!-- The commands you ran and what they showed. A bug fix should name the test that fails without it. -->

## Checklist

- [ ] `make ci` passes locally (format, clippy with `-D warnings`, tests, layering, license and docs checks).
- [ ] New behaviour has a test, and a bug fix has a test that fails without the fix.
- [ ] If a tool's name, arguments or description changed: regenerated `docs/TOOLS.md` (`scripts/gen-tools-doc.sh`) and the golden files.
- [ ] User-visible change is described in `CHANGELOG.md` under *Unreleased*.
- [ ] No secrets, personal paths or private data in the diff, comments or commit messages.
- [ ] Comments and documentation are in English.

By contributing you agree to follow the [Code of Conduct](../CODE_OF_CONDUCT.md).
