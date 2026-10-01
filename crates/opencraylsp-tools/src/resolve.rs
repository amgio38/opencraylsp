//! Turning the model's target — a symbol name, or a path with line and column —
//! into one exact location, or a short list when the name is ambiguous.
//!
//! Why this module exists: a model should not have to count line and column
//! characters to ask "where is `Foo::bar`". LSP has no name-based lookup that
//! returns a position, so the lookup is built here from `workspace/symbol` plus
//! a small, explicit filter chain. When a name matches several symbols the
//! answer is a list of candidates, never a guess.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use opencraylsp_core::backend::{LanguageInfo, LspBackend, LspError, PositionEncoding, Served};
use opencraylsp_proto::ToolOutput;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::render_error;
use crate::format::{self, Boundary, LineIndex};
use crate::operations::{Site, Symbol};
use crate::position;
use crate::resolve_container;

/// The one LSP method a symbol lookup is built from.
const METHOD: &str = "workspace/symbol";

/// How many near-miss suggestions a `not_found` answer carries.
const MAX_SUGGESTIONS: usize = 5;

/// A server's own result cap for `workspace/symbol` (rust-analyzer answers at
/// most this many, and fuzzily). Reaching it means a miss may be hiding past it.
const SERVER_RESULT_CAP: usize = 30;

/// How many servers a name-only lookup fans out to at most.
const MAX_FANOUT: usize = 3;

/// Most candidates kept from a `workspace/symbol` sweep before anything else
/// looks at them.
///
/// `workspace/symbol` has no protocol cap, and the transport will hand us a
/// 64 MiB frame, so this is the only thing standing between one very large
/// answer and a `dedupe`, a sort and a `Candidate::sort_key` allocation per
/// comparison. See the use in [`fold`].
const MAX_HITS: usize = 2_000;

/// How the model asked to locate something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSpec {
    /// A symbol name, optionally qualified (`Foo::bar`), narrowed by a path,
    /// a kind or a language.
    Symbol {
        name: String,
        path: Option<String>,
        kind: Option<String>,
        language: Option<String>,
    },
    /// A raw position: 1-based line, 1-based Unicode scalar column.
    Position {
        path: String,
        line: u32,
        column: u32,
    },
}

/// One place a symbol was found.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub site: Site,
    pub name: String,
    pub kind: u32,
    pub container: Option<String>,
    pub server: String,
    pub outside_workspace: bool,
}

impl Candidate {
    /// Inside the boundary first, then by path, line and column — so the order
    /// a model sees is stable and the workspace's own files come first.
    fn sort_key(&self, boundary: &Path) -> (bool, String, u32, u32) {
        (
            self.outside_workspace,
            display_path(boundary, self.site.path.as_deref().unwrap_or(Path::new(""))),
            self.site.line.unwrap_or(u32::MAX),
            self.site.character.unwrap_or(u32::MAX),
        )
    }
}

/// The outcome of a name lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    /// Exactly one symbol: the call may go straight to it.
    One(Candidate),
    /// Several symbols share the name: the caller lists them and asks again.
    Many(Vec<Candidate>),
    /// The server answered, and nothing matched; `suggestions` may be empty.
    NotFound { suggestions: Vec<Candidate> },
}

/// [`Resolved`] plus the caveats the caller should print after it.
///
/// A fan-out can partly fail (one server times out) or be untrustworthy (a
/// server is still indexing) while still producing an answer; those notes would
/// otherwise be lost, and an agent that is not told "results may be incomplete"
/// treats them as complete.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub resolved: Resolved,
    pub notes: Vec<String>,
}

/// Parses the top-level `Target` arguments.
///
/// `Err` is a ready-to-return `[invalid_args]` output.
pub fn parse_target(args: &Value) -> Result<TargetSpec, ToolOutput> {
    let Some(obj) = args.as_object() else {
        return Err(invalid_args("arguments must be a JSON object"));
    };

    let symbol = non_empty_string(obj, "symbol");
    let path = non_empty_string(obj, "path");
    let kind = non_empty_string(obj, "kind");
    let language = non_empty_string(obj, "language");
    let line = integer(obj, "line")?;
    let column = integer(obj, "column")?;

    // A complete position wins over a name. This is the retry an `[ambiguous]`
    // answer hands out, and a model that pastes it without remembering to remove
    // `symbol` must still be understood: the position already names one place, so
    // there is nothing for the name to disambiguate. Only a half-given position (a
    // line with no path, a line with no column) is a contradiction, and there the
    // error has to stand.
    let complete = path.is_some() && line.is_some() && column.is_some();
    if symbol.is_some() && !complete && (line.is_some() || column.is_some()) {
        return Err(invalid_args(
            "`symbol` and `line`/`column` are mutually exclusive: give a symbol name, \
             or a `path` with `line` and `column`. A complete `path`+`line`+`column` is \
             used as the position and `symbol` is ignored",
        ));
    }

    if let Some(name) = symbol.filter(|_| !complete) {
        return Ok(TargetSpec::Symbol {
            name,
            path,
            kind,
            language,
        });
    }

    match (path, line, column) {
        (Some(path), Some(line), Some(column)) => Ok(TargetSpec::Position { path, line, column }),
        (Some(_), Some(_), None) | (Some(_), None, Some(_)) => Err(invalid_args(
            "`line` and `column` must both be given (1-based) to locate a position",
        )),
        (Some(_), None, None) => Err(invalid_args(
            "give a `symbol` name, or a `path` together with `line` and `column`",
        )),
        _ => Err(invalid_args(
            "give a `symbol` name, or a `path` together with `line` and `column`",
        )),
    }
}

/// Resolves `spec` against `backend`, or returns a ready-to-return error.
pub async fn resolve(
    backend: &dyn LspBackend,
    spec: &TargetSpec,
    cancel: &CancellationToken,
) -> Result<Resolution, ToolOutput> {
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    match spec {
        TargetSpec::Position { path, line, column } => {
            resolve_position(backend, path, *line, *column).map(|candidate| Resolution {
                resolved: Resolved::One(candidate),
                notes: Vec::new(),
            })
        }
        TargetSpec::Symbol {
            name,
            path,
            kind,
            language,
        } => {
            resolve_symbol(
                backend,
                name,
                path.as_deref(),
                kind.as_deref(),
                language.as_deref(),
                cancel,
            )
            .await
        }
    }
}

/// A fuzzy name search, without the exact-name narrowing of [`resolve`]: this
/// is what `lsp_find_symbol` uses.
///
/// Returns the raw candidates and the fan-out's caveats, deduplicated and
/// sorted inside-the-workspace-first, or a ready-to-return error.
pub async fn search(
    backend: &dyn LspBackend,
    query: &str,
    path: Option<&str>,
    language: Option<&str>,
    cancel: &CancellationToken,
) -> Result<(Vec<Candidate>, Vec<String>), ToolOutput> {
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let languages = backend.languages();
    let boundary = backend.boundary();
    let inside = Boundary::new(&boundary);
    let targets = pick_targets(backend, &languages, path, language)?;
    let lines = LineIndex::lazy(boundary.clone());
    let outcomes: Vec<Outcome> = run_queries(backend, &targets, query, cancel)
        .await
        .into_iter()
        .map(|(label, result)| match result {
            Ok(served) => decode(&label, served, &lines),
            Err(error) => failure(&label, error),
        })
        .collect();
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let (hits, notes) = fold(outcomes, &inside)?;
    let mut hits = dedupe(hits);
    // Exact name, then name-prefix, then anywhere-in-the-name. A caller who
    // searched `discover` wants that symbol, not the twenty tests whose names
    // happen to contain it, so the tier is the first key and the original
    // ordering (inside-workspace, then path, then position) breaks ties within
    // a tier. Nothing is dropped: `parse_args` is still findable from `parse`.
    let query_key = query.trim().to_ascii_lowercase();
    hits.sort_by_key(|candidate| {
        (
            match_rank(&candidate.name, &query_key),
            candidate.sort_key(&boundary),
        )
    });
    // A `path` hint also scopes the answer: the caller named a file (or a
    // directory) and asked what is in it, so symbols living outside it are not
    // the answer even when a server returned them. Applied after the server
    // fan-out because only then is every candidate's file known; the same
    // prefix rule the renderer uses for display keeps "that path" and "what's
    // under it" the same decision.
    if let Some(path) = path {
        let Ok(root) = backend.resolve_path(path) else {
            // `pick_targets` already reported this path as unusable; if it got
            // this far the answer is simply unfiltered.
            return Ok((hits, notes));
        };
        hits.retain(|candidate| under(&candidate.site.uri, &root));
    }
    Ok((hits, notes))
}

/// How closely `name` matches what was searched for: 0 exact, 1 prefix,
/// 2 anywhere. Case-insensitive because a server may hand back `Discover`
/// for a query of `discover`.
pub fn match_rank(name: &str, lowered_query: &str) -> u8 {
    let name = name.trim().to_ascii_lowercase();
    if name == lowered_query {
        0
    } else if name.starts_with(lowered_query) {
        1
    } else {
        2
    }
}

/// Whether `uri` is `root` itself or something inside it.
///
/// Both sides are normalised the same way, so a trailing slash, a `.` segment
/// or the symlink spelling of the boundary cannot make a file under the root
/// look like it is outside it (or the reverse).
fn under(uri: &str, root: &Path) -> bool {
    let Ok(parsed) = url::Url::parse(uri) else {
        return false;
    };
    let Ok(path) = parsed.to_file_path() else {
        return false;
    };
    let path = normalise(&path);
    let root = normalise(root);
    path == root || path.starts_with(&root)
}

/// Lexical normalisation only: this decides what to show, so it must not touch
/// the filesystem (resolving a symlink here could move a file across the
/// boundary after the workspace check already passed).
fn normalise(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A raw position: checked against the file's length, no server involved.
///
/// The `Site` is expressed in the same unit as every other candidate — 0-based
/// line and UTF-16 code units, the encoding the client speaks to servers — so a
/// caller has exactly one conversion path for both (see [`normalise_site`]).
fn resolve_position(
    backend: &dyn LspBackend,
    path: &str,
    line: u32,
    column: u32,
) -> Result<Candidate, ToolOutput> {
    let resolved = backend.resolve_path(path).map_err(|e| render_error(&e))?;
    let boundary = backend.boundary();
    let lines = LineIndex::lazy(boundary.clone());
    let Some(all) = lines.lines_of(&resolved) else {
        return Err(ToolOutput::error(format!(
            "[io_error] cannot read `{}` to check the position: the file is missing, \
             is not UTF-8 text, or is too large to load",
            display_path(&boundary, &resolved)
        )));
    };
    if line == 0 || line as usize > all.len() {
        let count = all.len();
        let plural = if count == 1 { "line" } else { "lines" };
        return Err(invalid_args(&format!(
            "`line` {line} is past the end of `{}`, which has {count} {plural}",
            display_path(&boundary, &resolved)
        )));
    }
    let text = all
        .get((line - 1) as usize)
        .expect("`line` was just checked against the file length");
    // A column past the end of its line is the same mistake as a line past the
    // end of the file — an off-by-one in the caller's counting — and saying so
    // here costs one string comparison, against silently clamping to the line's
    // end and asking the server about nothing. The line length is in Unicode
    // scalars, the counting every other answer uses.
    let width = text.chars().count();
    if column == 0 || column as usize > width + 1 {
        return Err(invalid_args(&format!(
            "`column` {column} is past the end of line {line} of `{}`, which has {width} \
             character{} (a column one past the end points just after it)",
            display_path(&boundary, &resolved),
            if width == 1 { "" } else { "s" },
        )));
    }
    Ok(Candidate {
        site: Site {
            uri: file_uri(&resolved),
            path: Some(resolved),
            line: Some(line - 1),
            character: Some(position::to_server_column(
                text,
                column,
                PositionEncoding::Utf16,
            )),
        },
        name: String::new(),
        kind: 0,
        container: None,
        server: String::new(),
        outside_workspace: false,
    })
}

/// A name lookup: pick the servers, ask them all at once, then filter.
async fn resolve_symbol(
    backend: &dyn LspBackend,
    name: &str,
    path: Option<&str>,
    kind: Option<&str>,
    language: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Resolution, ToolOutput> {
    let (qualifiers, bare_name) = split_qualified(name);
    if bare_name.is_empty() {
        return Err(invalid_args("`symbol` does not name anything to look up"));
    }

    let languages = backend.languages();
    let boundary = backend.boundary();
    let inside = Boundary::new(&boundary);
    let targets = pick_targets(backend, &languages, path, language)?;

    let lines = LineIndex::lazy(boundary.clone());
    let outcomes: Vec<Outcome> = run_queries(backend, &targets, &bare_name, cancel)
        .await
        .into_iter()
        .map(|(label, result)| match result {
            Ok(served) => decode(&label, served, &lines),
            Err(error) => failure(&label, error),
        })
        .collect();
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let resolution = combine(
        backend,
        outcomes,
        &inside,
        &bare_name,
        kind,
        &qualifiers,
        cancel,
    )
    .await;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    resolution
}

/// One `workspace/symbol` query: against a named server, or against whatever
/// server owns `file` (the path hint case).
#[derive(Debug, Clone)]
enum Target {
    Server(String),
    File { label: String, path: PathBuf },
}

impl Target {
    fn label(&self) -> &str {
        match self {
            Target::Server(name) => name,
            Target::File { label, .. } => label,
        }
    }
}

fn pick_targets(
    backend: &dyn LspBackend,
    languages: &[LanguageInfo],
    path: Option<&str>,
    language: Option<&str>,
) -> Result<Vec<Target>, ToolOutput> {
    // A path hint wins: it names the file whose extension decides the server,
    // and the backend routes the request itself (it also owns the
    // `language_disabled` decision for extensions this connection did not
    // enable).
    if let Some(path) = path {
        let resolved = backend.resolve_path(path).map_err(|e| render_error(&e))?;
        // A directory has no extension to pick a server by, and asking the
        // backend to route a request "for" it fails with `no_server`. It is a
        // scope, not a file: fall through to the language hint or the fan-out,
        // and let the result filter keep only what lives under it.
        if !resolved.is_dir() {
            let label =
                server_for_extension(languages, extension_of(&resolved)).unwrap_or_default();
            return Ok(vec![Target::File {
                label,
                path: resolved,
            }]);
        }
    }

    // A language hint: the connection's enabled set decides whether it exists.
    if let Some(language) = language {
        return match enabled_languages(languages)
            .iter()
            .find(|info| info.name.eq_ignore_ascii_case(language))
        {
            Some(info) => Ok(vec![Target::Server(info.server.clone())]),
            None => Err(render_error(&LspError::LanguageDisabled {
                language: language.to_owned(),
                enabled: enabled_languages(languages)
                    .into_iter()
                    .map(|info| info.name.clone())
                    .collect(),
            })),
        };
    }

    // No hint: fan out to the servers that are actually usable here.
    let mut usable: Vec<&LanguageInfo> = enabled_languages(languages).into_iter().collect();
    if usable.is_empty() {
        // Nothing is enabled at all: the language set itself is the problem.
        return Err(ToolOutput::error(
            "[language_disabled] no language is enabled for this connection; no project \
             markers were found, so pass --languages to opencraylsp-mcp (or use --languages all)",
        ));
    }
    usable.retain(|info| info.installed);
    if usable.is_empty() {
        return Err(ToolOutput::error(format!(
            "[no_server] no enabled language server is installed and detected here; known \
             servers: {}; pass `path` or `language` to pick one",
            known_servers(&usable_known(languages))
        )));
    }
    // `detected` is a preference, not a filter: an installed
    // server in a workspace without markers may still answer.
    usable.sort_by(|a, b| {
        b.detected
            .cmp(&a.detected)
            .then_with(|| a.server.cmp(&b.server))
    });
    // One server may serve several languages (typescript and javascript share
    // `typescript-language-server`), so the same server is queried once.
    let mut seen_servers: Vec<String> = Vec::new();
    usable.retain(|info| {
        if seen_servers.contains(&info.server) {
            false
        } else {
            seen_servers.push(info.server.clone());
            true
        }
    });
    usable.truncate(MAX_FANOUT);
    Ok(usable
        .into_iter()
        .map(|info| Target::Server(info.server.clone()))
        .collect())
}

/// The languages this connection has enabled: `languages()` returns only
/// enabled entries, but the flag is honoured in case that ever changes.
fn enabled_languages(languages: &[LanguageInfo]) -> Vec<&LanguageInfo> {
    languages.iter().filter(|info| info.enabled).collect()
}

fn usable_known(languages: &[LanguageInfo]) -> Vec<&LanguageInfo> {
    enabled_languages(languages)
}

fn known_servers(languages: &[&LanguageInfo]) -> String {
    let mut seen = Vec::new();
    for info in languages {
        let entry = format!("{} ({})", info.server, info.extensions.join(", "));
        if !seen.contains(&entry) {
            seen.push(entry);
        }
    }
    seen.join(", ")
}

fn server_for_extension(languages: &[LanguageInfo], extension: Option<&str>) -> Option<String> {
    let extension = extension?;
    enabled_languages(languages)
        .into_iter()
        .find(|info| {
            info.extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case(extension))
        })
        .map(|info| info.server.clone())
}

fn extension_of(path: &Path) -> Option<&str> {
    path.extension().and_then(|e| e.to_str())
}

/// The result of one server's query.
#[derive(Debug)]
enum Outcome {
    Hits(Vec<Candidate>),
    /// The server answered and found nothing.
    Empty,
    /// The server is still indexing and did not produce a usable answer.
    Indexing(String),
    /// The server failed, or its answer could not be read; `code` is the
    /// rendered marker, `rendered` the error to return if nothing else answers.
    Failed {
        server: String,
        code: String,
        rendered: ToolOutput,
    },
}

/// One server's raw reply, before decoding.
///
/// Decoding needs the line index (it converts coordinates), and the line index
/// is not `Sync`, so it happens after the fan-out has joined rather than inside
/// the spawned-agnostic futures below.
type Reply = (String, Result<Served, LspError>);

/// A query runner: the future type is uniform so up to [`MAX_FANOUT`] of them
/// can be joined without boxing per call site.
type Query<'a> = Pin<Box<dyn Future<Output = Reply> + Send + 'a>>;

async fn run_queries(
    backend: &dyn LspBackend,
    targets: &[Target],
    name: &str,
    cancel: &CancellationToken,
) -> Vec<Reply> {
    let query = |target: &Target| -> Query<'_> {
        let params = json!({ "query": name });
        let label = target.label().to_owned();
        match target {
            Target::Server(server) => {
                let server = server.clone();
                Box::pin(async move {
                    let result = backend
                        .request_workspace(&server, METHOD, params, cancel)
                        .await;
                    (label, result)
                })
            }
            Target::File { path, .. } => {
                let path = path.clone();
                Box::pin(async move {
                    let result = backend.request(&path, METHOD, params, cancel).await;
                    (label, result)
                })
            }
        }
    };

    match targets {
        [] => Vec::new(),
        [only] => vec![query(only).await],
        [first, second] => {
            let (a, b) = tokio::join!(query(first), query(second));
            vec![a, b]
        }
        [first, second, third, ..] => {
            let (a, b, c) = tokio::join!(query(first), query(second), query(third));
            vec![a, b, c]
        }
    }
}

/// Turns an LSP-level failure into an [`Outcome`].
fn failure(server: &str, error: LspError) -> Outcome {
    if matches!(error, LspError::Indexing { .. }) {
        return Outcome::Indexing(server.to_owned());
    }
    let rendered = render_error(&error);
    let code = crate::error::error_code(&error).to_owned();
    Outcome::Failed {
        server: server.to_owned(),
        code,
        rendered,
    }
}

/// Decodes one server's `workspace/symbol` answer into candidates.
fn decode(server: &str, served: opencraylsp_core::backend::Served, lines: &LineIndex) -> Outcome {
    let encoding = served.encoding;
    match crate::operations::workspace_symbols(&served.value) {
        Ok(symbols) => {
            let hits: Vec<Candidate> = symbols
                .into_iter()
                .map(|symbol| candidate_from(symbol, server, encoding, lines))
                .collect();
            if hits.is_empty() {
                // A null/empty answer *while indexing* is not "not found".
                if served.indexing.is_some() {
                    Outcome::Indexing(server.to_owned())
                } else {
                    Outcome::Empty
                }
            } else {
                Outcome::Hits(hits)
            }
        }
        Err(shape) => {
            let rendered = ToolOutput::error(format!(
                "[invalid_response] the language server's answer to `{METHOD}` could not be \
                 read: {}",
                shape.detail
            ));
            Outcome::Failed {
                server: server.to_owned(),
                code: "invalid_response".to_owned(),
                rendered,
            }
        }
    }
}

fn candidate_from(
    symbol: Symbol,
    server: &str,
    encoding: PositionEncoding,
    lines: &LineIndex,
) -> Candidate {
    Candidate {
        site: normalise_site(symbol.site, encoding, lines),
        name: symbol.name,
        kind: symbol.kind,
        container: symbol.container,
        server: server.to_owned(),
        outside_workspace: false,
    }
}

/// Expresses a site in the client's outgoing unit — 0-based line, UTF-16 code
/// units — so a candidate carries the same coordinate system whichever path
/// produced it, and the caller converts exactly once: the client speaks UTF-16
/// to servers, whatever the model counts in.
///
/// A line that cannot be read (outside the boundary, or gone) keeps the
/// server's own coordinate: it is exact for the ASCII lines dependency sources
/// are made of, and it is never invented.
fn normalise_site(site: Site, encoding: PositionEncoding, lines: &LineIndex) -> Site {
    if matches!(encoding, PositionEncoding::Utf16) {
        return site;
    }
    let (Some(path), Some(line), Some(character)) = (&site.path, site.line, site.character) else {
        return site;
    };
    let Some(text) = lines.line(path, line) else {
        return site;
    };
    let scalar = position::scalar_from_units(&text, character, encoding);
    Site {
        character: Some(position::units_from_scalar(
            &text,
            scalar as usize,
            PositionEncoding::Utf16,
        )),
        ..site
    }
}

/// Folds the per-server outcomes into the hits they produced and the caveats to
/// print with them. `Err` means the lookup as a whole failed, or is too
/// untrustworthy to answer from.
fn fold(
    outcomes: Vec<Outcome>,
    boundary: &Boundary,
) -> Result<(Vec<Candidate>, Vec<String>), ToolOutput> {
    let mut notes = Vec::new();
    let mut hits: Vec<Candidate> = Vec::new();
    let mut first_failure: Option<ToolOutput> = None;
    let mut indexing = false;
    let mut answered = false;

    for outcome in outcomes {
        match outcome {
            Outcome::Hits(mut found) => {
                answered = true;
                hits.append(&mut found);
            }
            Outcome::Empty => answered = true,
            Outcome::Indexing(server) => {
                indexing = true;
                notes.push(format!(
                    "note: {server} is still indexing; results may be incomplete"
                ));
            }
            Outcome::Failed {
                server,
                code,
                rendered,
            } => {
                notes.push(format!("note: {server}: {code}"));
                first_failure.get_or_insert(rendered);
            }
        }
    }

    // A server may answer with any number of symbols it likes, and the frame it
    // arrives in may be 64 MiB, so the raw list is not one the rest of this
    // pipeline can afford: below this every step is O(n) or O(n log n) but all
    // of them allocate per candidate, and above it the daemon — which is shared
    // by every connection — stops answering anything at all.
    //
    // The cap is far above anything the tools show (`MAX_FIND_LIMIT` is 200) and
    // above rust-analyzer's own 30, so the ordinary case never reaches it. When
    // it does bite, a note says so: silently returning the first 2 000 of 50 000
    // would read as a complete answer, which is the one thing this crate is not
    // allowed to do.
    if hits.len() > MAX_HITS {
        let dropped = hits.len() - MAX_HITS;
        hits.truncate(MAX_HITS);
        notes.push(format!(
            "note: {dropped} further candidate(s) were dropped; at most {MAX_HITS} are \
             examined — narrow the query with a path, a qualifier or a language"
        ));
    }

    // A server that is still indexing and produced hits makes them "maybe
    // incomplete"; with no hits from anyone, its silence makes the whole answer
    // untrustworthy -- even when another server said "nothing here" -- which is
    // worse than "not found".
    if hits.is_empty() && indexing {
        return Err(indexing_output(&notes));
    }

    for candidate in &mut hits {
        candidate.outside_workspace = !is_inside(boundary, &candidate.site);
    }

    if hits.is_empty()
        && !answered
        && !indexing
        && let Some(rendered) = first_failure
    {
        return Err(rendered);
    }

    Ok((hits, notes))
}

/// The name lookup: the filter chain — exact name, then whitelist
/// qualification, then path/kind/language hints — plus the caveats the caller
/// prints.
async fn combine(
    backend: &dyn LspBackend,
    outcomes: Vec<Outcome>,
    boundary: &Boundary,
    name: &str,
    kind: Option<&str>,
    qualifiers: &[String],
    cancel: &CancellationToken,
) -> Result<Resolution, ToolOutput> {
    let (hits, mut notes) = fold(outcomes, boundary)?;
    if hits.is_empty() {
        return Ok(Resolution {
            resolved: Resolved::NotFound {
                suggestions: Vec::new(),
            },
            notes,
        });
    }

    let raw_count = hits.len();
    let selected = select_by_name(&hits, name, &mut notes);
    if selected.is_empty() {
        // Honest about the server's own cap: `workspace/symbol` is
        // fuzzy and bounded, so a miss may just mean the symbol is past the
        // window rather than absent.
        if raw_count >= SERVER_RESULT_CAP {
            notes.push(format!(
                "note: the server returned its maximum of {raw_count} results; the symbol \
                 may exist beyond them — add path or a qualifier"
            ));
        }
        return Ok(Resolution {
            resolved: Resolved::NotFound {
                suggestions: near_misses(&hits, name),
            },
            notes,
        });
    }
    let mut selected = retain_by_kind(selected, kind);
    if let Some(last) = qualifiers.last()
        && !selected.is_empty()
        && let Some(narrowed) =
            resolve_container::narrow_by_qualifier(backend, &selected, last, cancel).await
    {
        selected = narrowed;
    }
    let mut selected = dedupe(selected);
    if selected.is_empty() {
        return Ok(Resolution {
            resolved: Resolved::NotFound {
                suggestions: near_misses(&hits, name),
            },
            notes,
        });
    }
    // A workspace hit outranks a dependency hit: the same name in `std` must
    // not turn a one-answer lookup into `[ambiguous]`.
    if selected
        .iter()
        .any(|candidate| !candidate.outside_workspace)
    {
        selected.retain(|candidate| !candidate.outside_workspace);
    }
    selected.sort_by_key(|candidate| candidate.sort_key(boundary.root()));
    let resolved = if selected.len() == 1 {
        Resolved::One(selected.remove(0))
    } else {
        Resolved::Many(selected)
    };
    Ok(Resolution { resolved, notes })
}

fn indexing_output(notes: &[String]) -> ToolOutput {
    let servers: Vec<&str> = notes
        .iter()
        .filter_map(|note| note.strip_prefix("note: "))
        .filter_map(|rest| rest.strip_suffix(" is still indexing; results may be incomplete"))
        .collect();
    let which = if servers.is_empty() {
        "a language server".to_owned()
    } else {
        servers.join(", ")
    };
    ToolOutput::error(format!(
        "[indexing] {which} is still indexing, so results would be incomplete; retry in a few seconds"
    ))
}

/// Design step 3d(1)(2): the exact-name filter, case-sensitive first.
fn select_by_name(candidates: &[Candidate], name: &str, notes: &mut Vec<String>) -> Vec<Candidate> {
    let exact: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.name == name)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    // (2) exact, case-insensitive.
    let folded: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.name.eq_ignore_ascii_case(name))
        .cloned()
        .collect();
    if !folded.is_empty() {
        notes.push("matched case-insensitively".to_owned());
    }
    folded
}

/// Design step 3d(4): the kind the model asked for, by name.
fn retain_by_kind(candidates: Vec<Candidate>, kind: Option<&str>) -> Vec<Candidate> {
    match kind {
        Some(kind) => candidates
            .into_iter()
            .filter(|candidate| format::kind_name(candidate.kind).eq_ignore_ascii_case(kind))
            .collect(),
        None => candidates,
    }
}

/// The near misses a `not_found` answer offers: names that start with or
/// contain the wanted one (case-insensitively), at most [`MAX_SUGGESTIONS`].
fn near_misses(candidates: &[Candidate], name: &str) -> Vec<Candidate> {
    let wanted = name.to_lowercase();
    let mut out: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| {
            let lower = candidate.name.to_lowercase();
            lower.starts_with(&wanted) || lower.contains(&wanted)
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));

    out.truncate(MAX_SUGGESTIONS);
    out
}

/// Same `(uri, line, column)` twice is one place: two servers can answer with
/// the same symbol.
///
/// A `HashSet`, because the list this runs over is the server's whole
/// `workspace/symbol` answer. It used to be a `Vec` scanned with
/// `Vec::contains`, which is O(n²) with a `String` comparison per probe: a
/// server that answered with 500 000 symbols — well inside the 64 MiB frame the
/// transport accepts — took on the order of 10¹¹ comparisons, and the daemon
/// hosting every other connection never answered at all.
fn dedupe(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen: HashSet<(String, Option<u32>, Option<u32>)> =
        HashSet::with_capacity(candidates.len());
    let mut out = Vec::new();
    for candidate in candidates {
        let key = (
            candidate.site.uri.clone(),
            candidate.site.line,
            candidate.site.character,
        );
        if seen.insert(key) {
            out.push(candidate);
        }
    }
    out
}

/// Whether a site is inside the workspace, by the crate's one boundary rule.
///
/// This used to be its own test — `normalize(path).starts_with(normalize(boundary))`
/// — which collapses `..` but never follows a symlink. Two consequences, both
/// wrong in the same direction: a symlink inside the workspace pointing out of
/// it was labelled *inside*, so `combine` preferred it over a genuine workspace
/// hit and `resolve_container` went on to ask the server about it, at which
/// point the daemon read the link's target. The read path itself was already
/// protected, which is exactly why the label went unquestioned.
///
/// [`Boundary::inside`] is the same rule `LineIndex` and `callgraph` use, so
/// there is one answer to "is this path ours" in the crate rather than three.
fn is_inside(boundary: &Boundary, site: &Site) -> bool {
    match &site.path {
        Some(path) => boundary.inside(path).is_some(),
        // A non-`file:` URI is not in this workspace.
        None => false,
    }
}

/// Splits `A::B::c` into qualifiers (outermost first) and the bare name.
///
/// The separators are the ones the supported languages use: `::` (Rust, C++),
/// `.` (Go, Python, Java), `#` (Ruby), `\` (PHP namespaces) and `/`.
fn split_qualified(raw: &str) -> (Vec<String>, String) {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ':' if chars.peek() == Some(&':') => {
                chars.next();
                parts.push(std::mem::take(&mut current));
            }
            '.' | '#' | '\\' | '/' => parts.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    parts.push(current);
    let mut parts: Vec<String> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    let name = parts.pop().unwrap_or_default();
    (parts, name)
}

/// `path` relative to `boundary`, or the absolute path when it is outside.
pub(crate) fn display_path(boundary: &Path, path: &Path) -> String {
    let normalized = normalize(path);
    match normalized.strip_prefix(normalize(boundary)) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => normalized.display().to_string(),
    }
}

/// Resolves `.` and `..` without touching the disk, keeping a `..` that would
/// pop past the start (dropping it would point somewhere else).
fn normalize(path: &Path) -> PathBuf {
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

/// The `file://` URI a server expects.
pub(crate) fn file_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn invalid_args(message: &str) -> ToolOutput {
    ToolOutput::error(format!("[invalid_args] {message}"))
}

fn cancelled() -> ToolOutput {
    ToolOutput::error("[cancelled] the LSP request was cancelled")
}

fn non_empty_string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn integer(obj: &Map<String, Value>, key: &str) -> Result<Option<u32>, ToolOutput> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(0) => Err(invalid_args(&format!("`{key}` must be 1 or greater"))),
            Some(number) if number <= u64::from(u32::MAX) => Ok(Some(number as u32)),
            _ => Err(invalid_args(&format!(
                "`{key}` must be an integer of 1 or greater"
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencraylsp_core::backend::DiagnosticsReport;
    use opencraylsp_proto::{Indexing, StatusReport};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    // ---- a scripted backend: per-target answers, an optional delay, and a
    // ---- call log, so fan-out behaviour is observable.

    struct FakeBackend {
        boundary: PathBuf,
        encoding: PositionEncoding,
        languages: Vec<LanguageInfo>,
        responses: Mutex<HashMap<String, Result<Value, LspError>>>,
        delay: Mutex<Duration>,
        file_delay: Mutex<Duration>,
        indexing: Mutex<Option<Indexing>>,
        calls: Mutex<Vec<String>>,
        file_calls: Mutex<Vec<String>>,
    }

    impl FakeBackend {
        fn new(boundary: impl Into<PathBuf>) -> Self {
            Self {
                boundary: boundary.into(),
                encoding: PositionEncoding::Utf16,
                languages: Vec::new(),
                responses: Mutex::new(HashMap::new()),
                delay: Mutex::new(Duration::ZERO),
                file_delay: Mutex::new(Duration::ZERO),
                indexing: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
                file_calls: Mutex::new(Vec::new()),
            }
        }

        fn language(
            mut self,
            name: &str,
            server: &str,
            extensions: &[&str],
            installed: bool,
        ) -> Self {
            self.languages.push(LanguageInfo {
                name: name.to_owned(),
                server: server.to_owned(),
                extensions: extensions.iter().map(|e| (*e).to_owned()).collect(),
                root_markers: Vec::new(),
                installed,
                detected: true,
                enabled: true,
            });
            self
        }

        fn encoding(mut self, encoding: PositionEncoding) -> Self {
            self.encoding = encoding;
            self
        }

        fn respond(&self, key: &str, result: Result<Value, LspError>) {
            self.responses
                .lock()
                .expect("lock")
                .insert(key.to_owned(), result);
        }

        fn set_delay(&self, delay: Duration) {
            *self.delay.lock().expect("lock") = delay;
        }

        /// A delay applied only to file-routed requests (the container lookup),
        /// so a test can slow the lookup while the name query stays instant.
        fn set_file_delay(&self, delay: Duration) {
            *self.file_delay.lock().expect("lock") = delay;
        }

        fn file_calls(&self) -> Vec<String> {
            self.file_calls.lock().expect("lock").clone()
        }

        fn set_indexing(&self, indexing: Option<Indexing>) {
            *self.indexing.lock().expect("lock") = indexing;
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock").clone()
        }

        /// A delay that a cancellation can cut short, the way a real request
        /// abandons its wait.
        async fn pause_for(&self, delay: Duration, cancel: &CancellationToken) {
            let steps = delay.as_millis() / 10;
            for _ in 0..steps {
                if cancel.is_cancelled() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        async fn pause(&self, cancel: &CancellationToken) {
            let delay = *self.delay.lock().expect("lock");
            self.pause_for(delay, cancel).await;
        }

        fn served(&self, key: &str) -> Result<Served, LspError> {
            let reply = self
                .responses
                .lock()
                .expect("lock")
                .get(key)
                .cloned()
                .unwrap_or(Ok(Value::Null));
            reply.map(|value| Served {
                value,
                encoding: self.encoding,
                server: key.to_owned(),
                root: self.boundary.clone(),
                indexing: self.indexing.lock().expect("lock").clone(),
            })
        }
    }

    #[async_trait::async_trait]
    impl LspBackend for FakeBackend {
        async fn request(
            &self,
            file: &Path,
            _method: &str,
            _params: Value,
            cancel: &CancellationToken,
        ) -> Result<Served, LspError> {
            self.calls.lock().expect("lock").push(String::new());
            self.file_calls
                .lock()
                .expect("lock")
                .push(file.display().to_string());
            let delay = *self.file_delay.lock().expect("lock");
            self.pause_for(delay, cancel).await;
            if cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            self.served("")
        }

        async fn request_workspace(
            &self,
            server: &str,
            _method: &str,
            _params: Value,
            cancel: &CancellationToken,
        ) -> Result<Served, LspError> {
            self.calls.lock().expect("lock").push(server.to_owned());
            self.pause(cancel).await;
            if cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            self.served(server)
        }

        async fn diagnostics(
            &self,
            _file: &Path,
            _cancel: &CancellationToken,
        ) -> Result<DiagnosticsReport, LspError> {
            Err(LspError::Cancelled)
        }

        fn resolve_path(&self, file_path: &str) -> Result<PathBuf, LspError> {
            let joined = if Path::new(file_path).is_absolute() {
                PathBuf::from(file_path)
            } else {
                self.boundary.join(file_path)
            };
            let normalized = normalize(&joined);
            if normalized.starts_with(normalize(&self.boundary)) {
                Ok(normalized)
            } else {
                Err(LspError::OutsideWorkspace {
                    path: file_path.to_owned(),
                    boundary: self.boundary.display().to_string(),
                })
            }
        }

        fn boundary(&self) -> PathBuf {
            self.boundary.clone()
        }

        fn languages(&self) -> Vec<LanguageInfo> {
            self.languages.clone()
        }

        async fn status(&self) -> StatusReport {
            panic!("the resolver never asks for status")
        }

        async fn shutdown(&self) {}
    }

    // ---- helpers -----------------------------------------------------------

    fn workspace() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    fn uri(root: &Path, name: &str) -> String {
        format!("file://{}", root.join(name).display())
    }

    fn sym(
        name: &str,
        kind: u32,
        container: Option<&str>,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Value {
        let mut item = json!({
            "name": name,
            "kind": kind,
            "location": {
                "uri": uri,
                "range": { "start": { "line": line, "character": character } }
            }
        });
        if let Some(container) = container {
            item["containerName"] = json!(container);
        }
        item
    }

    async fn run(backend: &FakeBackend, args: Value) -> Result<Resolution, ToolOutput> {
        let spec = parse_target(&args).expect("the target parses");
        resolve(backend, &spec, &CancellationToken::new()).await
    }

    fn one(resolution: &Resolution) -> &Candidate {
        match &resolution.resolved {
            Resolved::One(candidate) => candidate,
            other => panic!("expected one candidate, got {other:?}"),
        }
    }

    fn many(resolution: &Resolution) -> &[Candidate] {
        match &resolution.resolved {
            Resolved::Many(candidates) => candidates,
            other => panic!("expected many candidates, got {other:?}"),
        }
    }

    fn timeout(server: &str) -> LspError {
        LspError::Timeout {
            server: server.to_owned(),
            method: METHOD.to_owned(),
            ms: 30_000,
        }
    }

    // ---- reading the file a target points at -------------------------------

    #[tokio::test]
    async fn a_target_in_a_file_that_cannot_be_read_says_so_without_claiming_it_exists() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);

        // Nothing was written at this path. The boundary checks pass (the path
        // is inside the workspace and is not `..`-laden), so what failed is the
        // read, and the read is the only thing the message may speak about.
        let Err(output) = run(
            &backend,
            json!({ "path": "missing.rs", "line": 1, "column": 1 }),
        )
        .await
        else {
            panic!("a file that cannot be read must not resolve to a position");
        };
        assert!(output.is_error, "{}", output.text);
        assert!(output.text.starts_with("[io_error] "), "{}", output.text);
        assert!(output.text.contains("missing.rs"), "{}", output.text);
        // Whether the file exists is exactly what is unknown here. An agent that
        // reads "it exists" goes hunting for a permissions problem that is not
        // there, instead of for the typo in the path.
        assert!(!output.text.contains("it exists"), "{}", output.text);
    }

    // ---- the decision table -----------------------------------------------

    #[tokio::test]
    async fn table_row_01_plain_function() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym(
                "parse",
                12,
                Some("util"),
                &uri(&root, "a.rs"),
                0,
                0
            )])),
        );
        let resolution = run(&backend, json!({ "symbol": "parse" })).await.unwrap();
        assert_eq!(one(&resolution).name, "parse");
    }

    #[tokio::test]
    async fn table_row_02_qualified_method() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([
                sym("new", 6, Some("Foo"), &uri(&root, "a.rs"), 1, 0),
                sym("new", 6, Some("Bar"), &uri(&root, "b.rs"), 1, 0),
            ])),
        );
        let resolution = run(&backend, json!({ "symbol": "Foo::new" }))
            .await
            .unwrap();
        assert_eq!(one(&resolution).container.as_deref(), Some("Foo"));
    }

    #[tokio::test]
    async fn table_row_03_same_name_many() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([
                sym("new", 6, Some("Foo"), &uri(&root, "a.rs"), 1, 0),
                sym("new", 6, Some("Bar"), &uri(&root, "b.rs"), 1, 0),
            ])),
        );
        let resolution = run(&backend, json!({ "symbol": "new" })).await.unwrap();
        assert_eq!(many(&resolution).len(), 2);
    }

    #[tokio::test]
    async fn table_row_04_dot_qualified() {
        // Real shape (gopls v0.22.0 fixture): a symbol's `containerName` is its
        // *package* (`example.com/probe`) and its `name` is bare, so the
        // spelling that disambiguates in Go is `probe.New`.
        let resolution = run_fixture(
            "gopls_workspace_symbol_new.json",
            vec![("go", "gopls", &["go"])],
            "probe.New",
        )
        .await;
        assert_eq!(one(&resolution).name, "New");
    }

    #[tokio::test]
    async fn table_row_05_case_sensitive_preferred() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([
                sym("foo", 12, None, &uri(&root, "a.rs"), 0, 0),
                sym("Foo", 23, None, &uri(&root, "a.rs"), 5, 0),
            ])),
        );
        let resolution = run(&backend, json!({ "symbol": "foo" })).await.unwrap();
        assert_eq!(one(&resolution).name, "foo");
        assert!(resolution.notes.is_empty());
    }

    #[tokio::test]
    async fn table_row_06_case_insensitive_match() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym("foo", 12, None, &uri(&root, "a.rs"), 0, 0)])),
        );
        let resolution = run(&backend, json!({ "symbol": "Foo" })).await.unwrap();
        assert_eq!(one(&resolution).name, "foo");
        assert!(
            resolution
                .notes
                .iter()
                .any(|note| note.contains("matched case-insensitively"))
        );
    }

    #[tokio::test]
    async fn table_row_07_not_found_suggestions() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([
                sym("handle_request", 12, None, &uri(&root, "a.rs"), 0, 0),
                sym("handle_error", 12, None, &uri(&root, "a.rs"), 5, 0),
            ])),
        );
        let resolution = run(&backend, json!({ "symbol": "handle" })).await.unwrap();
        match resolution.resolved {
            Resolved::NotFound { suggestions } => {
                assert_eq!(suggestions.len(), 2);
                let names: Vec<&str> = suggestions.iter().map(|c| c.name.as_str()).collect();
                assert!(names.contains(&"handle_request") && names.contains(&"handle_error"));
            }
            other => panic!("expected not_found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn table_row_08_impl_generic_container() {
        // Real shape: rust-analyzer spells an impl block `impl TaskRunner for
        // Recording`, so both the trait and the type are offered as containers.
        // The candidate is placed at the coordinates the recorded
        // `documentSymbol` fixture gives `start`, so nothing here is invented.
        let tree = fixture("rust_analyzer_document_symbol_task_runner.json");
        let (line, character) = nodes_under(&tree["result"], &[])
            .into_iter()
            .find(|(name, _, _, chain)| {
                name == "start" && chain.iter().any(|ancestor| ancestor.contains("Recording"))
            })
            .map(|(_, line, character, _)| (line, character))
            .expect("the recorded tree nests `start` under the impl");

        let backend = FakeBackend::new("/ws").language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym(
                "start",
                6,
                None,
                "file:///ws/crates/core-lib/src/task_runner.rs",
                line,
                character
            )])),
        );
        backend.respond("", Ok(tree["result"].clone()));
        let resolution = run(&backend, json!({ "symbol": "Recording::start" }))
            .await
            .unwrap();
        assert_eq!(one(&resolution).name, "start");
    }

    #[tokio::test]
    async fn table_row_09_qualifier_mismatch_single_reverts() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym(
                "new",
                6,
                Some("Foo"),
                &uri(&root, "a.rs"),
                1,
                0
            )])),
        );
        let resolution = run(&backend, json!({ "symbol": "Baz::new" }))
            .await
            .unwrap();
        assert_eq!(one(&resolution).container.as_deref(), Some("Foo"));
    }

    #[tokio::test]
    async fn table_row_10_qualifier_mismatch_two_reverts() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([
                sym("new", 6, Some("Foo"), &uri(&root, "a.rs"), 1, 0),
                sym("new", 6, Some("Bar"), &uri(&root, "b.rs"), 1, 0),
            ])),
        );
        let resolution = run(&backend, json!({ "symbol": "Baz::new" }))
            .await
            .unwrap();
        assert_eq!(many(&resolution).len(), 2);
    }

    #[tokio::test]
    async fn table_row_11_last_qualifier_only() {
        // Real fixtures: the symbol sits inside `mod tests`, so the last
        // qualifier `tests` names its ancestor and the lookup settles on one.
        let resolution = run_fixture_with_tree(
            "rust_analyzer_workspace_symbol_task_runner.json",
            "rust_analyzer_document_symbol_task_runner.json",
            "tests::a_registered_runner_is_found_by_its_mode_and_can_be_removed",
        )
        .await;
        assert_eq!(
            one(&resolution).name,
            "a_registered_runner_is_found_by_its_mode_and_can_be_removed"
        );
    }

    #[tokio::test]
    async fn table_row_12_dedupes_identical_sites() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root)
            .language("rust", "rust-analyzer", &["rs"], true)
            .language("go", "gopls", &["go"], true);
        let item = sym("x", 12, None, &uri(&root, "a.rs"), 3, 4);
        backend.respond("rust-analyzer", Ok(json!([item.clone()])));
        backend.respond("gopls", Ok(json!([item])));
        let resolution = run(&backend, json!({ "symbol": "x" })).await.unwrap();
        assert_eq!(one(&resolution).name, "x");
    }

    #[tokio::test]
    async fn table_row_13_prefers_inside_workspace() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([
                sym("map", 12, None, "file:///usr/lib/std.rs", 3, 0),
                sym("map", 12, None, &uri(&root, "a.rs"), 1, 0),
            ])),
        );
        let resolution = run(&backend, json!({ "symbol": "map" })).await.unwrap();
        let candidate = one(&resolution);
        assert!(!candidate.outside_workspace);
    }

    #[tokio::test]
    async fn table_row_14_outside_workspace_kept() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym(
                "map",
                12,
                None,
                "file:///usr/lib/std.rs",
                3,
                0
            )])),
        );
        let resolution = run(&backend, json!({ "symbol": "map" })).await.unwrap();
        assert!(one(&resolution).outside_workspace);
    }

    #[tokio::test]
    async fn table_row_15_kind_filter() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([
                sym("Thing", 23, None, &uri(&root, "a.rs"), 0, 0),
                sym("Thing", 12, None, &uri(&root, "a.rs"), 9, 0),
            ])),
        );
        let resolution = run(&backend, json!({ "symbol": "Thing", "kind": "function" }))
            .await
            .unwrap();
        assert_eq!(one(&resolution).kind, 12);
    }

    #[tokio::test]
    async fn table_row_16_non_ascii_name() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        let name = "\u{8655}\u{7406}\u{8acb}\u{6c42}";
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym(name, 12, None, &uri(&root, "a.rs"), 0, 0)])),
        );
        let resolution = run(&backend, json!({ "symbol": name })).await.unwrap();
        assert_eq!(one(&resolution).name, name);
    }

    #[test]
    fn table_row_17_blank_symbol_is_invalid() {
        let error = parse_target(&json!({ "symbol": "  " })).unwrap_err();
        assert!(error.text.starts_with("[invalid_args]"));
    }

    #[test]
    fn table_row_18_symbol_and_line_are_exclusive() {
        let error = parse_target(&json!({ "symbol": "Foo", "line": 3 })).unwrap_err();
        assert!(error.text.starts_with("[invalid_args]"));
        assert!(error.text.contains("mutually exclusive"));
    }

    /// A half-given position with a name is still a contradiction: there is no
    /// file to look in, or no column to go on.
    #[test]
    fn table_row_18b_a_half_given_position_with_a_symbol_is_rejected() {
        for extra in [
            json!({ "line": 3, "column": 1 }),
            json!({ "path": "a.rs", "line": 3 }),
        ] {
            let mut args = json!({ "symbol": "Foo" });
            for (key, value) in extra.as_object().unwrap() {
                args[key] = value.clone();
            }
            let error = parse_target(&args).unwrap_err();
            assert!(error.text.contains("mutually exclusive"), "{}", error.text);
        }
    }

    /// The `[ambiguous]` answer hands out `path`+`line`+`column`; a model that
    /// pastes those in beside the name it already had must still be understood,
    /// because the position names exactly one place. Rejecting it sends the model
    /// back to the tool to guess a second time over the same lookup.
    #[test]
    fn a_complete_position_wins_over_a_symbol() {
        let spec = parse_target(&json!({
            "symbol": "Foo",
            "path": "src/a.rs",
            "line": 12,
            "column": 4,
        }))
        .expect("a position is enough on its own");
        assert!(
            matches!(&spec, TargetSpec::Position { path, line, column }
                if path == "src/a.rs" && *line == 12 && *column == 4),
            "{spec:?}"
        );
    }

    /// And the name still works on its own, unchanged.
    /// A column past the end of its line is the caller's off-by-one, and saying
    /// so beats clamping to the line's end and asking the server about nothing.
    /// The length is in Unicode scalars, the counting the tools print.
    #[tokio::test]
    async fn a_column_past_the_end_of_its_line_is_an_honest_error() {
        let (_d, root) = workspace();
        std::fs::write(root.join("a.rs"), "let 變數 = 1;\n").expect("write");
        let backend = FakeBackend::new(&root);
        let error = run(&backend, json!({ "path": "a.rs", "line": 1, "column": 30 }))
            .await
            .unwrap_err();
        assert!(error.text.starts_with("[invalid_args]"), "{}", error.text);
        assert!(
            error.text.contains("11 characters"),
            "the real line length must be stated: {}",
            error.text
        );
        // `let 變數 = 1;` is 11 scalars and 15 bytes; the answer is about the
        // scalars, which is what the tools count in.
        assert!(error.text.contains("30 is past the end"), "{}", error.text);
    }

    /// One past the end is a real position: the editor puts the caret there.
    #[tokio::test]
    async fn a_column_one_past_the_end_is_still_a_position() {
        let (_d, root) = workspace();
        std::fs::write(root.join("a.rs"), "abc\n").expect("write");
        let backend = FakeBackend::new(&root);
        run(&backend, json!({ "path": "a.rs", "line": 1, "column": 4 }))
            .await
            .expect("column 4 of a 3-character line points just after it");
    }

    #[test]
    fn a_symbol_without_a_position_is_still_a_symbol() {
        let spec = parse_target(&json!({ "symbol": "Foo", "path": "src/a.rs" })).expect("symbol");
        assert!(matches!(spec, TargetSpec::Symbol { .. }), "{spec:?}");
    }

    #[test]
    fn table_row_19_line_without_column_is_invalid() {
        let error = parse_target(&json!({ "path": "a.rs", "line": 3 })).unwrap_err();
        assert!(error.text.starts_with("[invalid_args]"));
        assert!(error.text.contains("`line` and `column`"));
    }

    #[tokio::test]
    async fn table_row_20_line_past_end_is_invalid() {
        let (_d, root) = workspace();
        std::fs::write(root.join("a.rs"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").expect("write");
        let backend = FakeBackend::new(&root);
        let error = run(
            &backend,
            json!({ "path": "a.rs", "line": 999, "column": 1 }),
        )
        .await
        .unwrap_err();
        assert!(error.text.starts_with("[invalid_args]"));
        assert!(error.text.contains("10 lines"), "{}", error.text);
    }

    #[tokio::test]
    async fn table_row_21_indexing_is_not_not_found() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.set_indexing(Some(Indexing {
            message: "roots scanned".to_owned(),
            percent: Some(40),
        }));
        backend.respond("rust-analyzer", Ok(Value::Null));
        let error = run(&backend, json!({ "symbol": "Foo" })).await.unwrap_err();
        assert!(error.text.starts_with("[indexing]"), "{}", error.text);
    }

    /// One server answers "nothing" while another is still indexing: the
    /// second may hold the symbol, so the answer must not be `not_found`.
    #[test]
    fn an_empty_answer_beside_an_indexing_server_is_indexing() {
        let boundary = Boundary::new(Path::new("/ws"));
        let outcomes = vec![Outcome::Empty, Outcome::Indexing("gopls".to_owned())];
        let error = fold(outcomes, &boundary).unwrap_err();
        assert!(error.text.starts_with("[indexing]"), "{}", error.text);
    }

    #[tokio::test]
    async fn table_row_22_null_without_indexing_is_not_found() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond("rust-analyzer", Ok(Value::Null));
        let resolution = run(&backend, json!({ "symbol": "Foo" })).await.unwrap();
        assert!(matches!(resolution.resolved, Resolved::NotFound { .. }));
    }

    #[tokio::test]
    async fn table_row_23_partial_failure_keeps_results() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root)
            .language("rust", "rust-analyzer", &["rs"], true)
            .language("go", "gopls", &["go"], true);
        backend.respond("rust-analyzer", Err(timeout("rust-analyzer")));
        backend.respond(
            "gopls",
            Ok(json!([sym("Foo", 5, None, &uri(&root, "a.go"), 0, 0)])),
        );
        let resolution = run(&backend, json!({ "symbol": "Foo" })).await.unwrap();
        assert_eq!(one(&resolution).name, "Foo");
        assert!(
            resolution
                .notes
                .iter()
                .any(|note| note == "note: rust-analyzer: timeout"),
            "{:?}",
            resolution.notes
        );
    }

    #[tokio::test]
    async fn table_row_24_all_failures_return_the_first() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root)
            .language("rust", "rust-analyzer", &["rs"], true)
            .language("go", "gopls", &["go"], true);
        backend.respond("rust-analyzer", Err(timeout("rust-analyzer")));
        backend.respond("gopls", Err(timeout("gopls")));
        let error = run(&backend, json!({ "symbol": "Foo" })).await.unwrap_err();
        assert!(error.text.starts_with("[timeout]"), "{}", error.text);
    }

    #[tokio::test]
    async fn table_row_25_cancelled() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let spec = parse_target(&json!({ "symbol": "Foo" })).unwrap();
        let error = resolve(&backend, &spec, &cancel).await.unwrap_err();
        assert!(error.text.starts_with("[cancelled]"), "{}", error.text);
    }

    #[tokio::test]
    async fn table_row_26_empty_languages_is_language_disabled() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root);
        let error = run(&backend, json!({ "symbol": "Foo" })).await.unwrap_err();
        assert!(
            error.text.starts_with("[language_disabled]"),
            "{}",
            error.text
        );
        assert!(
            error.text.contains("no language is enabled"),
            "{}",
            error.text
        );
    }

    #[tokio::test]
    async fn table_row_27_path_extension_not_enabled_is_language_disabled() {
        let (_d, root) = workspace();
        std::fs::write(root.join("a.go"), "package a\n").expect("write");
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "",
            Err(LspError::LanguageDisabled {
                language: "go".to_owned(),
                enabled: vec!["rust".to_owned()],
            }),
        );
        let error = run(&backend, json!({ "symbol": "Foo", "path": "a.go" }))
            .await
            .unwrap_err();
        assert!(
            error.text.starts_with("[language_disabled]"),
            "{}",
            error.text
        );
        assert!(error.text.contains("enabled: rust"), "{}", error.text);
    }

    #[tokio::test]
    async fn table_row_28_a_failed_container_lookup_reverts_to_the_original_set() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym("new", 6, None, &uri(&root, "a.rs"), 1, 0)])),
        );
        backend.respond("", Err(timeout("rust-analyzer")));
        let resolution = run(&backend, json!({ "symbol": "Foo::new" }))
            .await
            .unwrap();
        assert_eq!(one(&resolution).name, "new");
    }

    #[tokio::test]
    async fn table_row_29_all_lookups_failing_reverts_too() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([
                sym("new", 6, None, &uri(&root, "a.rs"), 1, 0),
                sym("new", 6, None, &uri(&root, "b.rs"), 1, 0),
            ])),
        );
        backend.respond("", Err(timeout("rust-analyzer")));
        let resolution = run(&backend, json!({ "symbol": "Foo::new" }))
            .await
            .unwrap();
        assert_eq!(many(&resolution).len(), 2);
    }

    #[tokio::test]
    async fn table_row_30_a_capped_fuzzy_answer_says_it_may_be_truncated() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        let items: Vec<Value> = (0..30)
            .map(|index| {
                sym(
                    &format!("handler_{index:02}"),
                    12,
                    None,
                    &uri(&root, "a.rs"),
                    index,
                    0,
                )
            })
            .collect();
        backend.respond("rust-analyzer", Ok(json!(items)));
        let resolution = run(&backend, json!({ "symbol": "new" })).await.unwrap();
        assert!(matches!(resolution.resolved, Resolved::NotFound { .. }));
        assert!(
            resolution
                .notes
                .iter()
                .any(|note| note.contains("maximum of 30 results")),
            "{:?}",
            resolution.notes
        );
    }

    #[tokio::test]
    async fn table_row_31_cancelling_during_the_container_lookup_is_cancelled() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym("new", 6, None, &uri(&root, "a.rs"), 1, 0)])),
        );
        backend.respond("", Ok(Value::Null));
        backend.set_file_delay(Duration::from_millis(500));
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            trigger.cancel();
        });
        let spec = parse_target(&json!({ "symbol": "Foo::new" })).unwrap();
        let started = Instant::now();
        let error = resolve(&backend, &spec, &cancel).await.unwrap_err();
        assert!(error.text.starts_with("[cancelled]"), "{}", error.text);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn table_row_32_the_lookup_asks_at_most_eight_files() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        let items: Vec<Value> = (0..12)
            .map(|index| sym("new", 6, None, &uri(&root, &format!("a{index}.rs")), 1, 0))
            .collect();
        backend.respond("rust-analyzer", Ok(json!(items)));
        backend.respond("", Ok(Value::Null));
        let _ = run(&backend, json!({ "symbol": "Foo::new" }))
            .await
            .unwrap();
        assert!(
            backend.file_calls().len() <= 8,
            "{:?}",
            backend.file_calls()
        );
    }

    // ---- the rest of the behaviour ----------------------------------------

    #[tokio::test]
    async fn a_server_with_no_installed_language_is_a_no_server_error() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], false);
        let error = run(&backend, json!({ "symbol": "Foo" })).await.unwrap_err();
        assert!(error.text.starts_with("[no_server]"), "{}", error.text);
        assert!(error.text.contains("rust-analyzer (rs)"), "{}", error.text);
    }

    #[tokio::test]
    async fn a_path_hint_routes_through_the_file_and_picks_the_extension_server() {
        let (_d, root) = workspace();
        std::fs::write(root.join("a.rs"), "fn main() {}\n").expect("write");
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.respond("", Ok(json!([])));
        let resolution = run(&backend, json!({ "symbol": "main", "path": "a.rs" }))
            .await
            .unwrap();
        assert!(matches!(resolution.resolved, Resolved::NotFound { .. }));
        assert_eq!(backend.calls(), vec![String::new()], "asked via the file");
    }

    #[tokio::test]
    async fn fan_out_runs_queries_concurrently() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root)
            .language("rust", "rust-analyzer", &["rs"], true)
            .language("go", "gopls", &["go"], true);
        backend.set_delay(Duration::from_millis(200));
        backend.respond(
            "rust-analyzer",
            Ok(json!([sym("Foo", 5, None, &uri(&root, "a.rs"), 0, 0)])),
        );
        backend.respond(
            "gopls",
            Ok(json!([sym("Foo", 5, None, &uri(&root, "b.go"), 0, 0)])),
        );
        let started = Instant::now();
        let resolution = run(&backend, json!({ "symbol": "Foo" })).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(many(&resolution).len(), 2);
        assert!(
            elapsed < Duration::from_millis(350),
            "two 200 ms queries took {elapsed:?}; they did not run concurrently"
        );
    }

    #[tokio::test]
    async fn fan_out_asks_a_server_shared_by_two_languages_once() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root)
            .language(
                "typescript",
                "typescript-language-server",
                &["ts", "tsx"],
                true,
            )
            .language(
                "javascript",
                "typescript-language-server",
                &["js", "jsx"],
                true,
            );
        backend.respond(
            "typescript-language-server",
            Ok(json!([sym("Foo", 5, None, &uri(&root, "a.ts"), 0, 0)])),
        );
        let resolution = run(&backend, json!({ "symbol": "Foo" })).await.unwrap();
        assert_eq!(one(&resolution).name, "Foo");
        assert_eq!(backend.calls().len(), 1, "one server, one query");
    }

    #[tokio::test]
    async fn cancelling_midflight_abandons_the_wait() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        backend.set_delay(Duration::from_millis(600));
        backend.respond("rust-analyzer", Ok(json!([])));
        let cancel = CancellationToken::new();
        let spec = parse_target(&json!({ "symbol": "Foo" })).unwrap();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let error = resolve(&backend, &spec, &cancel).await.unwrap_err();
        assert!(error.text.starts_with("[cancelled]"), "{}", error.text);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "cancellation took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_language_hint_narrows_to_its_server() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root)
            .language("rust", "rust-analyzer", &["rs"], true)
            .language("go", "gopls", &["go"], true);
        backend.respond(
            "gopls",
            Ok(json!([sym("Foo", 5, None, &uri(&root, "a.go"), 0, 0)])),
        );
        backend.respond("rust-analyzer", Ok(json!([])));
        let resolution = run(&backend, json!({ "symbol": "Foo", "language": "go" }))
            .await
            .unwrap();
        assert_eq!(one(&resolution).name, "Foo");
        assert_eq!(backend.calls(), vec!["gopls".to_owned()]);
    }

    #[tokio::test]
    async fn an_unknown_language_hint_is_language_disabled() {
        let (_d, root) = workspace();
        let backend = FakeBackend::new(&root).language("rust", "rust-analyzer", &["rs"], true);
        let error = run(&backend, json!({ "symbol": "Foo", "language": "go" }))
            .await
            .unwrap_err();
        assert!(
            error.text.starts_with("[language_disabled]"),
            "{}",
            error.text
        );
    }

    /// The two paths — a name lookup and a raw position — must land in the same
    /// unit, and full-width characters are where the units disagree.
    #[tokio::test]
    async fn both_paths_agree_on_columns_across_full_width_characters() {
        // "let " (4 units) then two CJK scalars (1 UTF-16 unit, 3 UTF-8 bytes
        // each), then " = 1;".
        let line = "let \u{4e2d}\u{6587} = 1;";
        let (_d, root) = workspace();
        std::fs::write(root.join("a.rs"), format!("{line}\n")).expect("write");

        // The position path: the model's 7th column is the space after the two
        // full-width characters, which is UTF-16 unit 6.
        let position_backend = FakeBackend::new(&root);
        let resolution = run(
            &position_backend,
            json!({ "path": "a.rs", "line": 1, "column": 7 }),
        )
        .await
        .unwrap();
        assert_eq!(one(&resolution).site.character, Some(6));

        // The name path with a UTF-8 server: byte offset 7 is one past "let "
        // plus the three-byte first scalar, i.e. the second full-width scalar,
        // which is UTF-16 unit 5.
        let symbol_backend = FakeBackend::new(&root)
            .language("rust", "rust-analyzer", &["rs"], true)
            .encoding(PositionEncoding::Utf8);
        symbol_backend.respond(
            "rust-analyzer",
            Ok(json!([sym("x", 12, None, &uri(&root, "a.rs"), 0, 7)])),
        );
        let resolution = run(&symbol_backend, json!({ "symbol": "x" }))
            .await
            .unwrap();
        assert_eq!(one(&resolution).site.character, Some(5));
    }

    #[tokio::test]
    async fn a_position_candidate_renders_in_the_models_own_counting() {
        let line = "let \u{4e2d}\u{6587} = 1;";
        let (_d, root) = workspace();
        std::fs::write(root.join("a.rs"), format!("{line}\n")).expect("write");
        let backend = FakeBackend::new(&root);
        let resolution = run(&backend, json!({ "path": "a.rs", "line": 1, "column": 7 }))
            .await
            .unwrap();
        let lines = LineIndex::lazy(root.clone());
        let view = crate::resolve_render::candidate_view(&root, &lines);
        assert_eq!(view.position_of(&one(&resolution).site), "a.rs:1:7");
    }

    #[test]
    fn parse_target_accepts_both_shapes() {
        assert_eq!(
            parse_target(&json!({ "symbol": "Foo::bar" })).unwrap(),
            TargetSpec::Symbol {
                name: "Foo::bar".to_owned(),
                path: None,
                kind: None,
                language: None,
            }
        );
        assert_eq!(
            parse_target(&json!({ "path": "a.rs", "line": 2, "column": 3 })).unwrap(),
            TargetSpec::Position {
                path: "a.rs".to_owned(),
                line: 2,
                column: 3,
            }
        );
    }

    #[test]
    fn split_qualified_handles_every_separator() {
        assert_eq!(
            split_qualified("A::B::c"),
            (vec!["A".to_owned(), "B".to_owned()], "c".to_owned())
        );
        assert_eq!(
            split_qualified("Foo.bar"),
            (vec!["Foo".to_owned()], "bar".to_owned())
        );
        assert_eq!(
            split_qualified("App\\Ns\\Thing"),
            (vec!["App".to_owned(), "Ns".to_owned()], "Thing".to_owned())
        );
        assert_eq!(
            split_qualified("pkg/sub#Thing"),
            (vec!["pkg".to_owned(), "sub".to_owned()], "Thing".to_owned())
        );
        assert_eq!(split_qualified("plain"), (Vec::new(), "plain".to_owned()));
    }

    // ---- real-server fixtures (recorded, not invented) ---------------------

    /// A recorded `workspace/symbol` answer, so the tests run against the shape
    /// a real server actually sends rather than the shape we assumed.
    fn fixture(name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
    }

    async fn run_fixture(
        name: &str,
        languages: Vec<(&str, &str, &[&str])>,
        symbol: &str,
    ) -> Resolution {
        let answer = fixture(name);
        let mut backend = FakeBackend::new("/ws");
        for (language, server, extensions) in languages {
            backend = backend.language(language, server, extensions, true);
        }
        backend.respond(&backend.languages[0].server, Ok(answer["result"].clone()));
        run(&backend, json!({ "symbol": symbol })).await.unwrap()
    }

    /// Like [`run_fixture`], with a recorded `documentSymbol` answer served for
    /// the container lookup.
    async fn run_fixture_with_tree(
        symbol_fixture: &str,
        tree_fixture: &str,
        symbol: &str,
    ) -> Resolution {
        let answer = fixture(symbol_fixture);
        let tree = fixture(tree_fixture);
        let backend = FakeBackend::new("/ws").language("rust", "rust-analyzer", &["rs"], true);
        backend.respond("rust-analyzer", Ok(answer["result"].clone()));
        backend.respond("", Ok(tree["result"].clone()));
        run(&backend, json!({ "symbol": symbol })).await.unwrap()
    }

    /// Every node of a recorded `documentSymbol` answer as
    /// `(name, line, character, ancestor names)`.
    fn nodes_under(nodes: &Value, ancestors: &[String]) -> Vec<(String, u32, u32, Vec<String>)> {
        let mut out = Vec::new();
        for node in nodes.as_array().into_iter().flatten() {
            let name = node["name"].as_str().unwrap_or_default().to_owned();
            let line = node["range"]["start"]["line"].as_u64().unwrap_or(0) as u32;
            let character = node["range"]["start"]["character"].as_u64().unwrap_or(0) as u32;
            out.push((name.clone(), line, character, ancestors.to_vec()));
            let mut deeper = ancestors.to_vec();
            deeper.push(name);
            out.extend(nodes_under(&node["children"], &deeper));
        }
        out
    }

    /// gopls fills `containerName` with the **package path**, not the type.
    /// That is exactly why a qualifier is a best-effort hint
    /// and not a hard filter, and this fixture is what proves it.
    #[tokio::test]
    async fn gopls_fixture_resolves_a_real_package_qualified_symbol() {
        let resolution = run_fixture(
            "gopls_workspace_symbol_new.json",
            vec![("go", "gopls", &["go"])],
            "New",
        )
        .await;
        let candidate = one(&resolution);
        assert_eq!(candidate.name, "New");
        assert_eq!(candidate.kind, 12);
        assert_eq!(candidate.container.as_deref(), Some("example.com/probe"));
        assert_eq!(candidate.site.line, Some(10));
        assert!(!candidate.outside_workspace);
    }

    /// Real rust-analyzer answers `workspace/symbol` with **fuzzy
    /// (subsequence) matches and no `containerName` at all**: the 30 hits for
    /// `new` are `NewCronJob`, `HeadlessNetworkRequest`, …, none of them
    /// exactly `new`. The honest answer is a miss with near-miss suggestions —
    /// never the first hit dressed up as the symbol the model asked for.
    #[tokio::test]
    async fn rust_analyzer_fixture_never_guesses_among_fuzzy_hits() {
        let resolution = run_fixture(
            "rust_analyzer_workspace_symbol_new.json",
            vec![("rust", "rust-analyzer", &["rs"])],
            "new",
        )
        .await;
        match resolution.resolved {
            Resolved::NotFound { suggestions } => {
                assert!(!suggestions.is_empty(), "near misses should be offered");
                assert!(
                    suggestions
                        .iter()
                        .all(|candidate| candidate.name.to_lowercase().contains("new")),
                    "{suggestions:?}"
                );
            }
            other => panic!("expected not_found, got {other:?}"),
        }
    }

    /// Real rust-analyzer answers `LspConfig` with **two** exact hits (a
    /// re-export and the definition), which is a genuine ambiguity — the tool
    /// lists both rather than picking one.
    #[tokio::test]
    async fn rust_analyzer_fixture_reports_both_real_hits() {
        let resolution = run_fixture(
            "rust_analyzer_workspace_symbol_lspconfig.json",
            vec![("rust", "rust-analyzer", &["rs"])],
            "LspConfig",
        )
        .await;
        let candidates = many(&resolution);
        assert_eq!(candidates.len(), 2);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.name == "LspConfig")
        );
    }

    // ---- the sweep caps -----------------------------------------------

    /// The candidate list is bounded, and the bound is announced.
    #[test]
    fn a_huge_symbol_sweep_is_capped_and_says_so() {
        // More than `MAX_HITS` distinct sites, from a `workspace/symbol` answer that
        // is entirely legal: the protocol caps nothing and the transport accepts a
        // 64 MiB frame.
        let hits: Vec<Candidate> = (0..MAX_HITS + 500).map(candidate_at).collect();
        assert!(hits.len() > MAX_HITS);
        let (kept, notes) = fold(vec![Outcome::Hits(hits)], &Boundary::new("/ws")).expect("fold");
        assert_eq!(
            kept.len(),
            MAX_HITS,
            "the candidate list must be capped before anything else walks it"
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("500 further candidate(s) were dropped")),
            "a truncated answer must not read as complete: {notes:?}"
        );
    }

    /// A sweep at the cap is untouched, so the cap is a ceiling and not a wall.
    #[test]
    fn a_sweep_at_the_cap_keeps_every_candidate_and_adds_no_note() {
        let hits: Vec<Candidate> = (0..MAX_HITS).map(candidate_at).collect();
        let (kept, notes) = fold(vec![Outcome::Hits(hits)], &Boundary::new("/ws")).expect("fold");
        assert_eq!(kept.len(), MAX_HITS);
        assert!(
            !notes.iter().any(|note| note.contains("dropped")),
            "{notes:?}"
        );
    }

    /// `dedupe` is a hash lookup, not a scan.
    ///
    /// Timing rather than structure, because the property *is* the complexity: 40 000
    /// distinct sites are 8·10⁸ `String` comparisons through the linear scan this
    /// replaced, and a few million hash inserts. The budget is generous on purpose —
    /// it is there to catch a quadratic, not to measure anything.
    #[test]
    fn dedupe_handles_forty_thousand_distinct_sites_quickly() {
        let candidates: Vec<Candidate> = (0..40_000).map(candidate_at).collect();
        let started = std::time::Instant::now();
        let deduped = dedupe(candidates);
        let elapsed = started.elapsed();
        assert_eq!(deduped.len(), 40_000, "nothing should be dropped");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "deduping 40 000 sites took {elapsed:?}"
        );
    }

    /// And it still collapses the duplicates it exists to collapse.
    #[test]
    fn dedupe_still_collapses_repeated_sites() {
        let one = candidate_at(1);
        let same_site_other_name = Candidate {
            name: "other".to_owned(),
            ..candidate_at(1)
        };
        let deduped = dedupe(vec![
            one.clone(),
            same_site_other_name,
            candidate_at(2),
            one.clone(),
        ]);
        assert_eq!(deduped.len(), 2, "{deduped:?}");
    }

    /// A candidate at a distinct site, so `dedupe` has nothing to collapse.
    fn candidate_at(n: usize) -> Candidate {
        let line = u32::try_from(n).expect("the counts here are far below u32::MAX");
        Candidate {
            site: Site {
                path: Some(PathBuf::from(format!("/ws/file{n}.rs"))),
                uri: format!("file:///ws/file{n}.rs"),
                line: Some(line),
                character: Some(0),
            },
            name: format!("sym{n}"),
            kind: 12,
            container: None,
            server: "fake-ls".to_owned(),
            outside_workspace: false,
        }
    }

    // ---- the boundary label ---------------------------------

    /// A symlink inside the workspace is *not* labelled as inside it.
    ///
    /// `is_inside` used to do its own lexical comparison, which collapses `..`
    /// but cannot see a symlink. The label decides more than a note in the
    /// output: `combine` prefers an inside hit over an outside one, so a
    /// symlink pointing out of the workspace used to win the comparison and the
    /// daemon then read the link's target. The rule is now `Boundary::inside`,
    /// the same one `LineIndex` and `callgraph` use.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workspace_is_not_labelled_inside() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.rs"), "fn secret() {}").expect("secret");
        let workspace = tempfile::tempdir().expect("workspace");
        let link = workspace.path().join("link");
        symlink(outside.path(), &link).expect("symlink");

        let boundary = Boundary::new(workspace.path());
        let site = Site {
            path: Some(link.join("secret.rs")),
            uri: "file:///ws/link/secret.rs".to_owned(),
            line: Some(1),
            character: Some(0),
        };
        assert!(
            !is_inside(&boundary, &site),
            "a symlink leaving the workspace was labelled inside it"
        );
    }

    /// A real file inside is still inside, and a `..` that walks out is still
    /// out — so the change did not make the label blunter, only correct.
    #[test]
    fn the_boundary_label_still_sees_both_directions() {
        let workspace = tempfile::tempdir().expect("workspace");
        let inside = workspace.path().join("a.rs");
        std::fs::write(&inside, "fn a() {}").expect("write");
        let boundary = Boundary::new(workspace.path());

        let site = |path: PathBuf| Site {
            path: Some(path),
            uri: String::new(),
            line: Some(1),
            character: Some(0),
        };
        assert!(is_inside(&boundary, &site(inside.clone())));
        // Relative form, resolving to the same file.
        assert!(is_inside(
            &boundary,
            &site(workspace.path().join("sub/../a.rs"))
        ));
        // Out of bounds, lexically and by symlink-free `..`.
        assert!(!is_inside(&boundary, &site(PathBuf::from("/etc/passwd"))));
        assert!(!is_inside(
            &boundary,
            &site(workspace.path().join("../../etc/passwd"))
        ));
        // No path at all: a non-`file:` URI.
        let mut no_path = site(PathBuf::new());
        no_path.path = None;
        assert!(!is_inside(&boundary, &no_path));
    }

    /// The rule is the shared one, not a second copy: a boundary whose canonical
    /// form is unknown refuses, exactly as `LineIndex` does.
    #[test]
    fn the_boundary_label_fails_closed_like_every_other_boundary_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn a() {}").expect("write");
        // A root that resolves, with no canonical form recorded.
        let unresolved = Boundary::unresolved(dir.path());
        let site = Site {
            path: Some(file),
            uri: String::new(),
            line: Some(1),
            character: Some(0),
        };
        assert!(!is_inside(&unresolved, &site));
    }
}
