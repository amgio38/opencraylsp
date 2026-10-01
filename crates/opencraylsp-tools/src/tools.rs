//! The v1 `lsp_*` catalog: the schemas a model reads, and the
//! behaviour behind each name.
//!
//! Two things matter here and nowhere else in the crate:
//!
//! - **The description is the product.** A model picks a tool from its
//!   description alone, so each one says what it does, *when it beats grep*,
//!   and the shortest call that works.
//! - **Positions are always `path:line:column`, 1-based**, with a snippet when
//!   the file is inside the boundary. Every tool speaks the same dialect, so a
//!   model can chain one answer into the next call.

use std::path::{Path, PathBuf};

use opencraylsp_core::backend::{LspBackend, LspError, PositionEncoding, Served};
use opencraylsp_proto::{ToolAnnotations, ToolDef, ToolOutput};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::callgraph::{self, Direction};
use crate::error::render_error;
use crate::format::{self, LineIndex, View};
use crate::hint;
use crate::operations;
use crate::rename;
use crate::resolve::{self, Candidate, Resolved, TargetSpec};
use crate::resolve_render;

/// Every tool name, in catalog order.
pub const NAMES: [&str; 11] = [
    "lsp_status",
    "lsp_find_symbol",
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_implementations",
    "lsp_outline",
    "lsp_callers",
    "lsp_callees",
    "lsp_diagnostics",
    "lsp_rename_preview",
];

/// Longest snippet kept from a source line is [`crate::format`]'s business; the
/// tools only carry the number through the view.
const DEFAULT_FIND_LIMIT: usize = 50;
const MAX_FIND_LIMIT: usize = 200;
const DEFAULT_REFERENCE_LIMIT: usize = 100;

/// Most references `lsp_references` will list, whatever the caller asks for.
///
/// This tool was the one place with no ceiling: `lsp_find_symbol` caps at
/// `MAX_FIND_LIMIT` and every other tool is bounded by a count that is not the
/// model's to choose, but the reference `limit` was passed through as
/// `usize::MAX`. Since `limit` is a model-supplied argument, that made the
/// answer size the model's choice — and the answer has to survive a 4 MiB
/// transport line to be worth anything (see `crate::MAX_OUTPUT_BYTES`).
///
/// 500 is well above the 100 default and above what a rename-sized edit looks
/// like, and well below anything that would strain a context window.
const MAX_REFERENCE_LIMIT: usize = 500;

/// The sentence every tool that can see a file carries, so a model that hits a
/// language this connection did not enable does not retry forever.
const LANGUAGE_HINT: &str = " If you get [language_disabled], that language was not enabled for this \
                             connection; do not retry — use another tool.";

const STATUS_DESCRIPTION: &str = "Report the language servers running for this workspace: version, \
    pid, uptime, memory, connected clients, and each instance's state and indexing progress. Call it \
    before blaming the tools — it separates \"no server is running\" from \"the server is still \
    indexing\", which grep can never tell you. Example: lsp_status(). When a server shows [indexing], \
    wait a few seconds and retry rather than giving up on LSP.";

const FIND_SYMBOL_DESCRIPTION: &str = "Find symbols by name across the workspace with a \
    type-aware index. Better than grep because it matches declarations, not text: a comment or a \
    string containing the name is not a hit, and each result carries file, line and kind. Example: \
    lsp_find_symbol(query=\"parse\"). If you get [indexing], retry in a few seconds.";

const DEFINITION_DESCRIPTION: &str = "Jump from a symbol to where it is defined, following the \
    import that grep cannot: grep finds every file mentioning the name, the server finds the one \
    this use means. Example: lsp_definition(symbol=\"LspConfig\") — prefer symbol over \
    path+line+column, you should not have to count characters. If the answer is [indexing], retry \
    in a few seconds.";

const REFERENCES_DESCRIPTION: &str = "List every use of a symbol, grouped by file, as only the \
    compiler can: comments and strings are excluded, and uses across files and crates are found. \
    This is the tool to reach for instead of grep before a rename or a deletion. Example: \
    lsp_references(symbol=\"LspConfig::new\"). If the answer is [indexing], retry shortly.";

const HOVER_DESCRIPTION: &str = "Show the type and documentation of a symbol — the compiler's \
    view, not the raw text grep would return: resolved signature, parameter types, doc comment. \
    Example: lsp_hover(symbol=\"Config::load\"). If you get [indexing], wait a few seconds and \
    retry.";

const IMPLEMENTATIONS_DESCRIPTION: &str = "Find the types that implement a trait or interface, or \
    the methods that override one. grep cannot answer this: the spelling differs per language and \
    the answer crosses files. Example: lsp_implementations(symbol=\"LspBackend\"). If you get \
    [indexing], retry in a few seconds.";

const OUTLINE_DESCRIPTION: &str = "List the symbols a file declares, nested by scope, with kinds \
    and line numbers. Cheaper than reading the file and more accurate than grepping `fn`/`func`/\
    `def`: it is the language's own parse tree. Example: lsp_outline(path=\"src/lib.rs\"). If you \
    get [indexing], retry in a few seconds.";

const CALLERS_DESCRIPTION: &str = "List the functions that call this one, as an indented tree — the \
    question grep answers badly, since a comment naming it is not a caller. Follows `depth` levels \
    up (1-3; default 1); each line is `name  path:line:column`, and a place already shown says \
    `(see above)`. Use it before changing a signature. Example: \
    lsp_callers(symbol=\"LspConfig::new\"). If you get [indexing], retry in a few seconds.";

const CALLEES_DESCRIPTION: &str = "List the functions this one calls, as the language server \
    resolved them and as an indented tree — calls, not the text grep would return. Follows `depth` \
    levels down (1-3; default 1); each line is `name  path:line:column`. Use it to see what a \
    function touches before you change it. Example: lsp_callees(symbol=\"main\"). If you get \
    [indexing], retry in a few seconds.";

const DIAGNOSTICS_DESCRIPTION: &str = "Report the compiler and linter errors and warnings for a \
    file, with line and column. Use it right after you edit a file to check it still compiles — the \
    server's verdict, not your guess. Example: lsp_diagnostics(path=\"src/lib.rs\"). If it says the \
    diagnostics are not known yet, ask again instead of assuming the file is clean.";

const RENAME_PREVIEW_DESCRIPTION: &str = "Preview the whole-project edit for renaming a symbol, as \
    a unified diff — nothing on disk changes. Better than grep+sed: exact references, no comments \
    or strings, every affected file shown before you touch it. Example: \
    lsp_rename_preview(symbol=\"LspConfig::new\", new_name=\"build\"). If you get [indexing], retry \
    in a few seconds: a rename now could miss references.";

/// The catalog, in the order a model reads it.
///
/// Property descriptions are deliberately absent: the tool's own description
/// carries the meaning of `symbol`/`path`/`line`/`column`, and the catalog has
/// a hard size budget that rich per-property prose would blow.
pub fn defs() -> Vec<ToolDef> {
    vec![
        def("lsp_status", STATUS_DESCRIPTION, schema(Vec::new(), &[])),
        def(
            "lsp_find_symbol",
            capture(FIND_SYMBOL_DESCRIPTION, LANGUAGE_HINT),
            schema(
                vec![
                    (
                        "query",
                        described(
                            "string",
                            "Name to search for; fuzzy, so `parse` also finds `parse_args`.",
                        ),
                    ),
                    (
                        "kind",
                        described(
                            "string",
                            "One LSP SymbolKind name (`function`, `struct`, `enum`, ...), \
                             case-insensitive.",
                        ),
                    ),
                    (
                        "path",
                        described(
                            "string",
                            "A file or directory that scopes the answer to what is inside it.",
                        ),
                    ),
                    (
                        "language",
                        described(
                            "string",
                            "Restrict to one enabled language, e.g. `rust`, `go`.",
                        ),
                    ),
                    (
                        "limit",
                        bounded(
                            "integer",
                            1,
                            Some(MAX_FIND_LIMIT),
                            "Most symbols to list; default 50, maximum 200.",
                        ),
                    ),
                ],
                &["query"],
            ),
        ),
        def(
            "lsp_definition",
            DEFINITION_DESCRIPTION,
            target_schema(Vec::new(), &[]),
        ),
        def(
            "lsp_references",
            REFERENCES_DESCRIPTION,
            target_schema(
                vec![
                    (
                        "include_declaration",
                        described(
                            "boolean",
                            "Also list the declaration itself; default false.",
                        ),
                    ),
                    (
                        "limit",
                        bounded(
                            "integer",
                            1,
                            Some(MAX_REFERENCE_LIMIT),
                            "Most references to list; default 100.",
                        ),
                    ),
                ],
                &[],
            ),
        ),
        def(
            "lsp_hover",
            HOVER_DESCRIPTION,
            target_schema(Vec::new(), &[]),
        ),
        def(
            "lsp_implementations",
            IMPLEMENTATIONS_DESCRIPTION,
            target_schema(Vec::new(), &[]),
        ),
        def(
            "lsp_outline",
            OUTLINE_DESCRIPTION,
            schema(
                vec![(
                    "path",
                    described(
                        "string",
                        "File to outline, absolute or relative to the workspace root.",
                    ),
                )],
                &["path"],
            ),
        ),
        def(
            "lsp_callers",
            CALLERS_DESCRIPTION,
            target_schema(vec![("depth", depth_property())], &[]),
        ),
        def(
            "lsp_callees",
            CALLEES_DESCRIPTION,
            target_schema(vec![("depth", depth_property())], &[]),
        ),
        def(
            "lsp_diagnostics",
            DIAGNOSTICS_DESCRIPTION,
            schema(
                vec![(
                    "path",
                    described(
                        "string",
                        "File to diagnose, absolute or relative to the workspace root.",
                    ),
                )],
                &["path"],
            ),
        ),
        def(
            "lsp_rename_preview",
            RENAME_PREVIEW_DESCRIPTION,
            target_schema(
                vec![(
                    "new_name",
                    described(
                        "string",
                        "The new name: one identifier, no spaces, at most 200 characters.",
                    ),
                )],
                &["new_name"],
            ),
        ),
    ]
}

fn described(kind: &str, description: &str) -> Value {
    json!({ "type": kind, "description": description })
}

fn bounded(kind: &str, minimum: usize, maximum: Option<usize>, description: &str) -> Value {
    let mut value = json!({ "type": kind, "minimum": minimum, "description": description });
    if let Some(maximum) = maximum {
        value["maximum"] = json!(maximum);
    }
    value
}

fn depth_property() -> Value {
    bounded(
        "integer",
        1,
        Some(3),
        "How many levels to follow, 1 to 3; default 1.",
    )
}

/// The sentences a description must carry that are not part of its own prose.
fn capture(description: &str, extra: &str) -> String {
    format!("{description}{extra}")
}

fn def(name: &str, description: impl Into<String>, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.to_owned(),
        description: description.into(),
        input_schema,
        annotations: ToolAnnotations::default(),
    }
}

fn object(entries: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_owned(), value);
    }
    Value::Object(map)
}

/// The four `Target` arguments, flattened into the top level.
fn target_entries() -> Vec<(&'static str, Value)> {
    vec![
        (
            "symbol",
            described(
                "string",
                "Symbol name; may be qualified, e.g. `Foo::bar` or `pkg.Foo`.",
            ),
        ),
        (
            "path",
            described(
                "string",
                "File to look in; with line and column it is the position.",
            ),
        ),
        (
            "line",
            bounded(
                "integer",
                1,
                None,
                "1-based line; give it with column, and not with symbol.",
            ),
        ),
        (
            "column",
            bounded(
                "integer",
                1,
                None,
                "1-based character on that line; give it with line, not with symbol.",
            ),
        ),
    ]
}

fn schema(properties: Vec<(&str, Value)>, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": object(properties),
        "required": required,
        "additionalProperties": false
    })
}

fn target_schema(extra: Vec<(&'static str, Value)>, required: &[&str]) -> Value {
    let mut properties = target_entries();
    properties.extend(extra);
    schema(properties, required)
}

/// Runs the tool `name`. A failing tool is a [`ToolOutput`] with
/// `is_error = true` and a `[code] message` first line.
pub async fn call(
    backend: &dyn LspBackend,
    name: &str,
    args: Value,
    cancel: &CancellationToken,
) -> ToolOutput {
    match name {
        "lsp_status" => status(backend).await,
        "lsp_find_symbol" => find_symbol(backend, &args, cancel).await,
        "lsp_definition" => definition(backend, &args, cancel).await,
        "lsp_references" => references(backend, &args, cancel).await,
        "lsp_hover" => hover(backend, &args, cancel).await,
        "lsp_implementations" => implementations(backend, &args, cancel).await,
        "lsp_outline" => outline(backend, &args, cancel).await,
        "lsp_callers" => call_hierarchy(backend, &args, true, cancel).await,
        "lsp_callees" => call_hierarchy(backend, &args, false, cancel).await,
        "lsp_diagnostics" => diagnostics(backend, &args, cancel).await,
        "lsp_rename_preview" => rename::rename_preview(backend, &args, cancel).await,
        other => ToolOutput::error(format!(
            "[invalid_args] `{other}` is not a tool this server offers"
        )),
    }
}

// ---- lsp_status -----------------------------------------------------------

async fn status(backend: &dyn LspBackend) -> ToolOutput {
    let report = backend.status().await;
    let daemon = &report.daemon;
    let mut out = format!(
        "daemon {} pid={} uptime={}s rss={} clients={}",
        daemon.version,
        daemon.pid,
        daemon.uptime_secs,
        // The ceiling is shown when the daemon reports one, for the same reason
        // the CLI shows it: a memory figure with nothing to compare it against
        // does not tell a model whether the daemon is in trouble. A daemon too
        // old to report a ceiling keeps the plain form.
        match daemon.max_rss_mb {
            Some(limit) => format!("{}/{}MB", megabytes(daemon.rss_bytes), limit),
            None => megabytes(daemon.rss_bytes),
        },
        daemon.clients
    );
    if daemon.rss_over_limit {
        // Said plainly, because a model reading this otherwise sees a daemon
        // that looks fine while it is in fact leaking and refusing to restart.
        out.push_str(
            "\nnote: this daemon is over its own memory ceiling and has restarted too often \
             in the last hour; it is staying up and serving rather than restarting in a loop \
             (raise limits.daemon_max_rss_mb, or investigate the leak)",
        );
    }
    out.push_str(&format!(
        "\nlanguages: enabled={} ({}); not installed: {}",
        list(&report.enabled_languages),
        mode_word(report.language_mode),
        list(&report.not_installed)
    ));
    if report.instances.is_empty() {
        out.push_str("\nno language servers running");
    }
    for instance in &report.instances {
        out.push('\n');
        out.push_str(&format!(
            "{} {} {} rss={} idle={}s open_docs={} restarts={}",
            instance.server,
            instance.root,
            state_word(instance.state),
            megabytes(instance.rss_bytes),
            instance.idle_secs,
            instance.open_docs,
            instance.restarts
        ));
        if let Some(indexing) = &instance.indexing {
            out.push_str(&format!(" {}", progress(indexing)));
        }
    }
    ToolOutput::ok(out)
}

fn megabytes(bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) => format!("{:.1}MB", bytes as f64 / 1024.0 / 1024.0),
        None => "n/a".to_owned(),
    }
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

fn mode_word(mode: opencraylsp_proto::LanguageMode) -> &'static str {
    match mode {
        opencraylsp_proto::LanguageMode::Auto => "auto-detected",
        opencraylsp_proto::LanguageMode::Declared => "declared",
        opencraylsp_proto::LanguageMode::All => "all",
    }
}

fn state_word(state: opencraylsp_proto::InstanceState) -> &'static str {
    match state {
        opencraylsp_proto::InstanceState::Starting => "starting",
        opencraylsp_proto::InstanceState::Indexing => "indexing",
        opencraylsp_proto::InstanceState::Ready => "ready",
        opencraylsp_proto::InstanceState::Restarting => "restarting",
        opencraylsp_proto::InstanceState::Failed => "failed",
        opencraylsp_proto::InstanceState::Stopped => "stopped",
    }
}

fn progress(indexing: &opencraylsp_proto::Indexing) -> String {
    match indexing.percent {
        Some(percent) => format!("{} {percent}%", indexing.message),
        None => indexing.message.clone(),
    }
}

// ---- shared machinery -----------------------------------------------------

/// A view over the current answer: positions render relative to the boundary,
/// and snippets are read lazily, never outside it.
fn view<'a>(
    boundary: &'a Path,
    lines: &'a LineIndex,
    encoding: PositionEncoding,
    subject: Option<&'a Path>,
    max_results: usize,
) -> View<'a> {
    View {
        boundary,
        encoding,
        max_results,
        subject,
        lines,
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> ToolOutput {
    ToolOutput::error(format!("[invalid_args] {}", message.into()))
}

/// The `[not_found]` marker: the server looked, and there was nothing — a miss
/// is not an error.
pub(crate) fn not_found(message: impl std::fmt::Display) -> ToolOutput {
    ToolOutput::ok(format!("[not_found] {message}"))
}

fn with_markers(mut text: String, notes: &[String], served: &Served) -> ToolOutput {
    for note in notes {
        text.push('\n');
        text.push_str(note);
    }
    if served.indexing.is_some() && !is_empty_answer(&served.value) {
        text.push_str(&format!(
            "\nnote: {} is still indexing{}; results may be incomplete.",
            served.server,
            match served
                .indexing
                .as_ref()
                .and_then(|indexing| indexing.percent)
            {
                Some(percent) => format!(" ({percent}%)"),
                None => String::new(),
            }
        ));
    }
    ToolOutput::ok(text)
}

fn is_empty_answer(value: &Value) -> bool {
    value.is_null() || value.as_array().is_some_and(|items| items.is_empty())
}

/// The servers a lookup would use here, for the `not_found` sentence.
fn asked_servers(backend: &dyn LspBackend) -> String {
    let servers: Vec<String> = backend
        .languages()
        .into_iter()
        .filter(|language| language.enabled && language.installed)
        .map(|language| language.server)
        .collect();
    if servers.is_empty() {
        "any enabled server".to_owned()
    } else {
        servers.join(", ")
    }
}

/// Resolves the model's `Target` arguments to exactly one site, or returns the
/// answer to hand back (ambiguous candidates, a miss, or an error).
pub(crate) async fn locate(
    backend: &dyn LspBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<(Candidate, Vec<String>), ToolOutput> {
    let spec = resolve::parse_target(args)?;
    let target = target_name(&spec);
    let resolution = resolve::resolve(backend, &spec, cancel).await?;
    let boundary = backend.boundary();
    let lines = LineIndex::lazy(boundary.clone());
    let render_view = resolve_render::candidate_view(&boundary, &lines);
    match resolution.resolved {
        Resolved::One(candidate) => Ok((candidate, resolution.notes)),
        Resolved::Many(candidates) => Err(resolve_render::output(
            resolve_render::render_ambiguous(&target, &candidates, &render_view),
            &resolution.notes,
        )),
        Resolved::NotFound { suggestions } => Err(resolve_render::output(
            resolve_render::render_not_found(
                &target,
                &[asked_servers(backend)],
                &suggestions,
                &render_view,
            ),
            &resolution.notes,
        )),
    }
}

fn target_name(spec: &TargetSpec) -> String {
    match spec {
        TargetSpec::Symbol { name, .. } => name.clone(),
        TargetSpec::Position { path, line, column } => format!("{path}:{line}:{column}"),
    }
}

/// The file a resolved candidate lives in; a non-`file:` URI cannot carry a
/// request.
pub(crate) fn file_of(candidate: &Candidate) -> Result<PathBuf, ToolOutput> {
    candidate
        .site
        .path
        .clone()
        .ok_or_else(|| invalid("this symbol lives outside the file system and cannot be queried"))
}

pub(crate) fn position_params(candidate: &Candidate, file: &Path) -> Value {
    json!({
        "textDocument": { "uri": crate::resolve::file_uri(file) },
        "position": {
            "line": candidate.site.line.unwrap_or(0),
            "character": candidate.site.character.unwrap_or(0)
        }
    })
}

/// The ready-to-return answer for an empty result.
fn empty(what: &str, servers: &str) -> ToolOutput {
    not_found(format!(
        "{what}: the language server returned nothing in {servers}"
    ))
}

/// The same miss for a *position*-targeted tool, with the line's identifiers
/// attached so the next `column` does not have to be guessed.
///
/// Every position-targeted tool answers a miss this way, from one function: a
/// hint that existed in only some of them would be worse than none, because the
/// model would learn to expect it and stop reading the answers that lack it.
fn empty_at(what: &str, servers: &str, candidate: &Candidate, lines: &LineIndex) -> ToolOutput {
    hint::not_found_at_position(
        &format!("{what}: the language server returned nothing in {servers}"),
        &candidate.site,
        lines,
    )
}

/// What to add to a miss that came from naming a symbol rather than a position.
///
/// A name resolves to where the symbol is *declared*, and some servers
/// (intelephense answers hover and references there with nothing) only answer at
/// a *use*. The bare miss reads as "this symbol has no references" when the
/// truth is "ask from a different place", so say so.
fn declaration_note(args: &Value, mut out: ToolOutput) -> ToolOutput {
    if string_arg(args, "symbol").is_some() && args.get("line").is_none() {
        out.text.push_str(
            "\nThe symbol was resolved to its declaration, where some language servers return \
             nothing. Retry with `path`+`line`+`column` pointing at a place that uses it.",
        );
    }
    out
}

/// A server refusing `implementation` because the position is a function or
/// method, not a type or interface. There is nothing to implement there, so it is
/// a miss with a reason, not an `[rpc_error]` the model cannot act on.
fn implementation_not_applicable(error: &LspError) -> bool {
    matches!(error, LspError::Rpc { message, .. }
        if message.contains("is a function, not a method"))
}

/// The LSP `SymbolKind` a language server reports for a **type alias**.
///
/// LSP has no alias kind, so servers fold aliases into `TypeParameter` (26);
/// rust-analyzer reports `pub type Foo = Bar;` as 26 and an ordinary generic
/// parameter `T` as nothing at all in `workspace/symbol` (verified against
/// rust-analyzer for both). That makes 26 a usable signal, but only for servers
/// that reuse it that way — see `alias_target_of`.
const TYPE_ALIAS_KIND: u32 = 26;

/// The type a type-alias declaration points at, if the resolved candidate is a
/// type alias — else `None`, which keeps whatever the caller would have got.
///
/// LSP has no alias kind, so servers fold aliases into `TypeParameter` (26);
/// rust-analyzer reports `pub type Foo = Bar;` as 26, while an ordinary generic
/// parameter `T` is not listed by `workspace/symbol` at all (both verified
/// against rust-analyzer). The kind alone is therefore not enough — 26 also
/// covers real type parameters — so the declaration line is read from disk and
/// must actually declare an alias. A server that never reports kind 26, or a
/// file that cannot be read, yields `None` and the plain miss, which is the
/// honest answer when lspd cannot tell the cases apart.
fn alias_target_of(candidate: &Candidate, lines: &LineIndex) -> Option<String> {
    use std::io::{BufRead, BufReader};

    if candidate.kind != TYPE_ALIAS_KIND {
        return None;
    }
    let line = candidate.site.line?;
    // The path came from a language server, so it gets the same treatment as
    // every other file the tools read: it must be inside the workspace, a
    // regular file (a FIFO or a device would block or never end), and no larger
    // than the shared read cap. Anything else is "cannot tell", which falls back
    // to the plain miss without reading a byte.
    let file = lines.inside(&file_of(candidate).ok()?)?;
    let meta = std::fs::metadata(&file).ok()?;
    if !meta.is_file() || meta.len() > format::MAX_FILE_BYTES {
        return None;
    }
    // No round trip: this runs on the miss path and must not turn one extra
    // question into a second server call. Only lines up to the declaration are
    // read.
    let declaration = BufReader::new(std::fs::File::open(&file).ok()?)
        .lines()
        .nth(line as usize)?
        .ok()?;
    alias_target(&declaration).map(str::to_owned)
}

/// Whether one source line declares a type alias, ignoring what it points at.
///
/// Deliberately syntax-shaped rather than name-shaped: `T` must not read as an
/// alias (`struct Holder<T>`), and a *use* of an alias must not either (`let x:
/// Alias`). The line must declare something with the `type` keyword, at the
/// start of the statement once `pub` (with any visibility parenthesis) is
/// skipped, and the declaration must carry an `=` — a body-less `type Foo;` is
/// not an alias, it names a type declared elsewhere.
#[cfg(test)]
fn declares_an_alias(declaration: &str) -> bool {
    alias_declaration(declaration).is_some()
}

/// The right-hand side of a `type X = Y` declaration, if the line is such a
/// declaration and the target is a single name worth suggesting.
///
/// `pub type Alias = Plain;` yields `Plain`. `None` means either "not an alias
/// declaration" or "an alias with no queryable name behind it" — the caller only
/// needs something it can point at, so both read the same. A tuple, a function
/// type and `Vec<u8>` are all aliases, but "ask about `Vec<u8>`" is not advice
/// a caller can act on, so `pub type Pair<T> = (T, T);` yields nothing.
fn alias_target(declaration: &str) -> Option<&str> {
    let target = alias_declaration(declaration)?;
    if is_queryable_type_name(target) {
        Some(target)
    } else {
        None
    }
}

/// The raw target text of an alias declaration, `None` if the line is not one.
fn alias_declaration(declaration: &str) -> Option<&str> {
    let declaration = declaration.split("//").next().unwrap_or(declaration).trim();
    let rest = match declaration.strip_prefix("pub") {
        Some(rest) => {
            let rest = rest.trim_start();
            match rest.strip_prefix('(') {
                Some(in_parens) => in_parens
                    .split_once(')')
                    .map(|(_, rest)| rest.trim_start())?,
                None => rest,
            }
        }
        None => declaration,
    };
    let rest = rest.strip_prefix("type")?;
    // The keyword must end here: `types::Thing` is not a declaration.
    match rest.chars().next() {
        None => {}
        Some(c) if c.is_whitespace() || c == '=' || c == ';' => {}
        Some(_) => return None,
    }
    // A declaration names something: `type Alias = Plain`, never `type = Plain`.
    // The name may carry generic parameters, which the `=` search below skips.
    let after_keyword = rest.trim_start();
    if !after_keyword
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        return None;
    }
    // An `=` with something after it is what makes it an alias rather than a
    // body-less declaration.
    let target = rest.split_once('=')?.1;
    let target = target.split(';').next().unwrap_or(target).trim();
    if target.is_empty() {
        return None;
    }
    Some(target)
}

/// Whether a right-hand side is a single type name a caller can go and query.
fn is_queryable_type_name(target: &str) -> bool {
    !(target.starts_with('(')
        || target.starts_with('[')
        || target.starts_with('&')
        || target.starts_with('*')
        || target.starts_with("dyn ")
        || target.starts_with("impl ")
        || target.contains('<')
        || target.contains('>')
        || target.contains('(')
        || target.contains(' '))
}

// ---- the location-based tools ---------------------------------------------

async fn definition(
    backend: &dyn LspBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> ToolOutput {
    let (candidate, notes) = match locate(backend, args, cancel).await {
        Ok(found) => found,
        Err(output) => return output,
    };
    let Ok(file) = file_of(&candidate) else {
        return invalid("this symbol lives outside the file system and cannot be queried");
    };
    let lines = LineIndex::lazy(backend.boundary());
    let served = match backend
        .request(
            &file,
            "textDocument/definition",
            position_params(&candidate, &file),
            cancel,
        )
        .await
    {
        Ok(served) => served,
        Err(error) => return render_error(&error),
    };
    let boundary = backend.boundary();
    let render_view = view(&boundary, &lines, served.encoding, Some(&file), usize::MAX);
    let (sites, skipped) = match operations::locations_with_skipped(&served.value) {
        Ok(decoded) => decoded,
        Err(shape) => return shape_error("lsp_definition", &shape.detail),
    };
    // A name-resolved target already IS a definition (workspace/symbol only
    // lists declarations). Some servers (intelephense) answer "go to
    // definition" on a declaration with nothing, so fall back to the symbol's
    // own site instead of reporting a miss the caller cannot act on.
    let mut notes = notes;
    let sites = if sites.is_empty() && !candidate.name.is_empty() {
        notes.push(
            "note: the server returned no separate definition; this is the symbol's own declaration."
                .to_owned(),
        );
        vec![candidate.site.clone()]
    } else {
        sites
    };
    if sites.is_empty() {
        return empty_at(
            "no definition was found",
            &served.server,
            &candidate,
            &lines,
        );
    }
    with_markers(
        with_skipped(format::definition(&sites, &render_view), skipped),
        &notes,
        &served,
    )
}

async fn implementations(
    backend: &dyn LspBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> ToolOutput {
    let (candidate, notes) = match locate(backend, args, cancel).await {
        Ok(found) => found,
        Err(output) => return output,
    };
    let Ok(file) = file_of(&candidate) else {
        return invalid("this symbol lives outside the file system and cannot be queried");
    };
    let lines = LineIndex::lazy(backend.boundary());
    let served = match backend
        .request(
            &file,
            "textDocument/implementation",
            position_params(&candidate, &file),
            cancel,
        )
        .await
    {
        Ok(served) => served,
        Err(error) if implementation_not_applicable(&error) => {
            return hint::not_found_at_position(
                "nothing to implement here: this position is a function or method, not a \
                 type or interface. Point `symbol` or `column` at the trait, interface or \
                 type name instead.",
                &candidate.site,
                &lines,
            );
        }
        Err(error) => return render_error(&error),
    };
    let boundary = backend.boundary();
    let render_view = view(&boundary, &lines, served.encoding, Some(&file), usize::MAX);
    let (sites, skipped) = match operations::locations_with_skipped(&served.value) {
        Ok(decoded) => decoded,
        Err(shape) => return shape_error("lsp_implementations", &shape.detail),
    };
    if sites.is_empty() {
        // An alias is not implementable: `type Alias = Plain` is a second name
        // for `Plain`, and anything that implements `Plain` already implements
        // `Alias`. Reporting "no implementation was found" leaves the caller
        // hunting for an implementor that cannot exist, so name the case and
        // point at the type worth asking about instead.
        if let Some(target) = alias_target_of(&candidate, &lines) {
            return hint::not_found_at_position(
                &format!(
                    "`{name}` is a type alias, so it has no implementations of its own — \
                     anything implementing the type it aliases already implements it. Ask \
                     about `{target}` instead.",
                    name = candidate.name,
                ),
                &candidate.site,
                &lines,
            );
        }
        return empty_at(
            "no implementation was found",
            &served.server,
            &candidate,
            &lines,
        );
    }
    with_markers(
        with_skipped(format::implementation(&sites, &render_view), skipped),
        &notes,
        &served,
    )
}

async fn references(
    backend: &dyn LspBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> ToolOutput {
    // Arguments are validated before anything is asked of a server, so a typo
    // is reported as a typo rather than as a miss.
    let include_declaration = match args.get("include_declaration") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return invalid("`include_declaration` must be true or false"),
    };
    let limit = match limit_of(args, DEFAULT_REFERENCE_LIMIT, MAX_REFERENCE_LIMIT) {
        Ok(limit) => limit,
        Err(output) => return output,
    };
    let (candidate, notes) = match locate(backend, args, cancel).await {
        Ok(found) => found,
        Err(output) => return output,
    };
    let Ok(file) = file_of(&candidate) else {
        return invalid("this symbol lives outside the file system and cannot be queried");
    };
    let lines = LineIndex::lazy(backend.boundary());
    let mut params = position_params(&candidate, &file);
    params["context"] = json!({ "includeDeclaration": include_declaration });
    let served = match backend
        .request(&file, "textDocument/references", params, cancel)
        .await
    {
        Ok(served) => served,
        Err(error) => return render_error(&error),
    };
    let boundary = backend.boundary();
    let render_view = view(&boundary, &lines, served.encoding, Some(&file), limit);
    let (sites, skipped) = match operations::locations_with_skipped(&served.value) {
        Ok(decoded) => decoded,
        Err(shape) => return shape_error("lsp_references", &shape.detail),
    };
    if sites.is_empty() {
        return declaration_note(
            args,
            empty_at(
                "no references were found",
                &served.server,
                &candidate,
                &lines,
            ),
        );
    }
    with_markers(
        with_skipped(format::references(&sites, &render_view), skipped),
        &notes,
        &served,
    )
}

async fn hover(backend: &dyn LspBackend, args: &Value, cancel: &CancellationToken) -> ToolOutput {
    let (candidate, notes) = match locate(backend, args, cancel).await {
        Ok(found) => found,
        Err(output) => return output,
    };
    let Ok(file) = file_of(&candidate) else {
        return invalid("this symbol lives outside the file system and cannot be queried");
    };
    let lines = LineIndex::lazy(backend.boundary());
    let served = match backend
        .request(
            &file,
            "textDocument/hover",
            position_params(&candidate, &file),
            cancel,
        )
        .await
    {
        Ok(served) => served,
        Err(error) => return render_error(&error),
    };
    let boundary = backend.boundary();
    let render_view = view(&boundary, &lines, served.encoding, Some(&file), usize::MAX);
    match operations::hover(&served.value) {
        Ok(Some(info)) => with_markers(format::hover(&info, &render_view), &notes, &served),
        Ok(None) => declaration_note(
            args,
            empty_at(
                "no hover information at this position",
                &served.server,
                &candidate,
                &lines,
            ),
        ),
        Err(shape) => shape_error("lsp_hover", &shape.detail),
    }
}

async fn call_hierarchy(
    backend: &dyn LspBackend,
    args: &Value,
    incoming: bool,
    cancel: &CancellationToken,
) -> ToolOutput {
    let direction = if incoming {
        Direction::Incoming
    } else {
        Direction::Outgoing
    };
    let depth = match depth_of(args) {
        Ok(depth) => u8::try_from(depth).unwrap_or(u8::MAX),
        Err(output) => return output,
    };
    // Resolving the name is the same for both directions; the walk below does
    // the rest.
    let (candidate, notes) = match locate(backend, args, cancel).await {
        Ok(found) => found,
        Err(output) => return output,
    };
    let tree = match callgraph::call_tree(backend, &candidate, direction, depth, cancel).await {
        Ok(tree) => tree,
        Err(output) => return output,
    };
    let boundary = backend.boundary();
    let lines = LineIndex::lazy(boundary.clone());
    let render_view = view(&boundary, &lines, tree.encoding, None, usize::MAX);
    let mut text = callgraph::render(&tree, &render_view);
    for note in &notes {
        text.push('\n');
        text.push_str(note);
    }
    ToolOutput::ok(text)
}

// ---- the file-based tools -------------------------------------------------

async fn outline(backend: &dyn LspBackend, args: &Value, cancel: &CancellationToken) -> ToolOutput {
    let Some(path) = string_arg(args, "path") else {
        return invalid("`lsp_outline` needs `path`");
    };
    let file = match backend.resolve_path(&path) {
        Ok(file) => file,
        Err(error) => return render_error(&error),
    };
    let lines = LineIndex::lazy(backend.boundary());
    let params = json!({ "textDocument": { "uri": crate::resolve::file_uri(&file) } });
    let served = match backend
        .request(&file, "textDocument/documentSymbol", params, cancel)
        .await
    {
        Ok(served) => served,
        Err(error) => return render_error(&error),
    };
    let boundary = backend.boundary();
    let render_view = view(&boundary, &lines, served.encoding, Some(&file), usize::MAX);
    match operations::document_symbols(&served.value) {
        Ok(list) if list.symbols.is_empty() && list.skipped == 0 => {
            empty("no symbols were found", &served.server)
        }
        Ok(list) => with_markers(format::document_symbols(&list, &render_view), &[], &served),
        Err(shape) => shape_error("lsp_outline", &shape.detail),
    }
}

async fn diagnostics(
    backend: &dyn LspBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> ToolOutput {
    let Some(path) = string_arg(args, "path") else {
        return invalid("`lsp_diagnostics` needs `path`");
    };
    let file = match backend.resolve_path(&path) {
        Ok(file) => file,
        Err(error) => return render_error(&error),
    };
    let lines = LineIndex::lazy(backend.boundary());
    let report = match backend.diagnostics(&file, cancel).await {
        Ok(report) => report,
        Err(error) => return render_error(&error),
    };
    let boundary = backend.boundary();
    let render_view = view(&boundary, &lines, report.encoding, Some(&file), usize::MAX);
    ToolOutput::ok(format::diagnostics(&report, &render_view))
}

/// Whether a candidate lives in an installed dependency directory.
fn is_dependency(candidate: &Candidate) -> bool {
    candidate.site.path.as_deref().is_some_and(|path| {
        path.components()
            .any(|part| part.as_os_str() == "vendor" || part.as_os_str() == "node_modules")
    })
}

async fn find_symbol(
    backend: &dyn LspBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> ToolOutput {
    let Some(query) = string_arg(args, "query") else {
        return invalid("`lsp_find_symbol` needs `query`");
    };
    let kind = match optional_string(args, "kind") {
        Ok(kind) => kind,
        Err(output) => return output,
    };
    // An unknown kind name is a caller mistake, and must not read as "that kind
    // has no symbols": one is an error the model can fix, the other sends it
    // off to add a symbol that already exists. So validate before fanning out.
    if let Some(kind) = kind
        .as_deref()
        .filter(|kind| format::kind_from_name(kind).is_none())
    {
        let legal = format::KIND_NAMES
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        return invalid(format!(
            "`kind` `{kind}` is not an LSP SymbolKind; expected one of: {legal}"
        ));
    }
    let path = match optional_string(args, "path") {
        Ok(path) => path,
        Err(output) => return output,
    };
    let language = match optional_string(args, "language") {
        Ok(language) => language,
        Err(output) => return output,
    };
    let limit = match limit_of(args, DEFAULT_FIND_LIMIT, MAX_FIND_LIMIT) {
        Ok(limit) => limit,
        Err(output) => return output,
    };
    let (mut candidates, notes) = match resolve::search(
        backend,
        &query,
        path.as_deref(),
        language.as_deref(),
        cancel,
    )
    .await
    {
        Ok(found) => found,
        Err(output) => return output,
    };
    if let Some(kind) = kind {
        candidates
            .retain(|candidate| format::kind_name(candidate.kind).eq_ignore_ascii_case(&kind));
    }
    // Third-party code goes last, not away: a name search across a PHP or JS
    // tree otherwise fills the limit with `vendor/` hits and pushes the
    // project's own symbols out. The sort is stable, so each group keeps the
    // server's ranking.
    candidates.sort_by_key(is_dependency);
    let boundary = backend.boundary();
    let lines = LineIndex::lazy(boundary.clone());
    let render_view = resolve_render::candidate_view(&boundary, &lines);
    if candidates.is_empty() {
        return ToolOutput::ok(with_notes(
            format!("[not_found] the language server returned no symbol matching `{query}`"),
            &notes,
        ));
    }
    let text = render_matches(&candidates, &query, &render_view, limit);
    ToolOutput::ok(with_notes(text, &notes))
}

/// Groups matches by file, the way a model scans them.
fn render_matches(
    candidates: &[Candidate],
    query: &str,
    render_view: &View<'_>,
    limit: usize,
) -> String {
    let shown = &candidates[..candidates.len().min(limit)];
    let mut groups: Vec<(String, Vec<&Candidate>)> = Vec::new();
    for candidate in shown {
        let key = display_of(candidate, render_view.boundary);
        match groups.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, list)) => list.push(candidate),
            None => groups.push((key, vec![candidate])),
        }
    }
    // Counted over what is shown, not over the whole match set: the header
    // describes the list the caller is reading, and claiming "0 exact" because
    // the one exact match fell past `limit` would send them looking for it.
    let lowered = query.trim().to_ascii_lowercase();
    let ranks: Vec<u8> = shown
        .iter()
        .map(|c| resolve::match_rank(&c.name, &lowered))
        .collect();
    let exact = ranks.iter().filter(|rank| **rank == 0).count();
    let prefix = ranks.iter().filter(|rank| **rank == 1).count();
    let mut out = format!(
        "Found {} symbol(s) matching `{query}` in {} file(s), showing {} ({} exact, \
         {} name-prefix, {} substring):",
        candidates.len(),
        groups.len(),
        shown.len(),
        exact,
        prefix,
        shown.len() - exact - prefix,
    );
    for (path, list) in groups {
        out.push('\n');
        out.push_str(&path);
        out.push(':');
        for candidate in list {
            out.push('\n');
            out.push_str("  ");
            out.push_str(&resolve_render::candidate_line(candidate, render_view));
        }
    }
    if candidates.len() > shown.len() {
        out.push_str(&format!(
            "\n... and {} more",
            candidates.len() - shown.len()
        ));
    }
    out
}

fn display_of(candidate: &Candidate, boundary: &Path) -> String {
    match &candidate.site.path {
        Some(path) => resolve::display_path(boundary, path),
        None => candidate.site.uri.clone(),
    }
}

fn with_notes(mut text: String, notes: &[String]) -> String {
    for note in notes {
        text.push('\n');
        text.push_str(note);
    }
    text
}

// ---- argument helpers -----------------------------------------------------

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, ToolOutput> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.trim().is_empty() => {
            Err(invalid(format!("`{key}` must not be empty")))
        }
        Some(Value::String(value)) => Ok(Some(value.trim().to_owned())),
        Some(_) => Err(invalid(format!("`{key}` must be a string"))),
    }
}

fn limit_of(args: &Value, default: usize, max: usize) -> Result<usize, ToolOutput> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(default),
        Some(value) => match value.as_u64() {
            Some(0) => Err(invalid("`limit` must be 1 or greater")),
            Some(number) => Ok((number as usize).min(max)),
            None => Err(invalid("`limit` must be an integer of 1 or greater")),
        },
    }
}

fn depth_of(args: &Value) -> Result<u32, ToolOutput> {
    match args.get("depth") {
        None | Some(Value::Null) => Ok(1),
        Some(value) => match value.as_u64() {
            Some(0) => Err(invalid("`depth` must be 1 or greater")),
            Some(number) if number <= u64::from(u32::MAX) => Ok(number as u32),
            _ => Err(invalid("`depth` must be an integer of 1 or greater")),
        },
    }
}

// ---- shared rendering helpers ---------------------------------------------

pub(crate) fn shape_error(tool: &str, detail: &str) -> ToolOutput {
    ToolOutput::error(format!(
        "[invalid_response] the language server's answer to `{tool}` could not be read: {detail}"
    ))
}

fn with_skipped(body: String, skipped: usize) -> String {
    if skipped == 0 {
        body
    } else {
        format!(
            "{body}\n({skipped} entr{} in the server's answer could not be read and were skipped.)",
            if skipped == 1 { "y" } else { "ies" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencraylsp_core::backend::{DiagnosticsReport, LanguageInfo, LspError, PositionEncoding};
    use opencraylsp_core::mock::MockBackend;
    use opencraylsp_proto::{
        DaemonInfo, Indexing, InstanceInfo, InstanceState, LanguageMode, Limits, StatusReport,
    };
    use serde_json::json;
    use std::path::PathBuf;

    fn languages() -> Vec<LanguageInfo> {
        vec![LanguageInfo {
            name: "rust".to_owned(),
            server: "rust-analyzer".to_owned(),
            extensions: vec!["rs".to_owned()],
            root_markers: vec!["Cargo.toml".to_owned()],
            installed: true,
            detected: true,
            enabled: true,
        }]
    }

    /// A mock rooted at `/ws` with one usable language, so a name lookup works.
    fn backend() -> MockBackend {
        let backend = MockBackend::new("/ws");
        backend.set_languages(languages());
        backend
    }

    async fn call(backend: &MockBackend, name: &str, args: Value) -> ToolOutput {
        tools_call(backend, name, args).await
    }

    async fn tools_call(backend: &MockBackend, name: &str, args: Value) -> ToolOutput {
        super::call(backend, name, args, &CancellationToken::new()).await
    }

    fn symbol(name: &str, kind: u32, container: Option<&str>, uri: &str, line: u32) -> Value {
        let mut item = json!({
            "name": name,
            "kind": kind,
            "location": { "uri": uri, "range": { "start": { "line": line, "character": 0 } } }
        });
        if let Some(container) = container {
            item["containerName"] = json!(container);
        }
        item
    }

    fn at(uri: &str, line: u32, character: u32) -> Value {
        json!({
            "uri": uri,
            "range": {
                "start": { "line": line, "character": character },
                "end": { "line": line, "character": character + 1 }
            }
        })
    }

    fn report(items: Vec<lsp_types::Diagnostic>, received: bool) -> DiagnosticsReport {
        DiagnosticsReport {
            items,
            encoding: PositionEncoding::Utf16,
            received_for_version: received,
            timed_out: !received,
            server: "rust-analyzer".to_owned(),
        }
    }

    // ---- lsp_status -------------------------------------------------------

    fn status_report(instances: Vec<InstanceInfo>) -> StatusReport {
        StatusReport {
            daemon: DaemonInfo {
                version: "0.1.0".to_owned(),
                pid: 42,
                uptime_secs: 7,
                rss_bytes: Some(3 * 1024 * 1024),
                clients: 2,
                max_rss_mb: Some(512),
                rss_over_limit: false,
            },
            limits: Limits {
                max_instances: 8,
                max_rss_mb: 6144,
                idle_shutdown_secs: 900,
                max_open_docs: 256,
            },
            enabled_languages: vec!["rust".to_owned()],
            language_mode: LanguageMode::Auto,
            not_installed: vec!["php".to_owned()],
            instances,
        }
    }

    fn instance(server: &str, state: InstanceState, indexing: Option<Indexing>) -> InstanceInfo {
        InstanceInfo {
            server: server.to_owned(),
            root: "/ws".to_owned(),
            state,
            pid: Some(9),
            rss_bytes: Some(64 * 1024 * 1024),
            idle_secs: 3,
            restarts: 0,
            memory_restarts: 0,
            open_docs: 12,
            indexing,
        }
    }

    #[tokio::test]
    async fn status_reports_the_daemon_the_languages_and_each_instance() {
        let backend = backend();
        backend.set_status(status_report(vec![instance(
            "rust-analyzer",
            InstanceState::Indexing,
            Some(Indexing {
                message: "roots scanned".to_owned(),
                percent: Some(40),
            }),
        )]));
        let out = call(&backend, "lsp_status", json!({})).await;
        assert!(!out.is_error);
        assert!(
            out.text
                .starts_with("daemon 0.1.0 pid=42 uptime=7s rss=3.0MB/512MB clients=2"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("languages: enabled=rust (auto-detected); not installed: php"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("rust-analyzer /ws indexing rss=64.0MB idle=3s open_docs=12 restarts=0 roots scanned 40%"),
            "{}",
            out.text
        );
    }

    /// A daemon that has given up restarting shows it. The status line itself
    /// would otherwise look perfectly healthy — memory figure, uptime, clients
    /// and all — while the daemon is in fact leaking and refusing to act on it.
    #[tokio::test]
    async fn status_says_when_the_daemon_is_over_its_ceiling_and_refusing() {
        let mut report = status_report(vec![]);
        report.daemon.rss_over_limit = true;
        let backend = backend();
        backend.set_status(report);
        let out = call(&backend, "lsp_status", json!({})).await;
        assert!(!out.is_error);
        assert!(
            out.text.contains("over its own memory ceiling"),
            "the refusal must be stated: {}",
            out.text
        );
        assert!(
            out.text.contains("raise limits.daemon_max_rss_mb"),
            "and said what to do about it: {}",
            out.text
        );
    }

    /// A daemon old enough not to know its own ceiling keeps the plain memory
    /// figure, rather than showing a dangling `/`.
    #[tokio::test]
    async fn status_of_a_daemon_without_a_ceiling_shows_no_ratio() {
        let mut report = status_report(vec![]);
        report.daemon.max_rss_mb = None;
        let backend = backend();
        backend.set_status(report);
        let out = call(&backend, "lsp_status", json!({})).await;
        assert!(
            out.text
                .starts_with("daemon 0.1.0 pid=42 uptime=7s rss=3.0MB clients=2"),
            "an absent ceiling must not leave a trailing slash: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn status_with_no_instances_says_so() {
        let backend = backend();
        backend.set_status(status_report(Vec::new()));
        let out = call(&backend, "lsp_status", json!({})).await;
        assert!(
            out.text.contains("no language servers running"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn status_marks_an_instance_without_progress_percent() {
        let backend = backend();
        backend.set_status(status_report(vec![instance(
            "gopls",
            InstanceState::Ready,
            Some(Indexing {
                message: "loading".to_owned(),
                percent: None,
            }),
        )]));
        let out = call(&backend, "lsp_status", json!({})).await;
        assert!(out.text.contains("gopls /ws ready"), "{}", out.text);
        assert!(out.text.trim_end().ends_with("loading"), "{}", out.text);
    }

    // ---- lsp_find_symbol --------------------------------------------------

    #[tokio::test]
    async fn find_symbol_groups_matches_by_file() {
        let backend = backend().with_encoding(PositionEncoding::Utf16);
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("parse", 12, Some("util"), "file:///ws/a.rs", 0),
                symbol("parse_all", 12, None, "file:///ws/b.rs", 4),
            ])),
        );
        let out = call(&backend, "lsp_find_symbol", json!({ "query": "parse" })).await;
        assert!(!out.is_error);
        assert!(
            out.text
                .starts_with("Found 2 symbol(s) matching `parse` in 2 file(s), showing 2"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("\na.rs:\n  a.rs:1:1  function `parse`  in util"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("\nb.rs:\n  b.rs:5:1  function `parse_all`"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn find_symbol_honours_the_limit_and_counts_the_rest() {
        let backend = backend();
        let items: Vec<Value> = (0..5)
            .map(|index| symbol(&format!("p{index}"), 12, None, "file:///ws/a.rs", index))
            .collect();
        backend.respond("workspace/symbol", Ok(json!(items)));
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "p", "limit": 2 }),
        )
        .await;
        assert!(out.text.contains("... and 3 more"), "{}", out.text);
    }

    #[tokio::test]
    async fn find_symbol_lists_dependencies_after_project_code() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("handleLogin", 12, None, "file:///ws/vendor/x/a.php", 0),
                symbol("handleLogin", 12, None, "file:///ws/app/b.php", 0),
            ])),
        );
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "handleLogin" }),
        )
        .await;
        let app = out.text.find("app/b.php").expect(&out.text);
        let vendor = out.text.find("vendor/x/a.php").expect(&out.text);
        assert!(app < vendor, "{}", out.text);
    }

    #[tokio::test]
    async fn find_symbol_filters_by_kind() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("Thing", 23, None, "file:///ws/a.rs", 0),
                symbol("Thing", 12, None, "file:///ws/a.rs", 9),
            ])),
        );
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "Thing", "kind": "function" }),
        )
        .await;
        assert!(out.text.starts_with("Found 1 symbol(s)"), "{}", out.text);
        assert!(out.text.contains("function `Thing`"), "{}", out.text);
        assert!(!out.text.contains("struct `Thing`"), "{}", out.text);
    }

    #[tokio::test]
    async fn find_symbol_with_nothing_found_is_a_miss_not_an_error() {
        let backend = backend();
        backend.respond("workspace/symbol", Ok(Value::Null));
        let out = call(&backend, "lsp_find_symbol", json!({ "query": "nope" })).await;
        assert!(!out.is_error);
        assert!(out.text.starts_with("[not_found]"), "{}", out.text);
    }

    /// B: a `path` hint is a scope, not just a language selector. A server
    /// answers a workspace symbol query with the whole index, so without this
    /// filter a caller asking about one file is handed every symbol on the
    /// machine — including files outside the path it named.
    #[tokio::test]
    async fn find_symbol_limits_results_to_the_given_path() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("Thing", 23, None, "file:///ws/src/a.rs", 0),
                symbol("Other", 23, None, "file:///elsewhere/b.rs", 0),
                // A sibling that shares the path prefix as a string but is not
                // inside it: "src/other" must not pass as "src".
                symbol("Neighbour", 23, None, "file:///ws/src/../lib.rs", 0),
            ])),
        );
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "T", "path": "src/a.rs" }),
        )
        .await;
        assert!(out.text.contains("struct `Thing`"), "{}", out.text);
        assert!(!out.text.contains("`Other`"), "{}", out.text);
    }

    /// B, the directory form: naming a directory answers "what is in here".
    #[tokio::test]
    async fn find_symbol_limits_results_to_a_named_directory() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("Inside", 23, None, "file:///ws/src/deep/a.rs", 0),
                symbol("Outside", 23, None, "file:///ws/tests/a.rs", 0),
            ])),
        );
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "a", "path": "src" }),
        )
        .await;
        assert!(out.text.contains("struct `Inside`"), "{}", out.text);
        assert!(!out.text.contains("`Outside`"), "{}", out.text);
    }

    /// A real directory has no extension to choose a server by. It used to be
    /// sent to the backend as if it were a file, which answered `no_server`; it
    /// must be a scope instead: the query goes to the language server by name,
    /// and the result is filtered to what lives under the directory.
    #[tokio::test]
    async fn find_symbol_with_a_real_directory_path_asks_the_server_not_the_file() {
        let (backend, dir) = on_disk_backend("pub struct Inside;\n");
        std::fs::create_dir(dir.path().join("src")).expect("create src dir");
        let inside = format!("file://{}/src/a.rs", dir.path().display());
        let outside = format!("file://{}/tests/a.rs", dir.path().display());
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("Inside", 23, None, &inside, 0),
                symbol("Outside", 23, None, &outside, 0),
            ])),
        );
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "a", "path": "src" }),
        )
        .await;
        assert!(out.text.contains("struct `Inside`"), "{}", out.text);
        assert!(!out.text.contains("`Outside`"), "{}", out.text);
        let calls = backend.calls();
        assert_eq!(
            calls[0].server.as_deref(),
            Some("rust-analyzer"),
            "a directory must be a scope, not a file to route by: {calls:?}"
        );
    }

    /// C: a typo in `kind` must read as a typo. Returning "[not_found]" would
    /// tell the model that kind has no symbols anywhere, and it would go and
    /// write one.
    #[tokio::test]
    async fn find_symbol_rejects_an_unknown_kind_and_lists_the_legal_ones() {
        let backend = backend();
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "x", "kind": "func" }),
        )
        .await;
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text.contains("`func` is not an LSP SymbolKind"),
            "{}",
            out.text
        );
        // The legal names must be listed, not merely refused.
        assert!(out.text.contains("`function`"), "{}", out.text);
        assert!(out.text.contains("`enumMember`"), "{}", out.text);
        // It must not be mistakable for "that kind simply has no symbols".
        assert!(!out.text.contains("[not_found]"), "{}", out.text);
    }

    /// C: a legal kind with no matches stays a plain miss, so the two cases are
    /// distinguishable in the output.
    #[tokio::test]
    async fn a_legal_kind_with_no_matches_is_still_a_miss_not_an_error() {
        let backend = backend();
        backend.respond("workspace/symbol", Ok(Value::Null));
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "x", "kind": "function" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.starts_with("[not_found]"), "{}", out.text);
    }

    /// C: the legal names are case-insensitive and tolerate the snake_case
    /// spelling a model reaches for first.
    #[tokio::test]
    async fn find_symbol_accepts_kind_names_case_insensitively() {
        for name in [
            "function",
            "Function",
            "FUNCTION",
            "enum_member",
            "EnumMember",
        ] {
            let backend = backend();
            backend.respond(
                "workspace/symbol",
                Ok(json!([
                    symbol("Fn", 12, None, "file:///ws/a.rs", 0),
                    symbol("Member", 22, None, "file:///ws/a.rs", 1),
                ])),
            );
            let out = call(
                &backend,
                "lsp_find_symbol",
                json!({ "query": "x", "kind": name }),
            )
            .await;
            assert!(!out.is_error, "{name}: {}", out.text);
        }
    }

    /// A: nothing is dropped, but the exact symbol comes first. Before this the
    /// order was whatever the server sent, so twenty tests containing
    /// "discover" in their name could bury the declaration the caller asked for.
    #[tokio::test]
    async fn find_symbol_ranks_exact_then_prefix_then_substring() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol(
                    "a_discover_helper_is_recovered",
                    12,
                    None,
                    "file:///ws/a.rs",
                    0
                ),
                symbol(
                    "discover_roots_respect_depth",
                    12,
                    None,
                    "file:///ws/b.rs",
                    0
                ),
                symbol("discover", 12, None, "file:///ws/c.rs", 0),
            ])),
        );
        let out = call(&backend, "lsp_find_symbol", json!({ "query": "discover" })).await;
        let exact = out.text.find("function `discover`").unwrap_or_else(|| {
            panic!("the exact symbol must be present: {}", out.text);
        });
        let prefix = out
            .text
            .find("function `discover_roots_respect_depth`")
            .unwrap_or_else(|| panic!("prefix match must be present: {}", out.text));
        let substring = out
            .text
            .find("function `a_discover_helper_is_recovered`")
            .unwrap_or_else(|| panic!("substring match must be present: {}", out.text));
        assert!(
            exact < prefix && prefix < substring,
            "order must be exact, prefix, substring; got:\n{}",
            out.text
        );
        assert!(
            out.text.contains("1 exact, 1 name-prefix, 1 substring"),
            "{}",
            out.text
        );
    }

    /// A: the ranking must not depend on case — a server may answer `discover`
    /// with `Discover`.
    #[tokio::test]
    async fn find_symbol_ranks_exact_case_insensitively() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("Discover_extra", 12, None, "file:///ws/a.rs", 0),
                symbol("Discover", 12, None, "file:///ws/b.rs", 0),
            ])),
        );
        let out = call(&backend, "lsp_find_symbol", json!({ "query": "discover" })).await;
        let exact = out.text.find("function `Discover`").unwrap();
        let prefix = out.text.find("function `Discover_extra`").unwrap();
        assert!(exact < prefix, "got:\n{}", out.text);
    }

    /// No `path`: the whole workspace, unchanged. B must not quietly narrow a
    /// search that never asked to be narrow.
    #[tokio::test]
    async fn find_symbol_without_a_path_still_searches_everything() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("Here", 23, None, "file:///ws/a.rs", 0),
                symbol("There", 23, None, "file:///ws/b.rs", 0),
            ])),
        );
        let out = call(&backend, "lsp_find_symbol", json!({ "query": "e" })).await;
        assert!(out.text.contains("struct `Here`"), "{}", out.text);
        assert!(out.text.contains("struct `There`"), "{}", out.text);
    }

    #[tokio::test]
    async fn find_symbol_needs_a_query() {
        let backend = backend();
        let out = call(&backend, "lsp_find_symbol", json!({})).await;
        assert!(out.is_error);
        assert!(out.text.contains("`query`"), "{}", out.text);
    }

    #[tokio::test]
    async fn find_symbol_reports_a_bad_kind_argument() {
        let backend = backend();
        let out = call(
            &backend,
            "lsp_find_symbol",
            json!({ "query": "x", "kind": 7 }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("`kind` must be a string"), "{}", out.text);
    }

    // ---- lsp_definition / implementations ---------------------------------

    #[tokio::test]
    async fn definition_resolves_a_name_and_renders_the_hit() {
        let backend = backend().with_encoding(PositionEncoding::Utf16);
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("LspConfig", 23, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond("textDocument/definition", Ok(at("file:///ws/b.rs", 2, 3)));
        let out = call(&backend, "lsp_definition", json!({ "symbol": "LspConfig" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.starts_with("Defined at b.rs:3:4"), "{}", out.text);
    }

    #[tokio::test]
    async fn definition_lists_every_definition_it_gets() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Thing", 23, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/definition",
            Ok(json!([
                at("file:///ws/a.rs", 1, 0),
                at("file:///ws/b.rs", 2, 0)
            ])),
        );
        let out = call(&backend, "lsp_definition", json!({ "symbol": "Thing" })).await;
        assert!(out.text.starts_with("Found 2 definitions:"), "{}", out.text);
    }

    #[tokio::test]
    async fn definition_with_nothing_found_is_a_miss() {
        // A position target has no declaration to fall back on.
        let (_dir, root) = full_width_fixture();
        let backend = utf8_answer(&root, "textDocument/definition", Value::Null);
        let out = call(
            &backend,
            "lsp_definition",
            json!({ "path": "a.rs", "line": 1, "column": 1 }),
        )
        .await;
        assert!(!out.is_error);
        assert!(out.text.starts_with("[not_found]"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_named_symbol_the_server_cannot_define_falls_back_to_its_own_declaration() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Thing", 23, None, "file:///ws/a.rs", 4)])),
        );
        backend.respond("textDocument/definition", Ok(Value::Null));
        let out = call(&backend, "lsp_definition", json!({ "symbol": "Thing" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.starts_with("Defined at a.rs:5"), "{}", out.text);
    }

    #[tokio::test]
    async fn definition_notes_a_still_indexing_server_when_it_has_results() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Thing", 23, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond("textDocument/definition", Ok(at("file:///ws/a.rs", 1, 0)));
        backend.set_indexing(Some(Indexing {
            message: "indexing".to_owned(),
            percent: Some(7),
        }));
        let out = call(&backend, "lsp_definition", json!({ "symbol": "Thing" })).await;
        assert!(
            out.text
                .contains("note: mock is still indexing (7%); results may be incomplete."),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn definition_passes_a_backend_error_through() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Thing", 23, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/definition",
            Err(LspError::Timeout {
                server: "rust-analyzer".to_owned(),
                method: "textDocument/definition".to_owned(),
                ms: 30000,
            }),
        );
        let out = call(&backend, "lsp_definition", json!({ "symbol": "Thing" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[timeout]"), "{}", out.text);
    }

    #[tokio::test]
    async fn definition_refuses_mutually_exclusive_arguments() {
        let backend = backend();
        let out = call(
            &backend,
            "lsp_definition",
            json!({ "symbol": "Thing", "line": 3 }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_args]"), "{}", out.text);
    }

    #[tokio::test]
    async fn an_ambiguous_name_lists_the_candidates() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                symbol("new", 6, Some("Foo"), "file:///ws/a.rs", 0),
                symbol("new", 6, Some("Bar"), "file:///ws/b.rs", 0),
            ])),
        );
        let out = call(&backend, "lsp_definition", json!({ "symbol": "new" })).await;
        assert!(!out.is_error);
        assert!(
            out.text.starts_with("[ambiguous] `new` matches 2 symbols."),
            "{}",
            out.text
        );
        assert!(out.text.contains("in Foo"), "{}", out.text);
    }

    #[tokio::test]
    async fn implementations_renders_its_hits() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Trait", 11, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/implementation",
            Ok(at("file:///ws/b.rs", 0, 0)),
        );
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Trait" }),
        )
        .await;
        assert!(!out.is_error);
        assert!(
            out.text.starts_with("Found 1 implementation(s):"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn implementations_with_nothing_found_is_a_miss() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Trait", 11, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond("textDocument/implementation", Ok(json!([])));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Trait" }),
        )
        .await;
        assert!(!out.is_error);
        assert!(out.text.starts_with("[not_found]"), "{}", out.text);
    }

    /// A server that is still indexing must be said so.
    /// Without the note a partial implementation list reads as a complete one,
    /// and the model concludes the type has no other implementors.
    #[tokio::test]
    async fn implementations_notes_a_still_indexing_server() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Trait", 11, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/implementation",
            Ok(at("file:///ws/b.rs", 0, 0)),
        );
        backend.set_indexing(Some(Indexing {
            message: "indexing".to_owned(),
            percent: Some(12),
        }));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Trait" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text
                .contains("note: mock is still indexing (12%); results may be incomplete."),
            "{}",
            out.text
        );
    }

    /// A backend failure must reach the caller as the failure it is, not as an
    /// empty result: `[timeout]` here, and a model that reads "[not_found]" would
    /// go and write a second implementor.
    #[tokio::test]
    async fn implementations_passes_a_backend_error_through() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Trait", 11, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/implementation",
            Err(LspError::Timeout {
                server: "rust-analyzer".to_owned(),
                method: "textDocument/implementation".to_owned(),
                ms: 30000,
            }),
        );
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Trait" }),
        )
        .await;
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.starts_with("[timeout]"), "{}", out.text);
        assert!(
            !out.text.contains("[not_found]"),
            "a failed request must not read as a miss: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn implementations_on_a_function_is_a_clear_miss() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/implementation",
            Err(LspError::Rpc {
                server: "gopls".to_owned(),
                code: 0,
                message: "f is a function, not a method".to_owned(),
            }),
        );
        let out = call(&backend, "lsp_implementations", json!({ "symbol": "f" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.starts_with("[not_found] nothing to implement"),
            "{}",
            out.text
        );
    }

    // ---- lsp_implementations on a type alias -------------------------------

    /// A mock whose root is a real temporary directory, so a tool that reads
    /// the declaration line off disk (as the alias check does) sees a real file.
    fn on_disk_backend(source: &str) -> (MockBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("lib.rs"), source).expect("write fixture");
        let backend = MockBackend::new(dir.path());
        backend.set_languages(languages());
        (backend, dir)
    }

    fn file_uri(dir: &tempfile::TempDir) -> String {
        format!("file://{}/lib.rs", dir.path().display())
    }

    /// The case the alias message exists for: the server has nothing, and the
    /// symbol is an alias. "no implementation was found" sends the caller off
    /// to hunt for an implementor that cannot exist.
    #[tokio::test]
    async fn implementations_on_a_type_alias_names_the_type_it_aliases() {
        let (backend, dir) = on_disk_backend("pub type Alias = Plain;\npub struct Plain;\n");
        let uri = file_uri(&dir);
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Alias", 26, None, &uri, 0)])),
        );
        backend.respond("textDocument/implementation", Ok(json!([])));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Alias" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text
                .contains("is a type alias, so it has no implementations"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("Ask about `Plain` instead"),
            "the message must point at the type worth asking about: {}",
            out.text
        );
    }

    /// The alias check reads a file named by a language server, so it must obey
    /// the workspace boundary like every other read: an alias declared in a file
    /// outside the workspace gets the plain miss, and its text is never read.
    #[tokio::test]
    async fn an_alias_outside_the_workspace_is_not_read() {
        let (backend, _dir) = on_disk_backend("pub struct Unrelated;\n");
        let elsewhere = tempfile::tempdir().expect("second temp dir");
        std::fs::write(
            elsewhere.path().join("lib.rs"),
            "pub type Alias = Plain;\npub struct Plain;\n",
        )
        .expect("write outside fixture");
        let uri = format!("file://{}/lib.rs", elsewhere.path().display());
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Alias", 26, None, &uri, 0)])),
        );
        backend.respond("textDocument/implementation", Ok(json!([])));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Alias" }),
        )
        .await;
        assert!(
            !out.text.contains("type alias"),
            "a file outside the workspace must not be read: {}",
            out.text
        );
    }

    /// A file above the shared read cap is not read either, even inside the
    /// workspace: one declaration line is not worth pulling a huge file in.
    #[tokio::test]
    async fn an_oversized_alias_file_is_not_read() {
        let (backend, dir) = on_disk_backend("pub type Alias = Plain;\n");
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("lib.rs"))
            .expect("open fixture")
            .set_len(crate::format::MAX_FILE_BYTES + 1)
            .expect("grow fixture");
        let uri = file_uri(&dir);
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Alias", 26, None, &uri, 0)])),
        );
        backend.respond("textDocument/implementation", Ok(json!([])));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Alias" }),
        )
        .await;
        assert!(
            !out.text.contains("type alias"),
            "a file above the cap must not be read: {}",
            out.text
        );
    }

    /// An ordinary struct also has no implementations — that is a real answer,
    /// and it must keep the plain wording. Only an alias gets the new message,
    /// otherwise every empty result would claim to be one.
    #[tokio::test]
    async fn implementations_on_an_ordinary_struct_keeps_the_plain_miss() {
        let (backend, dir) = on_disk_backend("pub struct Plain {\n    pub field: u32,\n}\n");
        let uri = file_uri(&dir);
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Plain", 23, None, &uri, 0)])),
        );
        backend.respond("textDocument/implementation", Ok(json!([])));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Plain" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("no implementation was found"),
            "{}",
            out.text
        );
        assert!(
            !out.text.contains("type alias"),
            "a plain struct must not be described as an alias: {}",
            out.text
        );
    }

    /// A type *parameter* shares SymbolKind 26 with an alias, so the kind alone
    /// cannot decide. `struct Holder<T>` on the declaration line is what tells
    /// the two apart, and it must keep the plain miss.
    #[tokio::test]
    async fn a_type_parameter_is_not_mistaken_for_an_alias() {
        let (backend, dir) = on_disk_backend("pub struct Holder<T> {\n    pub inner: T,\n}\n");
        let uri = file_uri(&dir);
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Holder", 26, None, &uri, 0)])),
        );
        backend.respond("textDocument/implementation", Ok(json!([])));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Holder" }),
        )
        .await;
        assert!(
            out.text.contains("no implementation was found"),
            "{}",
            out.text
        );
        assert!(
            !out.text.contains("type alias"),
            "a generic parameter must not be described as an alias: {}",
            out.text
        );
    }

    /// A symbol that does not exist never reaches the alias check — it is
    /// reported by `locate`, before any server is asked.
    #[tokio::test]
    async fn implementations_on_a_symbol_that_does_not_exist_stays_not_found() {
        let backend = backend();
        backend.respond("workspace/symbol", Ok(json!([])));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "NoSuchSymbol" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text
                .starts_with("[not_found] no symbol named `NoSuchSymbol`"),
            "{}",
            out.text
        );
        assert!(
            !out.text.contains("type alias"),
            "a missing symbol must not be described as an alias: {}",
            out.text
        );
    }

    /// A server that does find an implementor answers normally: the alias check
    /// only runs on the empty path, so it must not pre-empt a real answer.
    #[tokio::test]
    async fn implementations_on_an_alias_still_reports_what_the_server_found() {
        let (backend, dir) = on_disk_backend("pub type Alias = Plain;\n");
        let uri = file_uri(&dir);
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Alias", 26, None, &uri, 0)])),
        );
        backend.respond("textDocument/implementation", Ok(json!([at(&uri, 1, 0)])));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Alias" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.starts_with("Found 1 implementation(s):"),
            "a non-empty answer must be left alone: {}",
            out.text
        );
        assert!(!out.text.contains("type alias"), "{}", out.text);
    }

    /// A still-indexing server is said so, never mistaken for "this alias has
    /// no implementations": the alias message asserts a fact about the symbol
    /// that an indexing server has not confirmed yet.
    #[tokio::test]
    async fn implementations_on_an_alias_during_indexing_still_says_indexing() {
        let (backend, dir) = on_disk_backend("pub type Alias = Plain;\n");
        let uri = file_uri(&dir);
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("Alias", 26, None, &uri, 0)])),
        );
        backend.respond("textDocument/implementation", Ok(at(&uri, 0, 0)));
        backend.set_indexing(Some(Indexing {
            message: "indexing".to_owned(),
            percent: Some(12),
        }));
        let out = call(
            &backend,
            "lsp_implementations",
            json!({ "symbol": "Alias" }),
        )
        .await;
        assert!(
            !out.text.contains("has no implementations"),
            "indexing must win over the alias message: {}",
            out.text
        );
        assert!(
            out.text
                .contains("note: mock is still indexing (12%); results may be incomplete."),
            "{}",
            out.text
        );
    }

    /// The declaration shape, spelled out. These are the lines that must and
    /// must not read as an alias; `T` in `Holder<T>` and a *use* of `Alias` are
    /// the two that a name-based check would get wrong.
    #[test]
    fn alias_declarations_are_told_apart_from_everything_else() {
        for declaration in [
            "pub type Alias = Plain;",
            "type Alias = Plain;",
            "pub type Alias = crate::inner::Plain;",
            "pub(crate) type Alias = Plain;",
            "pub(super) type Alias = Plain;",
            "pub(in crate::x) type Alias = Plain;",
            "type Callback = fn(u32) -> u32;",
        ] {
            assert!(
                declares_an_alias(declaration),
                "must read as an alias: {declaration}"
            );
        }
        for declaration in [
            "pub struct Plain {",
            "pub struct Holder<T> {",
            "pub type Opaque;",
            "type = Plain;",
            "let x: Alias = Plain { field: 0 };",
            "pub fn types::thing() {}",
            "// type Alias = Plain;",
            "pub const Alias: u32 = 1;",
        ] {
            assert!(
                !declares_an_alias(declaration),
                "must NOT read as an alias: {declaration}"
            );
        }
    }

    /// The suggestion must name the type a caller can go and query, and refuse
    /// to invent one where there is no name to offer.
    #[test]
    fn the_alias_target_is_a_name_the_caller_can_query() {
        assert_eq!(alias_target("pub type Alias = Plain;"), Some("Plain"));
        assert_eq!(alias_target("type Alias = Plain"), Some("Plain"));
        assert_eq!(
            alias_target("pub type Alias = crate::inner::Plain;"),
            Some("crate::inner::Plain")
        );
        // No single queryable name behind these.
        assert_eq!(alias_target("pub type Pair<T> = (T, T);"), None);
        assert_eq!(alias_target("pub type Text = Vec<u8>;"), None);
        assert_eq!(alias_target("pub type P = &'static str;"), None);
        assert_eq!(alias_target("pub type Opaque;"), None);
    }

    // ---- lsp_references ---------------------------------------------------

    #[tokio::test]
    async fn references_group_by_file_and_ask_for_declarations_when_asked() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/references",
            Ok(json!([
                at("file:///ws/a.rs", 1, 0),
                at("file:///ws/b.rs", 2, 0)
            ])),
        );
        let out = call(
            &backend,
            "lsp_references",
            json!({ "symbol": "f", "include_declaration": true }),
        )
        .await;
        assert!(
            out.text.starts_with("Found 2 reference(s) in 2 file(s):"),
            "{}",
            out.text
        );
        let calls = backend.calls();
        let request = calls
            .iter()
            .find(|call| call.method == "textDocument/references")
            .expect("references was requested");
        assert_eq!(
            request.params["context"],
            json!({ "includeDeclaration": true })
        );
    }

    #[tokio::test]
    async fn references_stop_at_the_limit_and_say_how_many_were_left_out() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/references",
            Ok(json!([
                at("file:///ws/a.rs", 1, 0),
                at("file:///ws/a.rs", 2, 0),
                at("file:///ws/a.rs", 3, 0)
            ])),
        );
        let out = call(
            &backend,
            "lsp_references",
            json!({ "symbol": "f", "limit": 2 }),
        )
        .await;
        assert!(
            out.text.contains("... and 1 more not listed"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn references_with_nothing_found_is_a_miss() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond("textDocument/references", Ok(json!([{ "range": {} }])));
        let out = call(&backend, "lsp_references", json!({ "symbol": "f" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_response]"), "{}", out.text);
    }

    #[tokio::test]
    async fn references_rejects_a_bad_include_declaration() {
        let backend = backend();
        let out = call(
            &backend,
            "lsp_references",
            json!({ "symbol": "f", "include_declaration": "yes" }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("`include_declaration`"), "{}", out.text);
    }

    #[tokio::test]
    async fn references_rejects_a_zero_limit() {
        let backend = backend();
        let out = call(
            &backend,
            "lsp_references",
            json!({ "symbol": "f", "limit": 0 }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("`limit`"), "{}", out.text);
    }

    // ---- lsp_hover --------------------------------------------------------

    #[tokio::test]
    async fn hover_flattens_markdown_to_plain_text() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond(
            "textDocument/hover",
            Ok(json!({ "contents": { "kind": "markdown", "value": "**fn** `f`" } })),
        );
        let out = call(&backend, "lsp_hover", json!({ "symbol": "f" })).await;
        assert!(!out.is_error);
        assert!(out.text.contains("fn f"), "{}", out.text);
        assert!(!out.text.contains("**"), "{}", out.text);
    }

    #[tokio::test]
    async fn hover_with_a_null_answer_is_a_miss() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond("textDocument/hover", Ok(Value::Null));
        let out = call(&backend, "lsp_hover", json!({ "symbol": "f" })).await;
        assert!(!out.is_error);
        assert!(out.text.starts_with("[not_found]"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_symbol_miss_says_it_was_resolved_to_the_declaration() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond("textDocument/hover", Ok(Value::Null));
        backend.respond("textDocument/references", Ok(json!([])));
        for tool in ["lsp_hover", "lsp_references"] {
            let out = call(&backend, tool, json!({ "symbol": "f" })).await;
            assert!(
                out.text.contains("resolved to its declaration"),
                "{tool}: {}",
                out.text
            );
        }
        let out = call(
            &backend,
            "lsp_hover",
            json!({ "path": "a.rs", "line": 1, "column": 1 }),
        )
        .await;
        assert!(
            !out.text.contains("resolved to its declaration"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn hover_reports_an_unreadable_answer() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        backend.respond("textDocument/hover", Ok(json!({ "no_contents": 1 })));
        let out = call(&backend, "lsp_hover", json!({ "symbol": "f" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_response]"), "{}", out.text);
    }

    #[tokio::test]
    async fn hover_needs_a_target() {
        let backend = backend();
        let out = call(&backend, "lsp_hover", json!({})).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_args]"), "{}", out.text);
    }

    // ---- lsp_outline / lsp_diagnostics ------------------------------------

    #[tokio::test]
    async fn outline_renders_the_nesting() {
        let backend = backend();
        backend.respond(
            "textDocument/documentSymbol",
            Ok(json!([{
                "name": "m",
                "kind": 2,
                "range": { "start": { "line": 0, "character": 0 } },
                "selectionRange": { "start": { "line": 0, "character": 4 } },
                "children": [{
                    "name": "f",
                    "kind": 12,
                    "range": { "start": { "line": 1, "character": 4 } },
                    "selectionRange": { "start": { "line": 1, "character": 7 } }
                }]
            }])),
        );
        let out = call(&backend, "lsp_outline", json!({ "path": "a.rs" })).await;
        assert!(!out.is_error);
        assert!(out.text.starts_with("2 symbol(s):"), "{}", out.text);
        assert!(out.text.contains("\n    f (Function)"), "{}", out.text);
    }

    #[tokio::test]
    async fn outline_of_an_empty_answer_is_a_miss() {
        let backend = backend();
        backend.respond("textDocument/documentSymbol", Ok(Value::Null));
        let out = call(&backend, "lsp_outline", json!({ "path": "a.rs" })).await;
        assert!(!out.is_error);
        assert!(out.text.starts_with("[not_found]"), "{}", out.text);
    }

    #[tokio::test]
    async fn outline_needs_a_path() {
        let backend = backend();
        let out = call(&backend, "lsp_outline", json!({})).await;
        assert!(out.is_error);
        assert!(
            out.text.contains("`lsp_outline` needs `path`"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn diagnostics_says_a_clean_file_is_clean_once_the_server_said_so() {
        let backend = backend();
        backend.respond_diagnostics(Ok(report(Vec::new(), true)));
        let out = call(&backend, "lsp_diagnostics", json!({ "path": "a.rs" })).await;
        assert!(!out.is_error);
        assert!(
            out.text.starts_with("0 diagnostics for a.rs"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn diagnostics_never_claims_a_file_is_clean_without_a_publish() {
        let backend = backend();
        backend.respond_diagnostics(Ok(report(Vec::new(), false)));
        let out = call(&backend, "lsp_diagnostics", json!({ "path": "a.rs" })).await;
        assert!(
            out.text.contains("No diagnostics are known"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("NOT a statement that the file is clean"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn diagnostics_needs_a_path() {
        let backend = backend();
        let out = call(&backend, "lsp_diagnostics", json!({})).await;
        assert!(out.is_error);
        assert!(
            out.text.contains("`lsp_diagnostics` needs `path`"),
            "{}",
            out.text
        );
    }

    // ---- lsp_callers / lsp_callees ----------------------------------------

    fn call_item(uri: &str, line: u32, name: &str) -> Value {
        json!({
            "name": name,
            "kind": 12,
            "uri": uri,
            "range": { "start": { "line": line, "character": 0 } },
            "selectionRange": { "start": { "line": line, "character": 3 } },
            "data": { "opaque": true }
        })
    }

    /// The mock's `workspace/symbol` answer that resolves `alpha` in `a.rs`.
    fn alpha_symbol(backend: &MockBackend) {
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("alpha", 12, None, "file:///ws/a.rs", 0)])),
        );
    }

    #[tokio::test]
    async fn callers_resolves_the_name_and_renders_the_tree() {
        let backend = backend();
        alpha_symbol(&backend);
        backend.respond(
            "textDocument/prepareCallHierarchy",
            Ok(json!([call_item("file:///ws/a.rs", 0, "alpha")])),
        );
        backend.respond(
            "callHierarchy/incomingCalls",
            Ok(json!([{
                "from": call_item("file:///ws/b.rs", 10, "beta"),
                "fromRanges": [{ "start": { "line": 10, "character": 4 } }]
            }])),
        );
        let out = call(&backend, "lsp_callers", json!({ "symbol": "alpha" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            out.text,
            "alpha  a.rs:1:4\n  <- beta  b.rs:11:4  (call at b.rs:11:5)"
        );
    }

    #[tokio::test]
    async fn callees_uses_the_outgoing_end() {
        let backend = backend();
        alpha_symbol(&backend);
        backend.respond(
            "textDocument/prepareCallHierarchy",
            Ok(json!([call_item("file:///ws/a.rs", 0, "alpha")])),
        );
        backend.respond(
            "callHierarchy/outgoingCalls",
            Ok(json!([{
                "to": call_item("file:///ws/b.rs", 10, "beta"),
                "fromRanges": [{ "start": { "line": 0, "character": 4 } }]
            }])),
        );
        let out = call(&backend, "lsp_callees", json!({ "symbol": "alpha" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            out.text,
            "alpha  a.rs:1:4\n  -> beta  b.rs:11:4  (call at a.rs:1:5)"
        );
    }

    #[tokio::test]
    async fn a_walk_of_depth_two_marks_the_repeat() {
        // MockBackend answers every `incomingCalls` with the same list, so the
        // second level comes back to the one node already placed.
        let backend = backend();
        alpha_symbol(&backend);
        backend.respond(
            "textDocument/prepareCallHierarchy",
            Ok(json!([call_item("file:///ws/a.rs", 0, "alpha")])),
        );
        backend.respond(
            "callHierarchy/incomingCalls",
            Ok(json!([{
                "from": call_item("file:///ws/a.rs", 0, "alpha"),
                "fromRanges": [{ "start": { "line": 0, "character": 0 } }]
            }])),
        );
        let out = call(
            &backend,
            "lsp_callers",
            json!({ "symbol": "alpha", "depth": 2 }),
        )
        .await;
        assert_eq!(
            out.text,
            "alpha  a.rs:1:4\n  <- alpha  a.rs:1:4  (see above)"
        );
    }

    #[tokio::test]
    async fn callers_with_no_hierarchy_item_is_a_miss() {
        let backend = backend();
        alpha_symbol(&backend);
        backend.respond("textDocument/prepareCallHierarchy", Ok(Value::Null));
        let out = call(&backend, "lsp_callers", json!({ "symbol": "alpha" })).await;
        assert!(!out.is_error);
        assert!(out.text.starts_with("[not_found]"), "{}", out.text);
    }

    #[tokio::test]
    async fn callers_rejects_a_zero_depth() {
        let backend = backend();
        let out = call(
            &backend,
            "lsp_callers",
            json!({ "symbol": "alpha", "depth": 0 }),
        )
        .await;
        assert!(out.is_error);
        assert!(out.text.contains("`depth`"), "{}", out.text);
    }

    /// The same argument check, asked of the other direction. `callers` and
    /// `callees` share `call_hierarchy`, so the guard is shared too — but a
    /// shared guard reached only by one of the two tools is a guard that can
    /// regress on the other without anything going red.
    #[tokio::test]
    async fn callees_rejects_a_zero_depth() {
        let backend = backend();
        let out = call(
            &backend,
            "lsp_callees",
            json!({ "symbol": "alpha", "depth": 0 }),
        )
        .await;
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("`depth`"), "{}", out.text);
    }

    /// A non-numeric depth is a typo, and must be reported before any request
    /// reaches a server.
    #[tokio::test]
    async fn callees_rejects_a_non_numeric_depth() {
        let backend = backend();
        let out = call(
            &backend,
            "lsp_callees",
            json!({ "symbol": "alpha", "depth": "two" }),
        )
        .await;
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("`depth`"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_depth_past_the_cap_is_clamped_and_noted() {
        let backend = backend();
        alpha_symbol(&backend);
        backend.respond(
            "textDocument/prepareCallHierarchy",
            Ok(json!([call_item("file:///ws/a.rs", 0, "alpha")])),
        );
        backend.respond("callHierarchy/incomingCalls", Ok(json!([])));

        let clamped = call(
            &backend,
            "lsp_callers",
            json!({ "symbol": "alpha", "depth": 9 }),
        )
        .await;
        assert!(
            clamped.text.contains("note: depth clamped to 3"),
            "{}",
            clamped.text
        );

        let exact = call(
            &backend,
            "lsp_callers",
            json!({ "symbol": "alpha", "depth": 3 }),
        )
        .await;
        assert!(!exact.text.contains("clamped"), "{}", exact.text);
    }

    // ---- lsp_rename_preview -----------------------------------------------

    #[tokio::test]
    async fn rename_preview_needs_a_new_name() {
        let backend = backend();
        let out = call(&backend, "lsp_rename_preview", json!({ "symbol": "f" })).await;
        assert!(out.is_error);
        assert!(out.text.contains("`new_name`"), "{}", out.text);

        let out = call(&backend, "lsp_rename_preview", json!({})).await;
        assert!(out.text.starts_with("[invalid_args]"), "{}", out.text);
    }

    #[tokio::test]
    async fn rename_preview_still_needs_a_target() {
        // The arguments are validated before anything is asked of the server,
        // so a model that calls it with the wrong shape gets `[invalid_args]`
        // like any other tool (the preview itself lives in `rename`).
        let backend = backend();
        let out = call(&backend, "lsp_rename_preview", json!({ "new_name": "g" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_args]"), "{}", out.text);
        assert!(backend.calls().is_empty(), "{:?}", backend.calls());
    }

    // ---- the whole catalogue ----------------------------------------------

    #[tokio::test]
    async fn every_name_in_the_catalog_is_dispatchable() {
        let backend = backend();
        for name in NAMES {
            let out = tools_call(&backend, name, json!({})).await;
            assert!(!out.text.is_empty(), "{name} answered nothing");
        }
    }

    #[tokio::test]
    async fn a_call_with_a_non_object_argument_is_rejected_where_it_matters() {
        // `lsp_status` takes nothing, so any value is fine; a target tool must
        // still say what is wrong rather than panicking.
        let backend = backend();
        let out = call(&backend, "lsp_definition", Value::Null).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_args]"), "{}", out.text);
    }

    #[test]
    fn a_non_file_candidate_cannot_be_queried() {
        let candidate = Candidate {
            site: crate::operations::Site {
                path: None,
                uri: "jdt://contents/String.class".to_owned(),
                line: Some(1),
                character: Some(1),
            },
            name: "String".to_owned(),
            kind: 5,
            container: None,
            server: "jdt".to_owned(),
            outside_workspace: false,
        };
        assert!(file_of(&candidate).is_err());
    }

    #[test]
    fn the_empty_answer_helper_marks_a_miss() {
        let out = empty("no definition was found", "rust-analyzer");
        assert!(!out.is_error);
        assert!(out.text.starts_with("[not_found]"));
    }

    #[test]
    fn megabytes_render_with_one_decimal_or_n_a() {
        assert_eq!(megabytes(None), "n/a");
        assert_eq!(megabytes(Some(1024 * 1024)), "1.0MB");
    }

    #[test]
    fn lists_render_none_when_empty() {
        assert_eq!(list(&[]), "none");
        assert_eq!(list(&["rust".to_owned()]), "rust");
    }

    #[test]
    fn a_missing_line_uses_zero_rather_than_panicking() {
        let candidate = Candidate {
            site: crate::operations::Site {
                path: Some(PathBuf::from("/ws/a.rs")),
                uri: "file:///ws/a.rs".to_owned(),
                line: None,
                character: None,
            },
            name: "x".to_owned(),
            kind: 12,
            container: None,
            server: "rust-analyzer".to_owned(),
            outside_workspace: false,
        };
        let params = position_params(&candidate, Path::new("/ws/a.rs"));
        assert_eq!(params["position"], json!({ "line": 0, "character": 0 }));
    }

    /// Full-width characters are where a byte offset, a UTF-16 unit and the
    /// model's column all disagree; the answer must come back in the model's
    /// counting even though the server spoke UTF-8.
    fn full_width_fixture() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("temp dir");
        let line = "let \u{4e2d}\u{6587} = 1;";
        std::fs::write(dir.path().join("a.rs"), format!("{line}\n")).expect("write fixture");
        let root = dir.path().to_string_lossy().to_string();
        (dir, root)
    }

    fn utf8_answer(root: &str, method: &str, value: Value) -> MockBackend {
        let backend = MockBackend::new(root).with_encoding(PositionEncoding::Utf8);
        backend.set_languages(languages());
        backend.respond(method, Ok(value));
        backend
    }

    #[tokio::test]
    async fn definition_converts_a_full_width_column_end_to_end() {
        let (_dir, root) = full_width_fixture();
        // Byte 7 is "let " (4) plus the three-byte first full-width scalar, so
        // it is the model's 6th column.
        let backend = utf8_answer(
            &root,
            "textDocument/definition",
            json!({
                "uri": format!("file://{root}/a.rs"),
                "range": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 10 } }
            }),
        );
        let out = call(
            &backend,
            "lsp_definition",
            json!({ "path": "a.rs", "line": 1, "column": 7 }),
        )
        .await;
        assert!(out.text.starts_with("Defined at a.rs:1:6"), "{}", out.text);
    }

    #[tokio::test]
    async fn hover_converts_a_full_width_column_end_to_end() {
        let (_dir, root) = full_width_fixture();
        let backend = utf8_answer(
            &root,
            "textDocument/hover",
            json!({
                "contents": { "kind": "markdown", "value": "**fn** `f`" },
                "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 7 } }
            }),
        );
        let out = call(
            &backend,
            "lsp_hover",
            json!({ "path": "a.rs", "line": 1, "column": 6 }),
        )
        .await;
        assert!(out.text.starts_with("Hover at a.rs:1:5:"), "{}", out.text);
        assert!(out.text.contains("fn f"), "{}", out.text);
    }

    #[tokio::test]
    async fn the_outgoing_column_is_utf16_for_a_full_width_line() {
        let (_dir, root) = full_width_fixture();
        let backend = utf8_answer(&root, "textDocument/hover", Value::Null);
        // The model's 7th column is UTF-16 unit 6 on this line.
        let _ = call(
            &backend,
            "lsp_hover",
            json!({ "path": "a.rs", "line": 1, "column": 7 }),
        )
        .await;
        let calls = backend.calls();
        let request = calls
            .iter()
            .find(|call| call.method == "textDocument/hover")
            .expect("hover was requested");
        assert_eq!(request.params["position"]["character"], json!(6));
    }

    /// `lsp_references` clamps an oversized `limit` instead of obeying it.
    ///
    /// This tool was the one place the limit was passed through as `usize::MAX`,
    /// so the size of the answer was whatever the model asked for. The schema
    /// advertises the ceiling, and the code enforces it whatever the caller says
    /// — a model is not a trusted input.
    #[tokio::test]
    async fn references_clamp_an_oversized_limit_instead_of_obeying_it() {
        let backend = backend();
        backend.respond(
            "workspace/symbol",
            Ok(json!([symbol("f", 12, None, "file:///ws/a.rs", 0)])),
        );
        let sites: Vec<Value> = (0..MAX_REFERENCE_LIMIT + 50)
            .map(|n| {
                at(
                    "file:///ws/a.rs",
                    u32::try_from(n).expect("well under u32::MAX"),
                    0,
                )
            })
            .collect();
        backend.respond("textDocument/references", Ok(json!(sites)));
        let out = call(
            &backend,
            "lsp_references",
            json!({ "symbol": "f", "limit": 1_000_000_000u64 }),
        )
        .await;
        assert!(
            out.text
                .contains(&format!("... and {} more not listed", 50)),
            "the clamp must still report what was left out: {}",
            &out.text[..out.text.len().min(300)]
        );
        // The number of listed rows is the ceiling, not the caller's number.
        // The one group header also ends in `a.rs:`, hence the second test.
        let listed = out
            .text
            .lines()
            .filter(|l| l.contains("a.rs:") && !l.trim_end().ends_with(':'))
            .count();
        assert!(
            listed <= MAX_REFERENCE_LIMIT,
            "{listed} rows were listed, over the {MAX_REFERENCE_LIMIT} ceiling"
        );
    }

    /// And the schema says so, so a model does not have to discover it by
    /// hitting the ceiling.
    #[test]
    fn the_references_schema_advertises_its_ceiling() {
        let schema = defs()
            .into_iter()
            .find(|def| def.name == "lsp_references")
            .expect("lsp_references is in the catalog")
            .input_schema;
        assert_eq!(
            schema["properties"]["limit"]["maximum"],
            json!(MAX_REFERENCE_LIMIT)
        );
    }
}
