//! opencraylspd configuration (`~/.config/opencraylsp/config.toml`).
//!
//! Layout :
//!
//! ```toml
//! allowed_roots = ["/srv/shared"]      # extra directories files may be opened from
//! warmup = false
//! watch_interval_ms = 3000
//!
//! [limits]                             # every key optional
//! max_instances = 8
//! max_rss_mb = 8192
//! idle_shutdown_secs = 900
//!
//! [[server]]                           # same `name` as a preset replaces it
//! name = "rust-analyzer"
//! command = "rust-analyzer"
//! extensions = { rs = "rust" }
//! root_markers = ["Cargo.toml"]
//! ```
//!
//! Built-in presets cover the supported languages; a preset is only *usable*
//! when its command is on `PATH` (see [`crate::manager::command_exists`]), and
//! a user `[[server]]` with the same name replaces it wholesale. Unknown keys
//! and out-of-range values are errors that name the offending key (and, for
//! syntax problems, the line) — a config that silently ignores a typo would
//! leave the operator debugging the wrong thing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_WRITE_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_RESTARTS: u32 = 3;
const DEFAULT_STARTUP_GRACE_MS: u64 = 3_000;
const DEFAULT_IDLE_SHUTDOWN_SECS: u64 = 900;
const DEFAULT_DIAGNOSTICS_SETTLE_MS: u64 = 1_500;
const DEFAULT_DIAGNOSTICS_TIMEOUT_MS: u64 = 20_000;
const DEFAULT_MAX_RESULTS: usize = 100;
const DEFAULT_MAX_INSTANCES: usize = 8;
const DEFAULT_MAX_RSS_MB: u64 = 8_192;

/// An idle daemon measures around 6.7 MB; 512 MiB is a runaway guard, not a
/// working budget. See [`LspConfig::daemon_max_rss_mb`].
const DEFAULT_DAEMON_MAX_RSS_MB: u64 = 512;
const DEFAULT_MAX_OPEN_DOCS: usize = 256;
const DEFAULT_MEMORY_SAMPLE_MS: u64 = 5_000;
const DEFAULT_WATCH_INTERVAL_MS: u64 = 3_000;
const DEFAULT_WARMUP_MAX_DEPTH: usize = 4;
const DEFAULT_WARMUP_MAX_INSTANCES: usize = 16;

/// Why a config could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The file named explicitly (`--config`) does not exist.
    #[error("config file `{0}` does not exist")]
    Missing(String),
    /// The file exists but could not be read.
    #[error("cannot read config file `{path}`: {reason}")]
    Unreadable { path: String, reason: String },
    /// TOML syntax error or unknown key; the message carries line and column.
    #[error("invalid config: {0}")]
    Syntax(String),
    /// A value is out of range or a server entry is unusable.
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct LspConfig {
    pub startup_timeout_ms: u64,
    /// How long after start a server that has reported no progress yet still
    /// counts as starting up, so its first empty answers are reported as
    /// `indexing` rather than trusted. `0` disables the grace period.
    pub startup_grace_ms: u64,
    pub request_timeout_ms: u64,
    /// Upper bound on one write to a language server's stdin. A server that
    /// stops reading fills the pipe and parks every writer forever, so this
    /// bounds each write; a timed-out write leaves a partial frame, and the
    /// instance is restarted to resynchronise.
    pub write_timeout_ms: u64,
    /// How many times a crashed server is restarted before it is refused until
    /// opencraylspd restarts — without a cap every query after a crash would spawn a
    /// fresh child.
    pub max_restarts: u32,
    /// A server with no request for this long is shut down to give its memory
    /// back (rust-analyzer can hold gigabytes). `0` disables idle shutdown.
    pub idle_shutdown_secs: u64,
    pub diagnostics_settle_ms: u64,
    pub diagnostics_timeout_ms: u64,
    /// Cap on listed references / symbols; the rest is summarized as a count.
    pub max_results: usize,
    /// Most language-server instances alive at once.
    pub max_instances: usize,
    /// Per-instance resident-memory ceiling (process tree), in MiB, applied to
    /// a server at rest. While a server is indexing the ceiling is doubled:
    /// rust-analyzer measured at a 7.5 GiB peak settling to 5.1 GiB on a large
    /// workspace, and restarting it mid-index would only start the peak over.
    pub max_rss_mb: u64,
    /// Ceiling on the daemon's *own* resident memory, in MiB.
    ///
    /// An idle daemon measures around 6.7 MB, so this is a runaway guard rather
    /// than a working budget: the daemon shuts down once it is exceeded and the
    /// client starts a fresh one. It exists because a leak in the daemon itself
    /// is invisible to `max_rss_mb`, which only governs language servers.
    pub daemon_max_rss_mb: u64,
    /// How often instance memory is sampled.
    pub memory_sample_ms: u64,
    /// Most documents kept open per instance.
    pub max_open_docs: usize,
    /// Extra directories, besides the workspace boundary, whose files may be
    /// opened.
    pub allowed_roots: Vec<PathBuf>,
    /// Start every discovered (server, project root) in the background as
    /// soon as possible so indexing is under way before the first question.
    pub warmup: bool,
    /// How often running servers are told about files that changed on disk
    /// (`workspace/didChangeWatchedFiles`). `0` disables the watcher.
    pub watch_interval_ms: u64,
    /// How deep below the boundary warm-up looks for project root markers.
    pub warmup_max_depth: usize,
    /// Upper bound on servers warm-up starts.
    pub warmup_max_instances: usize,
    /// Directory names skipped by warm-up discovery and the watcher, on top
    /// of the built-in ones (hidden directories, `node_modules`, `target`, …).
    pub warmup_exclude: Vec<String>,
    /// Keyed by server name.
    pub servers: BTreeMap<String, ServerConfig>,
}

impl Default for LspConfig {
    /// Defaults with **no** servers; use [`LspConfig::from_toml_str`] or
    /// [`LspConfig::load`] to get the built-in presets.
    fn default() -> Self {
        Self {
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            startup_grace_ms: DEFAULT_STARTUP_GRACE_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            write_timeout_ms: DEFAULT_WRITE_TIMEOUT_MS,
            max_restarts: DEFAULT_MAX_RESTARTS,
            idle_shutdown_secs: DEFAULT_IDLE_SHUTDOWN_SECS,
            diagnostics_settle_ms: DEFAULT_DIAGNOSTICS_SETTLE_MS,
            diagnostics_timeout_ms: DEFAULT_DIAGNOSTICS_TIMEOUT_MS,
            max_results: DEFAULT_MAX_RESULTS,
            max_instances: DEFAULT_MAX_INSTANCES,
            max_rss_mb: DEFAULT_MAX_RSS_MB,
            daemon_max_rss_mb: DEFAULT_DAEMON_MAX_RSS_MB,
            memory_sample_ms: DEFAULT_MEMORY_SAMPLE_MS,
            max_open_docs: DEFAULT_MAX_OPEN_DOCS,
            allowed_roots: Vec::new(),
            warmup: false,
            watch_interval_ms: DEFAULT_WATCH_INTERVAL_MS,
            warmup_max_depth: DEFAULT_WARMUP_MAX_DEPTH,
            warmup_max_instances: DEFAULT_WARMUP_MAX_INSTANCES,
            warmup_exclude: Vec::new(),
            servers: BTreeMap::new(),
        }
    }
}

/// One language server.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Executable to spawn (looked up on `PATH` when not absolute).
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the child.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// File extension (without the dot) → LSP `languageId`.
    pub extensions: BTreeMap<String, String>,
    /// Files whose presence marks a project root, e.g. `Cargo.toml`. The
    /// topmost directory holding one, at or below the boundary, becomes the
    /// server's root. Empty means "the boundary itself".
    #[serde(default)]
    pub root_markers: Vec<String>,
    /// Overrides the workspace boundary for this server only.
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    /// Sent verbatim as `initializationOptions`.
    #[serde(default)]
    pub initialization_options: Option<Value>,
    /// Returned for `workspace/configuration` requests; `null`s when absent.
    #[serde(default)]
    pub settings: Option<Value>,
}

/// A `[[server]]` table. Spelled out field by field rather than flattening
/// [`ServerConfig`]: serde cannot combine `flatten` with `deny_unknown_fields`,
/// and a typo in a server entry must be an error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    extensions: BTreeMap<String, String>,
    #[serde(default)]
    root_markers: Vec<String>,
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    initialization_options: Option<Value>,
    #[serde(default)]
    settings: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    startup_timeout_ms: Option<u64>,
    startup_grace_ms: Option<u64>,
    request_timeout_ms: Option<u64>,
    write_timeout_ms: Option<u64>,
    max_restarts: Option<u32>,
    idle_shutdown_secs: Option<u64>,
    diagnostics_settle_ms: Option<u64>,
    diagnostics_timeout_ms: Option<u64>,
    max_results: Option<usize>,
    max_instances: Option<usize>,
    max_rss_mb: Option<u64>,
    daemon_max_rss_mb: Option<u64>,
    memory_sample_ms: Option<u64>,
    max_open_docs: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    limits: RawLimits,
    #[serde(default)]
    allowed_roots: Vec<PathBuf>,
    warmup: Option<bool>,
    watch_interval_ms: Option<u64>,
    warmup_max_depth: Option<usize>,
    warmup_max_instances: Option<usize>,
    #[serde(default)]
    warmup_exclude: Vec<String>,
    #[serde(default, rename = "server")]
    servers: Vec<RawServer>,
}

/// The built-in server presets, keyed by server name.
pub fn presets() -> BTreeMap<String, ServerConfig> {
    fn server(
        command: &str,
        args: &[&str],
        extensions: &[(&str, &str)],
        markers: &[&str],
    ) -> ServerConfig {
        ServerConfig {
            command: command.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: BTreeMap::new(),
            extensions: extensions
                .iter()
                .map(|(e, l)| ((*e).to_owned(), (*l).to_owned()))
                .collect(),
            root_markers: markers.iter().map(|m| (*m).to_owned()).collect(),
            workspace: None,
            initialization_options: None,
            settings: None,
        }
    }
    let mut rust = server("rust-analyzer", &[], &[("rs", "rust")], &["Cargo.toml"]);
    // Without this rust-analyzer only sees files a client opened; its own
    // watcher keeps the index fresh for edits made by other tools.
    rust.initialization_options = Some(serde_json::json!({"files": {"watcher": "server"}}));
    let typescript = server(
        "typescript-language-server",
        &["--stdio"],
        &[
            ("ts", "typescript"),
            ("tsx", "typescriptreact"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
        ],
        &["tsconfig.json", "jsconfig.json", "package.json"],
    );
    BTreeMap::from([
        ("rust-analyzer".to_owned(), rust),
        (
            "gopls".to_owned(),
            server("gopls", &[], &[("go", "go")], &["go.work", "go.mod"]),
        ),
        (
            "intelephense".to_owned(),
            server(
                "intelephense",
                &["--stdio"],
                &[("php", "php")],
                &["composer.json"],
            ),
        ),
        ("typescript-language-server".to_owned(), typescript),
        (
            "pyright-langserver".to_owned(),
            server(
                "pyright-langserver",
                &["--stdio"],
                &[("py", "python"), ("pyi", "python")],
                &["pyproject.toml", "setup.py", "requirements.txt"],
            ),
        ),
    ])
}

impl LspConfig {
    /// Parses `src` and layers it over the defaults and the built-in presets.
    pub fn from_toml_str(src: &str) -> Result<Self, ConfigError> {
        Self::parse(src, true)
    }

    /// Like [`Self::from_toml_str`] but without the built-in presets: only the
    /// servers `src` declares exist. Used where a test needs exact control.
    pub fn from_toml_str_without_presets(src: &str) -> Result<Self, ConfigError> {
        Self::parse(src, false)
    }

    fn parse(src: &str, with_presets: bool) -> Result<Self, ConfigError> {
        let raw: RawConfig =
            toml::from_str(src).map_err(|err| ConfigError::Syntax(err.to_string()))?;
        let mut config = Self::default();
        if with_presets {
            config.servers = presets();
        }
        let limits = raw.limits;
        config.startup_timeout_ms = positive(
            "limits.startup_timeout_ms",
            limits.startup_timeout_ms,
            config.startup_timeout_ms,
        )?;
        config.request_timeout_ms = positive(
            "limits.request_timeout_ms",
            limits.request_timeout_ms,
            config.request_timeout_ms,
        )?;
        config.write_timeout_ms = positive(
            "limits.write_timeout_ms",
            limits.write_timeout_ms,
            config.write_timeout_ms,
        )?;
        config.startup_grace_ms = limits.startup_grace_ms.unwrap_or(config.startup_grace_ms);
        config.max_restarts = limits.max_restarts.unwrap_or(config.max_restarts);
        config.idle_shutdown_secs = limits
            .idle_shutdown_secs
            .unwrap_or(config.idle_shutdown_secs);
        config.diagnostics_settle_ms = positive(
            "limits.diagnostics_settle_ms",
            limits.diagnostics_settle_ms,
            config.diagnostics_settle_ms,
        )?;
        config.diagnostics_timeout_ms = positive(
            "limits.diagnostics_timeout_ms",
            limits.diagnostics_timeout_ms,
            config.diagnostics_timeout_ms,
        )?;
        config.max_results = positive(
            "limits.max_results",
            limits.max_results.map(|n| n as u64),
            config.max_results as u64,
        )? as usize;
        config.max_instances = positive(
            "limits.max_instances",
            limits.max_instances.map(|n| n as u64),
            config.max_instances as u64,
        )? as usize;
        config.max_rss_mb = positive("limits.max_rss_mb", limits.max_rss_mb, config.max_rss_mb)?;
        config.memory_sample_ms = positive(
            "limits.memory_sample_ms",
            limits.memory_sample_ms,
            config.memory_sample_ms,
        )?;
        config.daemon_max_rss_mb = positive(
            "limits.daemon_max_rss_mb",
            limits.daemon_max_rss_mb,
            config.daemon_max_rss_mb,
        )?;
        config.max_open_docs = positive(
            "limits.max_open_docs",
            limits.max_open_docs.map(|n| n as u64),
            config.max_open_docs as u64,
        )? as usize;
        config.allowed_roots = raw.allowed_roots;
        config.warmup = raw.warmup.unwrap_or(config.warmup);
        config.watch_interval_ms = raw.watch_interval_ms.unwrap_or(config.watch_interval_ms);
        config.warmup_max_depth = positive(
            "warmup_max_depth",
            raw.warmup_max_depth.map(|n| n as u64),
            config.warmup_max_depth as u64,
        )? as usize;
        config.warmup_max_instances = positive(
            "warmup_max_instances",
            raw.warmup_max_instances.map(|n| n as u64),
            config.warmup_max_instances as u64,
        )? as usize;
        config.warmup_exclude = raw.warmup_exclude;
        for raw_server in raw.servers {
            let name = raw_server.name;
            let mut server = ServerConfig {
                command: raw_server.command,
                args: raw_server.args,
                env: raw_server.env,
                extensions: raw_server.extensions,
                root_markers: raw_server.root_markers,
                workspace: raw_server.workspace,
                initialization_options: raw_server.initialization_options,
                settings: raw_server.settings,
            };
            if name.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "a [[server]] has an empty `name`".to_owned(),
                ));
            }
            if server.command.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "[[server]] `{name}` has an empty `command`"
                )));
            }
            // Accept ".rs" as well as "rs": the dot is a common slip and harmless.
            server.extensions = server
                .extensions
                .into_iter()
                .map(|(ext, lang)| (ext.trim_start_matches('.').to_owned(), lang))
                .filter(|(ext, _)| !ext.is_empty())
                .collect();
            if server.extensions.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "[[server]] `{name}` maps no `extensions`"
                )));
            }
            config.servers.insert(name, server);
        }
        Ok(config)
    }

    /// Loads the config file.
    ///
    /// `explicit` (from `--config`) must exist. Without it the default path
    /// ([`default_config_path`]) is read if present; an absent default file
    /// simply means "presets and defaults".
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let (path, required) = match explicit {
            Some(path) => (path.to_owned(), true),
            None => match default_config_path() {
                Some(path) => (path, false),
                None => return Self::from_toml_str(""),
            },
        };
        // The config decides which programs are executed and which
        // directories may be read, so a file another user could write is as
        // good as code injection. Checked before it is parsed.
        check_private(&path)?;
        match std::fs::read_to_string(&path) {
            Ok(src) => Self::from_toml_str(&src),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => {
                Self::from_toml_str("")
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(ConfigError::Missing(path.display().to_string()))
            }
            Err(err) => Err(ConfigError::Unreadable {
                path: path.display().to_string(),
                reason: err.to_string(),
            }),
        }
    }

    /// The server that owns files with `extension` (no leading dot), and the
    /// LSP `languageId` it maps to.
    ///
    /// When two servers claim the same extension the one whose name sorts
    /// first wins — deterministic, and warned about once at parse time would
    /// need state, so the rule is documented here instead.
    pub fn server_for_extension(&self, extension: &str) -> Option<(&str, &ServerConfig, &str)> {
        self.servers.iter().find_map(|(name, server)| {
            server
                .extensions
                .get(extension)
                .map(|language| (name.as_str(), server, language.as_str()))
        })
    }
}

/// `$XDG_CONFIG_HOME/opencraylsp/config.toml`, else `$HOME/.config/opencraylsp/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    config_path_from(&|key| std::env::var(key).ok())
}

/// [`default_config_path`] with the environment injected.
pub fn config_path_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let non_empty = |key: &str| env(key).filter(|v| !v.is_empty());
    if let Some(xdg) = non_empty("XDG_CONFIG_HOME") {
        return Some(Path::new(&xdg).join("opencraylsp").join("config.toml"));
    }
    non_empty("HOME").map(|home| {
        Path::new(&home)
            .join(".config")
            .join("opencraylsp")
            .join("config.toml")
    })
}

/// Refuses a config file that is not private to its owner.
///
/// Two conditions, both required:
/// * it must be owned by the user running the daemon, and
/// * it must not be writable by group or other.
///
/// A config names the programs to execute (`[[server]] command`) and the extra
/// directories that may be opened (`allowed_roots`), so anyone who can write
/// it chooses what runs as this user. World-writable config files in a shared
/// home or a world-writable checkout are a realistic way to reach that, so the
/// check is on the file rather than on its directory.
fn check_private(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        // A file that does not exist is `load`'s business (an absent default is
        // fine, a missing `--config` is an error), not this one's.
        Err(_) => return Ok(()),
    };
    // With the uid undiscoverable there is nothing to compare against, so the
    // ownership half is skipped; the permission half still applies.
    if let Some(me) = uid_of_current_user()
        && meta.uid() != me
    {
        return Err(ConfigError::Invalid(format!(
            "config file `{}` is owned by uid {} but the daemon runs as uid {me}; \
             refusing to read a config another user can control",
            path.display(),
            meta.uid()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(ConfigError::Invalid(format!(
            "config file `{}` is mode {:o}, which lets group or other write it; \
             the config chooses which programs to run, so run `chmod 600 {}`",
            path.display(),
            meta.mode() & 0o777,
            path.display()
        )));
    }
    Ok(())
}

/// The uid of the current process, or `None` when it cannot be determined.
///
/// Deliberately local: `opencraylsp-core` must not depend on the daemon's socket
/// policy to check a file's owner.
fn uid_of_current_user() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .or_else(|_| std::fs::metadata(std::env::var_os("HOME").unwrap_or_default()))
        .ok()
        .map(|m| m.uid())
}

fn positive(key: &str, value: Option<u64>, default: u64) -> Result<u64, ConfigError> {
    match value {
        None => Ok(default),
        Some(0) => Err(ConfigError::Invalid(format!(
            "`{key}` must be greater than zero"
        ))),
        Some(n) => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(src: &str) -> LspConfig {
        LspConfig::from_toml_str_without_presets(src).expect("config parses")
    }

    fn err(src: &str) -> String {
        LspConfig::from_toml_str_without_presets(src)
            .expect_err("config must be rejected")
            .to_string()
    }

    #[test]
    fn empty_source_gives_defaults_without_servers() {
        let config = parse("");
        assert_eq!(config, LspConfig::default());
        assert!(config.servers.is_empty());
        assert_eq!(config.idle_shutdown_secs, 900);
        assert_eq!(config.max_instances, 8);
        assert_eq!(config.write_timeout_ms, 10_000);
        assert_eq!(config.max_rss_mb, 8192);
        assert_eq!(config.memory_sample_ms, 5000);
        // An idle daemon measures around 6.7 MB, so 512 MiB is a ceiling meant
        // to catch a runaway, not a working budget.
        assert_eq!(config.daemon_max_rss_mb, 512);
        assert_eq!(config.max_open_docs, 256);
        assert_eq!(config.startup_timeout_ms, 60_000);
        assert_eq!(config.startup_grace_ms, 3_000);
    }

    #[test]
    fn with_presets_the_five_servers_exist() {
        let config = LspConfig::from_toml_str("").unwrap();
        let names: Vec<&str> = config.servers.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "gopls",
                "intelephense",
                "pyright-langserver",
                "rust-analyzer",
                "typescript-language-server"
            ]
        );
        let ts = &config.servers["typescript-language-server"];
        assert_eq!(ts.args, vec!["--stdio"]);
        assert_eq!(ts.extensions["tsx"], "typescriptreact");
        assert_eq!(ts.extensions["mjs"], "javascript");
        assert_eq!(
            config.servers["rust-analyzer"].initialization_options,
            Some(json!({"files": {"watcher": "server"}}))
        );
        assert_eq!(
            config.servers["gopls"].root_markers,
            vec!["go.work", "go.mod"]
        );
    }

    #[test]
    fn full_table_parses() {
        let config = parse(
            r#"
            allowed_roots = ["/opt/shared", "/srv/x"]
            warmup = true
            watch_interval_ms = 0
            warmup_max_depth = 2
            warmup_max_instances = 3
            warmup_exclude = ["BAK"]

            [limits]
            startup_timeout_ms = 5000
            startup_grace_ms = 0
            request_timeout_ms = 7000
            write_timeout_ms = 4000
            max_restarts = 0
            idle_shutdown_secs = 0
            diagnostics_settle_ms = 100
            diagnostics_timeout_ms = 900
            max_results = 10
            max_instances = 2
            max_rss_mb = 512
            memory_sample_ms = 250
            daemon_max_rss_mb = 256
            max_open_docs = 16

            [[server]]
            name = "rust"
            command = "rust-analyzer"
            args = ["--log-file", "x"]
            extensions = { ".rs" = "rust" }
            root_markers = ["Cargo.toml"]
            workspace = "/ws"
            env = { RA_LOG = "info" }
            initialization_options = { files = { watcher = "server" } }
            settings = { a = 1 }
            "#,
        );
        assert_eq!(config.startup_timeout_ms, 5000);
        assert_eq!(config.startup_grace_ms, 0);
        assert_eq!(config.request_timeout_ms, 7000);
        assert_eq!(config.write_timeout_ms, 4000);
        assert_eq!(config.max_restarts, 0);
        assert_eq!(config.idle_shutdown_secs, 0);
        assert_eq!(config.diagnostics_settle_ms, 100);
        assert_eq!(config.diagnostics_timeout_ms, 900);
        assert_eq!(config.max_results, 10);
        assert_eq!(config.max_instances, 2);
        assert_eq!(config.max_rss_mb, 512);
        assert_eq!(config.memory_sample_ms, 250);
        assert_eq!(config.daemon_max_rss_mb, 256);
        assert_eq!(config.max_open_docs, 16);
        assert_eq!(
            config.allowed_roots,
            vec![PathBuf::from("/opt/shared"), PathBuf::from("/srv/x")]
        );
        assert!(config.warmup);
        assert_eq!(config.watch_interval_ms, 0);
        assert_eq!(config.warmup_max_depth, 2);
        assert_eq!(config.warmup_max_instances, 3);
        assert_eq!(config.warmup_exclude, vec!["BAK"]);
        let server = &config.servers["rust"];
        assert_eq!(server.command, "rust-analyzer");
        assert_eq!(server.args, vec!["--log-file", "x"]);
        assert_eq!(server.extensions["rs"], "rust", "leading dot is trimmed");
        assert_eq!(server.workspace, Some(PathBuf::from("/ws")));
        assert_eq!(server.env["RA_LOG"], "info");
        assert_eq!(server.settings, Some(json!({"a": 1})));
    }

    #[test]
    fn user_server_replaces_the_preset_with_the_same_name() {
        let config = LspConfig::from_toml_str(
            r#"
            [[server]]
            name = "gopls"
            command = "/opt/gopls"
            extensions = { go = "go" }
            "#,
        )
        .unwrap();
        let gopls = &config.servers["gopls"];
        assert_eq!(gopls.command, "/opt/gopls");
        assert!(
            gopls.root_markers.is_empty(),
            "replaced wholesale, not merged"
        );
        assert_eq!(config.servers.len(), 5, "other presets are untouched");
    }

    #[test]
    fn unknown_keys_are_rejected_with_their_name() {
        for (src, needle) in [
            ("bogus = 1", "bogus"),
            ("[limits]\nmax_instancez = 3", "max_instancez"),
            (
                "[[server]]\nname='a'\ncommand='x'\nextensions={a='b'}\nnope=1",
                "nope",
            ),
        ] {
            let message = err(src);
            assert!(message.contains(needle), "{message}");
        }
    }

    #[test]
    fn syntax_errors_carry_the_line() {
        let message = err("warmup = true\n\n[limits\nmax_instances = 3");
        assert!(message.contains("line"), "{message}");
    }

    #[test]
    fn out_of_range_values_name_the_key() {
        for key in [
            "startup_timeout_ms",
            "request_timeout_ms",
            "write_timeout_ms",
            "diagnostics_settle_ms",
            "diagnostics_timeout_ms",
            "max_results",
            "max_instances",
            "max_rss_mb",
            "memory_sample_ms",
            "daemon_max_rss_mb",
            "max_open_docs",
        ] {
            let message = err(&format!("[limits]\n{key} = 0"));
            assert!(message.contains(key), "{message}");
        }
        assert!(err("[limits]\nmemory_sample_ms = 0").contains("memory_sample_ms"));
        assert!(err("warmup_max_depth = 0").contains("warmup_max_depth"));
        assert!(err("warmup_max_instances = 0").contains("warmup_max_instances"));
        assert!(LspConfig::from_toml_str_without_presets("[limits]\nmax_restarts = -1").is_err());
    }

    /// A ceiling that is not a positive integer is a configuration mistake, and
    /// a negative one must be refused just as a zero is. `daemon_max_rss_mb` is
    /// read as `u64`, so a negative arrives as a TOML type error rather than as
    /// a value — either way it must not become a silently absent limit, which
    /// would leave the daemon unguarded.
    #[test]
    fn the_daemon_rss_ceiling_refuses_a_non_positive_value() {
        for src in [
            "[limits]\ndaemon_max_rss_mb = 0",
            "[limits]\ndaemon_max_rss_mb = -1",
        ] {
            let message = err(src);
            assert!(
                message.contains("daemon_max_rss_mb"),
                "the error must name the field: {message}"
            );
        }
    }

    /// The field is a real setting, not an unknown one: `deny_unknown_fields`
    /// means a typo of an accepted name is still a typo.
    #[test]
    fn the_daemon_rss_ceiling_is_accepted_by_deny_unknown_fields() {
        // A near miss that must be refused, so the accepted spelling is exact.
        let message = err("[limits]\ndaemon_max_rss = 256");
        assert!(message.contains("daemon_max_rss"), "{message}");
        assert!(parse("[limits]\ndaemon_max_rss_mb = 256").daemon_max_rss_mb == 256);
    }

    #[test]
    fn unusable_server_entries_are_errors_not_silent_skips() {
        assert!(
            err("[[server]]\nname=' '\ncommand='x'\nextensions={a='b'}").contains("empty `name`")
        );
        assert!(
            err("[[server]]\nname='a'\ncommand=' '\nextensions={a='b'}")
                .contains("empty `command`")
        );
        assert!(
            err("[[server]]\nname='a'\ncommand='x'\nextensions={ '.' = 'b' }")
                .contains("maps no `extensions`")
        );
        assert!(err("[[server]]\nname='a'\ncommand='x'").contains("extensions"));
    }

    #[test]
    fn server_for_extension_picks_the_first_by_name() {
        let config = parse(
            r#"
            [[server]]
            name = "b"
            command = "b"
            extensions = { x = "bx" }
            [[server]]
            name = "a"
            command = "a"
            extensions = { x = "ax", y = "ay" }
            "#,
        );
        let (name, _, language) = config.server_for_extension("x").unwrap();
        assert_eq!((name, language), ("a", "ax"));
        assert!(config.server_for_extension("zzz").is_none());
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert_eq!(
            LspConfig::load(Some(&missing)),
            Err(ConfigError::Missing(missing.display().to_string()))
        );
    }

    #[test]
    fn explicit_config_file_is_loaded_over_presets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, "[limits]\nmax_instances = 3\n").unwrap();
        let config = LspConfig::load(Some(&path)).unwrap();
        assert_eq!(config.max_instances, 3);
        assert!(config.servers.contains_key("rust-analyzer"));
    }

    #[test]
    fn unreadable_config_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where a file is expected: reading fails with a non-NotFound error.
        let error = LspConfig::load(Some(dir.path())).unwrap_err();
        assert!(matches!(error, ConfigError::Unreadable { .. }), "{error:?}");
    }

    #[test]
    fn config_path_prefers_xdg_then_home() {
        let env = |pairs: Vec<(&'static str, &'static str)>| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(
            config_path_from(&env(vec![("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")])),
            Some(PathBuf::from("/x/opencraylsp/config.toml"))
        );
        assert_eq!(
            config_path_from(&env(vec![("HOME", "/h")])),
            Some(PathBuf::from("/h/.config/opencraylsp/config.toml"))
        );
        assert_eq!(
            config_path_from(&env(vec![("XDG_CONFIG_HOME", ""), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.config/opencraylsp/config.toml"))
        );
        assert_eq!(config_path_from(&env(vec![])), None);
    }

    // ---- a config another user could write is not read ----

    fn write_config(path: &Path, body: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A config a stranger could write decides which programs run as this
    /// user, so it is refused rather than parsed.
    #[test]
    fn a_world_writable_config_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "", 0o666);
        let err = LspConfig::load(Some(&path)).expect_err("a world-writable config is refused");
        let text = err.to_string();
        assert!(text.contains("chmod 600"), "the fix must be named: {text}");
    }

    #[test]
    fn a_group_writable_config_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "", 0o664);
        assert!(LspConfig::load(Some(&path)).is_err());
    }

    /// The ordinary case must keep working: a 0600 config this user owns is
    /// read exactly as before.
    #[test]
    fn a_private_config_is_read_normally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(
            &path,
            "[[server]]\nname = \"x\"\ncommand = \"true\"\nextensions = { rs = \"rust\" }\n",
            0o600,
        );
        let config = LspConfig::load(Some(&path)).expect("a private config loads");
        assert!(config.servers.contains_key("x"));
    }

    /// 0644 — readable by everyone but writable only by the owner — is fine:
    /// the risk is being able to *change* what runs, not being able to read it.
    #[test]
    fn a_readable_but_not_writable_config_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "", 0o644);
        assert!(LspConfig::load(Some(&path)).is_ok());
    }

    /// An absent default config is still fine (that is "presets and
    /// defaults"); the privacy check must not turn it into an error.
    #[test]
    fn an_absent_config_is_not_a_privacy_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nothing-here.toml");
        assert!(LspConfig::load(Some(&path)).is_err(), "explicit must exist");
        // Not explicit: `check_private` on a missing file must stay quiet, so
        // `load`'s own NotFound handling still produces Missing.
        let err = LspConfig::load(Some(&path)).unwrap_err();
        assert!(matches!(err, ConfigError::Missing(_)), "{err:?}");
    }
}
