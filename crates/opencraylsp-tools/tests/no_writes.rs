//! `lsp_rename_preview` promises never to write, and a promise is worth no more
//! than the code behind it. There is no writer anywhere in `src/`; this test
//! reads every source file and fails the moment one appears.
//!
//! Test code is out of scope — fixtures have to create files — so every
//! `#[cfg(test)]`-gated item is cut out before the scan. What is *not* out of
//! scope is anything after the test module: the file is scanned whole, and only
//! the item the attribute attaches to is dropped, so a writer appended below
//! `mod tests` is still found.

use std::fs;
use std::path::{Path, PathBuf};

/// The calls that would mean this crate writes project files. They are spelled
/// out here, outside `src/`, so the scan can look for them there.
const FORBIDDEN: [&str; 8] = [
    "fs::write",
    "fs::rename",
    "fs::remove_",
    "fs::create_dir",
    "File::create",
    "File::options",
    "OpenOptions",
    "write_all",
];

/// The attribute that marks an item as test-only in this crate.
const CFG_TEST: &str = "#[cfg(test)]";

/// Every `.rs` file under `dir`, recursively.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

/// `text` with every `#[cfg(test)]`-gated item removed and everything else
/// kept. A scan that stopped at the first attribute would be blind to code
/// below the test module, which is exactly where a stray writer would hide.
fn non_test_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(CFG_TEST) {
        out.push_str(&rest[..at]);
        rest = after_gated_item(&rest[at..]);
    }
    out.push_str(rest);
    out
}

/// The text after the item `#[cfg(test)]` is attached to: the matching `}` of
/// its body, or the `;` of its statement.
///
/// Braces inside strings, char literals, raw strings and comments are not
/// counted, so a test module holding `"}"` cannot end the cut early and blind
/// the scan to the rest of the file.
fn after_gated_item(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut index = CFG_TEST.len();
    let mut depth = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => index = after_line(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = after_block_comment(bytes, index)
            }
            b'"' => index = after_string(bytes, index),
            b'\'' => index = after_char_or_lifetime(bytes, index),
            b'r' | b'R' | b'b' | b'B' if raw_string_open(bytes, index) => {
                index = after_raw_string(bytes, index);
            }
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                index += 1;
                if depth == 0 {
                    return &text[index..];
                }
            }
            // A `}` with nothing open is not ours to match; a `;` at that
            // depth ends a statement-shaped item (`use`, `static`).
            b';' if depth == 0 => return &text[index + 1..],
            _ => index += 1,
        }
    }
    // Unterminated item: nothing left to scan in this file.
    &text[text.len()..]
}

/// The index just past the newline ending the `//` comment starting at `index`.
fn after_line(bytes: &[u8], index: usize) -> usize {
    match bytes[index..].iter().position(|byte| *byte == b'\n') {
        Some(offset) => index + offset + 1,
        None => bytes.len(),
    }
}

/// The index just past the (nestable) block comment starting at `index`.
fn after_block_comment(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 0usize;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"/*") {
            depth += 1;
            index += 2;
        } else if bytes[index..].starts_with(b"*/") {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return index;
            }
        } else {
            index += 1;
        }
    }
    bytes.len()
}

/// The index just past the `"…"` string starting at `index`.
fn after_string(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// The index just past a char literal (`'x'`, `'\''`, `'\u{1f600}'`), or just
/// past the quote of a lifetime (`'a`), which has no closing quote.
fn after_char_or_lifetime(bytes: &[u8], index: usize) -> usize {
    if bytes.get(index + 1) == Some(&b'\\') {
        let mut cursor = index + 2;
        while cursor < bytes.len() {
            if bytes[cursor] == b'\'' {
                return cursor + 1;
            }
            cursor += 1;
        }
        return bytes.len();
    }
    if bytes.get(index + 2) == Some(&b'\'') {
        return index + 3;
    }
    index + 1
}

/// Whether a raw string (`r"`, `r#"`, `br#"`) opens at `index`, i.e. the prefix
/// is not part of a longer identifier.
fn raw_string_open(bytes: &[u8], index: usize) -> bool {
    let identifier = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    if index > 0 && identifier(bytes[index - 1]) {
        return false;
    }
    let mut cursor = index;
    if matches!(bytes[cursor], b'b' | b'B') {
        cursor += 1;
    }
    if !matches!(bytes.get(cursor), Some(b'r' | b'R')) {
        return false;
    }
    cursor += 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    bytes.get(cursor) == Some(&b'"')
}

/// The index just past the raw string opening at `index`.
fn after_raw_string(bytes: &[u8], index: usize) -> usize {
    let mut cursor = index;
    if matches!(bytes[cursor], b'b' | b'B') {
        cursor += 1;
    }
    cursor += 1; // `r`
    let mut hashes = 0usize;
    while bytes.get(cursor) == Some(&b'#') {
        hashes += 1;
        cursor += 1;
    }
    cursor += 1; // `"`
    while cursor < bytes.len() {
        if bytes[cursor] == b'"' {
            let closing = cursor + 1;
            if bytes[closing..].starts_with(&vec![b'#'; hashes]) {
                return closing + hashes;
            }
        }
        cursor += 1;
    }
    bytes.len()
}

#[test]
fn the_crate_never_writes_to_a_file() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    // A scan of nothing passes trivially; make sure it looked at real files.
    assert!(
        files.len() > 5,
        "the scan found almost no source: {files:?}"
    );

    for file in files {
        let text = fs::read_to_string(&file).expect("read source");
        let code = non_test_code(&text);
        // Nothing gated may survive the cut. If something did, the cut stopped
        // early — which both reads test code as production code (a fixture's
        // own `fs::write` would fail the build) and, worse, would stop the scan
        // before real code further down the file.
        assert!(
            !code.contains(CFG_TEST),
            "{} still contains `{CFG_TEST}` after the cut",
            file.display()
        );
        for forbidden in FORBIDDEN {
            assert!(
                !code.contains(forbidden),
                "{} uses `{forbidden}` in non-test code: the rename preview must never write",
                file.display()
            );
        }
    }
}

#[test]
fn code_after_a_test_module_still_reaches_the_scan() {
    let text = "\
fn real() {}
#[cfg(test)]
mod tests {
    fn fixture() {
        let brace = \"}\";
        let _ = brace;
    }
}
fn appended() { let _ = std::fs::write(\"x\", \"y\"); }
";
    let code = non_test_code(text);
    assert!(!code.contains("mod tests"), "{code}");
    assert!(
        !code.contains("fixture"),
        "the test module must be cut: {code}"
    );
    // The whole point: what sits *below* the test module is still scanned.
    assert!(code.contains("std::fs::write"), "{code}");
    assert!(code.contains("fn appended"), "{code}");
}

#[test]
fn braces_inside_literals_and_comments_do_not_end_the_cut() {
    let text = "\
#[cfg(test)]
mod tests {
    // a stray } in a line comment
    /* a stray } in a /* nested */ block comment */
    const S: &str = \"}\";
    const C: char = '}';
    const E: char = '\\'';
    const R: &str = r#\" } \"#;
}
fn after() { let _ = std::fs::write(\"x\", \"y\"); }
";
    let code = non_test_code(text);
    assert!(
        !code.contains("const S"),
        "the test module must be cut: {code}"
    );
    assert!(code.contains("fn after"), "{code}");
    assert!(code.contains("std::fs::write"), "{code}");
}

#[test]
fn a_gated_statement_is_cut_to_its_semicolon() {
    let code = non_test_code("#[cfg(test)]\nuse std::fs::write;\nfn kept() {}\n");
    assert!(!code.contains("use std::fs::write"), "{code}");
    assert!(code.contains("fn kept"), "{code}");
}

#[test]
fn a_file_with_no_test_module_is_scanned_whole() {
    let code = non_test_code("fn only() { let _ = std::fs::write(\"x\", \"y\"); }\n");
    assert!(code.contains("std::fs::write"), "{code}");
}

#[test]
fn lifetimes_are_not_mistaken_for_unterminated_chars() {
    let code = non_test_code(
        "#[cfg(test)]\nmod t { fn f<'a>(x: &'a str) -> &'a str { x } }\nfn kept() {}\n",
    );
    assert!(code.contains("fn kept"), "{code}");
    assert!(!code.contains("fn f<"), "{code}");
}
