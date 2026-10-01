//! A scriptable fake language server for the end-to-end suite.
//!
//! Deliberately built with `std` + `serde_json` only: a test double that
//! depended on `opencraylsp-core` would be testing the product with itself.
//!
//! It keeps the text the client opened for each URI and answers from that
//! buffer - like a real server, which indexes what the client synced, not the
//! disk. Only a file that was never opened is read from disk. That is what
//! makes T5 meaningful: if the product stops sending `didChange`, the answers
//! here go stale and the test fails.
//!
//! Switches: `--delay-ms=N`, `--crash-after=N`, `--alloc-mb=N`,
//! `--progress-ms=N`, `--ambiguous=NAME`, `--record-events=PATH`, `--name=ID`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};

#[derive(Debug, Default, Clone)]
struct Options {
    delay_ms: u64,
    crash_after: Option<u64>,
    alloc_mb: usize,
    progress_ms: u64,
    ambiguous: String,
    name: String,
    record_events: Option<String>,
}

impl Options {
    fn parse(args: &[String]) -> Self {
        let mut opts = Options::default();
        for arg in args {
            if let Some(n) = arg.strip_prefix("--delay-ms=") {
                opts.delay_ms = n.parse().unwrap_or(0);
            } else if let Some(n) = arg.strip_prefix("--crash-after=") {
                opts.crash_after = n.parse().ok();
            } else if let Some(n) = arg.strip_prefix("--alloc-mb=") {
                opts.alloc_mb = n.parse().unwrap_or(0);
            } else if let Some(n) = arg.strip_prefix("--progress-ms=") {
                opts.progress_ms = n.parse().unwrap_or(0);
            } else if let Some(n) = arg.strip_prefix("--ambiguous=") {
                opts.ambiguous = n.to_owned();
            } else if let Some(n) = arg.strip_prefix("--name=") {
                opts.name = n.to_owned();
            } else if let Some(p) = arg.strip_prefix("--record-events=") {
                opts.record_events = Some(p.to_owned());
            }
        }
        opts
    }
}

fn record(opts: &Options, line: &str) {
    if let Some(path) = &opts.record_events
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn send(value: &serde_json::Value, responses_sent: &mut u64, opts: &Options) {
    let text = serde_json::to_string(value).unwrap_or_default();
    let framed = format!("Content-Length: {}\r\n\r\n{text}", text.len());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(framed.as_bytes());
    let _ = out.flush();
    *responses_sent += 1;
    if let Some(limit) = opts.crash_after
        && *responses_sent >= limit
    {
        std::process::exit(1);
    }
}

fn read_message(reader: &mut BufReader<std::io::StdinLock<'_>>) -> Option<serde_json::Value> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim().to_owned();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("Content-Length")
        {
            length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0u8; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// A `file://` URI back to a path. The suite only ever uses tempdir paths, so
/// this does not need percent-decoding.
fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(uri.strip_prefix("file://")?))
}

/// The text to answer about: what the client synced, else the disk.
fn text_for(uri: &str, texts: &HashMap<String, (i64, String)>) -> Option<String> {
    if let Some((_, text)) = texts.get(uri) {
        return Some(text.clone());
    }
    std::fs::read_to_string(uri_to_path(uri)?).ok()
}

/// The identifier around `character` on `line` (both 0-based LSP units).
fn identifier_at(text: &str, line: u64, character: u64) -> Option<String> {
    let line = text.lines().nth(line as usize)?;
    let chars: Vec<char> = line.chars().collect();
    let mut start = character as usize;
    if start > chars.len() {
        return None;
    }
    if start == chars.len() {
        start = start.saturating_sub(1);
    }
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut left = start;
    while left > 0 && word(chars[left - 1]) {
        left -= 1;
    }
    let mut right = start;
    while right < chars.len() && word(chars[right]) {
        right += 1;
    }
    if left == right {
        return None;
    }
    Some(chars[left..right].iter().collect())
}

/// Every whole-word occurrence of `word`, as `(line, character)`.
fn occurrences(text: &str, word: &str) -> Vec<(u64, u64)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let needle: Vec<char> = word.chars().collect();
        if needle.is_empty() || chars.len() < needle.len() {
            continue;
        }
        for start in 0..=chars.len() - needle.len() {
            if chars[start..start + needle.len()] != needle[..] {
                continue;
            }
            let before =
                start == 0 || !(chars[start - 1].is_alphanumeric() || chars[start - 1] == '_');
            let after = start + needle.len();
            let after_ok =
                after == chars.len() || !(chars[after].is_alphanumeric() || chars[after] == '_');
            if before && after_ok {
                found.push((index as u64, start as u64));
            }
        }
    }
    found
}

fn location(uri: &str, line: u64, character: u64, len: usize) -> serde_json::Value {
    serde_json::json!({
        "uri": uri,
        "range": {
            "start": {"line": line, "character": character},
            "end": {"line": line, "character": character + len as u64},
        },
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = Options::parse(&args);
    if opts.alloc_mb > 0 {
        let mut ballast = vec![0u8; opts.alloc_mb << 20];
        for page in (0..ballast.len()).step_by(4096) {
            ballast[page] = 1;
        }
        std::mem::forget(ballast);
    }
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut responses_sent: u64 = 0;
    let mut texts: HashMap<String, (i64, String)> = HashMap::new();
    let _ = &opts.name;

    while let Some(message) = read_message(&mut reader) {
        let method = message
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_owned();
        let id = message
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        if id.is_null() {
            match method.as_str() {
                "exit" => std::process::exit(0),
                "textDocument/didOpen" => {
                    let doc = &params["textDocument"];
                    let uri = doc["uri"].as_str().unwrap_or("").to_owned();
                    let version = doc["version"].as_i64().unwrap_or(1);
                    let text = doc["text"].as_str().unwrap_or("").to_owned();
                    texts.insert(uri.clone(), (version, text));
                    record(&opts, &format!("didOpen {uri} {version}"));
                }
                "textDocument/didChange" => {
                    let doc = &params["textDocument"];
                    let uri = doc["uri"].as_str().unwrap_or("").to_owned();
                    let version = doc["version"].as_i64().unwrap_or(1);
                    // Full-text sync: the newest content is the whole document.
                    if let Some(text) = params["contentChanges"][0]["text"].as_str() {
                        texts.insert(uri.clone(), (version, text.to_owned()));
                    }
                    record(&opts, &format!("didChange {uri} {version}"));
                }
                "textDocument/didClose" => {
                    let uri = params["textDocument"]["uri"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    texts.remove(&uri);
                    record(&opts, &format!("didClose {uri}"));
                }
                "initialized" if opts.progress_ms > 0 => {
                    let token = "fake/index";
                    send(
                        &serde_json::json!({"jsonrpc": "2.0", "method": "$/progress",
                            "params": {"token": token, "value": {"kind": "begin", "title": "Indexing", "percentage": 0}}}),
                        &mut responses_sent,
                        &opts,
                    );
                    let (opts, token) = (opts.clone(), token);
                    let wait = opts.progress_ms;
                    std::thread::spawn(move || {
                        let mut sent = 0u64;
                        std::thread::sleep(std::time::Duration::from_millis(wait / 2));
                        send(
                            &serde_json::json!({"jsonrpc": "2.0", "method": "$/progress",
                                "params": {"token": token, "value": {"kind": "report", "percentage": 50}}}),
                            &mut sent,
                            &opts,
                        );
                        std::thread::sleep(std::time::Duration::from_millis(wait - wait / 2));
                        send(
                            &serde_json::json!({"jsonrpc": "2.0", "method": "$/progress",
                                "params": {"token": token, "value": {"kind": "end"}}}),
                            &mut sent,
                            &opts,
                        );
                    });
                }
                _ => {}
            }
            continue;
        }

        if opts.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(opts.delay_ms));
        }

        let position = (
            params["position"]["line"].as_u64().unwrap_or(0),
            params["position"]["character"].as_u64().unwrap_or(0),
        );
        let result = match method.as_str() {
            "initialize" => serde_json::json!({
                "capabilities": {
                    "positionEncoding": "utf-16",
                    "textDocumentSync": 1,
                    "definitionProvider": true,
                    "referencesProvider": true,
                    "hoverProvider": true,
                    "workspaceSymbolProvider": true,
                    "documentSymbolProvider": true,
                    "implementationProvider": true,
                    "renameProvider": true,
                },
            }),
            "shutdown" => serde_json::Value::Null,
            "workspace/symbol" => {
                if opts.ambiguous.is_empty() {
                    serde_json::json!([])
                } else {
                    serde_json::json!([
                        {"name": opts.ambiguous, "kind": 12,
                         "location": {"uri": "file:///a.rs", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}},
                        {"name": opts.ambiguous, "kind": 6,
                         "location": {"uri": "file:///b.rs", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}},
                    ])
                }
            }
            "textDocument/definition" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                text_for(uri, &texts)
                    .and_then(|text| {
                        identifier_at(&text, position.0, position.1).map(|w| (text, w))
                    })
                    .and_then(|(text, word)| {
                        occurrences(&text, &word)
                            .first()
                            .map(|(l, c)| serde_json::json!([location(uri, *l, *c, word.len())]))
                    })
                    .unwrap_or(serde_json::Value::Null)
            }
            "textDocument/references" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                text_for(uri, &texts)
                    .and_then(|text| {
                        identifier_at(&text, position.0, position.1).map(|w| (text, w))
                    })
                    .map(|(text, word)| {
                        serde_json::Value::Array(
                            occurrences(&text, &word)
                                .into_iter()
                                .map(|(l, c)| location(uri, l, c, word.len()))
                                .collect(),
                        )
                    })
                    .unwrap_or(serde_json::Value::Null)
            }
            "textDocument/hover" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                text_for(uri, &texts)
                    .and_then(|text| identifier_at(&text, position.0, position.1))
                    .map(
                        |word| serde_json::json!({"contents": {"kind": "markdown", "value": word}}),
                    )
                    .unwrap_or(serde_json::Value::Null)
            }
            _ => serde_json::Value::Null,
        };

        send(
            &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
            &mut responses_sent,
            &opts,
        );
    }
}
