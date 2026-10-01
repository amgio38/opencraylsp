//! Background warm-up and file watching.
//!
//! Why: a language server builds its index only after it starts, and
//! rust-analyzer needs a minute or more on a large workspace. Starting servers
//! lazily meant the first question of a session always hit a cold index and
//! came back empty. Warm-up starts every discovered project's server as soon as
//! the plugin is built; the watcher then tells running servers about files that
//! changed on disk, so the index follows edits made by any tool — not only the
//! documents this plugin happens to have open.
//!
//! This file holds the pure parts (discovery, snapshots, diffs) so they can be
//! tested without a server; `manager.rs` drives them.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::config::{LspConfig, ServerConfig};

/// Directory names never worth indexing or watching: build output, dependency
/// caches and VCS metadata. Hidden directories (leading `.`) are skipped too.
const BUILTIN_EXCLUDES: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "__pycache__",
    "venv",
];

/// How deep a sample file is searched for below a project root.
const SAMPLE_MAX_DEPTH: usize = 8;

/// Cap on files one snapshot records, so a pathological tree cannot turn the
/// watcher into a CPU sink. Past it the snapshot is truncated (and logged).
const SNAPSHOT_MAX_FILES: usize = 100_000;

/// Whether a directory named `name` is skipped.
pub fn is_excluded(name: &str, extra: &[String]) -> bool {
    name.starts_with('.') || BUILTIN_EXCLUDES.contains(&name) || extra.iter().any(|e| e == name)
}

fn child_dirs(dir: &Path, extra: &[String]) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !is_excluded(&e.file_name().to_string_lossy(), extra))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

fn has_marker(dir: &Path, markers: &[String]) -> bool {
    markers.iter().any(|m| dir.join(m).is_file())
}

fn owns(path: &Path, server: &ServerConfig) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| server.extensions.contains_key(ext))
}

/// The topmost project roots for `server` at or below `boundary`, each paired
/// with one source file of that server's language to open.
///
/// "Topmost" matches the routing rule in `find_root`: a Cargo workspace member
/// must resolve to the workspace, so once a directory holds a marker nothing
/// below it is a separate root. A root with no file of the language is
/// dropped — there is nothing to open and nothing to index.
pub fn discover(
    boundary: &Path,
    server: &ServerConfig,
    config: &LspConfig,
) -> Vec<(PathBuf, PathBuf)> {
    let mut roots = Vec::new();
    if server.root_markers.is_empty() {
        roots.push(boundary.to_owned());
    } else {
        let mut queue = VecDeque::from([(boundary.to_owned(), 0usize)]);
        while let Some((dir, depth)) = queue.pop_front() {
            if has_marker(&dir, &server.root_markers) {
                roots.push(dir);
                continue;
            }
            if depth >= config.warmup_max_depth {
                continue;
            }
            for child in child_dirs(&dir, &config.warmup_exclude) {
                queue.push_back((child, depth + 1));
            }
        }
    }
    roots
        .into_iter()
        .filter_map(|root| sample_file(&root, server, &config.warmup_exclude).map(|f| (root, f)))
        .collect()
}

/// The shallowest file under `root` that `server` owns (breadth-first, so a
/// top-level source file wins over one buried in a fixture directory).
pub fn sample_file(root: &Path, server: &ServerConfig, extra: &[String]) -> Option<PathBuf> {
    let mut queue = VecDeque::from([(root.to_owned(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .filter(|p| owns(p, server))
            .collect();
        files.sort();
        if let Some(first) = files.into_iter().next() {
            return Some(first);
        }
        if depth < SAMPLE_MAX_DEPTH {
            for child in child_dirs(&dir, extra) {
                queue.push_back((child, depth + 1));
            }
        }
    }
    None
}

/// Takes one item from each list in turn until all are drained, so a fixed
/// budget is shared fairly between servers.
pub fn interleave<T>(lists: Vec<Vec<T>>) -> Vec<T> {
    let mut iters: Vec<std::vec::IntoIter<T>> = lists.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::new();
    loop {
        let mut progressed = false;
        for iter in &mut iters {
            if let Some(item) = iter.next() {
                out.push(item);
                progressed = true;
            }
        }
        if !progressed {
            return out;
        }
    }
}

/// Modification times of every file under a root that the server cares about:
/// its source files plus its root markers (a changed `Cargo.toml` or
/// `package.json` changes the project model).
pub type Snapshot = HashMap<PathBuf, SystemTime>;

/// Records `root`'s watched files, or `None` when the tree exceeds the file
/// cap. A truncated snapshot depends on directory iteration order, so diffing
/// two of them reported thousands of phantom Created/Deleted changes every
/// pass; such a root is simply not watched.
pub fn snapshot(root: &Path, server: &ServerConfig, extra: &[String]) -> Option<Snapshot> {
    let mut out = Snapshot::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if !is_excluded(&entry.file_name().to_string_lossy(), extra) {
                    stack.push(path);
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let is_marker = server
                .root_markers
                .iter()
                .any(|m| entry.file_name().to_string_lossy() == m.as_str());
            if !(is_marker || owns(&path, server)) {
                continue;
            }
            if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                out.insert(path, modified);
            }
            if out.len() > SNAPSHOT_MAX_FILES {
                tracing::warn!(root = %root.display(), "lsp watcher: more than {SNAPSHOT_MAX_FILES} files; root not watched");
                return None;
            }
        }
    }
    Some(out)
}

/// LSP `FileChangeType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}

/// What changed between two snapshots, sorted by path for stable output.
pub fn diff(before: &Snapshot, after: &Snapshot) -> Vec<(PathBuf, Change)> {
    let mut changes: Vec<(PathBuf, Change)> = Vec::new();
    for (path, modified) in after {
        match before.get(path) {
            None => changes.push((path.clone(), Change::Created)),
            Some(old) if old != modified => changes.push((path.clone(), Change::Changed)),
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changes.push((path.clone(), Change::Deleted));
        }
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    changes
}

/// `workspace/didChangeWatchedFiles` params for `changes`.
pub fn watched_files_params(changes: &[(PathBuf, Change)]) -> Value {
    let items: Vec<Value> = changes
        .iter()
        .map(|(path, kind)| json!({"uri": crate::pool::uri_for_path(path), "type": *kind as u8}))
        .collect();
    json!({ "changes": items })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn server(markers: &[&str], exts: &[&str]) -> ServerConfig {
        ServerConfig {
            command: "x".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            extensions: exts
                .iter()
                .map(|e| ((*e).to_owned(), "lang".to_owned()))
                .collect(),
            root_markers: markers.iter().map(|m| (*m).to_owned()).collect(),
            workspace: None,
            initialization_options: None,
            settings: None,
        }
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn excludes_hidden_builtin_and_configured_names() {
        let extra = vec!["BAK".to_owned()];
        assert!(is_excluded(".git", &extra));
        assert!(is_excluded("node_modules", &extra));
        assert!(is_excluded("target", &extra));
        assert!(is_excluded("BAK", &extra));
        assert!(!is_excluded("src", &extra));
    }

    #[test]
    fn discover_finds_topmost_roots_and_skips_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        // Cargo workspace with a member: only the top counts.
        touch(&ws.join("proj/Cargo.toml"));
        touch(&ws.join("proj/src/main.rs"));
        touch(&ws.join("proj/member/Cargo.toml"));
        touch(&ws.join("proj/member/src/lib.rs"));
        // Second project, deeper.
        touch(&ws.join("group/two/Cargo.toml"));
        touch(&ws.join("group/two/lib.rs"));
        // Excluded trees and a marker with no sources.
        touch(&ws.join("BAK/old/Cargo.toml"));
        touch(&ws.join("BAK/old/a.rs"));
        touch(&ws.join("node_modules/pkg/Cargo.toml"));
        touch(&ws.join("empty/Cargo.toml"));
        let config = LspConfig {
            warmup_exclude: vec!["BAK".to_owned()],
            ..LspConfig::default()
        };
        let found = discover(ws, &server(&["Cargo.toml"], &["rs"]), &config);
        let roots: Vec<PathBuf> = found.iter().map(|(r, _)| r.clone()).collect();
        // Breadth-first: the shallower project is found first.
        assert_eq!(roots, vec![ws.join("proj"), ws.join("group/two")]);
        assert_eq!(found[0].1, ws.join("proj/src/main.rs"));
        assert_eq!(found[1].1, ws.join("group/two/lib.rs"));
    }

    #[test]
    fn discover_respects_depth_and_markerless_servers() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        touch(&ws.join("a/b/c/go.mod"));
        touch(&ws.join("a/b/c/main.go"));
        let shallow = LspConfig {
            warmup_max_depth: 2,
            ..LspConfig::default()
        };
        assert!(discover(ws, &server(&["go.mod"], &["go"]), &shallow).is_empty());
        let deep = LspConfig::default();
        assert_eq!(discover(ws, &server(&["go.mod"], &["go"]), &deep).len(), 1);
        // No markers: the boundary itself is the root.
        let found = discover(ws, &server(&[], &["go"]), &deep);
        assert_eq!(found, vec![(ws.to_owned(), ws.join("a/b/c/main.go"))]);
        assert!(discover(ws, &server(&[], &["php"]), &deep).is_empty());
    }

    #[test]
    fn snapshot_tracks_sources_and_markers_only() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        touch(&ws.join("Cargo.toml"));
        touch(&ws.join("src/a.rs"));
        touch(&ws.join("README.md"));
        touch(&ws.join("target/debug/b.rs"));
        let snap = snapshot(ws, &server(&["Cargo.toml"], &["rs"]), &[]).unwrap();
        let mut paths: Vec<&PathBuf> = snap.keys().collect();
        paths.sort();
        assert_eq!(paths, vec![&ws.join("Cargo.toml"), &ws.join("src/a.rs")]);
    }

    #[test]
    fn diff_reports_created_changed_deleted() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_secs(1);
        let before: Snapshot = [
            (PathBuf::from("/w/a.rs"), t0),
            (PathBuf::from("/w/b.rs"), t0),
        ]
        .into();
        let after: Snapshot = [
            (PathBuf::from("/w/a.rs"), t1),
            (PathBuf::from("/w/c.rs"), t0),
        ]
        .into();
        assert_eq!(
            diff(&before, &after),
            vec![
                (PathBuf::from("/w/a.rs"), Change::Changed),
                (PathBuf::from("/w/b.rs"), Change::Deleted),
                (PathBuf::from("/w/c.rs"), Change::Created),
            ]
        );
        assert!(diff(&after, &after).is_empty());
        let params = watched_files_params(&diff(&before, &after));
        assert_eq!(params["changes"][0]["type"], 2);
        assert_eq!(params["changes"][1]["type"], 3);
        assert_eq!(params["changes"][2]["type"], 1);
        assert_eq!(params["changes"][2]["uri"], "file:///w/c.rs");
    }
    #[test]
    fn interleave_takes_turns() {
        assert_eq!(
            interleave(vec![vec![1, 2, 3], vec![10], vec![], vec![20, 21]]),
            vec![1, 10, 20, 2, 21, 3]
        );
        assert!(interleave::<u8>(vec![]).is_empty());
    }
}
