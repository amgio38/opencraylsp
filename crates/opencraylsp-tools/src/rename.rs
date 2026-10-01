//! `lsp_rename_preview`: the whole-project edit a rename
//! would make, as a unified diff — and never a write.
//!
//! This is the one place in the crate that has to be certain about a negative.
//! A preview that quietly edited the disk would be the worst failure this tool
//! could have, so the module reads project files, applies the server's
//! `WorkspaceEdit` to a copy in memory, and prints a diff; there is no writer
//! in `src/` at all, and `tests/no_writes.rs` scans the tree to keep it that
//! way.
//!
//! Everything the server sends is treated as a proposal, not a fact: a file
//! outside the boundary is never read, an edit whose range would split a
//! character is refused rather than guessed at, and when the server is still
//! indexing the answer is honest failure instead of a diff that may be missing
//! half its hunks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use opencraylsp_core::backend::{LspBackend, LspError, PositionEncoding};
use opencraylsp_proto::ToolOutput;
use serde_json::{Map, Value, json};
use similar::TextDiff;
use tokio_util::sync::CancellationToken;

use crate::error::render_error;
use crate::format::{LineIndex, MAX_FILE_BYTES, View};
use crate::position;
use crate::resolve::{self, Candidate};
use crate::tools;

/// Most files one preview shows.
pub const MAX_FILES: usize = 50;
/// Most edits one preview applies, checked between files (a file is never
/// split, so the last file shown may push the total past the cap).
pub const MAX_EDITS: usize = 500;

/// Most edits accepted from **one** file.
///
/// `MAX_EDITS` is checked between files and `edits` only advances after a
/// successful preview, so the very first file passed the check whatever it
/// carried: a `documentChanges` array with a million edits for one file got a
/// million edits decoded, a million ranges allocated and a million-element
/// sort, with no cap applying at any point. A per-file ceiling is what makes
/// the total ceiling mean anything.
const MAX_FILE_EDITS: usize = MAX_EDITS;
/// Most bytes of diff text one preview prints, checked between files.
///
/// The budget covers the *whole* answer — diff text, resource lines and skip
/// lines — not just the diffs. Guarding one of the three made the cap advisory.
pub const MAX_DIFF_BYTES: usize = 64 * 1024;

/// How many skip lines are collected before the rest are only counted.
///
/// A second bound on the same list, because a byte budget alone does not bound
/// *work*: a workspace where every file is unreadable produces one skip line
/// per file, and a server that names a million of them would otherwise format a
/// million strings before the byte check ever ran.
const MAX_LISTED: usize = 200;

/// How long the diff for one file may run before `similar` gives up on it.
///
/// `similar`'s default deadline is none — its own documentation says a diff
/// "will take as long as it takes" — and Myers is O(N·D) in the number of lines
/// and the edit distance between them. A rename that rewrites a large file
/// wholesale therefore has no bound at all, and runs in the daemon that every
/// other connection shares. Five seconds is far longer than any diff a model can
/// read; past it the answer degrades to an approximation and says so.
pub const DIFF_DEADLINE: Duration = Duration::from_secs(5);
/// Longest `new_name` accepted: a name, not a paragraph.
const MAX_NAME_CHARS: usize = 200;
/// JSON-RPC `MethodNotFound`: the server does not implement the method.
const METHOD_NOT_FOUND: i64 = -32601;

/// Runs `lsp_rename_preview`: resolve, ask the server what it would rename, and
/// render the result as a diff.
pub async fn rename_preview(
    backend: &dyn LspBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> ToolOutput {
    let name = match new_name(args) {
        Ok(name) => name,
        Err(output) => return output,
    };
    let (candidate, notes) = match tools::locate(backend, args, cancel).await {
        Ok(found) => found,
        Err(output) => return output,
    };
    if name == candidate.name {
        return tools::invalid("new_name is identical to the current name");
    }
    let Ok(file) = tools::file_of(&candidate) else {
        return tools::invalid("this symbol lives outside the file system and cannot be renamed");
    };
    let boundary = backend.boundary();

    // `prepareRename` is optional: it turns "the server will refuse" into a
    // clean `[not_renamable]` instead of a raw `[rpc_error]`. A server that
    // does not implement it answers -32601, and the rename below is the real
    // answer anyway.
    match backend
        .request(
            &file,
            "textDocument/prepareRename",
            tools::position_params(&candidate, &file),
            cancel,
        )
        .await
    {
        Ok(served) if renamable(&served.value) => {}
        Ok(served) => {
            return not_renamable(&boundary, &candidate, served.encoding, &served.value);
        }
        Err(LspError::Rpc {
            code: METHOD_NOT_FOUND,
            ..
        }) => {}
        Err(error) => {
            return rename_failure(&boundary, &candidate, PositionEncoding::Utf16, &error);
        }
    }

    let mut params = tools::position_params(&candidate, &file);
    params["newName"] = json!(name);
    let served = match backend
        .request(&file, "textDocument/rename", params, cancel)
        .await
    {
        Ok(served) => served,
        // The encoding is only needed to print the position back, and a failed
        // request never told us which one the server used; the server's own
        // default is the honest stand-in here (a column off by one is the point
        // of this path, not something to paper over).
        Err(error) => {
            return rename_failure(&boundary, &candidate, PositionEncoding::Utf16, &error);
        }
    };
    if served.indexing.is_some() {
        // A rename computed while the index is still being built can miss
        // references, and a preview that silently misses them is worse than no
        // preview at all.
        return ToolOutput::error(format!(
            "[indexing] {} is still indexing; a rename now could miss references, so nothing is \
             previewed. Retry in a few seconds.",
            served.server
        ));
    }
    let edit = match WorkspaceEdit::parse(&served.value, &boundary) {
        Ok(edit) => edit,
        Err(detail) => return tools::shape_error("lsp_rename_preview", &detail),
    };
    if edit.is_empty() {
        return ToolOutput::ok("[not_found] rename produced no edits");
    }

    let lines = LineIndex::lazy(boundary.clone());
    let mut text = render(
        &boundary,
        &lines,
        &candidate.name,
        &name,
        served.encoding,
        edit,
        DIFF_DEADLINE,
    );
    for note in &notes {
        text.push('\n');
        text.push_str(note);
    }
    ToolOutput::ok(text)
}

/// Validates `new_name`: one identifier, no whitespace.
fn new_name(args: &Value) -> Result<String, ToolOutput> {
    let name = match args.get("new_name") {
        None | Some(Value::Null) => {
            return Err(tools::invalid("`lsp_rename_preview` needs `new_name`"));
        }
        Some(Value::String(value)) => value.trim(),
        Some(_) => return Err(tools::invalid("`new_name` must be a string")),
    };
    if name.is_empty() {
        return Err(tools::invalid("`lsp_rename_preview` needs `new_name`"));
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(tools::invalid(
            "`new_name` must not contain whitespace or control characters",
        ));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(tools::invalid(format!(
            "`new_name` must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_owned())
}

/// True when `prepareRename` said the position can be renamed (anything but
/// `null`, and not an empty object pretending to be a range).
fn renamable(value: &Value) -> bool {
    !value.is_null() && value.as_object().is_none_or(|object| !object.is_empty())
}

// ---- what a failed rename says ---------------------------------------------

/// JSON-RPC `InvalidParams`.
///
/// Sent by a server when the position in the request does not point at anything
/// it can work with — which, for a position-targeted tool, almost always means
/// the `column` is beside the identifier rather than on it. Servers phrase it
/// their own way (`No references found at position`, `cannot rename`, …), so the
/// code alone does not identify the case and the message alone does not either:
/// an `InvalidParams` that says `invalid params` is a real complaint about the
/// arguments, not a lost position.
const INVALID_PARAMS: i64 = -32602;

/// Message fragments that, together with [`INVALID_PARAMS`], mean "there is no
/// renameable symbol here" rather than "these arguments are wrong".
///
/// Matched case-insensitively: the wording is the server's, not ours, and the
/// same idea comes back in a handful of shapes.
const NO_SYMBOL_HERE: &[&str] = &[
    "no references found",
    "cannot be renamed",
    "cannot rename",
    "not renameable",
    "no symbol",
];

/// Turns a rename failure into the answer it deserves.
///
/// Anything that is not [`INVALID_PARAMS`] plus a "nothing here" message keeps
/// the raw `[rpc_error]`: reading a server complaint more generously than it was
/// written would hide the only detail an operator can act on.
fn rename_failure(
    boundary: &Path,
    candidate: &Candidate,
    encoding: PositionEncoding,
    error: &LspError,
) -> ToolOutput {
    if let LspError::Rpc { code, message, .. } = error
        && *code == INVALID_PARAMS
    {
        let lowered = message.to_lowercase();
        if NO_SYMBOL_HERE.iter().any(|hint| lowered.contains(hint)) {
            // The same hint every other position-targeted miss carries, from the
            // same function: a rename that finds nothing at the position is a
            // miss about a `column`, not a different kind of failure.
            let mut text = format!(
                "[not_renamable] there is no renameable symbol at {}: the language server found \
                 nothing there. Check that `column` points at the identifier, not at the space \
                 before it.",
                View {
                    boundary,
                    encoding,
                    max_results: usize::MAX,
                    subject: candidate.site.path.as_deref(),
                    lines: &LineIndex::lazy(boundary.to_path_buf()),
                }
                .position_of(&candidate.site)
            );
            let lines = LineIndex::lazy(boundary.to_path_buf());
            if let Some(hint) = crate::hint::hint_for(&candidate.site, &lines) {
                text.push('\n');
                text.push_str(&hint);
            }
            return ToolOutput::error(text);
        }
    }
    render_error(error)
}

pub(crate) fn not_renamable(
    boundary: &Path,
    candidate: &Candidate,
    encoding: PositionEncoding,
    value: &Value,
) -> ToolOutput {
    let reason = value.get("message").and_then(Value::as_str).unwrap_or(
        "the language server did not say why. It answered `null`, which means either nothing \
         renameable is at this position or the server does not implement rename (intelephense \
         does not)",
    );
    let lines = LineIndex::lazy(boundary.to_path_buf());
    let view = View {
        boundary,
        encoding,
        max_results: usize::MAX,
        subject: candidate.site.path.as_deref(),
        lines: &lines,
    };
    ToolOutput::error(format!(
        "[not_renamable] the symbol at {} cannot be renamed: {reason}",
        view.position_of(&candidate.site)
    ))
}

// ---- the workspace edit ---------------------------------------------------

/// A parsed `WorkspaceEdit`: the text edits to preview and the resource
/// operations to list.
#[derive(Debug, Default)]
struct WorkspaceEdit {
    files: Vec<FileEdit>,
    /// Index into `files` by URI, so merging two entries for one file does not
    /// scan the list. `documentChanges` is a server-controlled array and a
    /// 64 MiB frame holds on the order of a million and a half distinct-URI
    /// entries; the linear scan this replaces cost O(n²) `String` comparisons
    /// and never returned.
    by_uri: HashMap<String, usize>,
    /// Resource operations, already rendered: they are never previewed, only
    /// named, because there is no file content to diff.
    resources: Vec<String>,
}

/// The text edits for one file.
#[derive(Debug)]
struct FileEdit {
    uri: String,
    edits: Vec<TextEdit>,
    /// Edits the server sent beyond `MAX_FILE_EDITS`, which were not decoded.
    dropped: usize,
}

/// One `TextEdit`, in the server's own coordinates.
#[derive(Debug, Clone)]
struct TextEdit {
    start: (u32, u32),
    end: (u32, u32),
    new_text: String,
}

impl WorkspaceEdit {
    /// Parses `value`, which is `null` for "the server had nothing to change".
    ///
    /// `documentChanges` wins over `changes` when both are present: it is the
    /// form that can carry resource operations and versions, and LSP says a
    /// client that supports it should prefer it.
    fn parse(value: &Value, boundary: &Path) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        if !value.is_object() {
            return Err(format!(
                "a workspace edit must be an object, got {}",
                kind_of(value)
            ));
        }
        let mut edit = Self::default();
        if let Some(changes) = value.get("documentChanges") {
            let entries = changes
                .as_array()
                .ok_or_else(|| "`documentChanges` must be an array".to_owned())?;
            for entry in entries {
                edit.absorb(entry, boundary)?;
            }
        } else if let Some(changes) = value.get("changes") {
            let map = changes
                .as_object()
                .ok_or_else(|| "`changes` must map URIs to edit lists".to_owned())?;
            for (uri, edits) in map {
                let (edits, dropped) = read_edits(edits, "changes")?;
                edit.push(FileEdit {
                    uri: uri.clone(),
                    edits,
                    dropped,
                });
            }
        }
        Ok(edit)
    }

    /// One `documentChanges` entry: either a resource operation or a
    /// `TextDocumentEdit`.
    fn absorb(&mut self, entry: &Value, boundary: &Path) -> Result<(), String> {
        let object = entry.as_object().ok_or_else(|| {
            format!(
                "a document change must be an object, got {}",
                kind_of(entry)
            )
        })?;
        if let Some(kind) = object.get("kind").and_then(Value::as_str) {
            self.resources.push(resource_line(boundary, kind, object));
            return Ok(());
        }
        let uri = object
            .get("textDocument")
            .and_then(|document| document.get("uri"))
            .and_then(Value::as_str)
            .ok_or_else(|| "a document change has no `textDocument.uri`".to_owned())?;
        let edits = object
            .get("edits")
            .ok_or_else(|| "a document change has no `edits`".to_owned())?;
        let (edits, dropped) = read_edits(edits, "documentChanges")?;
        self.push(FileEdit {
            uri: uri.to_owned(),
            edits,
            dropped,
        });
        Ok(())
    }

    /// Adds a file's edits, merging into an earlier entry for the same URI so
    /// one file is one diff.
    fn push(&mut self, file: FileEdit) {
        match self.by_uri.get(&file.uri) {
            Some(&index) => {
                self.files[index].edits.extend(file.edits);
                self.files[index].dropped += file.dropped;
            }
            None => {
                self.by_uri.insert(file.uri.clone(), self.files.len());
                self.files.push(file);
            }
        }
    }

    /// True when the server proposed nothing at all.
    fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.files.iter().all(|file| file.edits.is_empty())
    }
}

/// The JSON type of a value, for an error a model can act on.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn read_edits(value: &Value, what: &str) -> Result<(Vec<TextEdit>, usize), String> {
    let array = value
        .as_array()
        .ok_or_else(|| format!("`{what}` edits must be an array, got {}", kind_of(value)))?;
    // Truncated rather than rejected: a rename that touches a few hundred more
    // places than the ceiling is still worth previewing, and the count of what
    // was dropped is printed with the diff. Refusing the whole file would throw
    // away a correct answer over a limit, which is the trade this crate makes
    // everywhere else.
    let edits = array
        .iter()
        .take(MAX_FILE_EDITS)
        .map(read_edit)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((edits, array.len().saturating_sub(MAX_FILE_EDITS)))
}

fn read_edit(edit: &Value) -> Result<TextEdit, String> {
    let range = edit
        .get("range")
        .ok_or_else(|| "a text edit has no `range`".to_owned())?;
    // `newText` is the model's own text; it is copied verbatim, so an empty one
    // (a deletion) is right when the server omitted it.
    let new_text = edit
        .get("newText")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(TextEdit {
        start: read_position(range.get("start"))?,
        end: read_position(range.get("end"))?,
        new_text,
    })
}

fn read_position(value: Option<&Value>) -> Result<(u32, u32), String> {
    let value = value.ok_or_else(|| "a text edit range has no `start` or `end`".to_owned())?;
    let line = value
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| "a range position has no `line`".to_owned())?;
    let character = value
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| "a range position has no `character`".to_owned())?;
    Ok((
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(character).unwrap_or(u32::MAX),
    ))
}

/// The line for a resource operation: named, never previewed.
fn resource_line(boundary: &Path, kind: &str, object: &Map<String, Value>) -> String {
    let uri = |key: &str| match object.get(key).and_then(Value::as_str) {
        Some(uri) => display_uri(boundary, uri),
        None => "<unknown>".to_owned(),
    };
    match kind {
        "create" => format!("resource operation (not previewed): create {}", uri("uri")),
        "rename" => format!(
            "resource operation (not previewed): rename {} -> {}",
            uri("oldUri"),
            uri("newUri")
        ),
        "delete" => format!("resource operation (not previewed): delete {}", uri("uri")),
        other => format!("resource operation (not previewed): {other}"),
    }
}

/// A file URI as the model reads paths, or the URI itself when it names no
/// file (a virtual document).
fn display_uri(boundary: &Path, uri: &str) -> String {
    match uri_path(uri) {
        Some(path) => resolve::display_path(boundary, &path),
        None => uri.to_owned(),
    }
}

fn uri_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

// ---- applying the edits in memory -----------------------------------------

/// Why a file was not previewed.
#[derive(Debug)]
enum Skip {
    /// The path is not inside the workspace boundary, so it was never read.
    Outside(String),
    /// The file could not be read.
    Unreadable(String, std::io::ErrorKind),
    /// The file is larger than [`MAX_FILE_BYTES`], so it was not read at all.
    TooLarge(String, u64),
    /// Two of the server's edits claim the same text.
    Overlapping(String),
}

impl Skip {
    fn line(&self) -> String {
        match self {
            Skip::Outside(path) => format!("skipped (outside workspace): {path}"),
            Skip::Unreadable(path, kind) => format!("skipped (unreadable): {path}: {kind:?}"),
            Skip::TooLarge(path, size) => format!(
                "skipped (too large to preview: {path} is {size} bytes, over the \
                 {MAX_FILE_BYTES}-byte limit)"
            ),
            Skip::Overlapping(path) => format!("skipped (overlapping edits): {path}"),
        }
    }
}

/// The diff for one file, or why it was not produced.
fn preview_file(
    boundary: &Path,
    lines: &LineIndex,
    file: &FileEdit,
    encoding: PositionEncoding,
    diff_deadline: Duration,
) -> Result<Option<(String, usize)>, Skip> {
    let Some(path) = uri_path(&file.uri) else {
        return Err(Skip::Outside(file.uri.clone()));
    };
    let display = resolve::display_path(boundary, &path);
    // `LineIndex` owns the boundary rules: a `..` that walks out, or a symlink
    // that points out, is refused in one place rather than two.
    let Some(real) = lines.inside(&path) else {
        return Err(Skip::Outside(display));
    };
    // The same cap `LineIndex` applies before it reads a line for a snippet,
    // and for the same reason: this reader lives in a long-lived daemon shared
    // by every connection, and one result site inside an enormous file would
    // otherwise pull the whole thing in and hold it for the length of the
    // request — to print a diff the model cannot read anyway. Checked on the
    // metadata, before a byte is read.
    match std::fs::metadata(&real) {
        Ok(meta) if meta.len() > MAX_FILE_BYTES => {
            return Err(Skip::TooLarge(display, meta.len()));
        }
        Ok(_) => {}
        // A file that is not there yet (a `create` resource operation, say) is
        // not an error here; `read_to_string` below reports it properly.
        Err(_) => {}
    }
    let text = match std::fs::read_to_string(&real) {
        Ok(text) => text,
        Err(error) => return Err(Skip::Unreadable(display, error.kind())),
    };
    let Some(updated) = apply(&text, &file.edits, encoding) else {
        return Err(Skip::Overlapping(display));
    };
    if updated == text {
        // The server proposed an edit that changes nothing; a diff would be
        // empty, and an empty diff is noise.
        return Ok(None);
    }
    // A deadline, because `similar`'s default is to take as long as it takes:
    // Myers is O(N·D) in the lines and the edit distance, so a server that
    // replaces a large file wholesale can wedge this task — and the daemon it
    // runs in — indefinitely. On expiry `similar` falls back to a coarser diff
    // rather than hanging, and the answer says so instead of quietly showing
    // something approximate as if it were exact.
    let started = std::time::Instant::now();
    let diff = TextDiff::configure()
        .timeout(diff_deadline)
        .diff_lines(text.as_str(), updated.as_str());
    // `similar` does not report whether it hit the deadline, so measure it: a
    // diff that finished well inside the budget is exact, and one that took at
    // least the budget bailed and is an approximation. Guessing from the result
    // instead — a zero similarity ratio looks identical to a legitimate one-line
    // rewrite, and labelling that "approximate" is worse than saying nothing.
    let cut_off = started.elapsed() >= diff_deadline;
    let mut rendered = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{display}"), &format!("b/{display}"))
        .to_string();
    if cut_off {
        rendered.push_str(&format!(
            "\n# (the diff for this file hit the {}s limit and is approximate; check it \
             before applying)\n",
            diff_deadline.as_secs_f32()
        ));
    }
    Ok(Some((rendered, file.edits.len())))
}

/// Applies `edits` to `text` in memory, back to front.
///
/// `None` means the proposed edits are not applicable — two of them overlapping
/// the same text, or a range whose start is past its end. That is a server
/// mistake, and guessing which edit "wins" would produce a diff of something
/// nobody asked for.
fn apply(text: &str, edits: &[TextEdit], encoding: PositionEncoding) -> Option<String> {
    let starts = line_starts(text);
    let mut ranges = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = offset(text, &starts, edit.start, encoding);
        let end = offset(text, &starts, edit.end, encoding);
        if start > end {
            return None;
        }
        ranges.push((start, end, edit.new_text.as_str()));
    }
    ranges.sort_by_key(|(start, _, _)| *start);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return None;
        }
    }
    let mut out = text.to_owned();
    for (start, end, new_text) in ranges.into_iter().rev() {
        out.replace_range(start..end, new_text);
    }
    Some(out)
}

/// The byte offset where each line starts.
///
/// `u32`, not `usize`: these are byte offsets into a string the caller has
/// already refused to read past [`MAX_FILE_BYTES`] (16 MiB), so four bytes per
/// line is not a limit but a fact — and one word per newline is pure overhead.
/// At `usize` a 16 MiB file of `"a\n"` produced an 8-million-entry vector,
/// 64 MB from a 16 MB input, and the doubling growth on the way there peaked
/// near 128 MB.
fn line_starts(text: &str) -> Vec<u32> {
    let mut starts = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(u32::try_from(index + 1).expect("a file under MAX_FILE_BYTES fits u32"));
        }
    }
    starts
}

/// The byte offset of a server position in `text`.
///
/// A line past the end of the file clamps to the end of the text (an edit there
/// is empty, not a panic), and the character is converted through
/// [`crate::position`] so a column on a line full of CJK lands where the server
/// meant.
fn offset(text: &str, starts: &[u32], position: (u32, u32), encoding: PositionEncoding) -> usize {
    let (line, character) = position;
    let line = usize::try_from(line).expect("a u32 line number fits usize on every target");
    let Some(&start) = starts.get(line) else {
        return text.len();
    };
    let start = usize::try_from(start).expect("a u32 offset fits usize on every target");
    let end = starts.get(line + 1).map_or(text.len(), |next| {
        usize::try_from(*next).expect("a u32 offset fits usize") - 1
    });
    let raw = &text[start..end];
    // The line's columns are its content; a trailing CR is not a column.
    let line_text = raw.strip_suffix('\r').unwrap_or(raw);
    let scalar = position::scalar_from_units(line_text, character, encoding) as usize;
    start
        + line_text
            .char_indices()
            .nth(scalar)
            .map(|(byte, _)| byte)
            .unwrap_or(line_text.len())
}

// ---- rendering ------------------------------------------------------------

/// The whole answer: a header, one unified diff per file, then what was not
/// previewed and what was left out.
fn render(
    boundary: &Path,
    lines: &LineIndex,
    old: &str,
    new: &str,
    encoding: PositionEncoding,
    edit: WorkspaceEdit,
    diff_deadline: Duration,
) -> String {
    let WorkspaceEdit {
        files,
        resources,
        by_uri: _,
    } = edit;
    let dropped_edits: usize = files.iter().map(|file| file.dropped).sum();
    let mut diffs = String::new();
    let mut skipped = Vec::new();
    let mut shown = 0usize;
    let mut edits = 0usize;
    // Files read, including ones that turned out to change nothing. The
    // `MAX_FILES` budget is about work done, so it is checked against this; the
    // header reports `shown`, which only counts files that really changed. A
    // skipped file advances neither — see the loop.
    let mut processed = 0usize;
    let mut skipped_files = 0usize;
    let mut hidden_files = 0usize;
    let mut hidden_edits = 0usize;
    // Counts the lists the byte budget also has to cover.
    let mut listed = 0usize;
    let mut unlisted_skips = 0usize;
    let mut omitted_lists = 0usize;

    for (index, file) in files.iter().enumerate() {
        // The budget counts files that were actually read, not files that
        // turned out to change nothing: a no-op file costs a read and an
        // `apply` like any other, and an edit set made entirely of them must
        // still be cut off rather than walked to the end.
        //
        // A *skipped* file deliberately does not count here. It produces no
        // diff and no read, and the byte budget depends on those being counted all the way
        // to the end of the edit set — the answer has to account for every
        // skipped file, not just the first fifty, so the byte budget is what
        // stops that list.
        if processed >= MAX_FILES
            || edits >= MAX_EDITS
            || out_bytes(&diffs, &resources, &skipped) >= MAX_DIFF_BYTES
        {
            for rest in &files[index..] {
                hidden_files += 1;
                hidden_edits += rest.edits.len();
            }
            break;
        }
        match preview_file(boundary, lines, file, encoding, diff_deadline) {
            Ok(Some((diff, count))) => {
                diffs.push_str(&diff);
                shown += 1;
                edits += count;
                processed += 1;
            }
            // A file the server proposed an edit for that changes nothing: it
            // has no diff to print, so it counts in neither header number. The header
            // reports `F files, E edits` as what the rename would really change,
            // and an unchanged file is not part of that.
            Ok(None) => processed += 1,
            Err(skip) => {
                skipped_files += 1;
                if listed < MAX_LISTED {
                    skipped.push(skip.line());
                    listed += 1;
                } else {
                    unlisted_skips += 1;
                }
            }
        }
    }

    let mut out = format!(
        "[rename preview] `{old}` -> `{new}`: {shown} files, {edits} edits (nothing was written)"
    );
    if !diffs.is_empty() {
        out.push('\n');
        out.push_str(&diffs);
    }
    // `MAX_DIFF_BYTES` used to guard `diffs` alone, while these two lists were
    // appended with no limit at all — so the cap could be walked straight past
    // by an edit set made of skipped files (which never advance `shown`, `edits`
    // or `diffs`, and so never trip the loop's guard) or by a pile of resource
    // operations. Both are counted against the same budget now.
    for line in resources.iter().chain(skipped.iter()) {
        if out.len() >= MAX_DIFF_BYTES {
            omitted_lists += 1;
            continue;
        }
        out.push('\n');
        out.push_str(line);
    }
    if hidden_files > 0 {
        out.push_str(&format!(
            "\n... {hidden_files} more file(s) not shown ({hidden_edits} edits)"
        ));
    }
    if omitted_lists > 0 {
        out.push_str(&format!(
            "\n... and {omitted_lists} more line(s) of file and resource detail not shown \
             (the {MAX_DIFF_BYTES}-byte answer limit was reached)"
        ));
    }
    if dropped_edits > 0 {
        out.push_str(&format!(
            "\nnote: {dropped_edits} edit(s) beyond the {MAX_FILE_EDITS}-per-file limit were not \
             decoded, so this preview is incomplete"
        ));
    }
    if skipped_files > 0 {
        out.push_str(&format!("\nnote: {skipped_files} file(s) skipped"));
        if unlisted_skips > 0 {
            out.push_str(&format!(
                "; {unlisted_skips} of those got no line of their own (over the {MAX_LISTED}-line \
                 list limit)"
            ));
        }
    }
    out
}

/// Every byte the answer is built from, not just the diff text.
fn out_bytes(diffs: &str, resources: &[String], skipped: &[String]) -> usize {
    diffs.len()
        + resources.iter().map(String::len).sum::<usize>()
        + skipped.iter().map(String::len).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use opencraylsp_core::backend::LanguageInfo;
    use opencraylsp_core::mock::MockBackend;
    use opencraylsp_proto::Indexing;
    use tempfile::TempDir;

    use crate::tools;

    fn backend(boundary: &Path) -> MockBackend {
        let backend = MockBackend::new(boundary.to_path_buf());
        backend.set_languages(vec![LanguageInfo {
            name: "rust".to_owned(),
            server: "rust-analyzer".to_owned(),
            extensions: vec!["rs".to_owned()],
            root_markers: vec!["Cargo.toml".to_owned()],
            installed: true,
            detected: true,
            enabled: true,
        }]);
        backend
    }

    /// A workspace holding `src/a.rs` with `content`.
    fn workspace_with(content: &str) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        let file = src.join("a.rs");
        fs::write(&file, content).expect("write fixture");
        (dir, file)
    }

    fn uri_of(path: &Path) -> String {
        url::Url::from_file_path(path)
            .expect("absolute path")
            .to_string()
    }

    /// Scripts `workspace/symbol` so `name` resolves to `path`.
    fn resolves_to(backend: &MockBackend, name: &str, path: &Path) {
        backend.respond(
            "workspace/symbol",
            Ok(json!([{
                "name": name,
                "kind": 12,
                "location": {
                    "uri": uri_of(path),
                    "range": { "start": { "line": 0, "character": 3 } }
                }
            }])),
        );
    }

    /// A `prepareRename` answer that says the position is renamable.
    fn renamable_answer(backend: &MockBackend) {
        backend.respond(
            "textDocument/prepareRename",
            Ok(json!({
                "range": { "start": { "line": 0, "character": 3 },
                           "end": { "line": 0, "character": 6 } },
                "placeholder": "old"
            })),
        );
    }

    fn text_edit(start_line: u32, start: u32, end_line: u32, end: u32, new_text: &str) -> Value {
        json!({
            "range": {
                "start": { "line": start_line, "character": start },
                "end": { "line": end_line, "character": end }
            },
            "newText": new_text
        })
    }

    fn changes(entries: &[(&str, Vec<Value>)]) -> Value {
        let mut map = serde_json::Map::new();
        for (uri, edits) in entries {
            map.insert((*uri).to_owned(), Value::Array(edits.clone()));
        }
        json!({ "changes": Value::Object(map) })
    }

    async fn preview(backend: &MockBackend, args: Value) -> ToolOutput {
        tools::call(
            backend,
            "lsp_rename_preview",
            args,
            &CancellationToken::new(),
        )
        .await
    }

    /// The common setup: a workspace, `old` resolved, and a renamable answer.
    fn ready(content: &str) -> (TempDir, PathBuf, MockBackend) {
        let (dir, file) = workspace_with(content);
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &file);
        renamable_answer(&backend);
        (dir, file, backend)
    }

    // ---- the diff ---------------------------------------------------------

    #[tokio::test]
    async fn a_single_edit_is_previewed_as_a_diff() {
        let (_dir, file, backend) = ready("fn old() {}\n");
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[(
                &uri_of(&file),
                vec![text_edit(0, 3, 0, 6, "new")],
            )])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            out.text,
            concat!(
                "[rename preview] `old` -> `new`: 1 files, 1 edits (nothing was written)\n",
                "--- a/src/a.rs\n",
                "+++ b/src/a.rs\n",
                "@@ -1 +1 @@\n",
                "-fn old() {}\n",
                "+fn new() {}\n",
            )
        );
    }

    /// P2: the header counts files that actually change.
    ///
    /// A `TextEdit` that replaces text with itself produces an empty diff, and
    /// an empty diff is dropped as noise — but the file was still counted in
    /// `shown`, so a two-file answer in which one file is untouched claimed
    /// "2 files" while printing one. The header reports `F files, E edits` as the
    /// files the rename would really change, so a no-op file belongs in neither
    /// count.
    #[tokio::test]
    async fn a_file_with_no_real_change_is_not_counted_in_the_header() {
        let (dir, file) = workspace_with("old\n");
        let untouched = dir.path().join("b.rs");
        fs::write(&untouched, "old\n").expect("write b");
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &file);
        renamable_answer(&backend);
        // b.rs gets an edit that changes "old" into "old": identical text.
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[
                (&uri_of(&file), vec![text_edit(0, 0, 0, 3, "new")]),
                (&uri_of(&untouched), vec![text_edit(0, 0, 0, 3, "old")]),
            ])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(!out.is_error, "{}", out.text);
        let header = out.text.lines().next().expect("a header line");
        assert!(
            header.contains("1 files, 1 edits"),
            "only the file that really changes may be counted: {header}"
        );
        assert!(
            !out.text.contains("b.rs"),
            "a file with no real change must not be previewed: {}",
            out.text
        );
    }

    /// And when *every* file is a no-op, the header must not claim one changed.
    #[tokio::test]
    async fn an_answer_where_nothing_changes_says_zero() {
        let (dir, file) = workspace_with("old\n");
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &file);
        renamable_answer(&backend);
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[(
                &uri_of(&file),
                vec![text_edit(0, 0, 0, 3, "old")],
            )])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(!out.is_error, "{}", out.text);
        let header = out.text.lines().next().expect("a header line");
        assert!(
            header.contains("0 files, 0 edits"),
            "a rename that changes nothing must say so: {header}"
        );
    }

    /// The `MAX_FILES` budget still bounds the work when files change nothing.
    ///
    /// A no-op file no longer advances the header, so the guard used to count
    /// them through `shown`; counting it through a separate counter is only
    /// correct if that counter actually bounds the loop. An edit set of a
    /// hundred thousand no-op files must still stop at `MAX_FILES`.
    #[tokio::test]
    async fn a_file_cap_applies_to_files_that_change_nothing() {
        let (dir, file) = workspace_with("old\n");
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &file);
        renamable_answer(&backend);
        // Every file's edit is a no-op, so nothing advances the header at all.
        let noop = dir.path().join("noop.rs");
        fs::write(&noop, "old\n").expect("write noop");
        let entries: Vec<(String, Vec<Value>)> = (0..MAX_FILES + 20)
            .map(|n| {
                let other = dir.path().join(format!("n{n}.rs"));
                fs::write(&other, "old\n").expect("write n");
                (uri_of(&other), vec![text_edit(0, 0, 0, 3, "old")])
            })
            .collect();
        let borrowed: Vec<(&str, Vec<Value>)> = entries
            .iter()
            .map(|(uri, edits)| (uri.as_str(), edits.clone()))
            .collect();
        backend.respond("textDocument/rename", Ok(changes(&borrowed)));
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(!out.is_error, "{}", out.text);
        let header = out.text.lines().next().expect("a header line");
        assert!(
            header.contains("0 files, 0 edits"),
            "nothing changes, so nothing may be reported: {header}"
        );
        assert!(
            out.text.contains("more file(s) not shown"),
            "the file cap must still stop the loop: {}",
            out.text.lines().nth(1).unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn several_edits_in_one_file_are_applied_back_to_front() {
        let (_dir, file, backend) = ready("a\nb\nc\n");
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[(
                &uri_of(&file),
                vec![text_edit(0, 0, 0, 1, "x"), text_edit(2, 0, 2, 1, "z")],
            )])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        // Both lines changed and the middle one did not: the offsets of the
        // later edit are still valid while the earlier one is applied.
        assert_eq!(
            out.text,
            concat!(
                "[rename preview] `old` -> `new`: 1 files, 2 edits (nothing was written)\n",
                "--- a/src/a.rs\n",
                "+++ b/src/a.rs\n",
                "@@ -1,3 +1,3 @@\n",
                "-a\n",
                "+x\n",
                " b\n",
                "-c\n",
                "+z\n",
            )
        );
    }

    #[tokio::test]
    async fn several_files_each_get_a_diff() {
        let (dir, file) = workspace_with("old\n");
        let second = dir.path().join("b.rs");
        fs::write(&second, "old\n").expect("write b");
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &file);
        renamable_answer(&backend);
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[
                (&uri_of(&file), vec![text_edit(0, 0, 0, 3, "new")]),
                (&uri_of(&second), vec![text_edit(0, 0, 0, 3, "new")]),
            ])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.starts_with(
                "[rename preview] `old` -> `new`: 2 files, 2 edits (nothing was written)\n"
            ),
            "{}",
            out.text
        );
        assert_eq!(out.text.matches("--- a/").count(), 2, "{}", out.text);
    }

    #[tokio::test]
    async fn an_edit_on_a_full_width_line_uses_the_negotiated_encoding() {
        const CJK: &str = "\u{65e5}\u{672c}\u{8a9e}";
        let (dir, file) = workspace_with(&format!("abc{CJK}def\n"));
        let backend = backend(dir.path()).with_encoding(PositionEncoding::Utf16);
        resolves_to(&backend, "old", &file);
        renamable_answer(&backend);
        // The three CJK scalars are UTF-16 units 3..6; read as bytes they would
        // start inside the first character.
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[(
                &uri_of(&file),
                vec![text_edit(0, 3, 0, 6, "X")],
            )])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(
            out.text.contains(&format!("-abc{CJK}def\n")),
            "{}",
            out.text
        );
        assert!(out.text.contains("+abcXdef\n"), "{}", out.text);
    }

    #[tokio::test]
    async fn document_changes_are_read_as_well_as_changes() {
        let (_dir, file, backend) = ready("old\n");
        backend.respond(
            "textDocument/rename",
            Ok(json!({ "documentChanges": [{
                "textDocument": { "uri": uri_of(&file), "version": 4 },
                "edits": [ text_edit(0, 0, 0, 3, "new") ]
            }]})),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert_eq!(
            out.text,
            concat!(
                "[rename preview] `old` -> `new`: 1 files, 1 edits (nothing was written)\n",
                "--- a/src/a.rs\n",
                "+++ b/src/a.rs\n",
                "@@ -1 +1 @@\n",
                "-old\n",
                "+new\n",
            )
        );
    }

    #[tokio::test]
    async fn resource_operations_are_named_but_not_previewed() {
        let (_dir, file, backend) = ready("old\n");
        let moved = file.with_file_name("b.rs");
        backend.respond(
            "textDocument/rename",
            Ok(json!({ "documentChanges": [
                {
                    "textDocument": { "uri": uri_of(&file) },
                    "edits": [ text_edit(0, 0, 0, 3, "new") ]
                },
                { "kind": "rename", "oldUri": uri_of(&file), "newUri": uri_of(&moved) }
            ]})),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(
            out.text
                .contains("resource operation (not previewed): rename src/a.rs -> src/b.rs"),
            "{}",
            out.text
        );
        assert!(out.text.contains("1 files, 1 edits"), "{}", out.text);
    }

    // ---- what is not previewed --------------------------------------------

    #[tokio::test]
    async fn overlapping_edits_skip_that_file_and_the_others_still_show() {
        let (dir, file) = workspace_with("hello\n");
        let second = dir.path().join("b.rs");
        fs::write(&second, "world\n").expect("write b");
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &file);
        renamable_answer(&backend);
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[
                (
                    &uri_of(&file),
                    vec![text_edit(0, 0, 0, 3, "a"), text_edit(0, 1, 0, 4, "b")],
                ),
                (&uri_of(&second), vec![text_edit(0, 0, 0, 5, "earth")]),
            ])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("skipped (overlapping edits): src/a.rs"),
            "{}",
            out.text
        );
        assert!(out.text.contains("note: 1 file(s) skipped"), "{}", out.text);
        assert!(out.text.contains("+++ b/b.rs"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_file_outside_the_workspace_is_never_read() {
        let (_dir, file, backend) = ready("old\n");
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[
                (&uri_of(&file), vec![text_edit(0, 0, 0, 3, "new")]),
                ("file:///etc/passwd", vec![text_edit(0, 0, 0, 1, "x")]),
            ])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(
            out.text
                .contains("skipped (outside workspace): /etc/passwd"),
            "{}",
            out.text
        );
        assert!(out.text.contains("1 files, 1 edits"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_read_says_why() {
        let (dir, file, backend) = ready("old\n");
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[
                (&uri_of(&file), vec![text_edit(0, 0, 0, 3, "new")]),
                (
                    &uri_of(&dir.path().join("gone.rs")),
                    vec![text_edit(0, 0, 0, 1, "x")],
                ),
            ])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(
            out.text.contains("skipped (unreadable): gone.rs: NotFound"),
            "{}",
            out.text
        );
    }

    // ---- failure semantics ------------------------------------------------

    #[tokio::test]
    async fn a_null_prepare_rename_is_not_renamable() {
        let (dir, file) = workspace_with("old\n");
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &file);
        backend.respond("textDocument/prepareRename", Ok(Value::Null));
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[not_renamable] "), "{}", out.text);
        assert!(out.text.contains("src/a.rs:1:4"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_server_without_prepare_rename_still_previews() {
        let (_dir, file, backend) = ready("old\n");
        backend.respond(
            "textDocument/prepareRename",
            Err(LspError::Rpc {
                server: "rust-analyzer".to_owned(),
                code: -32601,
                message: "method not found".to_owned(),
            }),
        );
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[(
                &uri_of(&file),
                vec![text_edit(0, 0, 0, 3, "new")],
            )])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("+new"), "{}", out.text);
    }

    /// The wrong column is the most common way a rename fails, and the server's
    /// own complaint ("No references found at position") says nothing the model
    /// can use. It has to come back as a rename-specific answer that names the
    /// position and shows what *is* on the line.
    #[tokio::test]
    async fn a_position_with_nothing_at_it_is_reported_as_not_renamable() {
        let (_dir, _file, backend) = ready("fn old() {}\n");
        backend.respond(
            "textDocument/rename",
            Err(LspError::Rpc {
                server: "rust-analyzer".to_owned(),
                code: -32602,
                message: "No references found at position".to_owned(),
            }),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[not_renamable] "), "{}", out.text);
        assert!(
            out.text
                .contains("Check that `column` points at the identifier"),
            "the message must name the likely cause: {}",
            out.text
        );
        assert!(
            out.text.contains("Identifiers on line 1: fn@1 old@4"),
            "the line's identifiers must come from the shared hint: {}",
            out.text
        );
    }

    /// A full-width prefix is where a byte- or code-unit-based column goes
    /// wrong, and this is the one answer in the crate that hands a column back
    /// for the model to paste. Counting bytes here would send it to the wrong
    /// character on any line holding CJK text.
    #[tokio::test]
    async fn the_hint_counts_scalars_on_a_line_with_full_width_text() {
        let (_dir, _file, backend) = ready("let 變數 = old;\n");
        backend.respond(
            "textDocument/rename",
            Err(LspError::Rpc {
                server: "rust-analyzer".to_owned(),
                code: -32602,
                message: "No references found at position".to_owned(),
            }),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(
            out.text
                .contains("Identifiers on line 1: let@1 變數@5 old@10"),
            "columns must be Unicode scalars, not bytes: {}",
            out.text
        );
    }

    /// The mapping needs both the code and the wording. `-32602` on its own is a
    /// real complaint about the arguments, and must keep the server's words.
    #[tokio::test]
    async fn invalid_params_without_that_wording_stays_an_rpc_error() {
        let (_dir, _file, backend) = ready("fn old() {}\n");
        backend.respond(
            "textDocument/rename",
            Err(LspError::Rpc {
                server: "rust-analyzer".to_owned(),
                code: -32602,
                message: "invalid params".to_owned(),
            }),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.text.starts_with("[rpc_error] "), "{}", out.text);
        assert!(out.text.contains("invalid params"), "{}", out.text);
    }

    /// Likewise the wording alone: a different code means the server refused for
    /// some other reason, and the reason is the useful part.
    #[tokio::test]
    async fn that_wording_under_another_code_stays_an_rpc_error() {
        let (_dir, _file, backend) = ready("fn old() {}\n");
        backend.respond(
            "textDocument/rename",
            Err(LspError::Rpc {
                server: "rust-analyzer".to_owned(),
                code: -32001,
                message: "No references found at position".to_owned(),
            }),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.text.starts_with("[rpc_error] "), "{}", out.text);
    }

    /// `prepareRename` refusing for the same reason is the same answer.
    #[tokio::test]
    async fn prepare_rename_finding_nothing_is_reported_as_not_renamable() {
        let (_dir, _file, backend) = ready("fn old() {}\n");
        backend.respond(
            "textDocument/prepareRename",
            Err(LspError::Rpc {
                server: "rust-analyzer".to_owned(),
                code: -32602,
                message: "cannot rename".to_owned(),
            }),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.text.starts_with("[not_renamable] "), "{}", out.text);
    }

    /// A line with nothing identifier-shaped on it still gets the position and
    /// the reason; the hint is an addition, never a precondition.
    #[tokio::test]
    async fn a_line_with_no_identifiers_still_explains_itself() {
        let (_dir, _file, backend) = ready("   \n");
        backend.respond(
            "textDocument/rename",
            Err(LspError::Rpc {
                server: "rust-analyzer".to_owned(),
                code: -32602,
                message: "No references found at position".to_owned(),
            }),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.text.starts_with("[not_renamable] "), "{}", out.text);
        assert!(!out.text.contains("line 1 has"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_rename_rpc_error_is_reported_verbatim() {
        let (_dir, _file, backend) = ready("old\n");
        backend.respond(
            "textDocument/rename",
            Err(LspError::Rpc {
                server: "rust-analyzer".to_owned(),
                code: -32602,
                message: "invalid params".to_owned(),
            }),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[rpc_error] "), "{}", out.text);
        assert!(out.text.contains("invalid params"), "{}", out.text);
    }

    #[tokio::test]
    async fn an_empty_edit_is_a_miss_not_an_error() {
        let (_dir, _file, backend) = ready("old\n");
        backend.respond("textDocument/rename", Ok(json!({ "changes": {} })));
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(!out.is_error);
        assert_eq!(out.text, "[not_found] rename produced no edits");
    }

    #[tokio::test]
    async fn an_indexing_server_previews_nothing() {
        let (_dir, _file, backend) = ready("old\n");
        backend.set_indexing(Some(Indexing {
            message: "Indexing".to_owned(),
            percent: Some(40),
        }));
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[(
                "file:///ws/a.rs",
                vec![text_edit(0, 0, 0, 3, "new")],
            )])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[indexing] "), "{}", out.text);
    }

    #[tokio::test]
    async fn a_cancelled_rename_is_cancelled() {
        let (_dir, _file, backend) = ready("old\n");
        backend.respond("textDocument/rename", Err(LspError::Cancelled));
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[cancelled]"), "{}", out.text);
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_a_shape_error() {
        let (_dir, _file, backend) = ready("old\n");
        backend.respond("textDocument/rename", Ok(json!([])));
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_response]"), "{}", out.text);
    }

    // ---- new_name ---------------------------------------------------------

    #[tokio::test]
    async fn a_missing_or_empty_new_name_is_rejected() {
        let (_dir, _file, backend) = ready("old\n");
        for args in [
            json!({ "symbol": "old" }),
            json!({ "symbol": "old", "new_name": "   " }),
            json!({ "symbol": "old", "new_name": null }),
        ] {
            let out = preview(&backend, args.clone()).await;
            assert!(out.is_error, "{args}");
            assert!(out.text.contains("`new_name`"), "{args} -> {}", out.text);
        }
    }

    #[tokio::test]
    async fn a_new_name_with_whitespace_or_controls_is_rejected() {
        let (_dir, _file, backend) = ready("old\n");
        for name in ["a b", "a\tb", "a\nb", "a\u{7}b"] {
            let out = preview(&backend, json!({ "symbol": "old", "new_name": name })).await;
            assert!(out.is_error, "{name}");
            assert!(
                out.text.contains("whitespace or control"),
                "{name} -> {}",
                out.text
            );
        }
    }

    #[tokio::test]
    async fn a_new_name_that_is_not_a_string_is_rejected() {
        let (_dir, _file, backend) = ready("old\n");
        let out = preview(&backend, json!({ "symbol": "old", "new_name": 7 })).await;
        assert!(out.is_error);
        assert!(out.text.contains("must be a string"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_too_long_new_name_is_rejected() {
        let (_dir, _file, backend) = ready("old\n");
        let long = "x".repeat(MAX_NAME_CHARS + 1);
        let out = preview(&backend, json!({ "symbol": "old", "new_name": long })).await;
        assert!(out.is_error);
        assert!(out.text.contains("at most 200"), "{}", out.text);
    }

    #[tokio::test]
    async fn the_same_name_is_rejected_before_asking_the_server() {
        let (_dir, file, backend) = ready("old\n");
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "old" })).await;
        assert!(out.is_error);
        assert!(
            out.text.contains("identical to the current name"),
            "{}",
            out.text
        );
        // And it was decided here, not by the server.
        let methods: Vec<String> = backend
            .calls()
            .into_iter()
            .map(|call| call.method)
            .collect();
        assert!(
            !methods.contains(&"textDocument/rename".to_owned()),
            "{methods:?}"
        );
        let _ = file;
    }

    #[tokio::test]
    async fn an_ambiguous_name_is_reported_as_ambiguous() {
        let (dir, file) = workspace_with("old\n");
        let backend = backend(dir.path());
        let second = dir.path().join("b.rs");
        fs::write(&second, "old\n").expect("write b");
        backend.respond(
            "workspace/symbol",
            Ok(json!([
                { "name": "old", "kind": 12, "location": { "uri": uri_of(&file), "range": { "start": { "line": 0, "character": 0 } } } },
                { "name": "old", "kind": 12, "location": { "uri": uri_of(&second), "range": { "start": { "line": 0, "character": 0 } } } }
            ])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.text.starts_with("[ambiguous]"), "{}", out.text);
    }

    // ---- caps -------------------------------------------------------------

    #[tokio::test]
    async fn at_most_fifty_files_are_shown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for index in 0..51 {
            let path = dir.path().join(format!("f{index:02}.rs"));
            fs::write(&path, "old\n").expect("write fixture");
            paths.push(path);
        }
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &paths[0]);
        renamable_answer(&backend);
        let entries: Vec<(String, Vec<Value>)> = paths
            .iter()
            .map(|path| (uri_of(path), vec![text_edit(0, 0, 0, 3, "new")]))
            .collect();
        let refs: Vec<(&str, Vec<Value>)> = entries
            .iter()
            .map(|(uri, edits)| (uri.as_str(), edits.clone()))
            .collect();
        backend.respond("textDocument/rename", Ok(changes(&refs)));
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(
            out.text.starts_with(
                "[rename preview] `old` -> `new`: 50 files, 50 edits (nothing was written)"
            ),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("... 1 more file(s) not shown (1 edits)"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn at_most_five_hundred_edits_are_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for index in 0..3 {
            let path = dir.path().join(format!("g{index}.rs"));
            let content: String = (0..260).map(|_| "a\n").collect();
            fs::write(&path, content).expect("write fixture");
            paths.push(path);
        }
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &paths[0]);
        renamable_answer(&backend);
        let big: Vec<Value> = (0..250)
            .map(|line| text_edit(line, 0, line, 1, "b"))
            .collect();
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[
                (&uri_of(&paths[0]), big.clone()),
                (&uri_of(&paths[1]), big),
                (&uri_of(&paths[2]), vec![text_edit(0, 0, 0, 1, "b")]),
            ])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(
            out.text.starts_with(
                "[rename preview] `old` -> `new`: 2 files, 500 edits (nothing was written)"
            ),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("... 1 more file(s) not shown (1 edits)"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn the_diff_output_is_capped_at_sixty_four_kilobytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("h0.rs");
        let second = dir.path().join("h1.rs");
        fs::write(&first, "xxxxxxxxxx\n").expect("write h0");
        fs::write(&second, "y\n").expect("write h1");
        let backend = backend(dir.path());
        resolves_to(&backend, "old", &first);
        renamable_answer(&backend);
        let huge = "z".repeat(40).to_string().repeat(2000);
        backend.respond(
            "textDocument/rename",
            Ok(changes(&[
                (&uri_of(&first), vec![text_edit(0, 0, 0, 10, &huge)]),
                (&uri_of(&second), vec![text_edit(0, 0, 0, 1, "q")]),
            ])),
        );
        let out = preview(&backend, json!({ "symbol": "old", "new_name": "new" })).await;
        assert!(out.text.contains("1 files, 1 edits"), "{}", out.text);
        assert!(
            out.text.contains("... 1 more file(s) not shown (1 edits)"),
            "{}",
            out.text
        );
        assert!(out.text.len() < MAX_DIFF_BYTES * 2, "{}", out.text.len());
    }

    // ---- the units --------------------------------------------------------

    #[test]
    fn applying_edits_needs_no_disk() {
        let edits = [TextEdit {
            start: (0, 0),
            end: (0, 1),
            new_text: "x".to_owned(),
        }];
        assert_eq!(
            apply("ab\n", &edits, PositionEncoding::Utf32),
            Some("xb\n".to_owned())
        );
    }

    #[test]
    fn an_edit_whose_start_is_past_its_end_is_refused() {
        let edits = [TextEdit {
            start: (0, 4),
            end: (0, 1),
            new_text: String::new(),
        }];
        assert_eq!(apply("abc\n", &edits, PositionEncoding::Utf32), None);
    }

    #[test]
    fn a_position_past_the_end_of_the_file_is_clamped() {
        let starts = line_starts("ab\n");
        assert_eq!(offset("ab\n", &starts, (9, 0), PositionEncoding::Utf32), 3);
        assert_eq!(offset("ab\n", &starts, (0, 99), PositionEncoding::Utf32), 2);
    }

    #[test]
    fn a_workspace_edit_must_be_an_object() {
        let error = WorkspaceEdit::parse(&json!(3), Path::new("/ws")).unwrap_err();
        assert!(error.contains("a number"), "{error}");
        let error = WorkspaceEdit::parse(&json!({ "changes": [] }), Path::new("/ws")).unwrap_err();
        assert!(error.contains("`changes`"), "{error}");
        let error = WorkspaceEdit::parse(
            &json!({ "documentChanges": [ { "textDocument": {} } ] }),
            Path::new("/ws"),
        )
        .unwrap_err();
        assert!(error.contains("textDocument.uri"), "{error}");
    }

    #[test]
    fn duplicate_document_changes_merge_into_one_file() {
        let edit = WorkspaceEdit::parse(
            &json!({ "documentChanges": [
                { "textDocument": { "uri": "file:///ws/a.rs" }, "edits": [] },
                { "textDocument": { "uri": "file:///ws/a.rs" }, "edits": [
                    text_edit(0, 0, 0, 1, "x")
                ] }
            ]}),
            Path::new("/ws"),
        )
        .expect("parseable");
        assert_eq!(edit.files.len(), 1);
        assert_eq!(edit.files[0].edits.len(), 1);
        assert!(!edit.is_empty());
    }

    // ---- the resource caps --------------------------------------------
    //
    // Each of these is a bound that a server response could previously walk past.
    // The tests below are written so that removing the bound fails them: a cap that
    // is only ever exercised by an input that also trips another guard is not proof
    // of anything, so each one drives exactly one path.

    /// A file over the size cap is skipped, not read.
    #[test]
    fn a_file_over_the_size_cap_is_skipped_rather_than_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("big.rs");
        // Sparse: one seek to the end, so the test does not write 16 MB to disk.
        let handle = fs::File::create(&file).expect("create");
        handle.set_len(MAX_FILE_BYTES + 1).expect("set_len");
        drop(handle);

        let lines = LineIndex::lazy(dir.path().to_path_buf());
        let file_edit = FileEdit {
            uri: resolve::file_uri(&file),
            dropped: 0,
            edits: vec![TextEdit {
                start: (0, 0),
                end: (0, 0),
                new_text: "x".to_owned(),
            }],
        };
        let outcome = preview_file(
            dir.path(),
            &lines,
            &file_edit,
            PositionEncoding::Utf32,
            DIFF_DEADLINE,
        );
        let Err(skip) = outcome else {
            panic!("a {}-byte file must not be diffed", MAX_FILE_BYTES + 1);
        };
        assert!(
            matches!(skip, Skip::TooLarge(_, size) if size == MAX_FILE_BYTES + 1),
            "{skip:?}"
        );
        assert!(
            skip.line().contains("too large"),
            "the reason must reach the reader: {}",
            skip.line()
        );
    }

    /// Edits dropped at the per-file ceiling are announced, not silent.
    #[test]
    fn dropped_edits_are_reported_beside_the_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "alpha\n").expect("write");
        let mut edit = WorkspaceEdit::default();
        edit.push(FileEdit {
            uri: resolve::file_uri(&file),
            dropped: 7,
            edits: vec![TextEdit {
                start: (0, 0),
                end: (0, 5),
                new_text: "beta".to_owned(),
            }],
        });
        let lines = LineIndex::lazy(dir.path().to_path_buf());
        let out = render(
            dir.path(),
            &lines,
            "alpha",
            "beta",
            PositionEncoding::Utf32,
            edit,
            DIFF_DEADLINE,
        );
        assert!(out.contains("7 edit(s) beyond"), "{out}");
    }

    /// A file just under the cap is still previewed, so the check is a ceiling and
    /// not a wall.
    #[test]
    fn a_file_under_the_size_cap_is_still_previewed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("ok.rs");
        fs::write(&file, "fn old() {}\n").expect("write");
        let lines = LineIndex::lazy(dir.path().to_path_buf());
        let file_edit = FileEdit {
            uri: resolve::file_uri(&file),
            dropped: 0,
            edits: vec![TextEdit {
                start: (0, 3),
                end: (0, 6),
                new_text: "new".to_owned(),
            }],
        };
        let outcome = preview_file(
            dir.path(),
            &lines,
            &file_edit,
            PositionEncoding::Utf32,
            DIFF_DEADLINE,
        )
        .expect("no skip");
        let (diff, edits) = outcome.expect("a diff");
        assert_eq!(edits, 1);
        assert!(diff.contains("-fn old()"), "{diff}");
    }

    /// The diff has a deadline, and a diff that hits it says so.
    #[test]
    fn a_diff_that_exhausts_its_deadline_is_marked_approximate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("big.rs");
        // Two long, wholly different files: Myers is O(N·D) here, so with the
        // deadline at zero the diff cannot possibly finish exactly.
        let before: String = (0..20_000).map(|n| format!("line {n} aaa\n")).collect();
        let after: String = (0..20_000).map(|n| format!("other {n} bbb\n")).collect();
        fs::write(&file, &before).expect("write");
        let lines = LineIndex::lazy(dir.path().to_path_buf());
        let file_edit = FileEdit {
            uri: resolve::file_uri(&file),
            dropped: 0,
            // One edit covering the whole file, which is what a rename of a
            // file-wide symbol looks like.
            edits: vec![TextEdit {
                start: (0, 0),
                end: (19_999, 12),
                new_text: after,
            }],
        };
        let started = std::time::Instant::now();
        let (diff, _) = preview_file(
            dir.path(),
            &lines,
            &file_edit,
            PositionEncoding::Utf32,
            // A deadline nothing can meet: this is the assertion.
            Duration::from_nanos(1),
        )
        .expect("no skip")
        .expect("a diff");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "the deadline did not bound the diff: {elapsed:?}"
        );
        assert!(
            diff.contains("approximate"),
            "a diff that ran out of time must say so: {diff}"
        );
    }

    /// A diff that finishes inside its budget is not labelled approximate — the
    /// note has to mean something or it is noise.
    #[test]
    fn a_diff_within_its_deadline_is_not_marked_approximate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("small.rs");
        fs::write(&file, "fn old() {}\n").expect("write");
        let lines = LineIndex::lazy(dir.path().to_path_buf());
        let file_edit = FileEdit {
            uri: resolve::file_uri(&file),
            dropped: 0,
            edits: vec![TextEdit {
                start: (0, 3),
                end: (0, 6),
                new_text: "new".to_owned(),
            }],
        };
        let (diff, _) = preview_file(
            dir.path(),
            &lines,
            &file_edit,
            PositionEncoding::Utf32,
            DIFF_DEADLINE,
        )
        .expect("no skip")
        .expect("a diff");
        assert!(!diff.contains("approximate"), "{diff}");
        assert!(diff.contains("-fn old()"), "{diff}");
    }

    /// `MAX_DIFF_BYTES` bounds the whole answer, not just the diff text.
    ///
    /// The route that used to walk past it: an edit set made entirely of *skipped*
    /// files. A skip advances none of `shown`, `edits` or `diffs`, so the loop's
    /// guard never tripped no matter how many there were.
    #[test]
    fn an_edit_set_of_only_skipped_files_cannot_exceed_the_answer_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Inside the boundary by name but absent, so every one of these is a skip
        // rather than a diff.
        let missing = dir.path().join("gone.rs");
        let files: Vec<FileEdit> = (0..2_000)
            .map(|n| FileEdit {
                uri: resolve::file_uri(&missing).replace("gone.rs", &format!("gone{n}.rs")),
                dropped: 0,
                edits: vec![TextEdit {
                    start: (0, 0),
                    end: (0, 0),
                    new_text: "x".to_owned(),
                }],
            })
            .collect();
        let lines = LineIndex::lazy(dir.path().to_path_buf());
        let out = render(
            dir.path(),
            &lines,
            "old",
            "new",
            PositionEncoding::Utf32,
            WorkspaceEdit {
                files,
                resources: Vec::new(),
                by_uri: Default::default(),
            },
            DIFF_DEADLINE,
        );
        assert!(
            out.len() <= MAX_DIFF_BYTES + 1024,
            "the answer reached {} bytes, over the {MAX_DIFF_BYTES} cap",
            out.len()
        );
        // Every skip is accounted for: the lines that were listed, and the count of
        // the ones that were not. A silently dropped file would read as a complete
        // answer, which is the one thing this crate may not do.
        assert!(out.contains("2000 file(s) skipped"), "{out}");
        assert!(
            out.contains("got no line of their own"),
            "the unlisted skips must be counted: {out}"
        );
    }

    /// The byte budget itself: resource lines are not free.
    #[test]
    fn resource_lines_are_counted_against_the_answer_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = LineIndex::lazy(dir.path().to_path_buf());
        // No files at all, so the diff text is empty and the byte budget can only
        // be reached through the resource list — the case the old guard missed,
        // because it only ever looked at `diffs`.
        let resources: Vec<String> = (0..4_000)
            .map(|n| format!("create {n}: {}", "p".repeat(64)))
            .collect();
        let out = render(
            dir.path(),
            &lines,
            "old",
            "new",
            PositionEncoding::Utf32,
            WorkspaceEdit {
                files: Vec::new(),
                resources,
                by_uri: Default::default(),
            },
            DIFF_DEADLINE,
        );
        assert!(
            out.len() <= MAX_DIFF_BYTES + 1024,
            "the answer reached {} bytes, over the {MAX_DIFF_BYTES} cap",
            out.len()
        );
        assert!(
            out.contains("not shown"),
            "the reader must be told lines were dropped: {out}"
        );
    }

    /// Merging edits for one URI is a map lookup, and it still merges.
    #[test]
    fn many_distinct_uris_are_merged_without_a_quadratic_scan() {
        let started = std::time::Instant::now();
        let mut edit = WorkspaceEdit::default();
        for n in 0..50_000 {
            edit.push(FileEdit {
                uri: format!("file:///ws/file{n}.rs"),
                dropped: 0,
                edits: vec![TextEdit {
                    start: (0, 0),
                    end: (0, 1),
                    new_text: "x".to_owned(),
                }],
            });
        }
        // 50 000 hash inserts take milliseconds; the linear scan this replaced
        // needed on the order of 1.25·10⁹ string comparisons (many seconds). The
        // bound sits between the two, wide enough not to flake on a loaded host.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "50 000 distinct URIs took {:?}",
            started.elapsed()
        );
        assert_eq!(edit.files.len(), 50_000);
        // And the merge behaviour is unchanged: same URI, one entry, both edits.
        let mut merged = WorkspaceEdit::default();
        merged.push(FileEdit {
            uri: "file:///ws/one.rs".to_owned(),
            dropped: 0,
            edits: vec![TextEdit {
                start: (0, 0),
                end: (0, 1),
                new_text: "a".to_owned(),
            }],
        });
        merged.push(FileEdit {
            uri: "file:///ws/one.rs".to_owned(),
            dropped: 0,
            edits: vec![TextEdit {
                start: (1, 0),
                end: (1, 1),
                new_text: "b".to_owned(),
            }],
        });
        assert_eq!(merged.files.len(), 1);
        assert_eq!(merged.files[0].edits.len(), 2);
    }

    /// One file's edits are bounded, not just the total.
    ///
    /// `MAX_EDITS` was checked between files against a counter that only advanced
    /// after a successful preview, so the first file always passed whatever it
    /// carried. A `documentChanges` array with a million edits for one file decoded
    /// a million edits, allocated a million ranges and sorted them, with no cap
    /// applying at any point.
    #[test]
    fn a_single_file_cannot_carry_unbounded_edits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "alpha\nbeta\n").expect("write");
        let edits: Vec<Value> = (0..MAX_FILE_EDITS + 500)
            .map(|n| {
                let line = u32::try_from(n % 2).expect("two lines");
                json!({
                    "range": {
                        "start": {"line": line, "character": 0},
                        "end": {"line": line, "character": 5}
                    },
                    "newText": "x"
                })
            })
            .collect();
        let (decoded, dropped) = read_edits(&json!(edits), "documentChanges").expect("decodes");
        assert_eq!(
            decoded.len(),
            MAX_FILE_EDITS,
            "a single file must not be allowed to exceed the edit ceiling"
        );
        assert_eq!(dropped, 500, "the dropped edits must be counted, not lost");
    }

    /// The line index is four bytes per line, not eight.
    ///
    /// At `usize` a 16 MiB file of `"a\n"` — 8 million lines — produced a 64 MB
    /// vector, and the doubling growth on the way there peaked near 128 MB, from a
    /// 16 MB input. The file size is already refused past `MAX_FILE_BYTES`, so four
    /// bytes per offset is not a limit but a fact.
    #[test]
    fn the_line_index_is_four_bytes_per_line() {
        let body = "a\n".repeat(1_000);
        let starts = line_starts(&body);
        assert_eq!(starts.len(), 1_001);
        assert_eq!(starts[0], 0);
        assert_eq!(starts[1], 2);
        // The type is the assertion: this is a `Vec<u32>`, so a file of 8 million
        // lines is 32 MB rather than 64.
        let element = std::mem::size_of_val(&starts[0]);
        assert_eq!(element, 4, "a byte offset does not need eight bytes");
        // And it is still correct at the far end.
        assert_eq!(
            usize::try_from(*starts.last().expect("at least one line")).expect("fits"),
            body.len()
        );
    }

    /// And the index still reaches the end of the largest file the preview will
    /// read, which is the only reason narrowing the offset to `u32` is safe.
    ///
    /// Written through `usize::try_from` so the assertion is about the *value*
    /// and not the element width — the width is
    /// `the_line_index_is_four_bytes_per_line`'s job, and a test that cannot
    /// compile against the old type is not a test.
    #[test]
    fn a_line_index_reaches_the_end_of_a_maximum_sized_file() {
        // The largest file `preview_file` will read, of the shortest possible lines.
        let body = "a\n".repeat((MAX_FILE_BYTES / 2) as usize);
        assert!(body.len() as u64 <= MAX_FILE_BYTES);
        let starts = line_starts(&body);
        let last = *starts.last().expect("at least one line");
        assert_eq!(
            usize::try_from(last).expect("the index holds a byte offset"),
            body.len()
        );
    }
}
