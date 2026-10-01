# Throttle: this machine is shared. Override on the command line if needed.
JOBS ?= 14
export RUST_TEST_THREADS ?= 14

# Where cargo writes build output. Honors CARGO_TARGET_DIR the way cargo does,
# so `make install` and `make release-static` keep working when it is set.
TARGET_DIR ?= $(or $(CARGO_TARGET_DIR),target)

.PHONY: check fmt-check clippy test layering deny licenses install-test ci
check:
	cargo check --workspace --all-targets -j $(JOBS)
fmt-check:
	cargo fmt --all -- --check
clippy:
	cargo clippy --workspace --all-targets -j $(JOBS) -- -D warnings
test:
	cargo test --workspace -j $(JOBS)
layering:
	@if [ -x scripts/check-layering.sh ]; then scripts/check-layering.sh; else echo "layering check not installed yet"; fi
	@scripts/check-layering.test.sh
licenses:
	@scripts/check-licenses.sh

# The installer's own behaviour, with the network stubbed out. install.sh is
# mostly decisions about *failure* -- an unverified or mismatched release asset
# must not reach $BINDIR -- and those are exactly the paths a syntax check and a
# read-through miss.
install-test:
	@scripts/install-sh.test.sh

# The licence policy in deny.toml, enforced. This target does not install the
# tool: `cargo install cargo-deny --locked`, or rely on CI, which installs it
# with taiki-e/install-action and runs it on every push.
#
# It is in `ci` rather than left as a target nobody runs, because a policy file
# with no gate is a comment.
#
# A machine without cargo-deny is a hard stop by default: skipping silently is
# the fail-open this is meant to end. On a machine that cannot install it, say
# so out loud:
#
#   make ci ALLOW_MISSING_CARGO_DENY=1
#
# ...which prints a warning and continues. CI never needs that escape hatch: the
# workflow installs cargo-deny itself, so a merge gate can never be skipped.
DENY_FLAGS ?= --all-features check licenses bans advisories sources
ALLOW_MISSING_CARGO_DENY ?= 0

deny:
	@if command -v cargo-deny >/dev/null 2>&1; then \
		cargo deny $(DENY_FLAGS); \
	elif [ "$(ALLOW_MISSING_CARGO_DENY)" = 1 ]; then \
		echo "make deny: WARNING cargo-deny is not installed, so deny.toml was NOT checked." >&2; \
		echo "make deny: install it with 'cargo install cargo-deny --locked'." >&2; \
	else \
		echo "make deny: cargo-deny is not installed, so deny.toml was NOT checked." >&2; \
		echo "make deny: install it with 'cargo install cargo-deny --locked', or" >&2; \
		echo "make deny: re-run as 'make ci ALLOW_MISSING_CARGO_DENY=1' to accept that." >&2; \
		exit 1; \
	fi

ci: fmt-check clippy test layering docs-check licenses install-test deny

# --- release ----------------------------------------------------------------
# One self-contained Linux binary per program. The musl target is required; this
# target only checks for it and prints the install command, it never installs.
MUSL_TARGET ?= x86_64-unknown-linux-musl
DIST        ?= dist

.PHONY: release-static
release-static:
	@command -v rustup >/dev/null 2>&1 || { \
		echo "error: rustup is required to check for the $(MUSL_TARGET) target" >&2; exit 1; }
	@rustup target list --installed | grep -qx '$(MUSL_TARGET)' || { \
		echo "error: the $(MUSL_TARGET) target is not installed" >&2; \
		echo "       install it with: rustup target add $(MUSL_TARGET)" >&2; exit 1; }
	cargo build --release --target $(MUSL_TARGET) -j $(JOBS) -p opencraylspd -p opencraylsp-mcp
	mkdir -p $(DIST)
	rm -f $(DIST)/opencraylspd $(DIST)/opencraylsp-mcp
	cp $(TARGET_DIR)/$(MUSL_TARGET)/release/opencraylspd $(DIST)/opencraylspd
	cp $(TARGET_DIR)/$(MUSL_TARGET)/release/opencraylsp-mcp $(DIST)/opencraylsp-mcp
	@for bin in $(DIST)/opencraylspd $(DIST)/opencraylsp-mcp; do \
		file "$$bin" | grep -Eq 'statically linked|static-pie' || { \
			echo "error: $$bin is not a static executable" >&2; file "$$bin" >&2; exit 1; }; \
	done
	@$(DIST)/opencraylspd version
	@$(DIST)/opencraylsp-mcp --version

.PHONY: docs-check
docs-check:
	@scripts/check-docs.sh
	@scripts/check-tools-doc.sh

# --- install ----------------------------------------------------------------
# `make install` builds the two programs in release mode and copies them into
# $(DESTDIR)$(PREFIX)/bin. PREFIX defaults to ~/.local, so a plain install puts
# them on the PATH of a user-local setup. DESTDIR is the usual staging prefix
# for packagers; it is prepended, never substituted.
#
#   make install                     -> ~/.local/bin/{opencraylspd,opencraylsp-mcp}
#   make install PREFIX=/usr/local   -> /usr/local/bin/{...}
#   make install DESTDIR=/tmp/stage  -> /tmp/stage$(PREFIX)/bin/{...}
#
# The licence files go to $(PREFIX)/share/doc/opencraylsp, taken from the single list
# in scripts/doc-files.sh and removed by `make uninstall` with the programs.
#
# The pair is staged in a `mktemp -d` directory inside the destination, not at a
# name derived from the pid: anything able to write to the destination could
# predict such a name, leave a symlink there, and have `install` write through
# it. Same reasoning, and the same fix, as scripts/install.sh.
PREFIX  ?= $(HOME)/.local
BINDIR  ?= $(PREFIX)/bin
DESTDIR ?=

# The licence files that have to travel with the programs, from the single list
# in scripts/doc-files.sh. `make install` is by definition run in a checkout, so
# unlike scripts/install.sh it can read that file.
DOC_SUBDIR := share/doc/opencraylsp
DOC_SHARE  := $(DESTDIR)$(PREFIX)/share
DOCDIR    := $(DESTDIR)$(PREFIX)/$(DOC_SUBDIR)
DOC_FILES := $(shell . scripts/doc-files.sh 2>/dev/null && echo "$$OPENCRAYLSP_DOC_FILES")
ifeq ($(strip $(DOC_FILES)),)
$(error scripts/doc-files.sh did not list any licence files; \`make install\` would ship a binary with none)
endif

.PHONY: install uninstall install-docs uninstall-docs
install:
	cargo build --release --locked -j $(JOBS) -p opencraylspd -p opencraylsp-mcp
	@set -eu; \
	case "$(PREFIX)" in \
	  /bin|/sbin|*/bin|*/sbin) echo "error: PREFIX=$(PREFIX) ends in bin or sbin, so this would install into $(BINDIR)/bin; set PREFIX to the parent directory" >&2; exit 1;; \
	esac; \
	d="$(DESTDIR)$(BINDIR)"; \
	case "$$d" in ""|/bin|/sbin|/usr/bin|/usr/sbin) echo "error: refusing to install into '$$d' (set PREFIX, e.g. PREFIX=/usr/local, or DESTDIR when packaging)" >&2; exit 1;; esac; \
	install -d "$$d"; \
	s="$$(mktemp -d "$$d/.opencraylsp.install.XXXXXX")"; a="$$s/opencraylspd"; b="$$s/opencraylsp-mcp"; \
	trap 'rm -rf "$$s"' EXIT INT TERM; \
	install -m 0755 "$(TARGET_DIR)/release/opencraylspd" "$$a"; \
	install -m 0755 "$(TARGET_DIR)/release/opencraylsp-mcp" "$$b"; \
	av=$$("$$a" --version 2>/dev/null | head -n1); av=$${av#* }; \
	bv=$$("$$b" --version 2>/dev/null | head -n1); bv=$${bv#* }; \
	if [ -z "$$av" ] || [ -z "$$bv" ]; then echo "error: staged binaries do not run" >&2; exit 1; fi; \
	if [ "$$av" != "$$bv" ]; then echo "error: version mismatch: opencraylspd $$av vs opencraylsp-mcp $$bv; refusing to install a mixed pair" >&2; exit 1; fi; \
	mv -f "$$a" "$$d/opencraylspd"; \
	mv -f "$$b" "$$d/opencraylsp-mcp"; \
	rm -rf "$$s"; \
	trap - EXIT INT TERM; \
	echo "installed opencraylspd and opencraylsp-mcp ($$av) into $$d"
	@$(MAKE) --no-print-directory install-docs
	@echo "next: claude mcp add opencraylsp -- opencraylsp-mcp --languages auto"
	@$(MAKE) --no-print-directory install-lsp LSP_LANGS="$(LSP_LANGS)" LSP_FROM_INSTALL=1

# Its own targets, so that installing the licence files and removing them again
# can be tested without building the workspace in release mode first. `install`
# and `uninstall` are the only callers; the licence set does not change shape
# between the two paths.
install-docs:
	@set -eu; for f in $(DOC_FILES); do \
	  test -f "$$f" || { echo "error: $$f is listed in scripts/doc-files.sh but not in this checkout" >&2; exit 1; }; \
	  install -d "$(DOCDIR)/$$(dirname "$$f")"; \
	  install -m 0644 "$$f" "$(DOCDIR)/$$f"; \
	done
	@echo "installed the licence files into $(DOCDIR)"

uninstall-docs:
	@case "$(DOCDIR)" in /share/doc/opencraylsp) echo "error: refusing to remove $(DOCDIR)" >&2; exit 1;; esac
	@if [ -d "$(DOCDIR)" ] && [ ! -L "$(DOCDIR)" ]; then \
	  rm -rf "$(DOCDIR)"; \
	  rmdir "$(dir $(DOCDIR))" 2>/dev/null || true; \
	  rmdir "$(DOC_SHARE)" 2>/dev/null || true; \
	  echo "removed the licence files from $(DOCDIR)"; \
	fi

# --- language servers -------------------------------------------------------
# No language server is installed unless you ask for it:
#
#   make install                -> programs, then a menu (terminal only)
#   make install php js         -> programs + intelephense + the TS/JS server
#   make install-lsp go rust    -> language servers only, no rebuild
#
# Languages: rust, go, js (alias ts), php, python. The words are no-op goals so
# they can follow `install` on the command line.
LSP_WORDS  := rust go js ts php python
LSP_LANGS  ?= $(filter $(LSP_WORDS),$(MAKECMDGOALS))

.PHONY: install-lsp $(LSP_WORDS)
$(LSP_WORDS):
	@:

install-lsp:
	@sh scripts/install-lsp.sh $(LSP_LANGS)

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/opencraylspd" "$(DESTDIR)$(BINDIR)/opencraylsp-mcp"
	@$(MAKE) --no-print-directory uninstall-docs
	@echo "removed opencraylspd and opencraylsp-mcp from $(DESTDIR)$(BINDIR)"

