//! Language names, aliases, per-connection selection and workspace detection
//! .
//!
//! A *language* here is a lower-case canonical name (`rust`, `go`, `php`,
//! `typescript`, `javascript`, `python`; a configured server may add its own).
//! One server may serve several languages — `typescript-language-server`
//! serves both `typescript` and `javascript` — so enabling either starts the
//! same instance.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::ServerConfig;

/// The languages the built-in presets cover, in display order.
pub const BUILTIN_LANGUAGES: [&str; 6] =
    ["rust", "go", "php", "typescript", "javascript", "python"];

/// Aliases accepted on the command line, shown in error messages.
pub const ALIAS_HELP: &str = "aliases: rs, golang, ts, js, py, ts/js, tsjs, node";

/// Directory names never entered while looking for project markers.
const SKIPPED_DIRS: [&str; 5] = ["node_modules", "target", "vendor", "dist", "build"];

/// How deep below the workspace project markers are looked for.
const DETECT_DEPTH: usize = 2;

/// Directory entries examined per directory, so a huge flat directory cannot
/// stall the handshake.
const DETECT_ENTRY_CAP: usize = 2_000;

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageSelection {
    /// Nothing declared: enable what the workspace's project markers show.
    Auto,
    /// `all`: every language a configured server provides.
    All,
    /// An explicit set of canonical names.
    Explicit(BTreeSet<String>),
}

/// A name that is neither a language nor an alias.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown language `{input}`; valid: {}, all, auto ({ALIAS_HELP})", .valid.join(", "))]
pub struct UnknownLanguage {
    pub input: String,
    pub valid: Vec<String>,
}

/// The canonical language of a file extension for the built-in set.
pub fn language_of_extension(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "go" => Some("go"),
        "php" => Some("php"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" | "pyi" => Some("python"),
        _ => None,
    }
}

/// The canonical language of `extension` for `server`: the built-in mapping,
/// else the server's own `languageId` for that extension (lower-cased), so a
/// custom server's languages can be selected by name too.
pub fn language_for(server: &ServerConfig, extension: &str) -> Option<String> {
    let ext = extension.to_ascii_lowercase();
    let language_id = server.extensions.get(&ext)?;
    Some(
        language_of_extension(&ext)
            .map(str::to_owned)
            .unwrap_or_else(|| language_id.to_ascii_lowercase()),
    )
}

/// Every canonical language `server` provides.
pub fn server_languages(server: &ServerConfig) -> BTreeSet<String> {
    server
        .extensions
        .keys()
        .filter_map(|ext| language_for(server, ext))
        .collect()
}

/// Every language provided by any of `servers`.
pub fn known_languages(servers: &BTreeMap<String, ServerConfig>) -> BTreeSet<String> {
    servers.values().flat_map(server_languages).collect()
}

/// The names shown as valid: the built-ins plus any custom ones.
pub fn valid_names(extra: &BTreeSet<String>) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_LANGUAGES.iter().map(|s| (*s).to_owned()).collect();
    names.extend(
        extra
            .iter()
            .filter(|n| !BUILTIN_LANGUAGES.contains(&n.as_str()))
            .cloned(),
    );
    names
}

fn expand_alias(token: &str) -> Option<Vec<&'static str>> {
    Some(match token {
        "rust" | "rs" => vec!["rust"],
        "go" | "golang" => vec!["go"],
        "php" => vec!["php"],
        "typescript" | "ts" => vec!["typescript"],
        "javascript" | "js" => vec!["javascript"],
        "python" | "py" => vec!["python"],
        "tsjs" | "ts/js" | "js/ts" | "node" | "nodejs" => vec!["typescript", "javascript"],
        _ => return None,
    })
}

/// Turns raw user input (as sent in `hello.languages`) into a selection.
///
/// `None`, an empty list, or only blanks/`auto` mean [`LanguageSelection::Auto`].
/// `all` anywhere means [`LanguageSelection::All`]. Entries may themselves
/// hold comma-separated names. `extra_known` lists custom language names
/// provided by configured servers, which are accepted as-is.
pub fn normalize(
    input: Option<&[String]>,
    extra_known: &BTreeSet<String>,
) -> Result<LanguageSelection, UnknownLanguage> {
    let Some(entries) = input else {
        return Ok(LanguageSelection::Auto);
    };
    let tokens: Vec<String> = entries
        .iter()
        .flat_map(|entry| entry.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() || tokens.iter().all(|t| t == "auto") {
        return Ok(LanguageSelection::Auto);
    }
    if tokens.iter().any(|t| t == "all") {
        return Ok(LanguageSelection::All);
    }
    let mut set = BTreeSet::new();
    for token in &tokens {
        if let Some(names) = expand_alias(token) {
            set.extend(names.into_iter().map(str::to_owned));
        } else if extra_known.contains(token) {
            set.insert(token.clone());
        } else {
            return Err(UnknownLanguage {
                input: token.clone(),
                valid: valid_names(extra_known),
            });
        }
    }
    Ok(LanguageSelection::Explicit(set))
}

/// Which languages the project markers under `workspace` indicate.
///
/// Looks for any server's `root_markers` in the workspace and in directories
/// up to two levels below it (hidden directories and `node_modules`,
/// `target`, `vendor`, `dist`, `build` are skipped). A server whose markers
/// are found contributes all the languages it provides.
pub fn detect(workspace: &Path, servers: &BTreeMap<String, ServerConfig>) -> BTreeSet<String> {
    let wanted: BTreeSet<&str> = servers
        .values()
        .flat_map(|s| s.root_markers.iter().map(String::as_str))
        .collect();
    let found = find_markers(workspace, &wanted);
    servers
        .values()
        .filter(|s| s.root_markers.iter().any(|m| found.contains(m.as_str())))
        .flat_map(server_languages)
        .collect()
}

/// The subset of `wanted` present in `workspace` or up to [`DETECT_DEPTH`]
/// levels below.
fn find_markers<'a>(workspace: &Path, wanted: &BTreeSet<&'a str>) -> BTreeSet<&'a str> {
    let mut found = BTreeSet::new();
    let mut level = vec![workspace.to_owned()];
    for depth in 0..=DETECT_DEPTH {
        let mut next = Vec::new();
        for dir in &level {
            for marker in wanted {
                if !found.contains(marker) && dir.join(marker).exists() {
                    found.insert(*marker);
                }
            }
            if depth == DETECT_DEPTH || found.len() == wanted.len() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten().take(DETECT_ENTRY_CAP) {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir && !name.starts_with('.') && !SKIPPED_DIRS.contains(&name.as_ref()) {
                    next.push(entry.path());
                }
            }
        }
        level = next;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::presets;

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn set(items: &[&str]) -> LanguageSelection {
        LanguageSelection::Explicit(items.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn normalize_table() {
        let cases: Vec<(Option<Vec<&str>>, LanguageSelection)> = vec![
            (None, LanguageSelection::Auto),
            (Some(vec![]), LanguageSelection::Auto),
            (Some(vec![""]), LanguageSelection::Auto),
            (Some(vec!["auto"]), LanguageSelection::Auto),
            (Some(vec![" , "]), LanguageSelection::Auto),
            (Some(vec!["all"]), LanguageSelection::All),
            (Some(vec!["ALL"]), LanguageSelection::All),
            (Some(vec!["rust", "all"]), LanguageSelection::All),
            (Some(vec!["ts"]), set(&["typescript"])),
            (Some(vec!["TS", "js"]), set(&["typescript", "javascript"])),
            (Some(vec!["tsjs"]), set(&["typescript", "javascript"])),
            (Some(vec!["ts/js"]), set(&["typescript", "javascript"])),
            (Some(vec!["node"]), set(&["typescript", "javascript"])),
            (Some(vec!["rs", "GOLANG"]), set(&["rust", "go"])),
            (Some(vec!["rust", "rust"]), set(&["rust"])),
            (Some(vec![" go , php "]), set(&["go", "php"])),
            (Some(vec!["rust,go"]), set(&["rust", "go"])),
            (Some(vec!["py"]), set(&["python"])),
        ];
        for (input, expected) in cases {
            let owned = input.as_ref().map(|v| strings(v));
            assert_eq!(
                normalize(owned.as_deref(), &none()),
                Ok(expected),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn unknown_language_lists_valid_names_and_aliases() {
        let error = normalize(Some(&strings(&["rust", "klingon"])), &none()).unwrap_err();
        assert_eq!(error.input, "klingon");
        assert_eq!(error.valid, strings(&BUILTIN_LANGUAGES));
        let text = error.to_string();
        assert!(
            text.contains("klingon") && text.contains("valid: rust, go"),
            "{text}"
        );
        assert!(text.contains("ts/js"), "{text}");
    }

    #[test]
    fn auto_mixed_with_a_language_is_rejected() {
        let error = normalize(Some(&strings(&["auto", "rust"])), &none()).unwrap_err();
        assert_eq!(error.input, "auto");
    }

    #[test]
    fn custom_languages_from_configured_servers_are_accepted() {
        let extra: BTreeSet<String> = ["zig".to_owned()].into();
        assert_eq!(
            normalize(Some(&strings(&["zig", "rs"])), &extra),
            Ok(set(&["rust", "zig"]))
        );
        assert!(valid_names(&extra).contains(&"zig".to_owned()));
    }

    #[test]
    fn extension_mapping_covers_the_builtin_languages() {
        for (ext, lang) in [
            ("rs", "rust"),
            ("go", "go"),
            ("php", "php"),
            ("ts", "typescript"),
            ("TSX", "typescript"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("js", "javascript"),
            ("jsx", "javascript"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
            ("py", "python"),
            ("pyi", "python"),
        ] {
            assert_eq!(language_of_extension(ext), Some(lang), "{ext}");
        }
        assert_eq!(language_of_extension("zig"), None);
    }

    #[test]
    fn typescript_preset_serves_both_typescript_and_javascript() {
        let presets = presets();
        let ts = &presets["typescript-language-server"];
        assert_eq!(
            server_languages(ts),
            ["javascript".to_owned(), "typescript".to_owned()].into()
        );
        assert_eq!(language_for(ts, "tsx").as_deref(), Some("typescript"));
        assert_eq!(language_for(ts, "jsx").as_deref(), Some("javascript"));
        assert_eq!(language_for(ts, "rs"), None);
        assert_eq!(
            known_languages(&presets),
            BUILTIN_LANGUAGES.iter().map(|s| (*s).to_owned()).collect()
        );
    }

    #[test]
    fn custom_server_uses_its_language_id() {
        let mut server = presets()["gopls"].clone();
        server.extensions = [("zig".to_owned(), "Zig".to_owned())].into();
        assert_eq!(language_for(&server, "zig").as_deref(), Some("zig"));
        assert_eq!(server_languages(&server), ["zig".to_owned()].into());
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn detected(setup: impl FnOnce(&Path)) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path());
        detect(dir.path(), &presets()).into_iter().collect()
    }

    #[test]
    fn detect_table() {
        assert_eq!(detected(|d| touch(&d.join("Cargo.toml"))), ["rust"]);
        assert_eq!(
            detected(|d| {
                touch(&d.join("Cargo.toml"));
                touch(&d.join("go.mod"));
            }),
            ["go", "rust"]
        );
        assert_eq!(
            detected(|d| touch(&d.join("package.json"))),
            ["javascript", "typescript"]
        );
        assert_eq!(detected(|d| touch(&d.join("composer.json"))), ["php"]);
        assert_eq!(detected(|d| touch(&d.join("pyproject.toml"))), ["python"]);
        assert!(
            detected(|_| {}).is_empty(),
            "an empty directory shows nothing"
        );
    }

    #[test]
    fn detect_looks_two_levels_down_but_not_three() {
        assert_eq!(detected(|d| touch(&d.join("a/Cargo.toml"))), ["rust"]);
        assert_eq!(detected(|d| touch(&d.join("a/b/go.mod"))), ["go"]);
        assert!(detected(|d| touch(&d.join("a/b/c/Cargo.toml"))).is_empty());
    }

    #[test]
    fn detect_skips_hidden_and_vendored_directories() {
        for skipped in [
            ".git",
            ".hidden",
            "node_modules",
            "target",
            "vendor",
            "dist",
            "build",
        ] {
            assert!(
                detected(|d| touch(&d.join(skipped).join("Cargo.toml"))).is_empty(),
                "{skipped} must not be entered"
            );
        }
    }

    #[test]
    fn detect_ignores_marker_directories_and_symlinked_dirs() {
        // A *directory* named like a marker still counts as present (exists()),
        // but a symlinked directory is not descended into.
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        touch(&elsewhere.path().join("Cargo.toml"));
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("link")).unwrap();
        assert!(detect(dir.path(), &presets()).is_empty());
    }

    #[test]
    fn detect_on_a_missing_workspace_is_empty() {
        assert!(detect(Path::new("/definitely/not/here"), &presets()).is_empty());
    }
}
