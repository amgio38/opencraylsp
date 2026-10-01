//! Rendering a symbol lookup that did not settle on one place: several
//! candidates (`ambiguous`) or none (`not_found`).
//!
//! Kept out of `format.rs` because these answers are about a *lookup*, not
//! about an LSP response: there is no server value to decode, only candidates
//! this crate assembled. Both texts are `is_error = false` — a lookup that
//! needs disambiguation has worked, it just needs one more word from the model.

use std::path::Path;

use opencraylsp_core::backend::PositionEncoding;

use crate::format::{self, LineIndex, View};
use crate::resolve::Candidate;

/// How many candidates a `[ambiguous]` answer lists before summarizing.
const SHOWN: usize = 10;

/// How many near-miss suggestions a `[not_found]` answer lists.
const MAX_SUGGESTIONS: usize = 5;

/// The `[ambiguous]` text: every candidate, numbered, with a retry that can be
/// copied straight back.
///
/// The retry is spelled out rather than described, because the obvious reading of
/// "call again with path, line and column" is to add those to the call that just
/// failed, and the model cannot tell from there whether the `symbol` it is
/// already holding has to go. Each candidate therefore carries the exact
/// arguments to send, and the header says what happens to a `symbol` left in
/// (it is ignored, because a complete position names one place on its own).
pub fn render_ambiguous(name: &str, candidates: &[Candidate], view: &View<'_>) -> String {
    let mut out = format!(
        "[ambiguous] `{name}` matches {} symbols. Call the same tool again with `path`, \
         `line` and `column` of one of them; a complete `path`+`line`+`column` is used as the \
         position, so `symbol` can be left in but is then ignored:",
        candidates.len()
    );
    for (index, candidate) in candidates.iter().take(SHOWN).enumerate() {
        out.push('\n');
        out.push_str(&format!(
            "{}. {}\n   retry: {}",
            index + 1,
            candidate_line(candidate, view),
            retry_arguments(candidate, view),
        ));
    }
    if candidates.len() > SHOWN {
        out.push_str(&format!(
            "\n(and {} more; refine with `path` or a qualifier such as `Foo::new`)",
            candidates.len() - SHOWN
        ));
    }
    out
}

/// The `path`/`line`/`column` arguments that select this candidate, as one line
/// ready to paste back.
///
/// A candidate with no usable position cannot be retried by position; the answer
/// then points at the other way in (a qualified name), which is what actually
/// works there.
fn retry_arguments(candidate: &Candidate, view: &View<'_>) -> String {
    match view.position_parts_of(&candidate.site) {
        Some((path, line, column)) => {
            format!("{{\"path\": {path:?}, \"line\": {line}, \"column\": {column}}}")
        }
        None => format!(
            "{{\"symbol\": {:?}}} (this one has no position; narrow by a qualified name instead)",
            candidate.name
        ),
    }
}

/// The `[not_found]` text: which servers were asked, and near misses if any.
pub fn render_not_found(
    name: &str,
    servers: &[String],
    suggestions: &[Candidate],
    view: &View<'_>,
) -> String {
    let asked = if servers.is_empty() {
        "any enabled server".to_owned()
    } else {
        servers.join(", ")
    };
    let mut out = format!("[not_found] no symbol named `{name}` in {asked}");
    if !suggestions.is_empty() {
        out.push_str("\nDid you mean:");
        for candidate in suggestions.iter().take(MAX_SUGGESTIONS) {
            out.push_str("\n  ");
            out.push_str(&candidate_line(candidate, view));
        }
    }
    out
}

/// The ready-to-return output for a lookup that produced candidates or a
/// miss, with the fan-out's caveats appended.
pub fn output(text: String, notes: &[String]) -> opencraylsp_proto::ToolOutput {
    let mut text = text;
    for note in notes {
        text.push('\n');
        text.push_str(note);
    }
    opencraylsp_proto::ToolOutput::ok(text)
}

/// The view a candidate is rendered through.
///
/// Every candidate `Site` is normalised to UTF-16 code units by `resolve`, so
/// this is the one conversion path: pass it to [`render_ambiguous`] or
/// [`render_not_found`] and the column comes out in the model's counting.
pub fn candidate_view<'a>(boundary: &'a Path, lines: &'a LineIndex) -> View<'a> {
    View {
        boundary,
        encoding: PositionEncoding::Utf16,
        max_results: usize::MAX,
        subject: None,
        lines,
    }
}

/// `path:line:column  kind `name`  in container`.
pub(crate) fn candidate_line(candidate: &Candidate, view: &View<'_>) -> String {
    let kind = format::kind_name(candidate.kind).to_ascii_lowercase();
    let container = candidate
        .container
        .as_deref()
        .map(|container| format!("  in {container}"))
        .unwrap_or_default();
    format!(
        "{}  {kind} `{}`{container}",
        view.position_of(&candidate.site),
        candidate.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::Site;
    use std::path::{Path, PathBuf};

    fn candidate(
        name: &str,
        kind: u32,
        container: Option<&str>,
        line: u32,
        column: u32,
    ) -> Candidate {
        Candidate {
            site: Site {
                path: Some(PathBuf::from("/ws/a.rs")),
                uri: "file:///ws/a.rs".to_owned(),
                line: Some(line),
                character: Some(column),
            },
            name: name.to_owned(),
            kind,
            container: container.map(str::to_owned),
            server: "rust-analyzer".to_owned(),
            outside_workspace: false,
        }
    }

    fn view<'a>(lines: &'a LineIndex) -> View<'a> {
        candidate_view(Path::new("/ws"), lines)
    }

    #[test]
    fn ambiguous_numbers_the_candidates_and_teaches_how_to_narrow() {
        let lines = LineIndex::lazy("/ws");
        lines.insert("/ws/a.rs", "fn f() {}\n");
        let candidates = vec![
            candidate("new", 6, Some("Foo"), 0, 3),
            candidate("new", 6, Some("Bar"), 0, 3),
        ];
        let text = render_ambiguous("new", &candidates, &view(&lines));
        assert!(
            text.starts_with("[ambiguous] `new` matches 2 symbols."),
            "{text}"
        );
        assert!(text.contains("1. a.rs:1:4  method `new`  in Foo"), "{text}");
        assert!(text.contains("2. a.rs:1:4  method `new`  in Bar"), "{text}");
    }

    /// The retry has to be pasteable as it stands: the exact arguments are
    /// spelled out, and the model is told what happens to the `symbol` it is
    /// holding, instead of having to discover that keeping it fails.
    #[test]
    fn ambiguous_hands_out_a_retry_that_can_be_pasted_as_it_stands() {
        let lines = LineIndex::lazy("/ws");
        lines.insert("/ws/a.rs", "fn f() {}\n");
        let candidates = vec![
            candidate("new", 6, Some("Foo"), 0, 3),
            candidate("new", 6, Some("Bar"), 0, 3),
        ];
        let text = render_ambiguous("new", &candidates, &view(&lines));
        assert!(text.contains("`symbol` can be left in"), "{text}");
        assert!(
            text.contains(r#"retry: {"path": "a.rs", "line": 1, "column": 4}"#),
            "{text}"
        );
    }

    /// A candidate the model cannot retry by position has to say so and offer the
    /// way that does work, rather than print an empty argument object.
    #[test]
    fn ambiguous_says_so_when_a_candidate_has_no_position() {
        let lines = LineIndex::lazy("/ws");
        lines.insert("/ws/a.rs", "fn f() {}\n");
        let mut without_a_line = candidate("new", 6, None, 0, 3);
        without_a_line.site.line = None;
        let text = render_ambiguous("new", &[without_a_line], &view(&lines));
        assert!(text.contains("has no position"), "{text}");
        assert!(text.contains(r#"{"symbol": "new"}"#), "{text}");
    }

    #[test]
    fn ambiguous_caps_the_list_and_counts_the_rest() {
        let lines = LineIndex::lazy("/ws");
        lines.insert("/ws/a.rs", "fn f() {}\n");
        let candidates: Vec<Candidate> = (0..13)
            .map(|index| candidate("new", 6, Some(&format!("T{index}")), 0, 0))
            .collect();
        let text = render_ambiguous("new", &candidates, &view(&lines));
        assert_eq!(text.matches("\n1. ").count(), 1);
        assert!(text.contains("\n10. "), "{text}");
        assert!(!text.contains("\n11. "), "{text}");
        assert!(
            text.contains("(and 3 more; refine with `path` or a qualifier such as `Foo::new`)")
        );
    }

    #[test]
    fn not_found_names_the_servers_and_offers_suggestions() {
        let lines = LineIndex::lazy("/ws");
        lines.insert("/ws/a.rs", "fn handle_request() {}\n");
        let suggestions = vec![candidate("handle_request", 12, None, 0, 3)];
        let text = render_not_found(
            "handle",
            &["rust-analyzer".to_owned()],
            &suggestions,
            &view(&lines),
        );
        assert_eq!(
            text.lines().next(),
            Some("[not_found] no symbol named `handle` in rust-analyzer")
        );
        assert!(text.contains("Did you mean:"), "{text}");
        assert!(text.contains("a.rs:1:4"), "{text}");
    }

    #[test]
    fn not_found_without_suggestions_is_one_line() {
        let lines = LineIndex::lazy("/ws");
        let text = render_not_found("x", &[], &[], &view(&lines));
        assert_eq!(
            text,
            "[not_found] no symbol named `x` in any enabled server"
        );
    }

    #[test]
    fn output_appends_the_fan_out_notes_and_stays_successful() {
        let output = output(
            "Defined at a.rs:1:4".to_owned(),
            &["note: gopls: timeout".to_owned()],
        );
        assert!(!output.is_error);
        assert_eq!(output.text, "Defined at a.rs:1:4\nnote: gopls: timeout");
    }
}
