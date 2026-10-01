//! The container lookup behind a qualified symbol name (`Foo::bar`, `Foo.bar`).
//!
//! Why this module exists: the model qualifies a name to disambiguate, but the
//! servers disagree about what "the container" is. `workspace/symbol` gives a
//! bare name, and only sometimes a `containerName`:
//!
//! - rust-analyzer sends **no** `containerName` at all;
//! - gopls fills it with the **package path** (`example.com/probe`), not the
//!   type.
//!
//! So the enclosing symbol is looked up where it actually is: the file the
//! candidate lives in, through `textDocument/documentSymbol`, walking the
//! hierarchy up from the candidate's position. The lookup is best effort — a
//! file that cannot be asked contributes nothing, and a qualifier that matches
//! nothing leaves the candidate set untouched rather than emptying it.

use std::path::{Path, PathBuf};

use opencraylsp_core::backend::{LspBackend, PositionEncoding, Served};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::format::LineIndex;
use crate::position;
use crate::resolve::{Candidate, file_uri};

/// At most this many candidates are looked up (the newest first twenty are
/// plenty to disambiguate a name).
pub const MAX_CANDIDATES: usize = 20;

/// At most this many files get one `documentSymbol` request each.
pub const MAX_FILES: usize = 8;

/// The `documentSymbol` method the lookup uses.
const METHOD: &str = "textDocument/documentSymbol";

/// Keeps the candidates whose last qualifier names an ancestor of the symbol.
///
/// `Some(kept)` means the lookup produced information and at least one
/// candidate matched; `None` means "keep what you had" — either no lookup
/// produced anything, or nothing matched. A qualifier is a hint, never a hard
/// filter.
pub async fn narrow_by_qualifier(
    backend: &dyn LspBackend,
    candidates: &[Candidate],
    qualifier: &str,
    cancel: &CancellationToken,
) -> Option<Vec<Candidate>> {
    let considered = &candidates[..candidates.len().min(MAX_CANDIDATES)];

    let files = distinct_files(considered);
    let trees = fetch_trees(backend, &files, cancel).await;
    if cancel.is_cancelled() {
        return None;
    }

    let lines = LineIndex::lazy(backend.boundary());
    let ranked: Vec<(usize, &Candidate)> = considered
        .iter()
        .filter_map(|candidate| {
            let chain = chain_of(candidate, &trees, &lines);
            if chain.is_empty() {
                return None;
            }
            rank_of(&chain, qualifier).map(|rank| (rank, candidate))
        })
        .collect();

    if ranked.is_empty() {
        return None;
    }

    // A match against a closer ancestor is a better one; the rest keep their
    // relative order.
    let mut kept: Vec<&(usize, &Candidate)> = ranked.iter().collect();
    kept.sort_by_key(|(rank, _)| usize::MAX - *rank);
    Some(
        kept.into_iter()
            .map(|(_, candidate)| (*candidate).clone())
            .collect(),
    )
}

/// The files to ask, in first-seen order, capped at [`MAX_FILES`].
fn distinct_files(candidates: &[Candidate]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        if candidate.outside_workspace {
            continue;
        }
        if let Some(path) = &candidate.site.path
            && !files.contains(path)
        {
            files.push(path.clone());
        }
    }
    files.truncate(MAX_FILES);
    files
}

/// One `documentSymbol` reply, kept raw: the hierarchy's ranges are what the
/// walk needs and a flattened list would have thrown them away.
type Tree = (PathBuf, Value, PositionEncoding);

/// A future whose output is uniform, so up to [`MAX_FILES`] can be joined
/// without boxing per call site.
type Query<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Option<Tree>> + Send + 'a>>;

async fn fetch_trees(
    backend: &dyn LspBackend,
    files: &[PathBuf],
    cancel: &CancellationToken,
) -> Vec<Tree> {
    let query = |index: usize| -> Query<'_> {
        if index >= files.len() {
            return Box::pin(async { None });
        }
        let path = files[index].clone();
        Box::pin(async move {
            let params = json!({ "textDocument": { "uri": file_uri(&path) } });
            match backend.request(&path, METHOD, params, cancel).await {
                Ok(Served {
                    value, encoding, ..
                }) => Some((path, value, encoding)),
                // A file that cannot be asked says nothing; it does not fail
                // the whole lookup.
                Err(_) => None,
            }
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
    [a, b, c, d, e, f, g, h].into_iter().flatten().collect()
}

/// The ancestor names of `candidate`'s symbol, outermost first, plus the
/// candidate's own `containerName` last segment.
fn chain_of(candidate: &Candidate, trees: &[Tree], lines: &LineIndex) -> Vec<String> {
    let mut chain = Vec::new();
    if let Some(path) = &candidate.site.path
        && let Some((_, value, encoding)) = trees.iter().find(|(tree_path, ..)| tree_path == path)
    {
        let context = Context {
            lines,
            path,
            encoding: *encoding,
        };
        if let (Some(line), Some(character)) = (candidate.site.line, candidate.site.character) {
            let point = context.point(line, character, PositionEncoding::Utf16);
            if let Some(items) = value.as_array()
                && let Some(ancestors) = deepest_chain(items, point, &[], &context)
            {
                chain = ancestors;
            }
        }
    }
    if let Some(container) = &candidate.container {
        let tail = package_tail(container);
        if !tail.is_empty() && !chain.contains(&tail) {
            chain.push(tail);
        }
    }
    chain
}

/// The position of the qualifier in `chain`, measured from the symbol outwards:
/// a closer ancestor ranks higher.
fn rank_of(chain: &[String], qualifier: &str) -> Option<usize> {
    chain
        .iter()
        .enumerate()
        .filter(|(_, name)| name.as_str() == qualifier)
        .map(|(index, _)| index + 1)
        .max()
}

/// Walks the hierarchy, returning the ancestors of the deepest symbol whose
/// range contains `point`.
struct Context<'a> {
    lines: &'a LineIndex,
    path: &'a Path,
    encoding: PositionEncoding,
}

impl Context<'_> {
    /// The point in Unicode scalar units on both sides, so the comparison does
    /// not depend on which encoding the two answers happened to use.
    fn point(&self, line: u32, character: u32, encoding: PositionEncoding) -> (u32, u32) {
        match self.lines.line(self.path, line) {
            Some(text) => (
                line,
                position::scalar_from_units(&text, character, encoding),
            ),
            None => (line, character),
        }
    }
}

fn deepest_chain(
    nodes: &[Value],
    point: (u32, u32),
    ancestors: &[String],
    context: &Context<'_>,
) -> Option<Vec<String>> {
    for node in nodes {
        let (Some(start), Some(end)) = (range_edge(node, "start"), range_edge(node, "end")) else {
            continue;
        };
        let start = context.point(start.0, start.1, context.encoding);
        let end = context.point(end.0, end.1, context.encoding);
        if !contains(start, end, point) {
            continue;
        }
        let mut deeper: Vec<String> = ancestors.to_vec();
        for name in container_names(node.get("name").and_then(Value::as_str).unwrap_or("")) {
            if !deeper.contains(&name) {
                deeper.push(name);
            }
        }
        if let Some(children) = node.get("children").and_then(Value::as_array)
            && let Some(found) = deepest_chain(children, point, &deeper, context)
        {
            return Some(found);
        }
        // The node itself is the deepest match; its ancestors are the chain.
        return Some(ancestors.to_vec());
    }
    None
}

fn range_edge(node: &Value, edge: &str) -> Option<(u32, u32)> {
    // A hierarchical `DocumentSymbol` carries `range`; a flat
    // `SymbolInformation` carries it inside `location`. Both are real answers,
    // and a server that does not support hierarchy sends the flat one.
    let range = node.get("range").or_else(|| {
        node.get("location")
            .and_then(|location| location.get("range"))
    })?;
    let position = range.get(edge)?;
    // Validated, not truncated: a coordinate that does not fit a `u32` is not a
    // coordinate, and casting one turns it into a plausible position in the
    // wrong place. Dropping it leaves the node without a range, which the
    // containment test below then simply does not match.
    let line = u32::try_from(position.get("line")?.as_u64()?).ok()?;
    let character = u32::try_from(position.get("character")?.as_u64()?).ok()?;
    Some((line, character))
}

fn contains(start: (u32, u32), end: (u32, u32), point: (u32, u32)) -> bool {
    start <= point && point <= end
}

/// A container name reduced to the type it names.
///
/// `impl Foo<T>` is `Foo`; gopls writes a method's receiver as `(*T).M` or
/// `T.M`, both of which name `T`; leading `*` and one layer of parentheses go.
pub fn normalise_name(raw: &str) -> String {
    let mut name = raw.trim();
    if let Some((head, _)) = name.rsplit_once('.') {
        name = head.trim();
    }
    if name.starts_with('(') && name.ends_with(')') {
        name = name[1..name.len() - 1].trim();
    }
    name = name.trim_start_matches('*').trim();
    for prefix in ["impl ", "trait ", "struct ", "enum ", "mod "] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest.trim();
        }
    }
    if let Some(index) = name.find('<') {
        name = name[..index].trim();
    }
    name.trim().to_owned()
}

/// Every name a node can be qualified by.
///
/// rust-analyzer spells an impl block `impl TaskRunner for Recording`, and a
/// model may reasonably write either `TaskRunner::start` or
/// `Recording::start`; both are offered. Everything else has one name.
pub fn container_names(raw: &str) -> Vec<String> {
    let base = normalise_name(raw);
    match base.split_once(" for ") {
        Some((trait_name, type_name)) => {
            let mut names = Vec::new();
            for part in [trait_name, type_name] {
                let name = normalise_name(part);
                if !name.is_empty() && !names.contains(&name) {
                    names.push(name);
                }
            }
            names
        }
        None => {
            if base.is_empty() {
                Vec::new()
            } else {
                vec![base]
            }
        }
    }
}

/// The last segment of a package or container path: `example.com/probe` is
/// `probe`, and `a::b::Baz` is `Baz`.
pub fn package_tail(raw: &str) -> String {
    let last = raw.rsplit(['/', ':']).next().unwrap_or(raw);
    last.rsplit('.').next().unwrap_or(last).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::Site;

    fn candidate(
        name: &str,
        container: Option<&str>,
        path: &str,
        line: u32,
        character: u32,
        outside: bool,
    ) -> Candidate {
        Candidate {
            site: Site {
                path: Some(PathBuf::from(path)),
                uri: format!("file://{path}"),
                line: Some(line),
                character: Some(character),
            },
            name: name.to_owned(),
            kind: 12,
            container: container.map(str::to_owned),
            server: "rust-analyzer".to_owned(),
            outside_workspace: outside,
        }
    }

    fn trees(value: Value) -> Vec<Tree> {
        vec![(PathBuf::from("/ws/a.rs"), value, PositionEncoding::Utf16)]
    }

    #[test]
    fn normalise_name_reduces_every_spelling() {
        assert_eq!(normalise_name("impl Foo<T>"), "Foo");
        assert_eq!(normalise_name("trait Bar"), "Bar");
        assert_eq!(normalise_name("struct Baz"), "Baz");
        assert_eq!(normalise_name("enum Qux"), "Qux");
        assert_eq!(normalise_name("(*T).M"), "T");
        assert_eq!(normalise_name("T.M"), "T");
        assert_eq!(normalise_name("*Thing"), "Thing");
        assert_eq!(normalise_name("tests"), "tests");
        assert_eq!(normalise_name("impl Foo<T> where T: Clone"), "Foo");
    }

    #[test]
    fn container_names_offer_both_sides_of_an_impl() {
        // rust-analyzer spells an impl block `impl TaskRunner for Recording`;
        // a model may qualify by either the trait or the type.
        assert_eq!(
            container_names("impl TaskRunner for Recording"),
            vec!["TaskRunner".to_owned(), "Recording".to_owned()]
        );
        assert_eq!(container_names("impl Foo<T>"), vec!["Foo".to_owned()]);
        assert_eq!(container_names("mod tests"), vec!["tests".to_owned()]);
        assert!(container_names("").is_empty());
    }

    #[test]
    fn package_tail_keeps_the_last_segment() {
        assert_eq!(package_tail("example.com/probe"), "probe");
        assert_eq!(package_tail("probe"), "probe");
        assert_eq!(package_tail("a/b/c.V"), "V");
        assert_eq!(package_tail("a::b::Baz"), "Baz");
        assert_eq!(package_tail("mod m.Foo"), "Foo");
    }

    #[test]
    fn rank_prefers_the_closest_ancestor() {
        let chain = vec!["outer".to_owned(), "inner".to_owned()];
        assert_eq!(rank_of(&chain, "outer"), Some(1));
        assert_eq!(rank_of(&chain, "inner"), Some(2));
        assert_eq!(rank_of(&chain, "missing"), None);
    }

    #[test]
    fn contains_is_inclusive() {
        assert!(contains((1, 0), (1, 5), (1, 0)));
        assert!(contains((1, 0), (1, 5), (1, 5)));
        assert!(!contains((1, 0), (1, 5), (1, 6)));
        assert!(!contains((1, 0), (1, 5), (0, 3)));
    }

    #[test]
    fn the_flat_answer_shape_is_understood_too() {
        // A server without hierarchical support sends `SymbolInformation`
        // (a `location`, no `children`): the range is still found, so the walk
        // does not blow up — it simply has no ancestors to offer.
        let flat = serde_json::json!([{
            "name": "runner",
            "kind": 12,
            "location": {
                "uri": "file:///ws/a.rs",
                "range": { "start": { "line": 3, "character": 1 }, "end": { "line": 5, "character": 2 } }
            }
        }]);
        assert_eq!(range_edge(&flat[0], "start"), Some((3, 1)));
        assert_eq!(range_edge(&flat[0], "end"), Some((5, 2)));
    }

    #[test]
    fn distinct_files_dedupes_skips_outside_and_caps() {
        let mut candidates = Vec::new();
        for index in 0..10 {
            candidates.push(candidate(
                "x",
                None,
                &format!("/ws/a{index}.rs"),
                0,
                0,
                false,
            ));
        }
        candidates.push(candidate("x", None, "/usr/lib/std.rs", 0, 0, true));
        candidates.push(candidate("x", None, "/ws/a0.rs", 0, 0, false));

        let files = distinct_files(&candidates);
        assert_eq!(files.len(), MAX_FILES);
        assert!(!files.iter().any(|file| file.ends_with("std.rs")));
        assert_eq!(files[0], PathBuf::from("/ws/a0.rs"));
    }

    #[test]
    fn chain_of_walks_the_hierarchy_to_the_ancestors() {
        let lines = LineIndex::lazy("/ws");
        let tree = json!([{
            "name": "tests",
            "kind": 2,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 99, "character": 0 } },
            "children": [{
                "name": "impl Foo<T>",
                "kind": 19,
                "range": { "start": { "line": 5, "character": 0 }, "end": { "line": 9, "character": 0 } },
                "children": [{
                    "name": "bar",
                    "kind": 6,
                    "range": { "start": { "line": 6, "character": 4 }, "end": { "line": 6, "character": 20 } }
                }]
            }]
        }]);
        let target = candidate("bar", None, "/ws/a.rs", 6, 8, false);
        assert_eq!(
            chain_of(&target, &trees(tree), &lines),
            vec!["tests".to_owned(), "Foo".to_owned()]
        );
    }

    #[test]
    fn chain_of_without_a_matching_node_keeps_the_container_name() {
        let lines = LineIndex::lazy("/ws");
        // A point outside every range, and no tree for the file at all.
        let outside = candidate("x", None, "/ws/a.rs", 200, 0, false);
        assert!(chain_of(&outside, &trees(json!([])), &lines).is_empty());
        assert!(chain_of(&outside, &[], &lines).is_empty());

        // gopls's package path is the only container information there is.
        let go = candidate("New", Some("example.com/probe"), "/ws/a.go", 0, 0, false);
        assert_eq!(chain_of(&go, &[], &lines), vec!["probe".to_owned()]);

        // A container already in the chain is not added twice.
        let duplicated = candidate("bar", Some("tests"), "/ws/a.rs", 6, 8, false);
        let tree = json!([{
            "name": "tests",
            "kind": 2,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 99, "character": 0 } },
            "children": [{
                "name": "bar",
                "kind": 6,
                "range": { "start": { "line": 6, "character": 4 }, "end": { "line": 6, "character": 20 } }
            }]
        }]);
        assert_eq!(
            chain_of(&duplicated, &trees(tree), &lines),
            vec!["tests".to_owned()]
        );
    }

    #[test]
    fn a_node_with_no_range_is_stepped_over() {
        let lines = LineIndex::lazy("/ws");
        let tree = json!([
            { "name": "nameless", "kind": 12 },
            {
                "name": "outer",
                "kind": 2,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 0 } },
                "children": [{ "name": "no range", "kind": 6 }]
            }
        ]);
        let target = candidate("outer", None, "/ws/a.rs", 3, 0, false);
        assert!(chain_of(&target, &trees(tree), &lines).is_empty());
    }
}
