//! Walking a call hierarchy: who calls this, or what this calls, level by
//! level, without exploding.
//!
//! The shape of the problem is not the protocol — LSP's call hierarchy is two
//! requests per node — it is that call graphs have cycles, diamonds and
//! thousand-node fan-outs. So the walk is breadth-first, keeps a set of the
//! places it has already put in the tree, and stops at hard caps; anything it
//! did not expand says so in the output rather than quietly disappearing.
//!
//! The walk never guesses: the first request that comes back `Indexing` at the
//! root fails the whole call (the model should retry), and a failure deeper in
//! the tree is reported on that node so the rest of the answer still arrives.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use opencraylsp_core::backend::{LspBackend, LspError, Served};
use opencraylsp_proto::ToolOutput;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{error_code, render_error};
use crate::format::{Boundary, LineIndex, View};
use crate::operations::Site;
use crate::resolve::{self, Candidate};

/// Deepest level the tool follows (its `depth` argument accepts 1 to 3).
pub const MAX_DEPTH: u8 = 3;

/// Most children listed under one node.
pub const MAX_CHILDREN: usize = 50;

/// Most nodes in the whole tree, roots included.
pub const MAX_NODES: usize = 150;

/// Most requests in flight at once: same-level requests run in parallel, at
/// most eight together.
pub const MAX_CONCURRENCY: usize = 8;

/// `textDocument/prepareCallHierarchy`, the request that names the root.
const PREPARE: &str = "textDocument/prepareCallHierarchy";

/// Which way to walk the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Who calls this (`callHierarchy/incomingCalls`).
    Incoming,
    /// What this calls (`callHierarchy/outgoingCalls`).
    Outgoing,
}

impl Direction {
    /// The request that returns the neighbours at this direction.
    fn method(self) -> &'static str {
        match self {
            Direction::Incoming => "callHierarchy/incomingCalls",
            Direction::Outgoing => "callHierarchy/outgoingCalls",
        }
    }

    /// The key naming the *other* end of a call in the answer.
    fn end(self) -> &'static str {
        match self {
            Direction::Incoming => "from",
            Direction::Outgoing => "to",
        }
    }

    /// The arrow drawn in front of each non-root line.
    fn arrow(self) -> &'static str {
        match self {
            Direction::Incoming => "<-",
            Direction::Outgoing => "->",
        }
    }
}

/// Why a node did not expand further.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Its children are listed below it.
    Expanded,
    /// The same place already appeared higher up the tree (a cycle or a
    /// diamond), so it is shown but not expanded again.
    SeeAbove,
    /// The place is outside the workspace: shown, never asked about.
    OutsideWorkspace,
    /// The server failed to answer for this node; the code says how.
    Failed(String),
    /// The node budget ran out before this node could be asked about.
    Truncated,
}

/// One line of the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// The name the server reported for this place.
    pub name: String,
    /// Where the place is.
    pub site: Site,
    /// The first call site, if any, in the server's own coordinates.
    pub call_site: Option<(u32, u32)>,
    /// The file `call_site` belongs to. Always the caller's file: for incoming
    /// calls that is this node, for outgoing calls it is the node's parent.
    pub call_site_file: Option<PathBuf>,
    /// How many further call sites the server reported beyond the first.
    pub more_call_sites: usize,
    pub state: State,
    pub children: Vec<Node>,
    /// Children the per-node cap (or the node budget) stopped from being listed.
    pub children_omitted: usize,
}

/// A whole answer.
#[derive(Debug, Clone, PartialEq)]
pub struct CallTree {
    pub direction: Direction,
    /// The encoding every position in the tree is expressed in: whichever the
    /// server that answered the root speaks.
    pub encoding: opencraylsp_core::backend::PositionEncoding,
    pub roots: Vec<Node>,
    /// Caveats to print under the tree.
    pub notes: Vec<String>,
}

impl CallTree {
    /// The number of nodes shown, roots included.
    pub fn len(&self) -> usize {
        count(&self.roots)
    }

    /// True only before a tree is built: a returned tree always has a root.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

/// The number of nodes in a forest, roots included.
fn count(nodes: &[Node]) -> usize {
    nodes.iter().map(|node| 1 + count(&node.children)).sum()
}

/// Walks the hierarchy from `root`, `depth` levels deep.
///
/// `depth` above [`MAX_DEPTH`] is clamped and noted; a `depth` of zero is the
/// caller's business to reject. Returns the tree, or a ready-to-return
/// [`ToolOutput`] when the whole call fails.
pub async fn call_tree(
    backend: &dyn LspBackend,
    root: &Candidate,
    direction: Direction,
    depth: u8,
    cancel: &CancellationToken,
) -> Result<CallTree, ToolOutput> {
    let clamped = depth > MAX_DEPTH;
    let depth = depth.min(MAX_DEPTH);
    let Some(file) = root.site.path.clone() else {
        return Err(ToolOutput::error(
            "[invalid_args] this symbol lives outside the file system and cannot be queried",
        ));
    };

    let served = backend
        .request(&file, PREPARE, prepare_params(&file, root), cancel)
        .await
        .map_err(|error| render_error(&error))?;
    let items = served.value.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        // With the identifiers on the line, like every other position-targeted
        // miss: an empty call hierarchy is nearly always a `column` that landed
        // between the identifier and its neighbour.
        let lines = LineIndex::lazy(backend.boundary());
        return Err(crate::hint::not_found_at_position(
            &format!(
                "the language server returned no call-hierarchy item for this position in {}",
                served.server
            ),
            &root.site,
            &lines,
        ));
    }

    let mut walker = Walker::new(backend, direction, cancel);
    // Roots count towards `MAX_NODES` like everything else. The cap used to be
    // checked only in `expandable` and `place_children`, so it never applied
    // here: a `prepareCallHierarchy` answer with a hundred thousand items —
    // legal, and well inside the 64 MB frame the transport accepts — built a
    // hundred-thousand-entry arena before the first level was walked, while the
    // constant's own documentation claims it bounds "the whole tree, roots
    // included".
    let mut roots_shown = 0usize;
    let total_roots = items.len();
    for item in &items {
        if walker.arena.len() >= MAX_NODES {
            walker.truncated = true;
            break;
        }
        walker.push_root(item, &file);
        roots_shown += 1;
    }
    let roots_omitted = total_roots - roots_shown;
    if roots_omitted > 0 {
        tracing::debug!(
            shown = roots_shown,
            total = total_roots,
            "call hierarchy roots cut at the node budget"
        );
    }
    for _ in 0..depth {
        if walker.frontier.is_empty() {
            break;
        }
        walker.expand_level().await?;
    }

    let mut notes = Vec::new();
    if clamped {
        notes.push(format!("note: depth clamped to {MAX_DEPTH}"));
    }
    if walker.truncated {
        // A cut with no number is only half an answer: 151 roots and 15 000 both
        // read as "truncated at 150 nodes", so the reader cannot tell whether the
        // answer was nearly complete or mostly missing. When roots were dropped
        // the note names both counts; a budget cut deeper in the tree has no
        // root count to report and keeps the plain wording.
        let note = match roots_omitted {
            0 => format!("note: truncated at {MAX_NODES} nodes"),
            omitted => format!(
                "note: truncated at {MAX_NODES} nodes ({total_roots} root(s) found, {roots_shown} shown, {omitted} not shown)"
            ),
        };
        notes.push(note);
    }
    if walker.failed > 0 {
        notes.push(format!(
            "note: {} node(s) could not be expanded",
            walker.failed
        ));
    }
    if let Some(indexing) = &served.indexing {
        // Same wording as `with_markers`: a non-empty answer may still be
        // incomplete while the server works.
        notes.push(format!(
            "note: {} is still indexing{}; results may be incomplete.",
            served.server,
            match indexing.percent {
                Some(percent) => format!(" ({percent}%)"),
                None => String::new(),
            }
        ));
    }

    Ok(CallTree {
        direction,
        encoding: served.encoding,
        roots: walker.finish(),
        notes,
    })
}

/// The `prepareCallHierarchy` parameters for the resolved position.
fn prepare_params(file: &Path, root: &Candidate) -> Value {
    json!({
        "textDocument": { "uri": resolve::file_uri(file) },
        "position": {
            "line": root.site.line.unwrap_or(0),
            "character": root.site.character.unwrap_or(0)
        }
    })
}

/// The `[cancelled]` output: a cancelled walk returns no half-tree.
fn cancelled() -> ToolOutput {
    ToolOutput::error("[cancelled] the LSP request was cancelled")
}

/// One node while the walk is still running.
#[derive(Debug)]
struct Placed {
    name: String,
    site: Site,
    call_site: Option<(u32, u32)>,
    call_site_file: Option<PathBuf>,
    more_call_sites: usize,
    state: State,
    parent: Option<usize>,
    children: Vec<usize>,
    children_omitted: usize,
    /// The raw item the server sent, echoed back in the next request.
    item: Option<Value>,
    level: u8,
}

/// The walk's mutable state.
struct Walker<'a> {
    backend: &'a dyn LspBackend,
    direction: Direction,
    cancel: &'a CancellationToken,
    /// The one place that decides whether a file is inside the workspace.
    ///
    /// Every URI the server sends is checked through this, so a node can only
    /// be expanded — which is what makes the daemon read the file — once
    /// [`Boundary::inside`] has resolved it. Built once per walk; unlike a
    /// [`LineIndex`] it holds no file cache, which is also what keeps it
    /// `Sync`: [`Self::ask`] shares `&self` across the futures it joins.
    boundary: Boundary,
    arena: Vec<Placed>,
    /// The places already put in the tree, keyed by `(uri, selectionRange.start)`.
    seen: HashMap<String, ()>,
    /// The nodes to expand on the next level.
    frontier: Vec<usize>,
    /// A node was left unexpanded because the node budget ran out.
    truncated: bool,
    /// How many nodes the server failed to answer about.
    failed: usize,
}

impl<'a> Walker<'a> {
    fn new(
        backend: &'a dyn LspBackend,
        direction: Direction,
        cancel: &'a CancellationToken,
    ) -> Self {
        Self {
            backend,
            direction,
            cancel,
            boundary: Boundary::new(backend.boundary()),
            arena: Vec::new(),
            seen: HashMap::new(),
            frontier: Vec::new(),
            truncated: false,
            failed: 0,
        }
    }

    /// Whether a server-reported item names a place outside the workspace.
    ///
    /// The check is [`LineIndex::inside`] and nothing else: it collapses `.`
    /// and `..` *and* follows symlinks, so neither `file:///ws/../../etc/passwd`
    /// nor a symlink inside the workspace pointing out of it can pass for a
    /// file the tool is allowed to open. A bare `Path::starts_with` cannot do
    /// this — `ParentDir` is just another component, so `/ws/../../etc/passwd`
    /// "starts with" `/ws` — and a percent-encoded `..` survives the URI parser
    /// as a literal `..`, which is why this goes through the one helper that
    /// was written for the job.
    ///
    /// A URI the OS cannot open counts as outside: there is no file to read, so
    /// there is nothing to expand.
    fn is_outside(&self, item: &Value) -> bool {
        item_path(item).is_none_or(|path| self.boundary.inside(&path).is_none())
    }

    /// Places a root. Roots are always shown — the model asked about them — and
    /// claim their place so a cycle back to one is a `see above`.
    ///
    /// A root is boundary-checked exactly like any other node. It is almost
    /// always inside (it describes the position the request was about), but the
    /// server chooses the URI, so a root that points out of the workspace is
    /// shown and marked rather than expanded.
    fn push_root(&mut self, item: &Value, source_file: &Path) {
        if let Some(key) = site_key(item) {
            self.seen.insert(key, ());
        }
        let outside = self.is_outside(item);
        self.arena.push(Placed {
            name: item_name(item),
            site: item_site(item),
            call_site: None,
            call_site_file: Some(source_file.to_path_buf()),
            more_call_sites: 0,
            state: if outside {
                State::OutsideWorkspace
            } else {
                State::Expanded
            },
            parent: None,
            children: Vec::new(),
            children_omitted: 0,
            item: Some(item.clone()),
            level: 0,
        });
        let id = self.arena.len() - 1;
        if matches!(self.arena[id].state, State::Expanded) {
            self.frontier.push(id);
        }
    }

    /// Asks about every node on the current frontier, at most
    /// [`MAX_CONCURRENCY`] at a time, and places what comes back.
    ///
    /// `Err` means the request was cancelled: the caller returns no tree at all.
    async fn expand_level(&mut self) -> Result<(), ToolOutput> {
        let parents = std::mem::take(&mut self.frontier);
        let mut next = Vec::new();
        for chunk in parents.chunks(MAX_CONCURRENCY) {
            let batch = self.expandable(chunk);
            if batch.is_empty() {
                continue;
            }
            let outcomes = self.ask(&batch).await;
            if self.cancel.is_cancelled()
                || outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, Err(LspError::Cancelled)))
            {
                return Err(cancelled());
            }
            for (parent, outcome) in batch.into_iter().zip(outcomes) {
                self.place_children(parent, outcome, &mut next);
            }
        }
        self.frontier = next;
        Ok(())
    }

    /// The chunk's parents that can still be asked about: those whose place is
    /// inside the workspace and that the node budget has not swallowed.
    fn expandable(&mut self, chunk: &[usize]) -> Vec<usize> {
        let mut batch = Vec::new();
        for &parent in chunk {
            if !matches!(self.arena[parent].state, State::Expanded) {
                continue;
            }
            if self.arena.len() >= MAX_NODES {
                self.arena[parent].state = State::Truncated;
                self.truncated = true;
                continue;
            }
            if self.arena[parent].site.path.is_none() {
                // A node with no readable file got `OutsideWorkspace` when it
                // was placed, so this is only reachable if that rule changes;
                // report it rather than send a request with a bogus path.
                self.arena[parent].state = State::Failed("outside_workspace".to_owned());
                self.failed += 1;
                continue;
            }
            batch.push(parent);
        }
        batch
    }

    /// Sends one request per parent, up to [`MAX_CONCURRENCY`] at once.
    async fn ask(&self, batch: &[usize]) -> Vec<Result<Served, LspError>> {
        let query = |slot: usize| -> Query<'_> {
            if slot >= batch.len() {
                return Box::pin(async { None });
            }
            let parent = batch[slot];
            let params = json!({ "item": self.arena[parent].item.clone().unwrap_or(Value::Null) });
            let file = self.arena[parent].site.path.clone().unwrap_or_default();
            Box::pin(async move {
                Some(
                    self.backend
                        .request(&file, self.direction.method(), params, self.cancel)
                        .await,
                )
            })
        };
        let (a, b, c, d, e, f, g, h) = tokio::join!(
            query(0),
            query(1),
            query(2),
            query(3),
            query(4),
            query(5),
            query(6),
            query(7)
        );
        [a, b, c, d, e, f, g, h]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    }

    /// Places the children one answer names under `parent`.
    fn place_children(
        &mut self,
        parent: usize,
        outcome: Result<Served, LspError>,
        next: &mut Vec<usize>,
    ) {
        let served = match outcome {
            Ok(served) => served,
            Err(error) => {
                self.arena[parent].state = State::Failed(error_code(&error).to_owned());
                self.failed += 1;
                return;
            }
        };
        let end = self.direction.end();
        let parent_file = self.arena[parent].site.path.clone();
        // A shape we cannot read is not an empty answer. The old
        // `unwrap_or_default()` turned a server that replied with an object
        // instead of an array into "this node has no callers", and the model
        // would act on that — the one reading of the answer the tool exists to
        // prevent.
        // `null` is the spec's own spelling of "no callers": the response type
        // is `CallHierarchy*Call[] | null`. Only a shape that is neither is a
        // failure.
        let no_entries = Vec::new();
        let entries = if served.value.is_null() {
            &no_entries
        } else if let Some(entries) = served.value.as_array() {
            entries
        } else {
            self.arena[parent].state = State::Failed("invalid_response".to_owned());
            self.failed += 1;
            return;
        };
        let mut listed = 0usize;
        for entry in entries {
            let Some(item) = entry.get(end) else { continue };
            if listed >= MAX_CHILDREN || self.arena.len() >= MAX_NODES {
                self.arena[parent].children_omitted += 1;
                if self.arena.len() >= MAX_NODES {
                    self.truncated = true;
                }
                continue;
            }
            let key = site_key(item);
            let fresh = key.as_ref().is_none_or(|key| !self.seen.contains_key(key));
            if let Some(key) = &key {
                self.seen.insert(key.clone(), ());
            }
            let outside = self.is_outside(item);
            let (call_site, more) = call_site_of(entry);
            let id = self.arena.len();
            self.arena.push(Placed {
                name: item_name(item),
                site: item_site(item),
                call_site,
                call_site_file: match self.direction {
                    // `fromRanges` are always relative to the caller. For
                    // incoming calls the caller is this item; for outgoing
                    // calls it is the node the request was about (the parent).
                    Direction::Incoming => item_path(item),
                    Direction::Outgoing => parent_file.clone(),
                },
                more_call_sites: more,
                state: if !fresh {
                    State::SeeAbove
                } else if outside {
                    State::OutsideWorkspace
                } else {
                    State::Expanded
                },
                parent: Some(parent),
                children: Vec::new(),
                children_omitted: 0,
                item: Some(item.clone()),
                level: self.arena[parent].level + 1,
            });
            self.arena[parent].children.push(id);
            listed += 1;
            if matches!(self.arena[id].state, State::Expanded) {
                next.push(id);
            }
        }
    }

    /// Assembles the immutable tree.
    fn finish(self) -> Vec<Node> {
        fn build(arena: &[Placed], id: usize) -> Node {
            let node = &arena[id];
            Node {
                name: node.name.clone(),
                site: node.site.clone(),
                call_site: node.call_site,
                call_site_file: node.call_site_file.clone(),
                more_call_sites: node.more_call_sites,
                state: node.state.clone(),
                children: node
                    .children
                    .iter()
                    .map(|child| build(arena, *child))
                    .collect(),
                children_omitted: node.children_omitted,
            }
        }
        let roots: Vec<usize> = self
            .arena
            .iter()
            .enumerate()
            .filter(|(_, node)| node.parent.is_none())
            .map(|(index, _)| index)
            .collect();
        roots.iter().map(|id| build(&self.arena, *id)).collect()
    }
}

/// The uniform future type a chunk is joined from.
type Query<'a> = Pin<Box<dyn Future<Output = Option<Result<Served, LspError>>> + Send + 'a>>;

// ---- reading one item -----------------------------------------------------

fn item_name(item: &Value) -> String {
    item.get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A server-reported coordinate, or `None` when it is not one.
///
/// `as u64 as u32` looks harmless and is not: `"line": 4294967295` survives the
/// cast intact, and everything downstream then does `line + 1`. In a release
/// build that wraps to 0 and the model is sent to line 1 of the wrong file; in
/// a build with overflow checks — every `cargo test`, and any `cargo build
/// --release` that sets it — it panics. Rejecting the value instead means a
/// nonsense coordinate is dropped, and the node is reported without a call site
/// rather than with a plausible wrong one.
fn coordinate(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok()
}

/// The file an item names, when its URI is one the OS can open.
fn item_path(item: &Value) -> Option<PathBuf> {
    item.get("uri")
        .and_then(Value::as_str)
        .and_then(|uri| url::Url::parse(uri).ok())
        .and_then(|url| url.to_file_path().ok())
}

fn item_site(item: &Value) -> Site {
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let start = item
        .get("selectionRange")
        .or_else(|| item.get("range"))
        .and_then(|range| range.get("start"));
    Site {
        path: item_path(item),
        uri,
        line: start
            .and_then(|start| start.get("line"))
            .and_then(coordinate),
        character: start
            .and_then(|start| start.get("character"))
            .and_then(coordinate),
    }
}

/// The deduplication key: one place in one file, identified by the item's uri
/// plus the start of its selection range.
fn site_key(item: &Value) -> Option<String> {
    let uri = item.get("uri").and_then(Value::as_str)?;
    let start = item
        .get("selectionRange")
        .or_else(|| item.get("range"))
        .and_then(|range| range.get("start"))?;
    let line = start.get("line").and_then(Value::as_u64)?;
    let character = start.get("character").and_then(Value::as_u64)?;
    Some(format!("{uri}:{line}:{character}"))
}

/// The first call site, and how many more there were.
fn call_site_of(entry: &Value) -> (Option<(u32, u32)>, usize) {
    let ranges = entry
        .get("fromRanges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sites: Vec<(u32, u32)> = ranges
        .iter()
        .filter_map(|range| {
            let start = range.get("start")?;
            Some((
                coordinate(start.get("line")?)?,
                coordinate(start.get("character")?)?,
            ))
        })
        .collect();
    match sites.split_first() {
        Some((first, rest)) => (Some(*first), rest.len()),
        None => (None, 0),
    }
}

// ---- rendering ------------------------------------------------------------

/// The tree as the model reads it: one entry per line, children indented under
/// their parent.
pub fn render(tree: &CallTree, view: &View<'_>) -> String {
    let arrow = tree.direction.arrow();
    let mut out = String::new();
    for (index, root) in tree.roots.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&node_line(root, view, 0, arrow));
        render_children(
            &root.children,
            root.children_omitted,
            view,
            1,
            arrow,
            &mut out,
        );
    }
    for note in &tree.notes {
        out.push('\n');
        out.push_str(note);
    }
    out
}

fn render_children(
    children: &[Node],
    omitted: usize,
    view: &View<'_>,
    level: usize,
    arrow: &str,
    out: &mut String,
) {
    for child in children {
        out.push('\n');
        out.push_str(&node_line(child, view, level, arrow));
        render_children(
            &child.children,
            child.children_omitted,
            view,
            level + 1,
            arrow,
            out,
        );
    }
    if omitted > 0 {
        out.push('\n');
        out.push_str(&indent(level));
        out.push_str(&format!("... and {omitted} more (not expanded)"));
    }
}

/// The leading whitespace for a nesting `level`: the root flush left, each
/// level one arrow's width in (`->` is two columns, so 2 then 5, and so on).
fn indent(level: usize) -> String {
    match level {
        0 => String::new(),
        _ => format!("  {}", "   ".repeat(level - 1)),
    }
}

fn node_line(node: &Node, view: &View<'_>, level: usize, arrow: &str) -> String {
    let mut line = indent(level);
    if level > 0 {
        line.push_str(arrow);
        line.push(' ');
    }
    line.push_str(&node.name);
    line.push_str("  ");
    line.push_str(&view.position_of(&node.site));
    if let Some(suffix) = suffix(node, view) {
        line.push_str(&suffix);
    }
    line
}

fn suffix(node: &Node, view: &View<'_>) -> Option<String> {
    match &node.state {
        State::Expanded => node.call_site.map(|site| call_site_text(node, site, view)),
        State::SeeAbove => Some("  (see above)".to_owned()),
        State::OutsideWorkspace => Some("  (outside workspace, not expanded)".to_owned()),
        State::Failed(code) => Some(format!("  (failed: {code})")),
        State::Truncated => Some("  (not expanded)".to_owned()),
    }
}

fn call_site_text(node: &Node, site: (u32, u32), view: &View<'_>) -> String {
    let position = match &node.call_site_file {
        Some(path) => view.position_of(&Site {
            path: Some(path.clone()),
            uri: String::new(),
            line: Some(site.0),
            character: Some(site.1),
        }),
        // `saturating_add`, not `+ 1`: a coordinate the server sent as
        // `u32::MAX` is representable, and adding one to it is not.
        None => format!("{}:{}", site.0.saturating_add(1), site.1.saturating_add(1)),
    };
    if node.more_call_sites == 0 {
        format!("  (call at {position})")
    } else {
        format!(
            "  (call at {position}, +{} call sites)",
            node.more_call_sites
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use opencraylsp_core::backend::{DiagnosticsReport, LanguageInfo, PositionEncoding};
    use opencraylsp_proto::{DaemonInfo, LanguageMode, Limits, StatusReport};

    use crate::format::LineIndex;

    // ---- the scripted backend --------------------------------------------

    /// What the backend answers when a node is expanded.
    #[derive(Clone)]
    enum Reply {
        /// Answer with this call list right away.
        Calls(Vec<Value>),
        /// Wait `ms`, then answer with this call list.
        Slow(u64, Vec<Value>),
        /// Wait `ms`, then fail — a real delay, not an instant error.
        Fail(u64, LspError),
        /// Answer with exactly this value, valid or not.
        Raw(Value),
    }

    /// A scripted [`LspBackend`] for call graphs.
    ///
    /// `MockBackend` answers one value per *method*, which cannot express a
    /// graph: every `incomingCalls` request would get the same answer. This one
    /// keys the answer on the node being expanded, records how many requests
    /// were ever in flight at once (so a test can prove the concurrency cap),
    /// and can cancel the token mid-walk. It refuses paths outside the boundary
    /// exactly as the real backend does — a looser double is how a suite goes
    /// green on a broken feature.
    struct GraphBackend {
        boundary: PathBuf,
        cancel: CancellationToken,
        prepare: Reply,
        replies: Mutex<HashMap<String, Reply>>,
        expanding: Mutex<usize>,
        cancel_after: Mutex<Option<usize>>,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        methods: Mutex<Vec<String>>,
    }

    impl GraphBackend {
        fn new(cancel: &CancellationToken) -> Self {
            Self {
                boundary: PathBuf::from("/ws"),
                cancel: cancel.clone(),
                prepare: Reply::Calls(vec![item("file:///ws/a.rs", 0, "alpha")]),
                replies: Mutex::new(HashMap::new()),
                expanding: Mutex::new(0),
                cancel_after: Mutex::new(None),
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                methods: Mutex::new(Vec::new()),
            }
        }

        /// The items `prepareCallHierarchy` returns.
        fn root(&mut self, items: Vec<Value>) {
            self.prepare = Reply::Calls(items);
        }

        /// Roots the boundary somewhere else, for the tests that need a real
        /// directory to resolve symlinks against.
        fn set_boundary(&mut self, boundary: PathBuf) {
            self.boundary = boundary;
        }

        /// Scripts the answer to `prepareCallHierarchy` itself.
        fn prepare(&mut self, reply: Reply) {
            self.prepare = reply;
        }

        /// Scripts the answer for one node (by its `name`).
        fn node(&mut self, name: &str, reply: Reply) {
            self.replies
                .lock()
                .expect("lock")
                .insert(name.to_owned(), reply);
        }

        /// Scripts a raw reply for one node, for answers that are not the shape
        /// the protocol promises.
        fn raw_calls(&mut self, name: &str, value: Value) {
            self.node(name, Reply::Raw(value));
        }

        /// Scripts an immediate call list for one node.
        fn calls(&mut self, name: &str, items: Vec<Value>) {
            self.node(name, Reply::Calls(items));
        }

        /// Cancels the token when the `n`th expansion request arrives.
        fn cancel_after(&self, nth: usize) {
            *self.cancel_after.lock().expect("lock") = Some(nth);
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }

        /// How many expansion requests (not `prepareCallHierarchy`) were sent.
        fn expansions(&self) -> usize {
            self.methods
                .lock()
                .expect("lock")
                .iter()
                .filter(|method| method.as_str() != PREPARE)
                .count()
        }

        fn served(&self, value: Value) -> Served {
            Served {
                value,
                encoding: PositionEncoding::Utf32,
                server: "fake-ls".to_owned(),
                root: self.boundary.clone(),
                indexing: None,
            }
        }

        async fn answer(&self, reply: Reply) -> Result<Served, LspError> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let result = match reply {
                Reply::Calls(items) => Ok(self.served(json!(items))),
                Reply::Slow(ms, items) => {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(self.served(json!(items)))
                }
                Reply::Fail(ms, error) => {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Err(error)
                }
                Reply::Raw(value) => Ok(self.served(value)),
            };
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    #[async_trait::async_trait]
    impl LspBackend for GraphBackend {
        async fn request(
            &self,
            file: &Path,
            method: &str,
            params: Value,
            cancel: &CancellationToken,
        ) -> Result<Served, LspError> {
            self.methods.lock().expect("lock").push(method.to_owned());
            if cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            let _ = file;
            if method == PREPARE {
                let reply = self.prepare.clone();
                return self.answer(reply).await;
            }
            let nth = {
                let mut count = self.expanding.lock().expect("lock");
                *count += 1;
                *count
            };
            if *self.cancel_after.lock().expect("lock") == Some(nth) {
                self.cancel.cancel();
                return Ok(self.served(json!([])));
            }
            let name = params
                .get("item")
                .and_then(|item| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let reply = self
                .replies
                .lock()
                .expect("lock")
                .get(&name)
                .cloned()
                .unwrap_or(Reply::Calls(Vec::new()));
            self.answer(reply).await
        }

        async fn request_workspace(
            &self,
            _server: &str,
            _method: &str,
            _params: Value,
            cancel: &CancellationToken,
        ) -> Result<Served, LspError> {
            if cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            Ok(self.served(Value::Null))
        }

        async fn diagnostics(
            &self,
            _file: &Path,
            cancel: &CancellationToken,
        ) -> Result<DiagnosticsReport, LspError> {
            if cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            Ok(DiagnosticsReport {
                items: Vec::new(),
                encoding: PositionEncoding::Utf32,
                received_for_version: false,
                timed_out: true,
                server: "fake-ls".to_owned(),
            })
        }

        fn resolve_path(&self, file_path: &str) -> Result<PathBuf, LspError> {
            let joined = if Path::new(file_path).is_absolute() {
                PathBuf::from(file_path)
            } else {
                self.boundary.join(file_path)
            };
            let mut normalized = PathBuf::new();
            for part in joined.components() {
                match part {
                    std::path::Component::ParentDir => {
                        normalized.pop();
                    }
                    std::path::Component::CurDir => {}
                    other => normalized.push(other),
                }
            }
            if normalized.starts_with(&self.boundary) {
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
            vec![LanguageInfo {
                name: "rust".to_owned(),
                server: "fake-ls".to_owned(),
                extensions: vec!["rs".to_owned()],
                root_markers: vec!["Cargo.toml".to_owned()],
                installed: true,
                detected: true,
                enabled: true,
            }]
        }

        async fn status(&self) -> StatusReport {
            StatusReport {
                daemon: DaemonInfo {
                    version: "test".to_owned(),
                    pid: 0,
                    uptime_secs: 0,
                    rss_bytes: None,
                    clients: 0,
                    max_rss_mb: None,
                    rss_over_limit: false,
                },
                limits: Limits {
                    max_instances: 8,
                    max_rss_mb: 4096,
                    idle_shutdown_secs: 900,
                    max_open_docs: 256,
                },
                enabled_languages: vec!["rust".to_owned()],
                language_mode: LanguageMode::Auto,
                not_installed: Vec::new(),
                instances: Vec::new(),
            }
        }

        async fn shutdown(&self) {}
    }

    // ---- helpers ----------------------------------------------------------

    fn candidate() -> Candidate {
        Candidate {
            site: Site {
                path: Some(PathBuf::from("/ws/a.rs")),
                uri: "file:///ws/a.rs".to_owned(),
                line: Some(0),
                character: Some(3),
            },
            name: "alpha".to_owned(),
            kind: 12,
            container: None,
            server: "fake-ls".to_owned(),
            outside_workspace: false,
        }
    }

    async fn walk(
        backend: &GraphBackend,
        direction: Direction,
        depth: u8,
        cancel: &CancellationToken,
    ) -> Result<CallTree, ToolOutput> {
        call_tree(backend, &candidate(), direction, depth, cancel).await
    }

    /// [`candidate`] at a real file, for the boundary tests: the whole point of
    /// those is what the file system says about a path, so the root has to be
    /// somewhere that exists.
    fn candidate_in(path: &Path) -> Candidate {
        Candidate {
            site: Site {
                path: Some(path.to_path_buf()),
                uri: resolve::file_uri(path),
                line: Some(0),
                character: Some(3),
            },
            name: "alpha".to_owned(),
            kind: 12,
            container: None,
            server: "fake-ls".to_owned(),
            outside_workspace: false,
        }
    }

    async fn walk_from(
        backend: &GraphBackend,
        root: &Candidate,
        direction: Direction,
        depth: u8,
        cancel: &CancellationToken,
    ) -> Result<CallTree, ToolOutput> {
        call_tree(backend, root, direction, depth, cancel).await
    }

    fn render_tree(tree: &CallTree) -> String {
        let lines = LineIndex::lazy(PathBuf::from("/ws"));
        let view = View {
            boundary: Path::new("/ws"),
            encoding: tree.encoding,
            max_results: usize::MAX,
            subject: None,
            lines: &lines,
        };
        render(tree, &view)
    }

    /// A `callHierarchy` item at `uri`, naming `name`.
    fn item(uri: &str, line: u32, name: &str) -> Value {
        json!({
            "name": name,
            "kind": 12,
            "uri": uri,
            "range": { "start": { "line": line, "character": 0 } },
            "selectionRange": { "start": { "line": line, "character": 3 } },
            "data": { "opaque": true }
        })
    }

    fn ranges(sites: &[(u32, u32)]) -> Value {
        Value::Array(
            sites
                .iter()
                .map(|(line, character)| {
                    json!({
                        "start": { "line": line, "character": character },
                        "end": { "line": line, "character": character + 1 }
                    })
                })
                .collect(),
        )
    }

    fn incoming(from: Value, sites: &[(u32, u32)]) -> Value {
        json!({ "from": from, "fromRanges": ranges(sites) })
    }

    fn outgoing(to: Value, sites: &[(u32, u32)]) -> Value {
        json!({ "to": to, "fromRanges": ranges(sites) })
    }

    fn timeout() -> LspError {
        LspError::Timeout {
            server: "fake-ls".to_owned(),
            method: "callHierarchy/incomingCalls".to_owned(),
            ms: 30000,
        }
    }

    // ---- the walk ---------------------------------------------------------

    #[tokio::test]
    async fn a_linear_chain_walks_as_deep_as_asked() {
        for (depth, expected) in [(1u8, 2usize), (2, 3), (3, 3)] {
            let cancel = CancellationToken::new();
            let mut backend = GraphBackend::new(&cancel);
            backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
            backend.calls(
                "alpha",
                vec![incoming(item("file:///ws/b.rs", 10, "beta"), &[(10, 4)])],
            );
            backend.calls(
                "beta",
                vec![incoming(item("file:///ws/c.rs", 20, "gamma"), &[(20, 4)])],
            );
            backend.calls("gamma", Vec::new());
            let tree = walk(&backend, Direction::Incoming, depth, &cancel)
                .await
                .expect("walk");
            assert_eq!(tree.len(), expected, "depth {depth}");
        }
    }

    #[tokio::test]
    async fn the_second_level_renders_children_indented_under_their_parent() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![incoming(item("file:///ws/b.rs", 10, "beta"), &[(10, 4)])],
        );
        backend.calls(
            "beta",
            vec![incoming(item("file:///ws/c.rs", 20, "gamma"), &[(20, 4)])],
        );
        backend.calls("gamma", Vec::new());
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        assert_eq!(
            render_tree(&tree),
            "alpha  a.rs:1:4\n  <- beta  b.rs:11:4  (call at b.rs:11:5)\n     <- gamma  c.rs:21:4  (call at c.rs:21:5)"
        );
    }

    #[tokio::test]
    async fn self_recursion_is_shown_once_then_marked() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![incoming(item("file:///ws/a.rs", 0, "alpha"), &[(0, 0)])],
        );
        let tree = walk(&backend, Direction::Incoming, 3, &cancel)
            .await
            .expect("walk");
        assert_eq!(tree.len(), 2);
        assert_eq!(
            render_tree(&tree),
            "alpha  a.rs:1:4\n  <- alpha  a.rs:1:4  (see above)"
        );
    }

    #[tokio::test]
    async fn mutual_recursion_terminates() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![incoming(item("file:///ws/b.rs", 10, "beta"), &[(10, 4)])],
        );
        backend.calls(
            "beta",
            vec![incoming(item("file:///ws/a.rs", 0, "alpha"), &[(0, 0)])],
        );
        let tree = walk(&backend, Direction::Incoming, 3, &cancel)
            .await
            .expect("walk");
        assert_eq!(tree.len(), 3);
        assert_eq!(render_tree(&tree).matches("(see above)").count(), 1);
    }

    #[tokio::test]
    async fn a_diamond_expands_a_shared_node_only_once() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![
                incoming(item("file:///ws/b.rs", 10, "beta"), &[(10, 4)]),
                incoming(item("file:///ws/c.rs", 20, "gamma"), &[(20, 4)]),
            ],
        );
        backend.calls(
            "beta",
            vec![incoming(item("file:///ws/d.rs", 30, "delta"), &[(30, 4)])],
        );
        backend.calls(
            "gamma",
            vec![incoming(item("file:///ws/d.rs", 30, "delta"), &[(30, 4)])],
        );
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        // alpha, beta, gamma, delta, delta(see above)
        assert_eq!(tree.len(), 5);
        let text = render_tree(&tree);
        assert_eq!(text.matches("delta").count(), 2);
        assert_eq!(text.matches("(see above)").count(), 1);
        // The shared node is asked about once: alpha, beta and gamma alone.
        assert_eq!(backend.expansions(), 3);
    }

    #[tokio::test]
    async fn one_node_lists_at_most_fifty_children() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        let children: Vec<Value> = (0..60)
            .map(|index| {
                incoming(
                    item(
                        &format!("file:///ws/n{index}.rs"),
                        index,
                        &format!("n{index}"),
                    ),
                    &[(index, 0)],
                )
            })
            .collect();
        backend.calls("alpha", children);
        for index in 0..60 {
            backend.calls(&format!("n{index}"), Vec::new());
        }
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        assert_eq!(tree.len(), 1 + MAX_CHILDREN);
        assert!(
            render_tree(&tree).contains("... and 10 more (not expanded)"),
            "{}",
            render_tree(&tree)
        );
    }

    #[tokio::test]
    async fn the_whole_tree_stops_at_the_node_budget() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        let level_one: Vec<Value> = (0..50)
            .map(|index| {
                incoming(
                    item(
                        &format!("file:///ws/m{index}.rs"),
                        index,
                        &format!("m{index}"),
                    ),
                    &[(index, 0)],
                )
            })
            .collect();
        backend.calls("alpha", level_one);
        for index in 0..50 {
            let leaves: Vec<Value> = (0..50)
                .map(|leaf| {
                    incoming(
                        item(
                            &format!("file:///ws/m{index}/l{leaf}.rs"),
                            leaf,
                            &format!("m{index}l{leaf}"),
                        ),
                        &[(leaf, 0)],
                    )
                })
                .collect();
            backend.calls(&format!("m{index}"), leaves);
        }
        let tree = walk(&backend, Direction::Incoming, 3, &cancel)
            .await
            .expect("walk");
        assert_eq!(tree.len(), MAX_NODES);
        assert!(
            render_tree(&tree).contains("note: truncated at 150 nodes"),
            "{}",
            render_tree(&tree)
        );
    }

    #[tokio::test]
    async fn a_depth_past_the_cap_is_clamped_and_noted() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls("alpha", Vec::new());
        let clamped = walk(&backend, Direction::Incoming, 9, &cancel)
            .await
            .expect("walk");
        assert!(render_tree(&clamped).contains("note: depth clamped to 3"));

        let exact = walk(&backend, Direction::Incoming, 3, &cancel)
            .await
            .expect("walk");
        assert!(!render_tree(&exact).contains("clamped"));
    }

    #[tokio::test]
    async fn a_failing_deep_node_is_marked_and_the_rest_survives() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![
                incoming(item("file:///ws/b.rs", 10, "beta"), &[(10, 4)]),
                incoming(item("file:///ws/c.rs", 20, "gamma"), &[(20, 4)]),
            ],
        );
        backend.node("beta", Reply::Fail(5, timeout()));
        backend.calls("gamma", Vec::new());
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        let text = render_tree(&tree);
        assert!(text.contains("(failed: timeout)"), "{text}");
        assert!(
            text.contains("note: 1 node(s) could not be expanded"),
            "{text}"
        );
        assert!(text.contains("gamma"), "{text}");
    }

    #[tokio::test]
    async fn works_when_every_deep_node_fails() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![
                incoming(item("file:///ws/b.rs", 10, "beta"), &[(10, 4)]),
                incoming(item("file:///ws/c.rs", 20, "gamma"), &[(20, 4)]),
            ],
        );
        backend.node("beta", Reply::Fail(2, timeout()));
        backend.node("gamma", Reply::Fail(2, timeout()));
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        assert_eq!(tree.len(), 3);
        assert!(
            render_tree(&tree).contains("note: 2 node(s) could not be expanded"),
            "{}",
            render_tree(&tree)
        );
    }

    #[tokio::test]
    async fn a_deep_indexing_node_is_a_failed_node_not_a_failed_call() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![incoming(item("file:///ws/b.rs", 10, "beta"), &[(10, 4)])],
        );
        backend.node(
            "beta",
            Reply::Fail(
                0,
                LspError::Indexing {
                    server: "fake-ls".to_owned(),
                    message: "roots scanned".to_owned(),
                    percent: Some(10),
                },
            ),
        );
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        assert!(render_tree(&tree).contains("(failed: indexing)"));
    }

    #[tokio::test]
    async fn a_node_outside_the_workspace_is_shown_but_not_asked_about() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![incoming(
                item("file:///elsewhere/x.rs", 3, "outer"),
                &[(3, 0)],
            )],
        );
        let tree = walk(&backend, Direction::Incoming, 3, &cancel)
            .await
            .expect("walk");
        let text = render_tree(&tree);
        assert!(text.contains("(outside workspace, not expanded)"), "{text}");
        assert_eq!(backend.expansions(), 1);
    }

    #[tokio::test]
    async fn several_prepare_items_become_several_roots() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![
            item("file:///ws/a.rs", 0, "alpha"),
            item("file:///ws/b.rs", 5, "beta"),
        ]);
        backend.calls(
            "alpha",
            vec![incoming(item("file:///ws/c.rs", 10, "gamma"), &[(10, 4)])],
        );
        backend.calls(
            "beta",
            vec![incoming(item("file:///ws/d.rs", 20, "delta"), &[(20, 4)])],
        );
        let tree = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect("walk");
        assert_eq!(tree.len(), 4);
        let text = render_tree(&tree);
        assert!(text.contains("alpha  a.rs:1:4"), "{text}");
        assert!(text.contains("beta  b.rs:6:4"), "{text}");
    }

    #[tokio::test]
    async fn an_empty_root_answer_is_a_miss_not_an_error() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(Vec::new());
        let out = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect_err("a miss");
        assert!(!out.is_error);
        assert!(out.text.starts_with("[not_found]"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_failing_root_fails_the_whole_call() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.prepare(Reply::Fail(5, timeout()));
        let out = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect_err("a failure");
        assert!(out.is_error);
        assert!(out.text.starts_with("[timeout]"), "{}", out.text);
    }

    #[tokio::test]
    async fn an_indexing_root_fails_the_whole_call() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.prepare(Reply::Fail(
            0,
            LspError::Indexing {
                server: "fake-ls".to_owned(),
                message: "Indexing".to_owned(),
                percent: Some(40),
            },
        ));
        let out = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect_err("indexing");
        assert!(out.text.starts_with("[indexing]"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_server_without_call_hierarchy_says_unsupported() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.prepare(Reply::Fail(
            0,
            LspError::Rpc {
                server: "fake-ls".to_owned(),
                code: -32601,
                message: "method not found".to_owned(),
            },
        ));
        let out = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect_err("unsupported");
        assert!(out.is_error);
        assert!(out.text.starts_with("[unsupported]"), "{}", out.text);
    }

    #[tokio::test]
    async fn cancelling_mid_walk_returns_no_half_tree() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![incoming(item("file:///ws/b.rs", 10, "beta"), &[(10, 4)])],
        );
        backend.calls(
            "beta",
            vec![incoming(item("file:///ws/c.rs", 20, "gamma"), &[(20, 4)])],
        );
        backend.cancel_after(2);
        let out = walk(&backend, Direction::Incoming, 3, &cancel)
            .await
            .expect_err("cancelled");
        assert!(out.is_error);
        assert!(out.text.starts_with("[cancelled]"), "{}", out.text);
    }

    #[tokio::test]
    async fn outgoing_walks_callees_the_same_way() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![outgoing(item("file:///ws/b.rs", 10, "beta"), &[(0, 4)])],
        );
        backend.calls(
            "beta",
            vec![outgoing(item("file:///ws/c.rs", 20, "gamma"), &[(10, 4)])],
        );
        backend.calls(
            "gamma",
            vec![outgoing(item("file:///ws/a.rs", 0, "alpha"), &[(20, 4)])],
        );

        let shallow = walk(&backend, Direction::Outgoing, 1, &cancel)
            .await
            .expect("walk");
        assert_eq!(shallow.len(), 2);
        assert_eq!(
            render_tree(&shallow),
            "alpha  a.rs:1:4\n  -> beta  b.rs:11:4  (call at a.rs:1:5)"
        );

        let deep = walk(&backend, Direction::Outgoing, 3, &cancel)
            .await
            .expect("walk");
        assert_eq!(deep.len(), 4);
        let text = render_tree(&deep);
        assert!(
            text.contains("-> gamma  c.rs:21:4  (call at b.rs:11:5)"),
            "{text}"
        );
        assert!(text.contains("(see above)"), "{text}");
    }

    #[tokio::test]
    async fn several_call_sites_are_summarised() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls(
            "alpha",
            vec![incoming(
                item("file:///ws/b.rs", 10, "beta"),
                &[(10, 4), (12, 6)],
            )],
        );
        backend.calls("beta", Vec::new());
        let tree = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect("walk");
        assert!(
            render_tree(&tree).contains("(call at b.rs:11:5, +1 call sites)"),
            "{}",
            render_tree(&tree)
        );
    }

    #[tokio::test]
    async fn same_level_requests_run_at_most_eight_at_a_time() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        let children: Vec<Value> = (0..24)
            .map(|index| {
                incoming(
                    item(
                        &format!("file:///ws/n{index}.rs"),
                        index,
                        &format!("n{index}"),
                    ),
                    &[(index, 0)],
                )
            })
            .collect();
        backend.calls("alpha", children);
        for index in 0..24 {
            backend.node(&format!("n{index}"), Reply::Slow(100, Vec::new()));
        }
        let started = Instant::now();
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        let elapsed = started.elapsed();
        assert_eq!(tree.len(), 25);
        assert_eq!(backend.peak(), MAX_CONCURRENCY);
        assert!(
            elapsed < Duration::from_millis(500),
            "24 nodes at 100 ms each took {elapsed:?}; they were not run eight at a time"
        );
    }

    // ---- rendering --------------------------------------------------------

    fn site(path: &str, line: u32, character: u32) -> Site {
        Site {
            path: Some(PathBuf::from(path)),
            uri: String::new(),
            line: Some(line),
            character: Some(character),
        }
    }

    fn leaf(name: &str, path: &str, line: u32, character: u32) -> Node {
        Node {
            name: name.to_owned(),
            site: site(path, line, character),
            call_site: None,
            call_site_file: None,
            more_call_sites: 0,
            state: State::Expanded,
            children: Vec::new(),
            children_omitted: 0,
        }
    }

    #[test]
    fn rendering_shows_nesting_call_sites_and_see_above() {
        let tree = CallTree {
            direction: Direction::Incoming,
            encoding: PositionEncoding::Utf32,
            roots: vec![Node {
                name: "foo::bar".to_owned(),
                site: site("/ws/src/foo.rs", 9, 7),
                call_site: None,
                call_site_file: None,
                more_call_sites: 0,
                state: State::Expanded,
                children: vec![
                    Node {
                        name: "baz::run".to_owned(),
                        site: site("/ws/src/baz.rs", 30, 4),
                        call_site: Some((34, 8)),
                        call_site_file: Some(PathBuf::from("/ws/src/baz.rs")),
                        more_call_sites: 1,
                        state: State::Expanded,
                        children: vec![Node {
                            name: "main".to_owned(),
                            site: site("/ws/src/main.rs", 4, 0),
                            call_site: Some((6, 4)),
                            call_site_file: Some(PathBuf::from("/ws/src/main.rs")),
                            more_call_sites: 0,
                            state: State::Expanded,
                            children: Vec::new(),
                            children_omitted: 0,
                        }],
                        children_omitted: 0,
                    },
                    Node {
                        name: "qux::go".to_owned(),
                        site: site("/ws/src/qux.rs", 7, 4),
                        call_site: None,
                        call_site_file: None,
                        more_call_sites: 0,
                        state: State::SeeAbove,
                        children: Vec::new(),
                        children_omitted: 0,
                    },
                ],
                children_omitted: 0,
            }],
            notes: Vec::new(),
        };
        assert_eq!(
            render_tree(&tree),
            "foo::bar  src/foo.rs:10:8\n  \
             <- baz::run  src/baz.rs:31:5  (call at src/baz.rs:35:9, +1 call sites)\n     \
             <- main  src/main.rs:5:1  (call at src/main.rs:7:5)\n  \
             <- qux::go  src/qux.rs:8:5  (see above)"
        );
    }

    #[test]
    fn notes_and_omitted_children_render_under_the_tree() {
        let mut root = leaf("alpha", "/ws/a.rs", 0, 0);
        root.children_omitted = 3;
        let tree = CallTree {
            direction: Direction::Incoming,
            encoding: PositionEncoding::Utf32,
            roots: vec![root],
            notes: vec!["note: truncated at 150 nodes".to_owned()],
        };
        let lines = LineIndex::lazy(PathBuf::from("/ws"));
        let view = View {
            boundary: Path::new("/ws"),
            encoding: PositionEncoding::Utf32,
            max_results: usize::MAX,
            subject: None,
            lines: &lines,
        };
        assert_eq!(
            render(&tree, &view),
            "alpha  a.rs:1:1\n  ... and 3 more (not expanded)\nnote: truncated at 150 nodes"
        );
    }

    #[test]
    fn a_call_site_without_a_file_still_prints_a_position() {
        let mut node = leaf("beta", "/ws/b.rs", 3, 2);
        node.call_site = Some((3, 2));
        node.more_call_sites = 2;
        let lines = LineIndex::lazy(PathBuf::from("/ws"));
        let view = View {
            boundary: Path::new("/ws"),
            encoding: PositionEncoding::Utf32,
            max_results: usize::MAX,
            subject: None,
            lines: &lines,
        };
        assert_eq!(
            suffix(&node, &view).as_deref(),
            Some("  (call at 4:3, +2 call sites)")
        );
    }

    #[test]
    fn every_state_has_a_suffix() {
        let lines = LineIndex::lazy(PathBuf::from("/ws"));
        let view = View {
            boundary: Path::new("/ws"),
            encoding: PositionEncoding::Utf32,
            max_results: usize::MAX,
            subject: None,
            lines: &lines,
        };
        let states = [
            (State::SeeAbove, "  (see above)"),
            (
                State::OutsideWorkspace,
                "  (outside workspace, not expanded)",
            ),
            (State::Failed("timeout".to_owned()), "  (failed: timeout)"),
            (State::Truncated, "  (not expanded)"),
        ];
        for (state, expected) in states {
            let mut node = leaf("x", "/ws/a.rs", 0, 0);
            node.state = state.clone();
            assert_eq!(suffix(&node, &view).as_deref(), Some(expected), "{state:?}");
        }
        let expanded = leaf("x", "/ws/a.rs", 0, 0);
        assert_eq!(suffix(&expanded, &view), None);
    }

    #[tokio::test]
    async fn a_node_with_no_file_is_reported_rather_than_requested() {
        let cancel = CancellationToken::new();
        let backend = GraphBackend::new(&cancel);
        let mut walker = Walker::new(&backend, Direction::Incoming, &cancel);
        walker.arena.push(Placed {
            name: "odd".to_owned(),
            site: Site {
                path: None,
                uri: "jdt://contents/String.class".to_owned(),
                line: Some(0),
                character: Some(0),
            },
            call_site: None,
            call_site_file: None,
            more_call_sites: 0,
            state: State::Expanded,
            parent: None,
            children: Vec::new(),
            children_omitted: 0,
            item: None,
            level: 0,
        });
        assert!(walker.expandable(&[0]).is_empty());
        assert_eq!(walker.failed, 1);
        assert!(matches!(
            walker.arena[0].state,
            State::Failed(ref code) if code == "outside_workspace"
        ));
    }

    // ---- the boundary -----------------------------------------------
    //
    // The walk decides whether to *expand* a node, and expanding is what makes
    // the daemon read the file and `didOpen` it to the server. So "is this URI
    // inside the workspace" is a security decision, and it has to hold against
    // the two ways a path can leave a directory without its name saying so:
    // `..` and a symlink. A bare `Path::starts_with` stops neither, because
    // `ParentDir` is just another path component.
    //
    // These need a real directory, so `Boundary`'s canonicalizing half has
    // something to resolve.

    /// A workspace with one file in it, plus a sibling directory outside it
    /// holding one secret file. Returns `(workspace, outside_dir, secret)`.
    fn boundary_fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret.rs");
        std::fs::write(&secret, "fn secret() {}").expect("secret");
        let inside = workspace.path().join("a.rs");
        std::fs::write(&inside, "fn alpha() {}").expect("a.rs");
        (workspace, outside, secret)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_inside_the_workspace_pointing_out_is_not_expanded() {
        use std::os::unix::fs::symlink;

        let (workspace, _outside, secret) = boundary_fixture();
        let link = workspace.path().join("link");
        symlink(secret.parent().expect("outside dir"), &link).expect("symlink");
        // The URI a hostile or confused server would send: lexically inside the
        // workspace, physically somewhere else entirely.
        let uri = format!("{}/secret.rs", resolve::file_uri(&link));

        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.set_boundary(workspace.path().to_path_buf());
        let root = workspace.path().join("a.rs");
        backend.root(vec![item(&resolve::file_uri(&root), 0, "alpha")]);
        // The node is marked `Expanded` by the caller, so if the boundary check
        // let it through the walk would try to expand it.
        backend.calls("alpha", vec![incoming(item(&uri, 0, "leaked"), &[(0, 4)])]);
        backend.calls(
            "leaked",
            vec![incoming(item("file:///etc/passwd", 0, "x"), &[])],
        );

        let tree = walk_from(
            &backend,
            &candidate_in(&root),
            Direction::Incoming,
            1,
            &cancel,
        )
        .await
        .expect("walk");

        let child = &tree.roots[0].children[0];
        assert_eq!(child.name, "leaked");
        assert_eq!(child.state, State::OutsideWorkspace, "{child:?}");
        // Not expanded: the daemon was never asked about it, so it never read
        // the file behind the symlink. One request total, for the root.
        assert_eq!(backend.expansions(), 1, "the node was expanded");
        assert!(child.children.is_empty());
    }

    #[tokio::test]
    async fn a_percent_encoded_dot_dot_uri_is_not_expanded() {
        let (workspace, _outside, _secret) = boundary_fixture();
        let root = workspace.path().join("a.rs");

        // `url` does not normalize a percent-encoded `..`: this parses to the
        // literal path `<ws>/../../etc/passwd`, which `Path::starts_with` calls
        // a child of `<ws>`.
        let uri = format!(
            "{}%2F..%2F..%2F..%2Fetc%2Fpasswd",
            resolve::file_uri(&root).trim_end_matches('/')
        );
        assert!(
            item_path(&item(&uri, 0, "leaked"))
                .expect("a file URI")
                .starts_with(workspace.path()),
            "the URI must still look like it is inside, or this test proves nothing"
        );

        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.set_boundary(workspace.path().to_path_buf());
        backend.root(vec![item(&resolve::file_uri(&root), 0, "alpha")]);
        backend.calls("alpha", vec![incoming(item(&uri, 0, "leaked"), &[(0, 4)])]);

        let tree = walk_from(
            &backend,
            &candidate_in(&root),
            Direction::Incoming,
            1,
            &cancel,
        )
        .await
        .expect("walk");

        let child = &tree.roots[0].children[0];
        assert_eq!(child.state, State::OutsideWorkspace, "{child:?}");
        assert_eq!(backend.expansions(), 1, "the node was expanded");
    }

    #[tokio::test]
    async fn a_real_file_inside_the_workspace_is_still_expanded() {
        // The other direction: the check must not be so eager that it refuses
        // the ordinary case. A file that really is under the boundary, named by
        // a path that goes through `.` and `..` on the way, is still inside.
        let (workspace, _outside, _secret) = boundary_fixture();
        let root = workspace.path().join("a.rs");
        let sub = workspace.path().join("sub");
        std::fs::create_dir(&sub).expect("sub");
        let b = sub.join("b.rs");
        std::fs::write(&b, "fn beta() {}").expect("b.rs");
        // `<ws>/sub/../sub/b.rs`: the `..` cancels out and never leaves.
        let uri = format!(
            "{}/sub/../sub/b.rs",
            resolve::file_uri(workspace.path()).trim_end_matches('/')
        );

        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.set_boundary(workspace.path().to_path_buf());
        backend.root(vec![item(&resolve::file_uri(&root), 0, "alpha")]);
        backend.calls("alpha", vec![incoming(item(&uri, 10, "beta"), &[(10, 4)])]);
        backend.calls("beta", Vec::new());

        let tree = walk_from(
            &backend,
            &candidate_in(&root),
            Direction::Incoming,
            2,
            &cancel,
        )
        .await
        .expect("walk");

        let child = &tree.roots[0].children[0];
        assert_eq!(child.state, State::Expanded, "{child:?}");
        // Two requests: the root, and `beta` — so it really was expanded, not
        // just labelled. The check must not refuse the ordinary case.
        assert_eq!(backend.expansions(), 2, "beta was not expanded");
    }

    #[tokio::test]
    async fn a_root_outside_the_workspace_is_shown_but_not_expanded() {
        // The server picks the root's URI too, so the root is checked like every
        // other node. It is almost always inside — it describes the position the
        // request was about — which is exactly why an unchecked root would go
        // unnoticed.
        let (workspace, outside, _secret) = boundary_fixture();
        let outside_file = outside.path().join("elsewhere.rs");
        std::fs::write(&outside_file, "fn elsewhere() {}").expect("elsewhere.rs");

        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.set_boundary(workspace.path().to_path_buf());
        backend.root(vec![item(&resolve::file_uri(&outside_file), 0, "stranger")]);
        backend.calls(
            "stranger",
            vec![incoming(item("file:///etc/passwd", 0, "x"), &[])],
        );

        let tree = walk_from(
            &backend,
            &candidate_in(&outside_file),
            Direction::Incoming,
            1,
            &cancel,
        )
        .await
        .expect("walk");

        assert_eq!(tree.roots[0].state, State::OutsideWorkspace);
        assert_eq!(
            backend.expansions(),
            0,
            "a root outside the boundary was expanded"
        );
    }

    // ---- the shape and the numbers ---------------

    /// A response the tool cannot read is a failure, not "no callers".
    ///
    /// `incomingCalls` that is not an array used to become an empty list, the
    /// node stayed `Expanded`, and the rendered line read as a plain
    /// `alpha  a.rs:1:4` — the model would conclude that nothing calls it,
    /// which is the one reading of the answer this tool exists to prevent.
    #[tokio::test]
    async fn a_malformed_expansion_answer_is_a_failure_not_an_empty_one() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        // An object where an array belongs.
        backend.raw_calls("alpha", json!({ "not": "an array" }));
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        let root = &tree.roots[0];
        assert_eq!(
            root.state,
            State::Failed("invalid_response".to_owned()),
            "{root:?}"
        );
        assert!(
            tree.notes
                .iter()
                .any(|note| note.contains("could not be expanded")),
            "the answer must not read as complete: {:?}",
            tree.notes
        );
    }

    /// The same for a string and a number — shapes that are neither a list nor
    /// the spec's `null`.
    #[tokio::test]
    async fn every_non_array_expansion_answer_is_a_failure() {
        for answer in [json!("nope"), json!(7)] {
            let cancel = CancellationToken::new();
            let mut backend = GraphBackend::new(&cancel);
            backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
            backend.raw_calls("alpha", answer.clone());
            let tree = walk(&backend, Direction::Incoming, 2, &cancel)
                .await
                .expect("walk");
            assert_eq!(
                tree.roots[0].state,
                State::Failed("invalid_response".to_owned()),
                "{answer} was read as an empty answer"
            );
        }
    }

    /// `null` is a legal answer meaning "no callers"; it must not be a failure.
    #[tokio::test]
    async fn a_null_expansion_answer_means_no_callers() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.raw_calls("alpha", json!(null));
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        assert_ne!(
            tree.roots[0].state,
            State::Failed("invalid_response".to_owned())
        );
        assert!(
            !tree
                .notes
                .iter()
                .any(|note| note.contains("could not be expanded")),
            "{:?}",
            tree.notes
        );
    }

    /// An empty array is a real answer — nothing calls it — and must not be
    /// turned into a failure, or every leaf in every graph is an error.
    #[tokio::test]
    async fn an_empty_expansion_answer_is_still_a_real_answer() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        backend.root(vec![item("file:///ws/a.rs", 0, "alpha")]);
        backend.calls("alpha", Vec::new());
        let tree = walk(&backend, Direction::Incoming, 2, &cancel)
            .await
            .expect("walk");
        assert_eq!(tree.roots[0].state, State::Expanded);
        assert!(tree.roots[0].children.is_empty());
        assert!(
            !tree
                .notes
                .iter()
                .any(|n| n.contains("could not be expanded"))
        );
    }

    /// Roots count towards the node budget.
    ///
    /// The cap was checked in `expandable` and `place_children` and nowhere
    /// else, so a `prepareCallHierarchy` answer with more items than the budget
    /// built an arena larger than the budget before the first level was walked —
    /// while the constant's own documentation says it bounds "the whole tree,
    /// roots included".
    #[tokio::test]
    async fn roots_are_capped_by_the_node_budget() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        let roots: Vec<Value> = (0..MAX_NODES + 50)
            .map(|n| {
                let line = u32::try_from(n).expect("well under u32::MAX");
                item(&format!("file:///ws/f{n}.rs"), line, &format!("f{n}"))
            })
            .collect();
        backend.root(roots);
        let tree = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect("walk");
        assert_eq!(
            tree.len(),
            MAX_NODES,
            "the tree must not be larger than the node budget"
        );
        assert!(
            tree.notes
                .iter()
                .any(|note| note.contains(&format!("truncated at {MAX_NODES} nodes"))),
            "a tree cut at the budget must say so: {:?}",
            tree.notes
        );
    }

    /// P2: a tree cut at the root budget has to say *how many* roots it left
    /// out, not merely that a cut happened.
    ///
    /// The note used to be a bare "truncated at 150 nodes": a reader who asked
    /// for callers and got a full-looking tree had no way to tell whether the
    /// server offered 151 items and one was dropped, or 15000 and 14850 were.
    /// Both render as the same sentence, so the number is the whole point.
    #[tokio::test]
    async fn a_root_cut_says_how_many_roots_were_left_out() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        let total = MAX_NODES + 50;
        let roots: Vec<Value> = (0..total)
            .map(|n| {
                let line = u32::try_from(n).expect("well under u32::MAX");
                item(&format!("file:///ws/f{n}.rs"), line, &format!("f{n}"))
            })
            .collect();
        backend.root(roots);
        let tree = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect("walk");
        let note = tree
            .notes
            .iter()
            .find(|note| note.contains("truncated at"))
            .expect("a truncation note");
        assert!(
            note.contains(&format!("{total} root(s) found, {MAX_NODES} shown")),
            "the note must name the found and shown root counts: {note}"
        );
    }

    /// The same note, when nothing was left out: no "left out" clause at all.
    /// A tree that fits must not claim roots are missing.
    #[tokio::test]
    async fn a_tree_that_fits_does_not_claim_roots_are_missing() {
        let cancel = CancellationToken::new();
        let mut backend = GraphBackend::new(&cancel);
        let roots: Vec<Value> = (0..10)
            .map(|n| {
                let line = u32::try_from(n).expect("well under u32::MAX");
                item(&format!("file:///ws/f{n}.rs"), line, &format!("f{n}"))
            })
            .collect();
        backend.root(roots);
        let tree = walk(&backend, Direction::Incoming, 1, &cancel)
            .await
            .expect("walk");
        assert!(
            !tree.notes.iter().any(|note| note.contains("root(s) found")),
            "a tree that fits must not carry a root-count note: {:?}",
            tree.notes
        );
    }

    /// A coordinate the server invented is dropped, not truncated onto a
    /// real line.
    #[test]
    fn a_saturated_coordinate_is_dropped_rather_than_wrapped() {
        // The largest value a `u32` can hold is a coordinate; one past it is not.
        assert_eq!(coordinate(&json!(4_294_967_295u64)), Some(u32::MAX));
        assert_eq!(coordinate(&json!(4_294_967_296u64)), None);
        assert_eq!(coordinate(&json!(-1)), None);
        assert_eq!(coordinate(&json!("4")), None);
        // And the site drops it rather than reporting line 1 of the wrong file.
        let item = json!({
            "name": "x",
            "uri": "file:///ws/a.rs",
            "selectionRange": {"start": {"line": 4_294_967_296u64, "character": 0}}
        });
        let site = item_site(&item);
        assert_eq!(
            site.line, None,
            "the unsaturable line must be dropped: {site:?}"
        );
        assert_eq!(
            site.character,
            Some(0),
            "the character was a real coordinate and is kept: {site:?}"
        );
    }

    /// The `+ 1` that turns a server line into the model's line cannot
    /// overflow.
    ///
    /// `u32::MAX + 1` panics in any build with overflow checks — every
    /// `cargo test` — and silently wraps to 0 in a release build, which prints
    /// `0:0` and sends the model to the top of the file. The path is reachable
    /// whenever the item's URI is not a `file:` one, since `call_site_file` is
    /// then `None`.
    #[test]
    fn a_saturated_call_site_prints_without_overflowing() {
        let node = Node {
            name: "alpha".to_owned(),
            site: Site {
                path: None,
                uri: "jdt://contents/String.class".to_owned(),
                line: None,
                character: None,
            },
            call_site: Some((u32::MAX, u32::MAX)),
            call_site_file: None,
            more_call_sites: 0,
            state: State::Expanded,
            children: Vec::new(),
            children_omitted: 0,
        };
        // The point is that this returns at all: overflow checks are on in a
        // test build, so `+ 1` would panic here rather than print.
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = LineIndex::lazy(dir.path().to_path_buf());
        let boundary = dir.path().to_path_buf();
        let view = View {
            boundary: &boundary,
            encoding: PositionEncoding::Utf32,
            max_results: usize::MAX,
            subject: None,
            lines: &lines,
        };
        let text = call_site_text(&node, (u32::MAX, u32::MAX), &view);
        assert!(!text.is_empty(), "{text}");
    }
}
