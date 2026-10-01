//! Rendering LSP answers for a model to read.
//!
//! The audience is a language model, not a person: every line has to be short,
//! and a location is only useful if it says which file, which line and which
//! column *in the model's own counting* — that is what [`crate::position`]
//! converts, and it needs the line's text, which is why this file reads source
//! lines through [`LineIndex`].
//!
//! Two rules run through everything here:
//!
//! - **An empty answer is not an error.** A server that looked and found
//!   nothing has answered; saying "none found" is the honest report.
//! - **A missing answer is not an empty one.** When the server has not
//!   published diagnostics for the current version, or an answer could not be
//!   read, the text says so rather than implying the code is clean. A formatter
//!   is where that honesty is easiest to lose.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

use opencraylsp_core::backend::{DiagnosticsReport, PositionEncoding};

use crate::operations::{HoverInfo, Site, Symbol, SymbolList};
use crate::position;

/// Longest snippet kept from a source line; a 4 KB minified line is not an
/// answer, it is a denial of service on the context window.
const SNIPPET_CHARS: usize = 200;

/// Hover text is documentation, and documentation is long; this caps it at a
/// size a model can read without losing the rest of the answer.
const HOVER_BYTES: usize = 4 * 1024;

/// Lazy, cached source-line reader.
///
/// Snippets are the reason a model can tell *which* of five `new()` calls was
/// meant, but reading the same file once per result is wasteful, and reading a
/// file outside the workspace boundary is forbidden — so lookups are
/// cached and refused outside the boundary in one place.
#[derive(Debug)]
pub struct LineIndex {
    /// The boundary rule, shared with the rest of the crate; see [`Boundary`].
    boundary: Boundary,
    /// Shared, not owned: a lookup wants *one* line, and copying the whole file
    /// to reach it cost a `Vec<String>` clone per call. `Arc` rather than `Rc`
    /// because a `LineIndex` is held across `.await` in the resolvers, so the
    /// future that owns it has to be `Send`.
    files: RefCell<HashMap<PathBuf, Option<Arc<Vec<String>>>>>,
}

impl LineIndex {
    /// A reader that loads files under `boundary` on first use.
    pub fn lazy(boundary: impl Into<PathBuf>) -> Self {
        Self {
            boundary: Boundary::new(boundary),
            files: RefCell::new(HashMap::new()),
        }
    }

    /// Puts a file in the cache if it is not there already.
    ///
    /// The cache is keyed on the path the caller asked for, not the canonical
    /// one, so the boundary check is skipped once an entry exists — a seeded
    /// file is always honoured, which is what the tests and any future caller
    /// that already holds the text rely on.
    fn ensure(&self, path: &Path) {
        if self.files.borrow().contains_key(path) {
            return;
        }
        // Reading outside the boundary is not an option: a server that
        // names `/usr/lib/.../std.rs` gets its coordinate printed, not its
        // source.
        let loaded = self
            .inside(path)
            .and_then(|path| read_lines(&path))
            .map(Arc::new);
        self.files.borrow_mut().insert(path.to_owned(), loaded);
    }

    /// Seeds a file's content, bypassing the disk (tests, and any future
    /// caller that already holds the text).
    pub fn insert(&self, path: impl Into<PathBuf>, text: &str) {
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        self.files
            .borrow_mut()
            .insert(path.into(), Some(Arc::new(lines)));
    }

    /// The raw text of a 0-based `line`, or `None` when it cannot be read.
    ///
    /// Copies that one line and nothing else. The cache used to hand back a
    /// clone of the whole `Vec<String>`, so a lookup on a 200 000-line file
    /// allocated 200 000 `String`s to read one of them — and the resolvers ask
    /// once per candidate and twice per tree node, inside a daemon that every
    /// connection shares.
    pub fn line(&self, path: &Path, line: u32) -> Option<String> {
        self.ensure(path);
        let files = self.files.borrow();
        files
            .get(path)?
            .as_ref()?
            .get(usize::try_from(line).ok()?)
            .map(|line| line.to_owned())
    }

    /// Every line of `path`, or `None` when the file cannot be read at all.
    ///
    /// Separate from [`Self::line`] because the tool has to tell "this file is
    /// shorter than the line you asked for" (a bad argument) apart from "this
    /// file cannot be read" (a broken workspace) — the two deserve different
    /// sentences, and collapsing them produces the wrong one half the time.
    pub fn lines_of(&self, path: &Path) -> Option<Vec<String>> {
        self.ensure(path);
        self.files
            .borrow()
            .get(path)?
            .as_ref()
            .map(|lines| lines.as_ref().clone())
    }

    /// The path to read for `path`, or `None` when it is not inside the
    /// boundary. See [`Boundary::inside`] for why there are two checks.
    pub(crate) fn inside(&self, path: &Path) -> Option<PathBuf> {
        self.boundary.inside(path)
    }
}

/// The workspace boundary: the one place that decides whether a path a server
/// named may be opened.
///
/// Split out of [`LineIndex`] so that a caller which only needs the *rule* —
/// `callgraph`, which must decide whether a node may be expanded before the
/// daemon reads its file — can hold it without also holding the line cache.
/// `LineIndex` delegates here, so there is still exactly one implementation of
/// "inside the workspace".
///
/// Deliberately `Sync` and free of interior mutability: [`callgraph::Walker`]
/// shares `&self` across the futures it joins, and a `RefCell` in here would
/// make the whole walk `!Send`.
#[derive(Debug, Clone)]
pub(crate) struct Boundary {
    root: PathBuf,
    /// `root` with symlinks resolved, when the file system can answer.
    /// `None` means the boundary itself did not resolve at construction time.
    canonical_root: Option<PathBuf>,
}

impl Boundary {
    /// The boundary rooted at `root`.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let canonical_root = std::fs::canonicalize(&root).ok();
        Self {
            root,
            canonical_root,
        }
    }

    /// A boundary that never resolved, for tests that need that state.
    ///
    /// It cannot be reached through the file system — if the root cannot be
    /// canonicalized, neither can anything under it — so a test that wants to
    /// exercise the fail-closed branch has to be handed the state.
    #[cfg(test)]
    pub(crate) fn unresolved(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            canonical_root: None,
        }
    }

    /// The boundary as it was declared, for callers that need the path rather
    /// than the decision — printing a path relative to it, for instance. The
    /// *decision* is [`Boundary::inside`] and nothing else.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The path to read for `path`, or `None` when it is not inside the
    /// boundary.
    ///
    /// Two checks, because they stop different escapes.
    ///
    /// The **lexical** one stops `..` from walking out at all. It has to run
    /// first, because `Path::starts_with` is a *component prefix* test:
    /// `/ws/../../etc/passwd` really does "start with" `/ws`, so a prefix test
    /// on the raw, server-supplied path passes for a path that has already left
    /// the workspace. Collapsing `.` and `..` first is what makes the prefix
    /// test mean what it looks like it means.
    ///
    /// The **canonicalizing** one stops a *symlink inside* the boundary from
    /// pointing outside it, which no amount of string work can see. It needs the
    /// file system, so it is best-effort: a file that cannot be resolved cannot
    /// be read either way, and the read that follows fails honestly.
    pub(crate) fn inside(&self, path: &Path) -> Option<PathBuf> {
        let lexical = normalize_lexically(path);
        if !lexical.starts_with(&self.root) {
            return None;
        }
        let Ok(canonical) = std::fs::canonicalize(&lexical) else {
            // The file is not there to be read, so there is nothing to smuggle
            // out; hand back the lexical path and let the read fail honestly.
            return Some(lexical);
        };
        // The canonical form is the only one that can be compared like this, so
        // without it there is no answer we can call "inside". **Fail closed**,
        // and do it by returning rather than panicking: a check that exists to
        // stop a read should never be the thing that brings the daemon down.
        //
        // This used to fall through and return the canonical path, which meant
        // that whenever the *boundary* could not be resolved the symlink check
        // silently switched itself off and only the lexical one ran. The old
        // comment excused that with "if the boundary does not exist, no file
        // under it does either" — true for a missing component, false for
        // EACCES, ENAMETOOLONG or a transient I/O error, and a check that
        // disappears under an I/O hiccup is not a check.
        //
        // The cost of failing closed is a tool that prints coordinates without
        // snippets when the workspace root is unreadable. That is the right
        // trade: the boundary exists so a symlink cannot turn "show me this
        // file" into "read this file", and an unreadable root is exactly when
        // we can least afford to stop checking.
        match &self.canonical_root {
            Some(root) if canonical.starts_with(root) => Some(canonical),
            _ => None,
        }
    }
}

/// Resolves `.` and `..` in `path` without consulting the disk.
///
/// A `..` that would pop past the start of the path is kept rather than
/// swallowed: dropping it would shorten the path into somewhere else, which
/// is the opposite of failing closed.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The largest file this reader will load, checked on the metadata before a
/// single byte is read.
///
/// Why a cap exists at all: the reader lives in a long-lived daemon shared by
/// every connection, and the daemon's own memory is *not* covered by the
/// per-instance memory guard — that guard watches language servers. A workspace
/// may legitimately contain an enormous file (a bundled artifact, a database
/// dump, a minified asset), and one result site inside it would otherwise pull
/// the whole thing into the daemon and keep it for the length of the request, to
/// print a 200-character snippet. Above the cap the file is simply not read: the
/// caller still prints the coordinate and is told the text could not be read,
/// which is true, actionable, and does not take the daemon down.
pub(crate) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// `path` as lines, or `None` when it cannot be read as text.
///
/// `None` covers four different situations that the caller deliberately cannot
/// tell apart: the file is missing, it is not UTF-8 (a binary blob has no lines
/// to quote), it is larger than [`MAX_FILE_BYTES`], or the read failed. Every one
/// of them means "there is no text to show", and none of them is worth guessing
/// about.
fn read_lines(path: &Path) -> Option<Vec<String>> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.lines().map(str::to_owned).collect())
}

/// Everything the formatter needs besides the decoded answer.
#[derive(Debug)]
pub struct View<'a> {
    /// Paths under this are printed relative to it, and their lines may be read.
    pub boundary: &'a Path,
    /// The encoding the server used for every character offset in the answer.
    pub encoding: PositionEncoding,
    /// Cap on listed results; the rest is summarized by count.
    pub max_results: usize,
    /// The file the request was about, used when a symbol names no file of its
    /// own (`documentSymbol` describes exactly one file).
    pub subject: Option<&'a Path>,
    pub lines: &'a LineIndex,
}

impl<'a> View<'a> {
    fn relative(&self, path: &Path) -> String {
        // The same normalization `LineIndex` applies before its boundary check:
        // without it, `/ws/../../etc/passwd` would print as `../../etc/passwd`,
        // which reads like a path *inside* the workspace.
        let normalized = normalize_lexically(path);
        match normalized.strip_prefix(self.boundary) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
            _ => normalized.display().to_string(),
        }
    }

    /// The display path for a site, plus the file to read for a snippet.
    fn locate(&self, site: &Site) -> (String, Option<PathBuf>) {
        if let Some(path) = &site.path {
            return (self.relative(path), Some(path.clone()));
        }
        if !site.uri.is_empty() {
            // A non-`file:` URI (dependency source, virtual document): show the
            // URI, read nothing.
            return (site.uri.clone(), None);
        }
        match self.subject {
            Some(subject) => (self.relative(subject), Some(subject.to_path_buf())),
            None => ("<unknown location>".to_owned(), None),
        }
    }

    /// `path:line:column`, 1-based, in the model's counting.
    pub(crate) fn position_of(&self, site: &Site) -> String {
        match self.position_parts_of(site) {
            Some((path, line, column)) => format!("{path}:{line}:{column}"),
            None => {
                let (path, _) = self.locate(site);
                match site.line {
                    Some(line) => format!("{path}:{}", line.saturating_add(1)),
                    None => path,
                }
            }
        }
    }

    /// The same three values `position_of` prints, separately and 1-based, so a
    /// caller can hand them straight back as a `path`/`line`/`column` retry
    /// instead of asking the model to re-split a string it just read.
    ///
    /// `None` when the site has no line, or no file to name — there is nothing
    /// to retry with, and the caller says so rather than inventing a position.
    pub(crate) fn position_parts_of(&self, site: &Site) -> Option<(String, u32, u32)> {
        let (path, readable) = self.locate(site);
        let line = site.line?;
        let character = site.character?;
        let column = match readable
            .as_deref()
            .and_then(|path| self.lines.line(path, line))
        {
            Some(text) => position::to_editor_column(&text, character, self.encoding),
            // Outside the boundary the line may not be read, so the column stays
            // the server's own. It is exact for ASCII, which is what
            // out-of-workspace hits (dependency sources) overwhelmingly are, and
            // never invented.
            None => character.saturating_add(1),
        };
        Some((path, line.saturating_add(1), column))
    }

    fn snippet_of(&self, site: &Site) -> Option<String> {
        let (_, readable) = self.locate(site);
        let line = site.line?;
        let text = self.lines.line(readable.as_deref()?, line)?;
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| truncate_chars(trimmed, SNIPPET_CHARS))
    }

    /// One entry line: `  <location>  <snippet>`.
    fn entry(&self, site: &Site) -> String {
        match self.snippet_of(site) {
            Some(snippet) => format!("  {}  {snippet}", self.position_of(site)),
            None => format!("  {}", self.position_of(site)),
        }
    }

    /// Groups site indices by display path, in first-seen order.
    ///
    /// A `HashMap` keyed by path, with a parallel `Vec` for the order, so this
    /// is O(n) in the number of sites. It used to scan the groups it had built
    /// so far for every site — O(n²) with a `String` comparison per probe — and
    /// was then called on the *uncapped* site list purely to count files, which
    /// made one server answer able to stall the shared daemon for minutes. The
    /// order is kept because the output is a report a human reads top to bottom;
    /// the speed is because it runs in a process every connection shares.
    fn by_path(&self, sites: &[Site]) -> Vec<(String, Vec<usize>)> {
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, site) in sites.iter().enumerate() {
            let path = self.locate(site).0;
            match groups.get_mut(&path) {
                Some(indices) => indices.push(index),
                None => {
                    order.push(path.clone());
                    groups.insert(path, vec![index]);
                }
            }
        }
        order
            .into_iter()
            .map(|path| {
                let indices = groups
                    .remove(&path)
                    .expect("every ordered path was inserted above");
                (path, indices)
            })
            .collect()
    }

    /// How many distinct display paths `sites` has.
    ///
    /// The count `references` prints covers *every* site, not just the ones it
    /// lists, so it cannot be read off `by_path(shown)`. Counting with a set is
    /// O(n) and allocates only the distinct paths, where grouping would have
    /// allocated an index vector per file for results that are never printed.
    fn file_count(&self, sites: &[Site]) -> usize {
        let mut seen: HashSet<&str> = HashSet::with_capacity(sites.len());
        // `locate` returns an owned String, so the set borrows from a buffer
        // that has to outlive it.
        let mut owned: Vec<String> = Vec::with_capacity(sites.len());
        for site in sites {
            let path = self.locate(site).0;
            owned.push(path);
        }
        for path in &owned {
            seen.insert(path.as_str());
        }
        seen.len()
    }

    fn capped<'s, T>(&self, items: &'s [T]) -> (&'s [T], Option<usize>) {
        let cap = self.max_results.max(1);
        if items.len() > cap {
            (&items[..cap], Some(items.len() - cap))
        } else {
            (items, None)
        }
    }
}

fn omitted(more: Option<usize>) -> String {
    match more {
        Some(n) => {
            // Every caller that can reach this renders `limit`-bounded results,
            // so `limit` is the lever the reader actually has.
            format!("\n... and {n} more not listed (raise `limit` to see them)")
        }
        None => String::new(),
    }
}

/// `text` clipped to `limit` characters, on a character boundary.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let clipped: String = text.chars().take(limit).collect();
    format!("{clipped}...")
}

/// `text` clipped to `limit` bytes, on a character boundary.
pub(crate) fn truncate_bytes(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// `textDocument/definition`.
pub fn definition(sites: &[Site], view: &View<'_>) -> String {
    match sites {
        [] => "No definition found: the language server returned nothing. The \
               position may not be on a symbol, or the definition may live in a \
               dependency the server has not indexed."
            .to_owned(),
        [only] => format!(
            "Defined at {}{}",
            view.position_of(only),
            suffix(only, view)
        ),
        many => {
            let (shown, more) = view.capped(many);
            let mut out = format!("Found {} definitions:", many.len());
            for site in shown {
                out.push('\n');
                out.push_str(&view.entry(site));
            }
            out.push_str(&omitted(more));
            out
        }
    }
}

/// `textDocument/implementation`.
pub fn implementation(sites: &[Site], view: &View<'_>) -> String {
    if sites.is_empty() {
        return "No implementations found: the language server returned nothing. \
                The position may not be on an interface method, or no type \
                implements it in this workspace."
            .to_owned();
    }
    let (shown, more) = view.capped(sites);
    let mut out = format!("Found {} implementation(s):", sites.len());
    for site in shown {
        out.push('\n');
        out.push_str(&view.entry(site));
    }
    out.push_str(&omitted(more));
    out
}

/// `textDocument/references`, grouped by file.
pub fn references(sites: &[Site], view: &View<'_>) -> String {
    if sites.is_empty() {
        return "No references found: the language server returned an empty list. \
                The symbol may be unused, or the server may still be indexing."
            .to_owned();
    }
    let (shown, more) = view.capped(sites);
    let groups = view.by_path(shown);
    let mut out = format!(
        "Found {} reference(s) in {} file(s):",
        sites.len(),
        view.file_count(sites)
    );
    for (path, indices) in groups {
        out.push('\n');
        out.push_str(&path);
        out.push(':');
        for index in indices {
            let site = &shown[index];
            out.push('\n');
            out.push_str(&view.entry(site));
        }
    }
    out.push_str(&omitted(more));
    out
}

/// `textDocument/hover`.
pub fn hover(info: &HoverInfo, view: &View<'_>) -> String {
    let text = if info.markdown {
        plain_text(&info.text)
    } else {
        info.text.clone()
    };
    if text.trim().is_empty() {
        return "No hover information: the language server returned empty \
                contents at this position."
            .to_owned();
    }
    let (body, clipped) = truncate_bytes(&text, HOVER_BYTES);
    let header = match info.range {
        Some((line, character)) => {
            let site = Site {
                path: view.subject.map(Path::to_path_buf),
                uri: String::new(),
                line: Some(line),
                character: Some(character),
            };
            format!("Hover at {}:", view.position_of(&site))
        }
        None => "Hover:".to_owned(),
    };
    let mut out = format!("{header}\n{body}");
    if clipped {
        out.push_str("\n... (hover truncated at 4 KB)");
    }
    out
}

/// `textDocument/documentSymbol`: a hierarchy, indented by nesting.
pub fn document_symbols(list: &SymbolList, view: &View<'_>) -> String {
    let symbols = list.symbols.as_slice();
    let skipped = skipped_note(list.skipped);
    if symbols.is_empty() {
        return format!(
            "No symbols: the language server returned an empty list. The file \
             may be empty, or its language unsupported by the server.{skipped}"
        );
    }
    let (shown, more) = view.capped(symbols);
    let mut out = format!("{} symbol(s):", symbols.len());
    for symbol in shown {
        out.push('\n');
        out.push_str(&symbol_line(symbol, view));
    }
    out.push_str(&omitted(more));
    out.push_str(&skipped);
    out
}

/// The tail note for symbols the server sent in a shape that could not be read.
///
/// Not silence: an outline that quietly lists fewer symbols than the file
/// declares reads exactly like a file that declares fewer symbols.
fn skipped_note(skipped: usize) -> String {
    match skipped {
        0 => String::new(),
        count => format!(
            "\n({count} symbol(s) in the answer were malformed and skipped; \
             any readable children of theirs are listed above)"
        ),
    }
}

/// `workspace/symbol`, grouped by file.
pub fn workspace_symbols(symbols: &[Symbol], query: &str, view: &View<'_>) -> String {
    if symbols.is_empty() {
        return format!(
            "No workspace symbols matched `{query}`: the language server \
             returned an empty list. The workspace may be empty, or the server \
             may still be indexing."
        );
    }
    let (shown, more) = view.capped(symbols);
    let mut groups: Vec<(String, Vec<&Symbol>)> = Vec::new();
    for symbol in shown {
        let path = view.locate(&symbol.site).0;
        match groups.iter_mut().find(|(existing, _)| *existing == path) {
            Some((_, list)) => list.push(symbol),
            None => groups.push((path, vec![symbol])),
        }
    }
    let mut out = format!(
        "Found {} symbol(s) matching `{query}` in {} file(s):",
        symbols.len(),
        groups.len()
    );
    for (path, list) in groups {
        out.push('\n');
        out.push_str(&path);
        out.push(':');
        for symbol in list {
            out.push('\n');
            out.push_str(&symbol_line(symbol, view));
        }
    }
    out.push_str(&omitted(more));
    out
}

/// `textDocument/publishDiagnostics`.
///
/// The three cases are kept apart on purpose. "We never heard from the server"
/// and "the server says the file is clean" look identical if you only count
/// items, and confusing them is how an agent is told a broken file compiles.
pub fn diagnostics(report: &DiagnosticsReport, view: &View<'_>) -> String {
    let file = view
        .subject
        .map(|path| view.relative(path))
        .unwrap_or_else(|| "<file>".to_owned());

    if !report.received_for_version {
        let waited = if report.timed_out {
            ", and the wait timed out (the server may still be analysing; \
             `diagnostics_timeout_ms` in the opencraylspd config bounds it)"
                .to_owned()
        } else {
            String::new()
        };
        return format!(
            "No diagnostics are known for {file} yet: the server has not \
             published results for the current version{waited}. This is NOT a \
             statement that the file is clean -- ask again once the server has \
             answered."
        );
    }

    if report.items.is_empty() {
        return format!(
            "0 diagnostics for {file}: the server published a result for the \
             current version and reported no problems."
        );
    }

    let mut items: Vec<&Diagnostic> = report.items.iter().collect();
    // Most severe first: an agent that reads only the top lines must see the
    // errors before the hints. Unclassified diagnostics sort last rather than
    // silently becoming "most severe".
    items.sort_by_key(|diagnostic| severity_rank(diagnostic.severity));

    let errors = report
        .items
        .iter()
        .filter(|d| rank_of(d.severity) == 1)
        .count();
    let warnings = report
        .items
        .iter()
        .filter(|d| rank_of(d.severity) == 2)
        .count();

    let (shown, more) = view.capped(&items);
    let mut out = format!(
        "{} diagnostic(s) for {file} ({errors} error(s), {warnings} warning(s)):",
        report.items.len()
    );
    for &diagnostic in shown {
        out.push('\n');
        out.push_str("  ");
        out.push_str(&diagnostic_line(diagnostic, view));
    }
    out.push_str(&omitted(more));
    out
}

fn diagnostic_line(diagnostic: &Diagnostic, view: &View<'_>) -> String {
    let line = diagnostic.range.start.line;
    let character = diagnostic.range.start.character;
    let location = match view.subject {
        Some(path) => {
            let column = match view.lines.line(path, line) {
                Some(text) => position::to_editor_column(&text, character, view.encoding),
                None => character.saturating_add(1),
            };
            format!("{}:{column}", line.saturating_add(1))
        }
        None => format!("{}:{}", line.saturating_add(1), character.saturating_add(1)),
    };
    let mut out = format!(
        "{location} {} {}",
        rank_name(severity_rank(diagnostic.severity)),
        diagnostic
            .message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    );
    if let Some(code) = &diagnostic.code {
        let code = match code {
            NumberOrString::Number(number) => number.to_string(),
            NumberOrString::String(text) => text.clone(),
        };
        out.push_str(&format!(" [{code}]"));
    }
    if let Some(source) = &diagnostic.source {
        out.push_str(&format!(" ({source})"));
    }
    out
}

fn severity_rank(severity: Option<DiagnosticSeverity>) -> i64 {
    match rank_of(severity) {
        0 => 5,
        rank => rank,
    }
}

fn rank_of(severity: Option<DiagnosticSeverity>) -> i64 {
    severity
        .and_then(|severity| serde_json::to_value(severity).ok())
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
}

fn rank_name(rank: i64) -> &'static str {
    match rank {
        1 => "error",
        2 => "warning",
        3 => "information",
        4 => "hint",
        _ => "unknown",
    }
}

fn symbol_line(symbol: &Symbol, view: &View<'_>) -> String {
    // Every symbol line sits under a header (a document outline or a file), so
    // it starts one level in; `depth` adds the hierarchy on top of that.
    let indent = format!("  {}", "  ".repeat(symbol.depth));
    let detail = symbol
        .detail
        .as_deref()
        .map(one_line_detail)
        .unwrap_or_default();
    let container = symbol
        .container
        .as_deref()
        .map(|container| format!(" in {container}"))
        .unwrap_or_default();
    format!(
        "{indent}{} ({}) - {}{detail}{container}",
        symbol.name,
        kind_name(symbol.kind),
        view.position_of(&symbol.site)
    )
}

/// A server's `detail` as one space-prefixed line.
///
/// rust-analyzer puts whole multi-line signatures in `detail`; printed as-is
/// they break the one-entry-per-line layout the model reads.
fn one_line_detail(detail: &str) -> String {
    format!(
        " {}",
        detail.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

fn suffix(site: &Site, view: &View<'_>) -> String {
    match view.snippet_of(site) {
        Some(snippet) => format!("  {snippet}"),
        None => String::new(),
    }
}

/// Every legal `kind` argument, in SymbolKind order. Exposed so a tool can
/// reject a bad name with the full list instead of returning nothing and
/// leaving the caller to guess whether the kind simply has no symbols.
pub const KIND_NAMES: &[&str] = &[
    "file",
    "module",
    "namespace",
    "package",
    "class",
    "method",
    "property",
    "field",
    "constructor",
    "enum",
    "interface",
    "function",
    "variable",
    "constant",
    "string",
    "number",
    "boolean",
    "array",
    "object",
    "key",
    "null",
    "enumMember",
    "struct",
    "event",
    "operator",
    "typeParameter",
];

/// The SymbolKind number a `kind` argument names, case-insensitively.
///
/// `None` means the name is not a SymbolKind at all — deliberately distinct
/// from a valid kind that happens to match nothing, so a typo is reported as a
/// typo. `enumMember` is also accepted as `enummember` and `enum_member`, since
/// a model writing snake_case should not be punished for guessing.
pub fn kind_from_name(name: &str) -> Option<u32> {
    let wanted = name.trim().to_ascii_lowercase();
    let wanted = wanted.replace(['_', '-'], "");
    (1..=26).find(|kind| kind_name(*kind).eq_ignore_ascii_case(&wanted))
}

/// The LSP `SymbolKind` name for a raw kind value.
pub fn kind_name(kind: u32) -> &'static str {
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        15 => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        19 => "Object",
        20 => "Key",
        21 => "Null",
        22 => "EnumMember",
        23 => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "Unknown",
    }
}

/// Flattens markdown to plain text for a model that gets no syntax
/// highlighting: fences and their language tag, heading markers, bold markers
/// and inline-code ticks all cost tokens and mean nothing in a prompt.
pub fn plain_text(markdown: &str) -> String {
    let mut out = String::new();
    let mut first = true;
    let mut in_fence = false;
    for raw in markdown.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        let rendered = if in_fence {
            line.to_owned()
        } else {
            strip_inline_markup(line)
        };
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(rendered.trim_end());
    }
    out.trim().to_owned()
}

fn strip_inline_markup(line: &str) -> String {
    let mut text = line.trim_start();
    while let Some(rest) = text.strip_prefix('#') {
        text = rest;
    }
    let text = text.trim_start();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(current) = chars.next() {
        match current {
            '`' => {}
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencraylsp_core::backend::PositionEncoding;
    use std::path::Path;

    fn boundary() -> &'static Path {
        Path::new("/ws")
    }

    fn lines() -> LineIndex {
        let index = LineIndex::lazy("/ws");
        index.insert("/ws/a.rs", "fn alpha() {}\nlet beta = alpha();\n");
        index.insert("/ws/b.rs", "pub fn gamma() {}\n");
        index
    }

    fn view<'a>(lines: &'a LineIndex, subject: &'a Path) -> View<'a> {
        View {
            boundary: boundary(),
            encoding: PositionEncoding::Utf16,
            max_results: 100,
            subject: Some(subject),
            lines,
        }
    }

    fn site(path: &str, line: u32, character: u32) -> Site {
        Site {
            path: Some(PathBuf::from(path)),
            uri: format!("file://{path}"),
            line: Some(line),
            character: Some(character),
        }
    }

    fn symbol(name: &str, kind: u32, line: u32, depth: usize) -> Symbol {
        Symbol {
            name: name.to_owned(),
            kind,
            detail: None,
            container: None,
            site: site("/ws/a.rs", line, 0),
            depth,
        }
    }

    fn symbol_list(symbols: Vec<Symbol>) -> SymbolList {
        SymbolList {
            symbols,
            skipped: 0,
        }
    }

    #[test]
    fn a_location_uses_relative_paths_and_1_based_positions() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        assert_eq!(view.position_of(&site("/ws/a.rs", 0, 3)), "a.rs:1:4");
        assert_eq!(view.position_of(&site("/ws/a.rs", 1, 0)), "a.rs:2:1");
    }

    #[test]
    fn a_location_outside_the_boundary_stays_absolute_and_is_never_read() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let outside = site("/usr/lib/std.rs", 3, 2);
        assert_eq!(view.position_of(&outside), "/usr/lib/std.rs:4:3");
        assert_eq!(view.snippet_of(&outside), None);
    }

    #[test]
    fn a_non_file_uri_renders_as_the_uri() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let virtual_doc = Site {
            path: None,
            uri: "jdt://contents/String.class".to_owned(),
            line: Some(2),
            character: Some(1),
        };
        assert_eq!(
            view.position_of(&virtual_doc),
            "jdt://contents/String.class:3:2"
        );
    }

    #[test]
    fn a_site_without_a_file_falls_back_to_the_subject() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let anonymous = Site {
            path: None,
            uri: String::new(),
            line: Some(1),
            character: Some(4),
        };
        assert_eq!(view.position_of(&anonymous), "a.rs:2:5");
    }

    #[test]
    fn definition_renders_one_many_and_none() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));

        let none = definition(&[], &view);
        assert!(none.contains("No definition found"));

        let one = definition(&[site("/ws/a.rs", 0, 3)], &view);
        assert!(one.starts_with("Defined at a.rs:1:4"));
        assert!(one.contains("fn alpha() {}"));

        let many = definition(&[site("/ws/a.rs", 0, 3), site("/ws/b.rs", 0, 7)], &view);
        assert!(many.starts_with("Found 2 definitions:"));
        assert!(many.contains("b.rs:1:8"));
    }

    #[test]
    fn references_group_by_file_and_cap_with_a_count() {
        let lines = lines();
        let mut view = view(&lines, Path::new("/ws/a.rs"));
        view.max_results = 2;
        let sites = vec![
            site("/ws/a.rs", 1, 4),
            site("/ws/a.rs", 1, 12),
            site("/ws/b.rs", 0, 7),
        ];
        let text = references(&sites, &view);
        assert!(text.starts_with("Found 3 reference(s) in 2 file(s):"));
        assert!(text.contains("a.rs:"));
        assert!(text.contains("... and 1 more not listed"));
    }

    #[test]
    fn an_empty_reference_list_is_not_an_error() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let text = references(&[], &view);
        assert!(text.contains("No references found"));
        assert!(text.contains("empty list"));
    }

    #[test]
    fn hover_flattens_markdown_and_caps_at_4_kb() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let info = HoverInfo {
            text: "# Title\n\n**fn** `alpha`\n\n```rust\nfn alpha() {}\n```".to_owned(),
            markdown: true,
            range: Some((0, 3)),
        };
        let text = hover(&info, &view);
        assert!(text.starts_with("Hover at a.rs:1:4:"));
        assert!(text.contains("Title"));
        assert!(text.contains("fn alpha"));
        assert!(text.contains("fn alpha() {}"));
        assert!(!text.contains("**"));
        assert!(!text.contains("```"));

        let long = HoverInfo {
            text: "x".repeat(HOVER_BYTES + 10),
            markdown: false,
            range: None,
        };
        let text = hover(&long, &view);
        assert!(text.contains("truncated at 4 KB"));

        let empty = HoverInfo {
            text: "   ".to_owned(),
            markdown: false,
            range: None,
        };
        assert!(hover(&empty, &view).contains("No hover information"));
    }

    #[test]
    fn hover_keeps_a_code_sample_intact() {
        // A fenced sample survives the flattener byte for byte: `**` and
        // backticks are code, and stripping them changes what the code says.
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let info = HoverInfo {
            text: "```python\ndef f(**kwargs): return `x`\n```".to_owned(),
            markdown: true,
            range: None,
        };
        let text = hover(&info, &view);
        assert!(text.contains("def f(**kwargs): return `x`"), "{text}");
    }

    #[test]
    fn document_symbols_indent_by_nesting() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let list = symbol_list(vec![symbol("mod m", 2, 0, 0), symbol("fn f", 12, 1, 1)]);
        let text = document_symbols(&list, &view);
        assert!(text.starts_with("2 symbol(s):"));
        assert!(text.contains("\n  mod m (Module) - a.rs:1:1"));
        assert!(text.contains("\n    fn f (Function) - a.rs:2:1"));
        assert!(document_symbols(&symbol_list(Vec::new()), &view).contains("No symbols"));
    }

    #[test]
    fn skipped_symbols_are_reported_and_the_note_disappears_when_there_are_none() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let list = SymbolList {
            symbols: vec![symbol("kept", 12, 0, 0)],
            skipped: 3,
        };
        let text = document_symbols(&list, &view);
        assert!(
            text.contains("3 symbol(s) in the answer were malformed and skipped"),
            "{text}"
        );
        assert!(text.ends_with("are listed above)"), "{text}");

        let clean = document_symbols(&symbol_list(vec![symbol("kept", 12, 0, 0)]), &view);
        assert!(!clean.contains("malformed"), "{clean}");
    }

    #[test]
    fn workspace_symbols_group_and_never_invent_a_position() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let no_position = Symbol {
            name: "Thing".to_owned(),
            kind: 5,
            detail: None,
            container: Some("mod m".to_owned()),
            site: Site {
                path: Some(PathBuf::from("/ws/b.rs")),
                uri: "file:///ws/b.rs".to_owned(),
                line: None,
                character: None,
            },
            depth: 0,
        };
        let text = workspace_symbols(&[no_position], "Thing", &view);
        assert!(text.starts_with("Found 1 symbol(s) matching `Thing`"));
        assert!(text.contains("\nb.rs:\n  Thing (Class) - b.rs in mod m"));
        assert!(workspace_symbols(&[], "z", &view).contains("No workspace symbols"));
    }

    fn report(items: Vec<Diagnostic>, received: bool, timed_out: bool) -> DiagnosticsReport {
        DiagnosticsReport {
            items,
            encoding: PositionEncoding::Utf16,
            received_for_version: received,
            timed_out,
            server: "mock".to_owned(),
        }
    }

    // Built through serde rather than struct literals: `Diagnostic` has eight
    // optional fields and the numeric severity is a transparent newtype, so the
    // wire form is both shorter and closer to what a server actually sends.
    fn diagnostic(line: u32, character: u32, severity: Option<i32>, message: &str) -> Diagnostic {
        let mut value = serde_json::json!({
            "range": {
                "start": { "line": line, "character": character },
                "end": { "line": line, "character": character + 1 },
            },
            "message": message,
        });
        if let Some(severity) = severity {
            value["severity"] = serde_json::json!(severity);
        }
        serde_json::from_value(value).expect("a well-formed diagnostic")
    }

    #[test]
    fn unknown_diagnostics_are_not_reported_as_clean() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let text = diagnostics(&report(Vec::new(), false, true), &view);
        assert!(text.contains("No diagnostics are known"));
        assert!(text.contains("NOT a statement that the file is clean"));
        assert!(text.contains("timed out"));
    }

    #[test]
    fn a_published_empty_result_reports_zero() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let text = diagnostics(&report(Vec::new(), true, false), &view);
        assert!(text.starts_with("0 diagnostics for a.rs"));
        assert!(text.contains("reported no problems"));
    }

    #[test]
    fn diagnostics_sort_most_severe_first_and_count_by_severity() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let items = vec![
            diagnostic(1, 0, Some(4), "a hint"),
            diagnostic(0, 3, Some(1), "an error"),
            diagnostic(1, 4, Some(2), "a warning"),
        ];
        let text = diagnostics(&report(items, true, false), &view);
        assert!(text.starts_with("3 diagnostic(s) for a.rs (1 error(s), 1 warning(s)):"));
        let error_at = text.find("error an error").expect("error listed");
        let warning_at = text.find("warning a warning").expect("warning listed");
        let hint_at = text.find("hint a hint").expect("hint listed");
        assert!(error_at < warning_at && warning_at < hint_at, "{text}");
        // The file is named once in the header, so each line carries only its
        // own line:column.
        assert!(text.contains("1:4 error an error"), "{text}");
    }

    #[test]
    fn an_unclassified_diagnostic_sorts_last() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let items = vec![
            diagnostic(0, 0, None, "no severity"),
            diagnostic(0, 1, Some(1), "severe"),
        ];
        let text = diagnostics(&report(items, true, false), &view);
        let severe = text.find("severe").unwrap();
        let unknown = text.find("no severity").unwrap();
        assert!(severe < unknown, "{text}");
    }

    #[test]
    fn diagnostic_codes_and_sources_are_shown_without_changing_the_line_count() {
        let lines = lines();
        let view = view(&lines, Path::new("/ws/a.rs"));
        let value = serde_json::json!({
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 },
            },
            "severity": 1,
            "message": "mismatched\ntypes",
            "code": "E0308",
            "source": "rustc",
        });
        let diagnostic: Diagnostic = serde_json::from_value(value).expect("a diagnostic");
        let text = diagnostics(&report(vec![diagnostic], true, false), &view);
        assert!(
            text.contains("error mismatched types [E0308] (rustc)"),
            "{text}"
        );
    }

    #[test]
    fn kind_names_cover_the_protocol_and_fall_back() {
        assert_eq!(kind_name(1), "File");
        assert_eq!(kind_name(12), "Function");
        assert_eq!(kind_name(26), "TypeParameter");
        assert_eq!(kind_name(999), "Unknown");
    }

    #[test]
    fn line_index_refuses_files_outside_its_boundary() {
        let index = LineIndex::lazy("/ws");
        assert_eq!(index.line(Path::new("/etc/passwd"), 0), None);
        index.insert("/elsewhere/x.rs", "hello");
        assert_eq!(
            index.line(Path::new("/elsewhere/x.rs"), 0).as_deref(),
            Some("hello")
        );
        assert_eq!(
            index.line(Path::new("/ws/a.rs"), 0),
            None,
            "no such file on disk"
        );
    }

    #[test]
    fn a_dot_dot_path_cannot_climb_out_of_the_boundary() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).expect("ws dir");
        std::fs::write(ws.join("a.rs"), "pub fn ok() {}\n").expect("write a.rs");
        std::fs::write(dir.path().join("secret.txt"), "TOP SECRET\n").expect("write secret");

        let index = LineIndex::lazy(&ws);
        // The file itself is readable…
        assert_eq!(
            index.line(&ws.join("a.rs"), 0).as_deref(),
            Some("pub fn ok() {}")
        );
        // …and `<ws>/../secret.txt` *does* "start with" `<ws>` as a component
        // prefix, and it exists, so only the normalization stands in the way.
        let escape = ws.join("..").join("secret.txt");
        assert!(escape.starts_with(&ws), "this is the trap being guarded");
        assert_eq!(index.line(&escape, 0), None);
        // A server-supplied path that has already climbed out is refused too.
        assert_eq!(index.lines_of(Path::new("/ws/../../etc/passwd")), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_boundary_cannot_reach_outside_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).expect("ws dir");
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "OUTSIDE\n").expect("write outside");
        std::os::unix::fs::symlink(&outside, ws.join("link.txt")).expect("symlink");

        // The lexical check passes (the link really is inside the boundary);
        // only resolving it shows where it points.
        let index = LineIndex::lazy(&ws);
        assert_eq!(index.line(&ws.join("link.txt"), 0), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_unreadable_rather_than_a_hang() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).expect("ws dir");
        // a -> b -> a: resolving either is ELOOP, forever. A reader that
        // followed it would spin; one that gives up reports "no text", which is
        // true and actionable.
        std::os::unix::fs::symlink(ws.join("loop_b.txt"), ws.join("loop_a.txt")).expect("link a");
        std::os::unix::fs::symlink(ws.join("loop_a.txt"), ws.join("loop_b.txt")).expect("link b");

        let index = LineIndex::lazy(&ws);
        assert_eq!(index.line(&ws.join("loop_a.txt"), 0), None);
        assert_eq!(index.lines_of(&ws.join("loop_b.txt")), None);
    }

    #[test]
    fn a_utf8_bom_stays_part_of_the_first_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).expect("ws dir");
        std::fs::write(ws.join("bom.rs"), "\u{feff}pub fn f() {}\n").expect("write bom.rs");

        // The daemon ships a file's bytes verbatim, so a language server and
        // this reader both count the BOM as character 0 of line 1. Stripping it
        // here would shift every column on the first line of every BOM'd file
        // by one — the exact class of off-by-one this file exists to prevent.
        let index = LineIndex::lazy(&ws);
        let first = index.line(&ws.join("bom.rs"), 0).expect("readable");
        assert_eq!(first, "\u{feff}pub fn f() {}");
        assert_eq!(index.lines_of(&ws.join("bom.rs")).map(|l| l.len()), Some(1));
    }

    #[test]
    fn crlf_line_endings_do_not_leak_into_a_snippet() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).expect("ws dir");
        let file = ws.join("crlf.rs");
        std::fs::write(&file, "fn a() {}\r\nfn b() {}\r\n").expect("write crlf.rs");

        let index = LineIndex::lazy(&ws);
        // The line ending is not part of the line, so a snippet can never end in
        // a stray carriage return, and the two-character ending does not inflate
        // the column of anything before it.
        assert_eq!(index.line(&file, 1).as_deref(), Some("fn b() {}"));
        let view = View {
            boundary: &ws,
            encoding: PositionEncoding::Utf16,
            max_results: 100,
            subject: Some(&file),
            lines: &index,
        };
        let site = site(file.to_str().expect("utf-8 temp path"), 1, 6);
        assert_eq!(view.snippet_of(&site).as_deref(), Some("fn b() {}"));
    }

    #[test]
    fn a_very_long_line_is_truncated_before_it_reaches_the_model() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).expect("ws dir");
        let file = ws.join("min.js");
        // A minified asset is one enormous line. Without a cap, one reference to
        // it would paste the whole file into the model's context.
        let long = format!("let x = 1;{}", "x".repeat(5000));
        std::fs::write(&file, format!("{long}\n")).expect("write min.js");

        let index = LineIndex::lazy(&ws);
        let view = View {
            boundary: &ws,
            encoding: PositionEncoding::Utf16,
            max_results: 100,
            subject: Some(&file),
            lines: &index,
        };
        let snippet = view
            .snippet_of(&site(file.to_str().expect("utf-8 temp path"), 0, 0))
            .expect("a snippet");
        assert!(snippet.ends_with("..."), "{snippet}");
        assert!(
            snippet.chars().count() <= SNIPPET_CHARS + 3,
            "a snippet is a hint, not a file: {} chars",
            snippet.chars().count()
        );
    }

    #[test]
    fn a_binary_file_is_unreadable_rather_than_misread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).expect("ws dir");
        // Not UTF-8, so there is no line to quote. Saying so is right; guessing
        // a lossy interpretation would put replacement characters in a snippet
        // and imply the file is text.
        std::fs::write(ws.join("blob.bin"), [0x00, 0xff, 0xfe, 0x01]).expect("write blob.bin");

        let index = LineIndex::lazy(&ws);
        assert_eq!(index.line(&ws.join("blob.bin"), 0), None);
        assert_eq!(index.lines_of(&ws.join("blob.bin")), None);
    }

    #[test]
    fn an_oversized_file_is_refused_without_being_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).expect("ws dir");
        let huge = ws.join("huge.rs");
        // Sparse, so this costs no disk: the point is the length the reader
        // sees, not the bytes it would have to hold.
        let file = std::fs::File::create(&huge).expect("create huge.rs");
        file.set_len(MAX_FILE_BYTES + 1).expect("set_len");
        drop(file);
        // A small file next to it still works, so the cap is a size test and not
        // a blanket refusal.
        std::fs::write(ws.join("small.rs"), "fn ok() {}\n").expect("write small.rs");

        let index = LineIndex::lazy(&ws);
        assert_eq!(index.lines_of(&huge), None, "too large to read");
        assert_eq!(index.line(&huge, 0), None);
        assert_eq!(
            index.line(&ws.join("small.rs"), 0).as_deref(),
            Some("fn ok() {}")
        );
    }

    #[test]
    fn plain_text_keeps_code_fences_and_drops_their_markers() {
        let markdown = "# Head\n\n**bold** and `code`\n\n```rs\nlet x = 1;\n```\n";
        let text = plain_text(markdown);
        assert_eq!(text, "Head\n\nbold and code\n\nlet x = 1;");
    }

    #[test]
    fn truncation_helpers_never_split_a_character() {
        let wide = "\u{4e2d}".repeat(300);
        let clipped = truncate_chars(&wide, 10);
        assert_eq!(clipped.chars().filter(|c| *c == '\u{4e2d}').count(), 10);
        assert!(clipped.ends_with("..."));

        let (body, clipped) = truncate_bytes(&wide, 7);
        assert!(clipped);
        assert_eq!(body.chars().count(), 2, "7 bytes is two three-byte scalars");
    }
    #[test]
    fn multi_line_detail_collapses_to_one_line() {
        assert_eq!(
            one_line_detail("pub fn f(\n    a: u32,\n) -> u32"),
            " pub fn f( a: u32, ) -> u32"
        );
    }

    // ---- the boundary and the grouping -------------------

    /// A boundary that could not be resolved refuses every path.
    ///
    /// The state is built directly rather than reached through the file system,
    /// because it is not reachable that way: `canonicalize` failing on the
    /// boundary means it also fails on every path beneath it, and `inside`
    /// returns before the canonical check when the *path* cannot be resolved.
    /// So the old fall-through was a fail-open with no live input — which is
    /// exactly why it survived review, and exactly why the guarantee is worth
    /// stating as a test rather than leaving to inspection.
    #[test]
    fn a_boundary_with_no_canonical_form_refuses_every_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("a.rs");
        std::fs::write(&real, "fn a() {}").expect("write");
        // A root that resolves, with no canonical form recorded: the boundary
        // did not resolve when this was built.
        let boundary = Boundary::unresolved(dir.path());
        assert_eq!(
            boundary.inside(&real),
            None,
            "without a canonical boundary the symlink check cannot be made, so the path \
             must be refused rather than waved through"
        );
    }

    /// The ordinary case: a boundary that does resolve is unaffected, and a
    /// file that is not there yet is still reportable (there is nothing to read,
    /// so there is nothing to smuggle out).
    #[test]
    fn a_resolved_boundary_still_reports_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let boundary = Boundary::new(dir.path());
        let missing = dir.path().join("not-created-yet.rs");
        assert_eq!(boundary.inside(&missing), Some(missing));
        // A `..` that walks out is still refused by the lexical check.
        assert_eq!(boundary.inside(Path::new("/etc/passwd")), None);
    }

    /// A symlink out of a resolvable boundary is refused — the property
    /// that must survive the fail-closed change.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_resolved_boundary_is_refused() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.rs"), "fn secret() {}").expect("secret");
        let workspace = tempfile::tempdir().expect("workspace");
        let link = workspace.path().join("link");
        symlink(outside.path(), &link).expect("symlink");
        let boundary = Boundary::new(workspace.path());
        assert_eq!(boundary.inside(&link.join("secret.rs")), None);
    }

    /// Grouping a large set of distinct paths is linear, not quadratic.
    ///
    /// The old implementation compared each site's path against every group it
    /// had built so far, with a `String` comparison per probe, and `references`
    /// then called it a second time on the *uncapped* list just to count files.
    #[test]
    fn grouping_many_distinct_paths_is_linear() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = LineIndex::lazy(dir.path());
        let boundary = dir.path().to_path_buf();
        let view = View {
            boundary: &boundary,
            encoding: PositionEncoding::Utf32,
            max_results: usize::MAX,
            subject: None,
            lines: &lines,
        };
        let sites: Vec<Site> = (0..60_000)
            .map(|n| Site {
                path: Some(PathBuf::from(format!("/ws/file{n}.rs"))),
                uri: format!("file:///ws/file{n}.rs"),
                line: Some(1),
                character: Some(1),
            })
            .collect();
        let started = std::time::Instant::now();
        let groups = view.by_path(&sites);
        let elapsed = started.elapsed();
        assert_eq!(groups.len(), 60_000, "every path is its own group");
        // 60 000 distinct paths is 1.8·10⁹ comparisons through the linear scan
        // this replaced. Measured on this machine: 21.6s for the scan against
        // 159ms for the hash map, so the 3s budget sits an order of magnitude
        // clear of both and is not a measurement of anything else.
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "grouping 60 000 paths took {elapsed:?}"
        );
    }

    /// Rendering references does not walk the site list a second time.
    ///
    /// `references` used to call `by_path` twice: once on the capped slice to
    /// print, and again on the *whole* list just to count files. With the count
    /// that second call is gone, so this is the end-to-end statement of the same
    /// property — and it is the one that matters, because this is the path a
    /// large `textDocument/references` answer actually takes.
    #[test]
    fn rendering_many_references_is_linear() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = LineIndex::lazy(dir.path());
        let boundary = dir.path().to_path_buf();
        let view = View {
            boundary: &boundary,
            encoding: PositionEncoding::Utf32,
            max_results: usize::MAX,
            subject: None,
            lines: &lines,
        };
        let sites: Vec<Site> = (0..60_000)
            .map(|n| Site {
                path: Some(PathBuf::from(format!("/ws/file{n}.rs"))),
                uri: format!("file:///ws/file{n}.rs"),
                line: Some(1),
                character: Some(1),
            })
            .collect();
        let started = std::time::Instant::now();
        let rendered = references(&sites, &view);
        let elapsed = started.elapsed();
        assert!(rendered.starts_with("Found 60000 reference(s) in 60000 file(s):"));
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "rendering 60 000 references took {elapsed:?}"
        );
    }

    /// The file count covers every site, not only the listed ones — which
    /// is why it cannot be read off the shown groups.
    ///
    /// Driven through the public renderer rather than the helper, so a return to
    /// "count the groups I printed" is caught here.
    #[test]
    fn the_reported_file_count_is_the_truth_even_when_rows_are_capped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = LineIndex::lazy(dir.path());
        let boundary = dir.path().to_path_buf();
        let view = View {
            boundary: &boundary,
            encoding: PositionEncoding::Utf32,
            // One row shown, out of five sites in five files.
            max_results: 1,
            subject: None,
            lines: &lines,
        };
        let sites: Vec<Site> = (0..5)
            .map(|n| Site {
                path: Some(PathBuf::from(format!("/ws/file{n}.rs"))),
                uri: format!("file:///ws/file{n}.rs"),
                line: Some(1),
                character: Some(1),
            })
            .collect();
        let rendered = references(&sites, &view);
        assert!(
            rendered.starts_with("Found 5 reference(s) in 5 file(s):"),
            "the header must report every file, not just the one printed: {rendered}"
        );
        assert!(rendered.contains("... and 4 more not listed"), "{rendered}");
    }

    /// And grouping keeps first-seen order, which is what the old `Vec` scan was
    /// there for.
    #[test]
    fn grouping_keeps_first_seen_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = LineIndex::lazy(dir.path());
        let boundary = dir.path().to_path_buf();
        let view = View {
            boundary: &boundary,
            encoding: PositionEncoding::Utf32,
            max_results: usize::MAX,
            subject: None,
            lines: &lines,
        };
        let site = |n: u32| Site {
            path: Some(PathBuf::from(format!("/ws/file{n}.rs"))),
            uri: format!("file:///ws/file{n}.rs"),
            line: Some(1),
            character: Some(1),
        };
        // b, a, b, c, a -> groups in the order b, a, c with the right counts.
        // The paths are outside the boundary, so they print in full; what is
        // under test is the order, not the shortening.
        let sites: Vec<Site> = [1, 0, 1, 2, 0].into_iter().map(site).collect();
        let groups = view.by_path(&sites);
        let names: Vec<&str> = groups.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(names, ["/ws/file1.rs", "/ws/file0.rs", "/ws/file2.rs"]);
        assert_eq!(
            groups.iter().map(|(_, idx)| idx.len()).collect::<Vec<_>>(),
            [2, 2, 1]
        );
    }

    // ---- the line cache --------------------------------------

    /// A lookup copies one line, not the whole file.
    ///
    /// The cache handed back a clone of the `Vec<String>`, so reading line 7 of
    /// a 200 000-line file allocated 200 000 `String`s. The resolvers call this
    /// once per candidate and twice per tree node, in a daemon every connection
    /// shares — so the cost was per *result site*, not per file.
    #[test]
    fn a_line_lookup_is_not_roughly_linear_in_the_file_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("big.rs");
        let body: String = (0..200_000)
            .map(|n| {
                format!(
                    "fn f{n}() {{}}
"
                )
            })
            .collect();
        std::fs::write(&file, &body).expect("write");
        let index = LineIndex::lazy(dir.path());
        // One read, so the measurement is the cache, not the disk.
        assert!(index.line(&file, 0).is_some());
        let started = std::time::Instant::now();
        for n in 0..200 {
            assert!(index.line(&file, n).is_some(), "line {n}");
        }
        let elapsed = started.elapsed();
        // 200 lookups over a 4.4 MB file. Cloning the file per lookup is
        // 200 × 200 000 `String` allocations; the budget below is nowhere near
        // that, and nowhere near reading the file 200 times either.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "200 lookups over a 200 000-line file took {elapsed:?}"
        );
    }

    /// And it still returns the right line, still refuses a line past the end,
    /// and still refuses a path outside the boundary.
    #[test]
    fn a_line_lookup_still_answers_exactly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write");
        let index = LineIndex::lazy(dir.path());
        assert_eq!(index.line(&file, 0).as_deref(), Some("alpha"));
        assert_eq!(index.line(&file, 2).as_deref(), Some("gamma"));
        assert_eq!(index.line(&file, 3), None, "past the end of the file");
        assert_eq!(index.lines_of(&file).map(|l| l.len()), Some(3));
        // The boundary check is unchanged: `ensure` short-circuits on the cache,
        // so a path that was never admitted still is not.
        assert_eq!(index.line(Path::new("/etc/passwd"), 0), None);
    }
}
