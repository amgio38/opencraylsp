# The files that have to travel with the programs.
#
# Apache-2.0 section 4(a) requires a copy of the License with every
# redistribution, and 4(d) requires carrying the NOTICE of any work that has
# one.  A binary in $BINDIR is a redistribution, so the licence cannot live only
# in the repository: install.sh, `make install` and the release tarball all have
# to put these under $PREFIX/share/doc/opencraylsp, next to what they are about.
#
# This file is the single list.  install.sh builds the tar member allowlist from
# it, uninstall.sh removes exactly it, the Makefile installs and removes it, and
# the release workflow tars exactly it -- so a licence file cannot quietly stop
# shipping in one of those and keep shipping in the others.
#
# Sourced, not executed, so the callers choose the interpretation.  POSIX sh
# only, so the release workflow and the Makefile can read it too.
#
#   OPENCRAYLSP_DOC_SUBDIR   where under $PREFIX the files land
#   OPENCRAYLSP_DOC_FILES    newline-separated paths, relative to the repository root

OPENCRAYLSP_DOC_SUBDIR='share/doc/opencraylsp'

# Our own licence, the notice the Apache-licensed dependency requires, the
# human-readable inventory, and then the texts and the machine-readable
# inventory that inventory points at.
OPENCRAYLSP_DOC_FILES='LICENSE
NOTICE
THIRD-PARTY-LICENSES.md
licenses/Apache-2.0.txt
licenses/MIT.txt
licenses/Unicode-3.0.txt
licenses/deps.tsv'
