//! Real language servers on tiny fixture projects (T11): each
//! language answers `textDocument/definition` through the pool, using the
//! built-in presets (plus the two overrides this machine needs for gopls'
//! location and TypeScript 5's tsserver).
//!
//! `cargo test -p opencraylsp-core --features testing --test live_languages -- --ignored --nocapture`

#![allow(clippy::print_stderr)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opencraylsp_core::{LanguageSelection, LspBackend, LspConfig, LspError, Pool};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Presets, with the TypeScript server pointed at a TypeScript 5 `tsserver`
/// (a global TypeScript 7 ships none). Set `OPENCRAYLSP_TEST_GOPLS` and
/// `OPENCRAYLSP_TEST_TSSERVER` when the tools are not on `PATH` / in a default place.
fn config() -> Arc<LspConfig> {
    let gopls = std::env::var("OPENCRAYLSP_TEST_GOPLS").unwrap_or_else(|_| "gopls".to_owned());
    let tsserver = std::env::var("OPENCRAYLSP_TEST_TSSERVER")
        .map(|path| format!("initialization_options = {{ tsserver = {{ path = {path:?} }} }}\n"))
        .unwrap_or_default();
    let src = format!(
        "[limits]\nstartup_grace_ms = 0\nstartup_timeout_ms = 180000\nrequest_timeout_ms = 120000\n\
         [[server]]\nname = \"gopls\"\ncommand = {gopls:?}\nextensions = {{ go = \"go\" }}\n\
         root_markers = [\"go.work\", \"go.mod\"]\n\
         [[server]]\nname = \"typescript-language-server\"\ncommand = \"typescript-language-server\"\n\
         args = [\"--stdio\"]\n\
         extensions = {{ ts = \"typescript\", js = \"javascript\" }}\n\
         root_markers = [\"tsconfig.json\", \"package.json\"]\n\
         {tsserver}"
    );
    Arc::new(LspConfig::from_toml_str(&src).unwrap())
}

/// Asks for the definition at (`line`, `character`) until the server has
/// indexed enough to answer, or gives up after `wait`.
async fn definition(
    backend: &dyn LspBackend,
    file: &Path,
    line: u32,
    character: u32,
    wait: Duration,
) -> Value {
    let started = Instant::now();
    loop {
        let outcome = backend
            .request(
                file,
                "textDocument/definition",
                json!({"textDocument": {"uri": format!("file://{}", file.display())},
                       "position": {"line": line, "character": character}}),
                &CancellationToken::new(),
            )
            .await;
        match outcome {
            Ok(served) if !served.value.is_null() && served.value != json!([]) => {
                eprintln!("  answered after {:?}", started.elapsed());
                return served.value;
            }
            Ok(_) | Err(LspError::Indexing { .. }) => {}
            Err(other) => panic!("{other}"),
        }
        assert!(started.elapsed() < wait, "no definition within {wait:?}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The 0-based start line of the first location in a definition answer.
fn target_line(value: &Value) -> u64 {
    let first = if value.is_array() { &value[0] } else { value };
    first["range"]["start"]["line"]
        .as_u64()
        .or_else(|| first["targetSelectionRange"]["start"]["line"].as_u64())
        .unwrap_or_else(|| panic!("no line in {value}"))
}

async fn check(
    name: &str,
    files: &[(&str, &str)],
    query_file: &str,
    line: u32,
    character: u32,
    expect_line: u64,
) {
    eprintln!("== {name}");
    let dir = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(dir.path()).unwrap();
    for (path, text) in files {
        write(&ws.join(path), text);
    }
    let pool = Pool::new(config());
    let backend = pool.bind(&ws, LanguageSelection::All);
    let file = ws.join(query_file);
    let value = definition(&*backend, &file, line, character, Duration::from_secs(120)).await;
    assert_eq!(target_line(&value), expect_line, "{name}: {value}");
    let status = backend.status().await;
    eprintln!(
        "  instances={} languages={:?}",
        status.instances.len(),
        status.enabled_languages
    );
    pool.shutdown().await;
}

#[tokio::test]
#[ignore = "live: needs rust-analyzer"]
async fn rust_definition() {
    check(
        "rust",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (
                "src/lib.rs",
                "pub fn alpha() -> u32 {\n    beta()\n}\npub fn beta() -> u32 {\n    1\n}\n",
            ),
        ],
        "src/lib.rs",
        1,
        5,
        3,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs gopls and go"]
async fn go_definition() {
    check(
        "go",
        &[
            ("go.mod", "module example.com/probe\n\ngo 1.21\n"),
            (
                "main.go",
                "package main\n\nfunc beta() int { return 1 }\n\nfunc alpha() int { return beta() }\n\nfunc main() { _ = alpha() }\n",
            ),
        ],
        "main.go",
        4,
        26,
        2,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs typescript-language-server and TS 5"]
async fn typescript_definition() {
    check(
        "typescript",
        &[
            ("package.json", "{\"name\":\"probe\",\"version\":\"1.0.0\"}\n"),
            ("tsconfig.json", "{\"compilerOptions\":{\"strict\":true}}\n"),
            (
                "a.ts",
                "export function beta(): number {\n  return 1;\n}\nexport function alpha(): number {\n  return beta();\n}\n",
            ),
        ],
        "a.ts",
        4,
        10,
        0,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs typescript-language-server and TS 5"]
async fn javascript_definition() {
    check(
        "javascript",
        &[
            ("package.json", "{\"name\":\"probe\",\"version\":\"1.0.0\"}\n"),
            ("jsconfig.json", "{\"compilerOptions\":{\"checkJs\":true}}\n"),
            (
                "a.js",
                "export function beta() {\n  return 1;\n}\nexport function alpha() {\n  return beta();\n}\n",
            ),
        ],
        "a.js",
        4,
        10,
        0,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs intelephense"]
async fn php_definition() {
    check(
        "php",
        &[
            ("composer.json", "{\"name\":\"probe/probe\"}\n"),
            (
                "a.php",
                "<?php\nfunction beta() {\n    return 1;\n}\nfunction alpha() {\n    return beta();\n}\n",
            ),
        ],
        "a.php",
        5,
        13,
        1,
    )
    .await;
}

#[tokio::test]
#[ignore = "live: needs rust-analyzer and cargo"]
async fn rust_diagnostics_go_from_an_error_to_clean() {
    // T6: a deliberate type error is reported on the right line, and once the
    // file is fixed the answer is an explicit "received, zero errors", never a
    // silent empty list.
    let dir = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(dir.path()).unwrap();
    write(
        &ws.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let file = ws.join("src/lib.rs");
    write(
        &file,
        "pub fn f() -> u32 {\n    let x: u32 = \"text\";\n    x\n}\n",
    );
    let src = "[limits]\nstartup_grace_ms = 0\nstartup_timeout_ms = 180000\n\
               diagnostics_settle_ms = 1500\ndiagnostics_timeout_ms = 90000\n";
    let pool = Pool::new(Arc::new(LspConfig::from_toml_str(src).unwrap()));
    let backend = pool.bind(&ws, LanguageSelection::All);
    let cancel = CancellationToken::new();

    let started = Instant::now();
    let report = loop {
        match backend.diagnostics(&file, &cancel).await {
            Ok(report) if !report.items.is_empty() => break report,
            Ok(_) | Err(LspError::Indexing { .. }) => {}
            Err(other) => panic!("{other}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(150),
            "no diagnostics in time"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    eprintln!(
        "error reported after {:?}: {:?}",
        started.elapsed(),
        report.items[0].message
    );
    assert!(report.received_for_version);
    assert_eq!(
        report.items[0].range.start.line, 1,
        "the error is on line 2"
    );

    write(
        &file,
        "pub fn f() -> u32 {\n    let x: u32 = 1;\n    x\n}\n",
    );
    let started = Instant::now();
    loop {
        match backend.diagnostics(&file, &cancel).await {
            Ok(report) if report.items.is_empty() && report.received_for_version => {
                eprintln!("clean after {:?}", started.elapsed());
                break;
            }
            Ok(_) | Err(LspError::Indexing { .. }) => {}
            Err(other) => panic!("{other}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "never became clean"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    pool.shutdown().await;
}
