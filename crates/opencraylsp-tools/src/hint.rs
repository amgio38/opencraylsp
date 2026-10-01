//! The one answer to "nothing at that position": a miss that says where the
//! candidates actually are.
//!
//! A position-targeted tool fails for one reason far more often than any other:
//! the `column` is a little to the left or right of the identifier rather than
//! on it. The server's own answer in that case is a bare "no definition found",
//! which leaves the caller to retry the same call with a different number and
//! again, for the same reason, blind.
//!
//! So the miss carries the line it was aimed at: every identifier on it, with
//! the column it starts at, in the same 1-based Unicode scalar counting every
//! other answer in this crate uses. A CJK or full-width prefix is exactly where a
//! byte- or code-unit-based column goes wrong, and this is the one place that
//! hands a column back for the model to paste, so it has to be the counting the
//! tools themselves accept.
//!
//! It reads the file, so there are three ways it declines to answer, all of them
//! quiet rather than wrong: a file outside the boundary (reading it would be the
//! very thing the boundary exists to prevent), a line with no identifiers, and a
//! line long enough that the hint would bury the miss.

use crate::format::LineIndex;
use crate::operations::Site;

/// Most identifiers listed before the rest are counted.
///
/// Eight is enough to cover almost any real line, and a line with more is
/// usually machine-generated — a table row, a long list — where the useful
/// answer is the count, not the fiftieth name.
const MAX_LISTED: usize = 8;

/// A line longer than this gets no hint at all.
///
/// The hint is appended to a sentence the model has to read; burying that
/// sentence under two thousand characters of source makes the miss harder to act
/// on, not easier. Minified assets and generated tables land here.
const MAX_HINT_LINE_CHARS: usize = 2000;

/// `[not_found] <what>` plus, when the line can be read usefully, the identifiers
/// on it and the column each one starts at.
///
/// `what` is the sentence about what was not found ("no definition was found"),
/// so this composes with each tool's own wording instead of replacing it. The
/// hint line goes on its own line below, which is where a model reads detail
/// without losing the marker on the first line.
pub(crate) fn not_found_at_position(
    what: &str,
    site: &Site,
    lines: &LineIndex,
) -> opencraylsp_proto::ToolOutput {
    let mut out = format!("[not_found] {what}");
    if let Some(hint) = hint_for(site, lines) {
        out.push('\n');
        out.push_str(&hint);
    }
    crate::resolve_render::output(out, &[])
}

/// The hint line, or `None` when there is nothing useful to say.
pub(crate) fn hint_for(site: &Site, lines: &LineIndex) -> Option<String> {
    let line = site.line?;
    let path = site.path.as_deref()?;
    let text = lines.line(path, line)?;
    if text.chars().count() > MAX_HINT_LINE_CHARS {
        return None;
    }
    let identifiers = identifiers_on(&text);
    if identifiers.is_empty() {
        return None;
    }
    let listed = identifiers.len().min(MAX_LISTED);
    let mut out = format!("Identifiers on line {}:", line + 1);
    for (column, name) in identifiers.iter().take(listed) {
        out.push_str(&format!(" {name}@{column}"));
    }
    if identifiers.len() > listed {
        out.push_str(&format!(" and {} more", identifiers.len() - listed));
    }
    Some(out)
}

/// One identifier per run of identifier characters, with its 1-based column.
///
/// The column counts Unicode scalars — `text[..byte].chars().count() + 1` — which
/// is the counting the tools accept on the way in and print on the way out. A
/// byte offset would be wrong on any line holding CJK text or an emoji, and a
/// UTF-16 code-unit count would be wrong on the emoji; a scalar count is the one
/// the rest of the crate already speaks.
fn identifiers_on(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (byte, ch) in text.char_indices() {
        if is_identifier_char(ch) {
            start.get_or_insert(byte);
        } else if let Some(begin) = start.take() {
            out.push((scalar_column(text, begin), &text[begin..byte]));
        }
    }
    if let Some(begin) = start {
        out.push((scalar_column(text, begin), &text[begin..]));
    }
    out
}

/// The 1-based Unicode scalar column of the scalar starting at `byte`.
fn scalar_column(text: &str, byte: usize) -> usize {
    text[..byte].chars().count() + 1
}

/// Identifier characters: a letter, a digit or `_`.
///
/// Close enough to Unicode's XID for this purpose and no more: the rule exists
/// so a model can see which runs of characters it may point `column` at, and a
/// name that starts with an emoji or a symbol is not one it will be aiming at.
/// `start.get_or_insert` above means a leading digit still counts as an
/// identifier, which is right for `0` in a numeric literal and harmless
/// otherwise.
fn is_identifier_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(line: u32) -> Site {
        Site {
            path: Some(std::path::PathBuf::from("/ws/a.rs")),
            uri: "file:///ws/a.rs".to_owned(),
            line: Some(line),
            character: Some(0),
        }
    }

    fn hint_for_text(text: &str) -> String {
        let lines = LineIndex::lazy("/ws");
        lines.insert("/ws/a.rs", text);
        hint_for(&site(0), &lines).unwrap_or_default()
    }

    /// The case that makes a byte-based column wrong: every CJK scalar is three
    /// UTF-8 bytes, so a byte count puts `foo` three times too far right.
    #[test]
    fn a_full_width_prefix_counts_scalars_not_bytes() {
        let out = hint_for_text("let 變數 = foo(1);\n");
        // `foo` starts at byte 15 but at scalar column 10: the two 3-byte CJK
        // scalars before it are six bytes and only two columns.
        assert_eq!(out, "Identifiers on line 1: let@1 變數@5 foo@10 1@14");
        assert_eq!("let 變數 = foo(1);".find("foo"), Some(13));
    }

    /// An emoji is one scalar but two UTF-16 code units, so the counting that
    /// the rest of the crate uses is neither bytes nor UTF-16.
    #[test]
    fn an_emoji_counts_as_one_column() {
        let out = hint_for_text("let 🎯 = target;\n");
        // Byte 5, scalar column 6 — and one column, not the two a UTF-16 count
        // would give the emoji.
        assert_eq!(out, "Identifiers on line 1: let@1 target@9");
        assert_eq!("let 🎯 = target;".find("target"), Some(11));
    }

    #[test]
    fn a_plain_line_is_listed_in_order() {
        assert_eq!(
            hint_for_text("fn helper(name: &str) {}\n"),
            "Identifiers on line 1: fn@1 helper@4 name@11 str@18"
        );
    }

    #[test]
    fn more_than_eight_identifiers_are_counted_not_dumped() {
        let out = hint_for_text("a b c d e f g h i j k\n");
        assert_eq!(
            out,
            "Identifiers on line 1: a@1 b@3 c@5 d@7 e@9 f@11 g@13 h@15 and 3 more"
        );
    }

    #[test]
    fn exactly_eight_are_all_listed_and_nothing_is_counted() {
        let out = hint_for_text("a b c d e f g h\n");
        assert!(!out.contains("more"), "{out}");
        assert!(out.ends_with("h@15"), "{out}");
    }

    /// A line with nothing to point at gets no hint: "Identifiers on line 3:"
    /// with an empty list is noise that reads like a bug.
    #[test]
    fn a_line_with_no_identifiers_gets_no_hint() {
        assert_eq!(hint_for_text("   \n"), "");
        assert_eq!(hint_for_text("\n"), "");
    }

    /// A minified line would bury the miss under its own hint.
    #[test]
    fn a_line_over_the_length_cap_gets_no_hint() {
        let long = "a".repeat(MAX_HINT_LINE_CHARS + 1);
        assert_eq!(hint_for_text(&format!("{long}\n")), "");
    }

    /// The boundary is what keeps a read inside the workspace, so the hint must
    /// not be the thing that walks around it.
    #[test]
    fn a_site_outside_the_boundary_gets_no_hint() {
        let lines = LineIndex::lazy("/ws");
        std::fs::write("/tmp/opencraylsp-hint-outside.rs", "fn outside() {}\n").ok();
        let mut outside = site(0);
        outside.path = Some(std::path::PathBuf::from("/tmp/opencraylsp-hint-outside.rs"));
        assert!(hint_for(&outside, &lines).is_none());
        let _ = std::fs::remove_file("/tmp/opencraylsp-hint-outside.rs");
    }

    /// A site with no line, or no file, has nothing to read.
    #[test]
    fn a_site_without_a_line_or_a_file_gets_no_hint() {
        let lines = LineIndex::lazy("/ws");
        let mut no_line = site(0);
        no_line.line = None;
        assert!(hint_for(&no_line, &lines).is_none());
        let mut no_path = site(0);
        no_path.path = None;
        assert!(hint_for(&no_path, &lines).is_none());
    }

    /// The miss itself is what the caller asked for; the hint is added to it,
    /// never in place of it, and the marker stays on the first line.
    #[test]
    fn the_marker_and_the_sentence_survive_the_hint() {
        let lines = LineIndex::lazy("/ws");
        lines.insert("/ws/a.rs", "fn helper() {}\n");
        let out = not_found_at_position("no definition was found", &site(0), &lines);
        assert!(!out.is_error, "a miss is not an error");
        assert_eq!(
            out.text.lines().next(),
            Some("[not_found] no definition was found")
        );
        assert!(
            out.text.contains("Identifiers on line 1: fn@1 helper@4"),
            "{}",
            out.text
        );
    }

    /// A file that cannot be read is not a failure: the miss still stands.
    #[test]
    fn an_unreadable_line_leaves_the_miss_intact() {
        let lines = LineIndex::lazy("/ws");
        let out = not_found_at_position(
            "no definition was found",
            &Site {
                path: Some(std::path::PathBuf::from("/ws/missing.rs")),
                uri: String::new(),
                line: Some(0),
                character: Some(0),
            },
            &lines,
        );
        assert_eq!(out.text, "[not_found] no definition was found");
    }
}
