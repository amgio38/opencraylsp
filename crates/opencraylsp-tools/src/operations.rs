//! The `lsp` tool's ten operations: how each maps to an LSP request, and how
//! the shapes a server answers with are decoded.
//!
//! Why the mapping and the decoding live in one file: the method a request
//! needs and the shape its answer arrives in are two halves of one fact, and
//! separating them is how a `findReferences` ends up parsed as a
//! `goToDefinition`.
//!
//! Decoding is written against raw JSON rather than deserialising into
//! `lsp_types` structs. That is deliberate: the protocol is loose exactly where
//! it hurts — `textDocument/definition` may answer with a `Location`, a
//! `LocationLink`, or an array of either; `textDocument/documentSymbol` may
//! answer with a hierarchical `DocumentSymbol[]` or a flat
//! `SymbolInformation[]`; `workspace/symbol` may omit the position entirely —
//! and a strict deserialiser turns those legal answers into errors. What
//! decoding must never do is turn an *unreadable* answer into an empty success:
//! "no references" and "I could not read the answer" are different sentences,
//! and only one of them is true.

use std::fmt;
use std::path::PathBuf;

use serde_json::{Value, json};

/// A symbol nesting deeper than this is not a real document; the cap keeps a
/// broken `children` chain from recursing the stack away.
const MAX_SYMBOL_DEPTH: usize = 32;

/// The ten operations the tool exposes, in the order the schema lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    GoToDefinition,
    FindReferences,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    GoToImplementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
    Diagnostics,
}

impl Operation {
    /// Every operation, in schema order.
    pub const ALL: [Operation; 10] = [
        Operation::GoToDefinition,
        Operation::FindReferences,
        Operation::Hover,
        Operation::DocumentSymbol,
        Operation::WorkspaceSymbol,
        Operation::GoToImplementation,
        Operation::PrepareCallHierarchy,
        Operation::IncomingCalls,
        Operation::OutgoingCalls,
        Operation::Diagnostics,
    ];

    /// Parses the model's `operation` argument.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.as_str() == raw)
    }

    /// The argument spelling of this operation.
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::GoToDefinition => "goToDefinition",
            Operation::FindReferences => "findReferences",
            Operation::Hover => "hover",
            Operation::DocumentSymbol => "documentSymbol",
            Operation::WorkspaceSymbol => "workspaceSymbol",
            Operation::GoToImplementation => "goToImplementation",
            Operation::PrepareCallHierarchy => "prepareCallHierarchy",
            Operation::IncomingCalls => "incomingCalls",
            Operation::OutgoingCalls => "outgoingCalls",
            Operation::Diagnostics => "diagnostics",
        }
    }

    /// Whether the model must give `line` and `character`.
    ///
    /// `documentSymbol` needs only the file, `workspaceSymbol` needs only the
    /// query, `diagnostics` needs only the file; everything else is a question
    /// *about a position*.
    pub fn needs_position(self) -> bool {
        matches!(
            self,
            Operation::GoToDefinition
                | Operation::FindReferences
                | Operation::Hover
                | Operation::GoToImplementation
                | Operation::PrepareCallHierarchy
                | Operation::IncomingCalls
                | Operation::OutgoingCalls
        )
    }

    /// Whether the model must give `query` (`workspaceSymbol` only).
    pub fn needs_query(self) -> bool {
        matches!(self, Operation::WorkspaceSymbol)
    }

    /// The LSP method of the first request, or `None` for `diagnostics`, which
    /// the backend owns end to end.
    pub fn method(self) -> Option<&'static str> {
        Some(match self {
            Operation::GoToDefinition => "textDocument/definition",
            Operation::FindReferences => "textDocument/references",
            Operation::Hover => "textDocument/hover",
            Operation::DocumentSymbol => "textDocument/documentSymbol",
            Operation::WorkspaceSymbol => "workspace/symbol",
            Operation::GoToImplementation => "textDocument/implementation",
            Operation::PrepareCallHierarchy
            | Operation::IncomingCalls
            | Operation::OutgoingCalls => "textDocument/prepareCallHierarchy",
            Operation::Diagnostics => return None,
        })
    }

    /// The second request a call-hierarchy operation sends, once it holds the
    /// item `prepareCallHierarchy` returned.
    ///
    /// The model gives a position, not an item; LSP's call hierarchy is a
    /// two-step protocol and hiding the first step is the tool's job.
    pub fn follow_up_method(self) -> Option<&'static str> {
        match self {
            Operation::IncomingCalls => Some("callHierarchy/incomingCalls"),
            Operation::OutgoingCalls => Some("callHierarchy/outgoingCalls"),
            _ => None,
        }
    }

    /// The JSON-RPC `params` for the first request.
    ///
    /// `line`/`character` are 0-based and already in the encoding the server
    /// speaks (see `tool.rs`); the operations that carry no position ignore
    /// them.
    pub fn request_params(self, uri: &str, line: u32, character: u32, query: &str) -> Value {
        let text_document = json!({ "uri": uri });
        let position = json!({ "line": line, "character": character });
        match self {
            Operation::DocumentSymbol => json!({ "textDocument": text_document }),
            Operation::WorkspaceSymbol => json!({ "query": query }),
            Operation::Diagnostics => Value::Null,
            Operation::FindReferences => json!({
                "textDocument": text_document,
                "position": position,
                // Declarations are references too: an agent asking "who uses
                // this" that is not shown the definition cannot tell a rename
                // target from a call site.
                "context": { "includeDeclaration": true },
            }),
            _ => json!({ "textDocument": text_document, "position": position }),
        }
    }
}

/// A position the server named, in the server's own coordinates.
///
/// The line and character stay 0-based and encoded until the formatter converts
/// them, because that is where the line's text — the only thing that can do the
/// conversion — is at hand.
#[derive(Debug, Clone, PartialEq)]
pub struct Site {
    /// The file, when the URI names one the OS can open.
    pub path: Option<PathBuf>,
    /// The URI exactly as the server sent it, kept for non-`file:` schemes
    /// (`jdt:`, `untitled:`, …) so a result outside the file system still
    /// renders as *something* instead of vanishing.
    pub uri: String,
    /// 0-based line; absent when the server named a file with no position.
    pub line: Option<u32>,
    /// 0-based character in the server's encoding.
    pub character: Option<u32>,
}

/// One symbol from `documentSymbol` or `workspaceSymbol`.
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    pub name: String,
    /// The raw `SymbolKind` integer; named by the formatter.
    pub kind: u32,
    pub detail: Option<String>,
    /// `SymbolInformation.containerName`, when the server sent one.
    pub container: Option<String>,
    pub site: Site,
    /// Nesting from `DocumentSymbol.children`; 0 for a top-level or flat symbol.
    pub depth: usize,
}

/// What `textDocument/documentSymbol` yielded, including what was unreadable.
///
/// The count matters: silently listing fewer symbols than the server sent looks
/// exactly like a file that declares fewer symbols, and an agent that believes
/// the outline is complete will draw the wrong conclusion about the code.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SymbolList {
    pub symbols: Vec<Symbol>,
    /// Entries the server sent that were not readable as a symbol. Their
    /// *readable children are still listed* — a malformed parent must not take
    /// a valid subtree with it — so this counts what is missing, not what was
    /// dropped wholesale.
    pub skipped: usize,
}

/// A `textDocument/hover` answer.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverInfo {
    pub text: String,
    /// Whether `text` is markdown (the formatter flattens it).
    pub markdown: bool,
    /// The hovered range, if the server sent one.
    pub range: Option<(u32, u32)>,
}

/// A response that is not any of the shapes the operation can produce.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeError {
    pub detail: String,
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

fn shape(detail: impl Into<String>) -> ShapeError {
    ShapeError {
        detail: detail.into(),
    }
}

/// Splits a URI into an openable path (only for `file:`) and the raw string.
fn site_prefix(uri: &str) -> (Option<PathBuf>, String) {
    let path = url::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok());
    (path, uri.to_owned())
}

fn parse_position(value: &Value) -> Option<(u32, u32)> {
    let line = value.get("line")?.as_u64()?;
    let character = value.get("character")?.as_u64()?;
    Some((line as u32, character as u32))
}

fn parse_range_start(value: &Value) -> Option<(u32, u32)> {
    parse_position(value.get("start")?)
}

fn parse_kind(value: &Value) -> u32 {
    value.get("kind").and_then(Value::as_u64).unwrap_or(0) as u32
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn site_of(uri: &str, start: Option<(u32, u32)>) -> Site {
    let (path, uri) = site_prefix(uri);
    Site {
        path,
        uri,
        line: start.map(|(line, _)| line),
        character: start.map(|(_, character)| character),
    }
}

/// Reads one `Location` or `LocationLink` into a [`Site`].
fn parse_location_item(item: &Value) -> Option<Site> {
    // A LocationLink names its target with `targetUri`; a Location uses `uri`.
    // Which key is present is the only reliable discriminator, because the two
    // shapes share no required field.
    if let Some(target) = item.get("targetUri").and_then(Value::as_str) {
        let start = item
            .get("targetSelectionRange")
            .or_else(|| item.get("targetRange"))
            .and_then(parse_range_start);
        return Some(site_of(target, start));
    }
    let uri = item.get("uri").and_then(Value::as_str)?;
    let start = item.get("range").and_then(parse_range_start);
    Some(site_of(uri, start))
}

/// Decodes `textDocument/definition` or `textDocument/implementation`.
///
/// A `null` or empty answer is a real "none", not an error — the server looked
/// and found nothing, and that is information.
pub fn locations(value: &Value) -> Result<Vec<Site>, ShapeError> {
    locations_with_skipped(value).map(|(sites, _)| sites)
}

/// Like [`locations`], plus how many entries were unreadable and skipped.
///
/// One malformed entry used to fail the whole answer, so 99 good references
/// were thrown away for one bad one. Now bad entries are skipped and counted
/// (the same rule `document_symbols` and `calls` follow); only an answer with
/// no readable entry at all is a shape error.
pub fn locations_with_skipped(value: &Value) -> Result<(Vec<Site>, usize), ShapeError> {
    let items = as_items(value, "a location, a list of locations, or null")?;
    let out: Vec<Site> = items.iter().filter_map(parse_location_item).collect();
    guard_readable(
        &out,
        items.len(),
        "location (neither `uri` nor `targetUri`)",
    )?;
    let skipped = items.len() - out.len();
    Ok((out, skipped))
}

/// Decodes `textDocument/documentSymbol`.
///
/// Both legal shapes are flattened into one list carrying a `depth`, which is
/// what the formatter's indentation wants: `DocumentSymbol` nests through
/// `children`, `SymbolInformation` does not nest at all.
pub fn document_symbols(value: &Value) -> Result<SymbolList, ShapeError> {
    let items = as_items(value, "a symbol list or null")?;
    let mut list = SymbolList::default();
    for item in items {
        // `SymbolInformation` carries a `location`; `DocumentSymbol` carries its
        // ranges directly. Which key is present is the discriminator the
        // protocol leaves to the client.
        if item.get("location").is_some() {
            match flat_symbol(item) {
                Some(symbol) => list.symbols.push(symbol),
                None => list.skipped += 1,
            }
        } else {
            collect_document_symbol(item, 0, &mut list);
        }
    }
    guard_readable(&list.symbols, items.len(), "document symbol")?;
    Ok(list)
}

/// Decodes `workspace/symbol`.
///
/// The protocol's `WorkspaceSymbol` may omit the position (only a `uri` is
/// guaranteed), so a site without a line is legitimate here rather than a
/// decode failure.
pub fn workspace_symbols(value: &Value) -> Result<Vec<Symbol>, ShapeError> {
    let items = as_items(value, "a symbol list or null")?;
    let mut out = Vec::new();
    for item in items {
        if let Some(symbol) = flat_symbol(item) {
            out.push(symbol);
        }
    }
    guard_readable(&out, items.len(), "workspace symbol")?;
    Ok(out)
}

/// Decodes `textDocument/hover`.
pub fn hover(value: &Value) -> Result<Option<HoverInfo>, ShapeError> {
    if value.is_null() {
        return Ok(None);
    }
    let contents = value
        .get("contents")
        .ok_or_else(|| shape("the hover answer had no `contents`"))?;
    let (text, markdown) = hover_text(contents);
    Ok(Some(HoverInfo {
        text,
        markdown,
        range: value.get("range").and_then(parse_range_start),
    }))
}

fn hover_text(contents: &Value) -> (String, bool) {
    match contents {
        // A bare string is markdown by the protocol's definition.
        Value::String(text) => (text.clone(), true),
        Value::Array(parts) => {
            let mut texts = Vec::with_capacity(parts.len());
            let mut markdown = false;
            for part in parts {
                let (text, md) = hover_text(part);
                markdown |= md;
                texts.push(text);
            }
            (texts.join("\n\n"), markdown)
        }
        Value::Object(_) => {
            let value = contents
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // A `MarkedString` with a `language` is a code sample
            // (`MarkedString::LanguageString`), not prose. Flattening it as
            // markdown eats the `**` in `def f(**kwargs)` and the backticks in
            // a shell snippet — the code *is* the answer, so it is fenced, and
            // the flattener leaves fenced lines alone.
            if let Some(language) = contents.get("language").and_then(Value::as_str) {
                return (format!("```{language}\n{value}\n```"), true);
            }
            let kind = contents
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("markdown");
            (value.to_owned(), kind == "markdown")
        }
        _ => (String::new(), false),
    }
}

/// Reads a `SymbolInformation`/`WorkspaceSymbol`-shaped item, which names its
/// file through `location`.
fn flat_symbol(item: &Value) -> Option<Symbol> {
    let location = item.get("location")?;
    let uri = location.get("uri").and_then(Value::as_str)?;
    let start = location.get("range").and_then(parse_range_start);
    Some(Symbol {
        name: string_field(item, "name").unwrap_or_default(),
        kind: parse_kind(item),
        detail: string_field(item, "detail"),
        container: string_field(item, "containerName"),
        site: site_of(uri, start),
        depth: 0,
    })
}

/// Appends a `DocumentSymbol` and its children; returns whether `item` itself
/// was readable.
/// Appends a `DocumentSymbol` and, unconditionally, its children.
///
/// The children are walked even when the parent itself is not readable. A
/// malformed header used to take its whole subtree with it and say nothing, so a
/// server that omitted one `range` silently deleted real symbols from the
/// answer; whatever is parseable is worth keeping, and whatever is not is
/// counted so the caller can say so out loud.
fn collect_document_symbol(item: &Value, depth: usize, list: &mut SymbolList) {
    if !push_document_symbol(item, depth, &mut list.symbols) {
        list.skipped += 1;
    }
    if depth + 1 < MAX_SYMBOL_DEPTH
        && let Some(children) = item.get("children").and_then(Value::as_array)
    {
        for child in children {
            collect_document_symbol(child, depth + 1, list);
        }
    }
}

fn push_document_symbol(item: &Value, depth: usize, out: &mut Vec<Symbol>) -> bool {
    let Some(name) = item.get("name").and_then(Value::as_str) else {
        return false;
    };
    // `selectionRange` names the symbol itself, `range` the whole declaration;
    // the name is the more useful answer to "where is this".
    let Some((line, character)) = item
        .get("selectionRange")
        .or_else(|| item.get("range"))
        .and_then(parse_range_start)
    else {
        return false;
    };
    out.push(Symbol {
        name: name.to_owned(),
        kind: parse_kind(item),
        detail: string_field(item, "detail"),
        container: None,
        site: Site {
            path: None,
            uri: String::new(),
            line: Some(line),
            character: Some(character),
        },
        depth,
    });
    true
}

/// A `null` answer and an empty list mean "none"; a non-empty list that decoded
/// to nothing means the answer was not what we asked for, and saying "none"
/// about it would be a lie.
fn as_items<'a>(value: &'a Value, expected: &str) -> Result<&'a [Value], ShapeError> {
    match value {
        Value::Null => Ok(&[]),
        Value::Array(items) => Ok(items.as_slice()),
        Value::Object(_) => Ok(std::slice::from_ref(value)),
        _ => Err(shape(format!("expected {expected}"))),
    }
}

fn guard_readable<T>(out: &[T], raw_len: usize, what: &str) -> Result<(), ShapeError> {
    if out.is_empty() && raw_len > 0 {
        return Err(shape(format!(
            "the answer listed {raw_len} entries but none was a readable {what}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn start(line: u32, character: u32) -> Value {
        json!({ "line": line, "character": character })
    }

    fn document_symbol(name: &str, kind: u32, line: u32, character: u32) -> Value {
        json!({
            "name": name,
            "kind": kind,
            "range": { "start": start(line, character), "end": start(line + 1, 0) },
            "selectionRange": { "start": start(line, character) },
        })
    }

    #[test]
    fn every_operation_round_trips_through_its_name() {
        for op in Operation::ALL {
            assert_eq!(Operation::parse(op.as_str()), Some(op));
        }
        assert_eq!(Operation::parse("nope"), None);
        assert_eq!(Operation::parse("GoToDefinition"), None);
        assert_eq!(Operation::ALL.len(), 10);
    }

    #[test]
    fn only_the_position_operations_demand_a_position() {
        for op in Operation::ALL {
            let expected = !matches!(
                op,
                Operation::DocumentSymbol | Operation::WorkspaceSymbol | Operation::Diagnostics
            );
            assert_eq!(op.needs_position(), expected, "{op:?}");
        }
        assert!(Operation::WorkspaceSymbol.needs_query());
        assert!(!Operation::Hover.needs_query());
    }

    #[test]
    fn methods_match_the_protocol() {
        assert_eq!(
            Operation::GoToDefinition.method(),
            Some("textDocument/definition")
        );
        assert_eq!(
            Operation::FindReferences.method(),
            Some("textDocument/references")
        );
        assert_eq!(Operation::Hover.method(), Some("textDocument/hover"));
        assert_eq!(
            Operation::DocumentSymbol.method(),
            Some("textDocument/documentSymbol")
        );
        assert_eq!(
            Operation::WorkspaceSymbol.method(),
            Some("workspace/symbol")
        );
        assert_eq!(
            Operation::GoToImplementation.method(),
            Some("textDocument/implementation")
        );
        assert_eq!(
            Operation::PrepareCallHierarchy.method(),
            Some("textDocument/prepareCallHierarchy")
        );
        assert_eq!(
            Operation::IncomingCalls.method(),
            Some("textDocument/prepareCallHierarchy")
        );
        assert_eq!(
            Operation::OutgoingCalls.method(),
            Some("textDocument/prepareCallHierarchy")
        );
        assert_eq!(Operation::Diagnostics.method(), None);

        assert_eq!(
            Operation::IncomingCalls.follow_up_method(),
            Some("callHierarchy/incomingCalls")
        );
        assert_eq!(
            Operation::OutgoingCalls.follow_up_method(),
            Some("callHierarchy/outgoingCalls")
        );
        assert_eq!(Operation::Hover.follow_up_method(), None);
    }

    #[test]
    fn params_carry_the_document_and_position() {
        let params = Operation::GoToDefinition.request_params("file:///a.rs", 4, 2, "");
        assert_eq!(
            params,
            json!({
                "textDocument": { "uri": "file:///a.rs" },
                "position": { "line": 4, "character": 2 }
            })
        );
    }

    #[test]
    fn references_ask_for_declarations_and_symbols_omit_the_position() {
        let params = Operation::FindReferences.request_params("file:///a.rs", 0, 0, "");
        assert_eq!(params["context"], json!({ "includeDeclaration": true }));

        let params = Operation::DocumentSymbol.request_params("file:///a.rs", 9, 9, "");
        assert!(params.get("position").is_none());

        let params = Operation::WorkspaceSymbol.request_params("file:///a.rs", 0, 0, "Config");
        assert_eq!(params, json!({ "query": "Config" }));
    }

    #[test]
    fn locations_read_null_singletons_arrays_and_links() {
        assert_eq!(locations(&Value::Null).unwrap(), Vec::new());
        assert_eq!(locations(&json!([])).unwrap(), Vec::new());

        let one = json!({
            "uri": "file:///ws/a.rs",
            "range": { "start": start(3, 7), "end": start(3, 9) }
        });
        let sites = locations(&one).unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(
            sites[0].path.as_deref(),
            Some(std::path::Path::new("/ws/a.rs"))
        );
        assert_eq!((sites[0].line, sites[0].character), (Some(3), Some(7)));

        let array = json!([one.clone(), one]);
        assert_eq!(locations(&array).unwrap().len(), 2);
    }

    #[test]
    fn location_links_prefer_the_selection_range() {
        let link = json!({
            "targetUri": "file:///ws/b.rs",
            "targetRange": { "start": start(10, 0) },
            "targetSelectionRange": { "start": start(12, 4) }
        });
        let sites = locations(&link).unwrap();
        assert_eq!(
            sites[0].path.as_deref(),
            Some(std::path::Path::new("/ws/b.rs"))
        );
        assert_eq!((sites[0].line, sites[0].character), (Some(12), Some(4)));
    }

    #[test]
    fn non_file_uris_are_kept_without_a_path() {
        let item = json!({
            "uri": "jdt://contents/java.lang/String.class",
            "range": { "start": start(1, 1) }
        });
        let sites = locations(&item).unwrap();
        assert_eq!(sites[0].path, None);
        assert_eq!(sites[0].uri, "jdt://contents/java.lang/String.class");
    }

    #[test]
    fn an_unreadable_answer_is_a_shape_error_not_an_empty_success() {
        assert!(locations(&json!([{ "range": { "start": start(1, 1) } }])).is_err());
        assert!(locations(&json!("nonsense")).is_err());
        assert!(locations(&json!([{ "nope": 1 }])).is_err());
    }

    #[test]
    fn hover_reads_every_content_shape() {
        let plain = hover(&json!({ "contents": "hello" })).unwrap().unwrap();
        assert_eq!(plain.text, "hello");
        assert!(plain.markdown);

        let markup = hover(&json!({
            "contents": { "kind": "markdown", "value": "**bold**" },
            "range": { "start": start(2, 1) }
        }))
        .unwrap()
        .unwrap();
        assert_eq!(markup.text, "**bold**");
        assert!(markup.markdown);
        assert_eq!(markup.range, Some((2, 1)));

        let array = hover(&json!({
            "contents": [ { "language": "rust", "value": "fn f()" }, "tail" ]
        }))
        .unwrap()
        .unwrap();
        // The language string is fenced (it is code, not prose), so it arrives
        // with its markers; the flattener removes them and leaves the code be.
        assert_eq!(array.text, "```rust\nfn f()\n```\n\ntail");

        let text_kind = hover(&json!({ "contents": { "kind": "plaintext", "value": "x" } }))
            .unwrap()
            .unwrap();
        assert!(!text_kind.markdown);

        assert_eq!(hover(&Value::Null).unwrap(), None);
        assert!(hover(&json!({})).is_err());
    }

    #[test]
    fn a_marked_string_with_a_language_is_code_not_prose() {
        // `MarkedString::LanguageString` has no `kind`; defaulting it to
        // markdown would strip the `**` that is the whole point of the sample.
        let code = hover(&json!({
            "contents": { "language": "python", "value": "def f(**kwargs): pass" }
        }))
        .unwrap()
        .unwrap();
        assert_eq!(code.text, "```python\ndef f(**kwargs): pass\n```");
        assert!(code.markdown, "fenced, so the flattener leaves it alone");

        // Mixed: the prose part is still flattened, the code part is not.
        let mixed = hover(&json!({
            "contents": [
                { "language": "sh", "value": "grep `foo` bar" },
                "**note**"
            ]
        }))
        .unwrap()
        .unwrap();
        assert!(mixed.text.contains("grep `foo` bar"));
        assert!(mixed.markdown);
    }

    #[test]
    fn document_symbols_nest_and_flatten_both_shapes() {
        let mut root = document_symbol("mod m", 2, 5, 4);
        root["children"] = json!([document_symbol("fn f", 12, 6, 3)]);
        let list = document_symbols(&json!([root])).unwrap();
        assert_eq!(list.skipped, 0);
        assert_eq!(list.symbols.len(), 2);
        assert_eq!(list.symbols[0].name, "mod m");
        assert_eq!(list.symbols[0].depth, 0);
        assert_eq!(list.symbols[0].site.line, Some(5), "selectionRange wins");
        assert_eq!(list.symbols[1].depth, 1);
        assert_eq!(list.symbols[1].kind, 12);

        let flat = json!([{
            "name": "f",
            "kind": 12,
            "location": {
                "uri": "file:///ws/a.rs",
                "range": { "start": start(5, 2) }
            }
        }]);
        let list = document_symbols(&flat).unwrap();
        assert_eq!(list.symbols.len(), 1);
        assert_eq!(list.symbols[0].depth, 0);
        assert_eq!(list.symbols[0].site.line, Some(5));
        assert_eq!(
            list.symbols[0].site.path.as_deref(),
            Some(std::path::Path::new("/ws/a.rs"))
        );
    }

    #[test]
    fn a_malformed_parent_keeps_its_readable_children_and_is_counted() {
        // The parent has a name but no range, so it cannot be listed; its
        // children are perfectly good symbols and dropping them would silently
        // delete real code from the outline.
        let answer = json!([{
            "name": "broken parent",
            "kind": 2,
            "children": [
                document_symbol("kept", 12, 3, 4),
                { "name": "also broken" },
                document_symbol("kept too", 12, 9, 1)
            ]
        }]);
        let list = document_symbols(&answer).unwrap();
        assert_eq!(list.skipped, 2, "the parent and the second child");
        let names: Vec<&str> = list.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["kept", "kept too"]);
        assert_eq!(list.symbols[0].depth, 1, "still nested under the parent");
    }

    #[test]
    fn document_symbol_recursion_is_bounded() {
        // A `children` chain far deeper than any real document must not recurse
        // the stack away.
        let mut node = document_symbol("leaf", 12, 0, 0);
        for _ in 0..(MAX_SYMBOL_DEPTH + 50) {
            let mut parent = document_symbol("n", 12, 0, 0);
            parent["children"] = json!([node]);
            node = parent;
        }
        let list = document_symbols(&json!([node])).unwrap();
        assert_eq!(list.symbols.len(), MAX_SYMBOL_DEPTH);
    }

    #[test]
    fn workspace_symbols_tolerate_a_missing_position() {
        let answer = json!([
            { "name": "Thing", "kind": 5, "location": { "uri": "file:///ws/a.rs" } },
            {
                "name": "Other",
                "kind": 12,
                "containerName": "mod m",
                "location": {
                    "uri": "file:///ws/b.rs",
                    "range": { "start": start(8, 4) }
                }
            }
        ]);
        let symbols = workspace_symbols(&answer).unwrap();
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].site.line, None);
        assert_eq!(
            symbols[0].site.path.as_deref(),
            Some(std::path::Path::new("/ws/a.rs"))
        );
        assert_eq!(symbols[1].container.as_deref(), Some("mod m"));
        assert_eq!(symbols[1].site.line, Some(8));
    }

    #[test]
    fn unreadable_symbol_lists_report_a_shape_error() {
        assert!(workspace_symbols(&json!([{ "nope": 1 }])).is_err());
        assert!(document_symbols(&json!([{ "nope": 1 }])).is_err());
        assert!(document_symbols(&json!([{ "name": "no range" }])).is_err());
        assert!(workspace_symbols(&json!(3)).is_err());
    }

    #[test]
    fn one_bad_location_does_not_discard_the_rest() {
        let mixed = json!([
            { "uri": "file:///w/a.rs", "range": { "start": { "line": 0, "character": 0 } } },
            { "range": { "start": { "line": 1, "character": 0 } } },
            { "uri": "file:///w/b.rs", "range": { "start": { "line": 2, "character": 0 } } }
        ]);
        let (sites, skipped) = locations_with_skipped(&mixed).unwrap();
        assert_eq!(sites.len(), 2);
        assert_eq!(skipped, 1);
        assert_eq!(locations(&mixed).unwrap().len(), 2);
    }
}
