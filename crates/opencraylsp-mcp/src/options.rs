//! Command line and environment resolution.
//!
//! The flags are parsed by clap, but layering the environment underneath them
//! is done here with the environment injected, so precedence can be tested
//! without touching the process environment (`std::env::set_var` is unsafe in
//! edition 2024, and racy besides).

use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::Parser;

/// Shown by `--help`, after clap's own text.
pub const AFTER_HELP: &str = "\
LANGUAGES:
    rust (rs), go (golang), php, typescript (ts), javascript (js), python (py)
    all             every language a configured server provides
    auto (default)  detect from the workspace's project markers

`--languages` beats `OPENCRAYLSP_LANGUAGES`; with neither, the daemon detects the
languages from the workspace. Names are comma separated and case insensitive;
an unknown name exits with code 2 and lists the valid ones. Enable a language
here only when this agent needs it: the daemon starts a server on demand.

The daemon is started automatically on first use, so `opencraylspd serve` is never
required. `--socket` picks a different daemon and `--embedded` runs the pool
inside this process instead of talking to one. A language server must be on
PATH for its language to work; `opencraylspd doctor` checks that.

EXAMPLE:
    claude mcp add opencraylsp -- opencraylsp-mcp --languages rust,go

Documentation: https://github.com/amgio38/opencraylsp";

/// The flags a user types. `--help`/`--version` are clap's.
#[derive(Debug, Parser)]
#[command(
    name = "opencraylsp-mcp",
    version,
    about = "MCP stdio server for the opencraylspd shared language-server pool",
    after_help = AFTER_HELP
)]
pub struct Cli {
    /// Workspace root the tools may read (default: the current directory).
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,
    /// Languages to enable, comma separated (default: auto-detect).
    #[arg(long, value_name = "LIST")]
    pub languages: Option<String>,
    /// Override the daemon socket path.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Config file, read by `--embedded` or passed to a daemon this client
    /// starts.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Run the pool in this process instead of talking to a daemon.
    #[arg(long)]
    pub embedded: bool,
    /// Serve the in-process fake host (development only).
    #[arg(long, hide = true)]
    pub fake_host: bool,
}

/// The flags resolved against the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Canonical, existing workspace directory.
    pub workspace: PathBuf,
    /// Raw language input for `hello.languages`; `None` means auto-detect.
    pub languages: Option<Vec<String>>,
    pub socket: PathBuf,
    pub config: Option<PathBuf>,
    pub embedded: bool,
}

/// Layers the environment under `cli` and validates the result.
///
/// `env` is injected so a test can supply `OPENCRAYLSP_WORKSPACE`/`OPENCRAYLSP_LANGUAGES`/
/// `OPENCRAYLSP_SOCKET` without touching the process environment.
pub fn resolve(cli: &Cli, env: &dyn Fn(&str) -> Option<String>) -> Result<Options, String> {
    let workspace = resolve_workspace(cli, env)?;
    let socket = cli
        .socket
        .clone()
        .or_else(|| {
            env("OPENCRAYLSP_SOCKET")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(opencraylsp_proto::paths::default_socket_path);
    let raw = cli
        .languages
        .clone()
        .or_else(|| env("OPENCRAYLSP_LANGUAGES").filter(|s| !s.is_empty()));
    // `--config` (else `OPENCRAYLSP_CONFIG`) applies to both backends: embedded reads
    // it, and the daemon path forwards it to a daemon this client starts.
    let config = cli.config.clone().or_else(|| {
        env("OPENCRAYLSP_CONFIG")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    });
    Ok(Options {
        workspace,
        languages: parse_languages(raw.as_deref())?,
        socket,
        config,
        embedded: cli.embedded,
    })
}

fn resolve_workspace(cli: &Cli, env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    let raw = cli
        .workspace
        .clone()
        .or_else(|| {
            env("OPENCRAYLSP_WORKSPACE")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let canonical = std::fs::canonicalize(&raw)
        .map_err(|e| format!("error: workspace {} is not usable: {e}", raw.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "error: workspace {} is not a directory",
            raw.display()
        ));
    }
    Ok(canonical)
}

/// The canonical built-in language names, used only to render the error for a
/// mixed `auto`.
const BUILTIN_LANGUAGES: [&str; 6] = ["rust", "go", "php", "typescript", "javascript", "python"];

/// Splits, trims, lower-cases and de-duplicates the raw language input.
///
/// Returns `Ok(None)` (= auto-detect) when nothing was asked for. `all`
/// anywhere means "everything" and is sent through as the single token `all`,
/// exactly as the contract says: the daemon owns alias resolution.
///
/// `auto` is only meaningful on its own. Mixing it with another name is an
/// error, matching the daemon (`auto_mixed_with_a_language_is_rejected`),
/// rather than silently dropping it.
pub fn parse_languages(raw: Option<&str>) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let tokens: Vec<String> = raw
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return Ok(None);
    }
    let autos = tokens
        .iter()
        .filter(|token| token.as_str() == "auto")
        .count();
    if autos > 0 && autos != tokens.len() {
        let valid: Vec<String> = BUILTIN_LANGUAGES.iter().map(|s| (*s).to_owned()).collect();
        return Err(unknown_language_message("auto", &valid));
    }
    if autos == tokens.len() {
        return Ok(None);
    }
    if tokens.iter().any(|token| token == "all") {
        return Ok(Some(vec!["all".to_owned()]));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for token in tokens {
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    Ok(if out.is_empty() { None } else { Some(out) })
}

/// The exact stderr line for an unknown language.
///
/// `input` may be a bare name (from the daemon's structured data) or the
/// daemon's own `unknown language \`x\`; valid: …` message; either way the
/// result is one clean sentence, never a message wrapped twice.
pub fn unknown_language_message(input: &str, valid: &[String]) -> String {
    let mut list: Vec<String> = valid.to_vec();
    for extra in ["all", "auto"] {
        if !list.iter().any(|item| item == extra) {
            list.push(extra.to_owned());
        }
    }
    format!(
        "error: unknown language \"{}\"; valid: {} (aliases: ts, js, rs, golang, py)",
        offending_name(input),
        list.join(", ")
    )
}

/// The offending name, whether `input` is one or the daemon's whole sentence.
fn offending_name(input: &str) -> String {
    if input.contains("unknown language") {
        for quote in ['`', '"'] {
            if let Some(start) = input.find(quote)
                && let Some(end) = input[start + quote.len_utf8()..].find(quote)
            {
                return input[start + quote.len_utf8()..start + quote.len_utf8() + end].to_owned();
            }
        }
    }
    input.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> Cli {
        Cli::try_parse_from(["opencraylsp-mcp"]).expect("parses")
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn languages_flag_beats_the_environment() {
        let mut c = cli();
        c.languages = Some("rust".to_owned());
        let options = resolve(&c, &env_of(&[("OPENCRAYLSP_LANGUAGES", "go")])).expect("resolve");
        assert_eq!(options.languages, Some(vec!["rust".to_owned()]));
    }

    #[test]
    fn workspace_flag_beats_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cli();
        c.workspace = Some(dir.path().to_owned());
        let options =
            resolve(&c, &env_of(&[("OPENCRAYLSP_WORKSPACE", "/nonexistent")])).expect("resolve");
        assert_eq!(
            options.workspace,
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn an_unknown_flag_is_a_clap_error() {
        assert!(Cli::try_parse_from(["opencraylsp-mcp", "--nope"]).is_err());
    }

    #[test]
    fn a_missing_workspace_is_rejected() {
        let mut c = cli();
        c.workspace = Some(PathBuf::from("/definitely/not/here"));
        let err = resolve(&c, &env_of(&[])).expect_err("missing");
        assert!(err.starts_with("error: workspace"), "{err}");
    }

    #[test]
    fn a_file_is_not_a_workspace() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut c = cli();
        c.workspace = Some(file.path().to_owned());
        let err = resolve(&c, &env_of(&[])).expect_err("a file is not a directory");
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn config_comes_from_the_flag_then_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cli();
        c.workspace = Some(dir.path().to_owned());
        assert_eq!(resolve(&c, &env_of(&[])).unwrap().config, None);

        c.config = Some(PathBuf::from("/flag.toml"));
        assert_eq!(
            resolve(&c, &env_of(&[("OPENCRAYLSP_CONFIG", "/env.toml")]))
                .unwrap()
                .config,
            Some(PathBuf::from("/flag.toml"))
        );

        c.config = None;
        assert_eq!(
            resolve(&c, &env_of(&[("OPENCRAYLSP_CONFIG", "/env.toml")]))
                .unwrap()
                .config,
            Some(PathBuf::from("/env.toml"))
        );
    }

    #[test]
    fn the_socket_prefers_flag_then_env_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cli();
        c.workspace = Some(dir.path().to_owned());
        c.socket = Some(PathBuf::from("/tmp/flag.sock"));
        assert_eq!(
            resolve(&c, &env_of(&[("OPENCRAYLSP_SOCKET", "/tmp/env.sock")]))
                .unwrap()
                .socket,
            PathBuf::from("/tmp/flag.sock")
        );
        c.socket = None;
        assert_eq!(
            resolve(&c, &env_of(&[("OPENCRAYLSP_SOCKET", "/tmp/env.sock")]))
                .unwrap()
                .socket,
            PathBuf::from("/tmp/env.sock")
        );
        assert_eq!(
            resolve(&c, &env_of(&[])).unwrap().socket,
            opencraylsp_proto::paths::default_socket_path()
        );
    }

    #[test]
    fn languages_parse_table() {
        // (input, expected)
        let cases: Vec<(&str, Option<Vec<&str>>)> = vec![
            ("", None),
            ("   ", None),
            ("auto", None),
            ("AUTO", None),
            ("auto,auto", None),
            ("all", Some(vec!["all"])),
            ("rust,go", Some(vec!["rust", "go"])),
            ("rust, go ,rust", Some(vec!["rust", "go"])),
            ("Rust,GO", Some(vec!["rust", "go"])),
            ("ts", Some(vec!["ts"])),
            (",rust,,", Some(vec!["rust"])),
            ("rust,all", Some(vec!["all"])),
        ];
        for (input, want) in cases {
            let got = parse_languages(Some(input)).expect("valid input");
            let want = want.map(|v| v.into_iter().map(str::to_owned).collect::<Vec<_>>());
            assert_eq!(got, want, "input {input:?}");
        }
        assert_eq!(parse_languages(None), Ok(None));
    }

    #[test]
    fn auto_mixed_with_a_language_is_rejected() {
        for input in ["auto,rust", "rust,auto", "auto,go"] {
            let error = parse_languages(Some(input)).expect_err("mixed auto is an error");
            assert_eq!(
                error,
                unknown_language_message("auto", &BUILTIN_LANGUAGES.map(str::to_owned)),
                "input {input:?}"
            );
            assert!(error.contains("unknown language \"auto\""), "{error}");
        }
    }

    #[test]
    fn the_unknown_language_message_is_the_documented_one() {
        let valid = [
            "rust".to_owned(),
            "go".to_owned(),
            "php".to_owned(),
            "typescript".to_owned(),
            "javascript".to_owned(),
            "python".to_owned(),
        ];
        assert_eq!(
            unknown_language_message("klingon", &valid),
            "error: unknown language \"klingon\"; valid: rust, go, php, typescript, javascript, python, all, auto (aliases: ts, js, rs, golang, py)"
        );
    }
}
