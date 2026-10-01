//! The `lsp_*` tool catalog: argument schemas, symbol resolution
//! and model-facing output formatting. Owned by the tools workstream.
//!
//! The two entry points below are the contract with the daemon and the
//! embedded mode; their signatures do not change. The catalog itself lives in
//! [`tools`].

use opencraylsp_core::LspBackend;
use opencraylsp_proto::{ToolDef, ToolOutput};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub mod callgraph;
pub mod error;
pub mod format;
pub mod hint;
pub mod operations;
pub mod position;
pub mod rename;
pub mod resolve;
pub mod resolve_container;
pub mod resolve_render;
pub mod tools;

/// Largest tool answer this crate will hand back, in bytes.
///
/// The daemon speaks one JSON-RPC message per line, and both ends of that line
/// refuse to buffer more than `opencraylsp_proto::rpc::MAX_LINE_BYTES` (4 MiB): a
/// longer reply is not delivered, it *disconnects the client* — so an
/// oversized answer is not a slow answer, it is no answer and a broken
/// connection, with nothing in the transcript to say why.
///
/// Nothing in the tool layer used to bound the byte size of what it built. The
/// per-tool `limit` arguments bound the *number of rows*, and a row is not a
/// fixed size: a symbol name, a diagnostic message or an `LspError::Rpc` text
/// all come from the language server and are as long as it likes. With a 64 MiB
/// transport frame in reach, one response could produce a 64 MiB string.
///
/// The margin is deliberate rather than round. Serializing a `String` into JSON
/// costs at worst 6 bytes per input byte (a control character becomes `\u00XX`),
/// so 256 KiB of text becomes at most 1.5 MiB on the wire — comfortably inside
/// the 4 MiB line limit even in the pathological case, which is the only case
/// the constant has to survive.
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Every tool this crate offers, in catalog order.
pub fn tool_defs() -> Vec<ToolDef> {
    tools::defs()
}

/// Runs the tool `name` against `backend`.
///
/// Never returns a protocol error: an unknown name or a failing tool is a
/// [`ToolOutput`] with `is_error = true` whose first line is `[code] message`.
///
/// This is the single seam every backend goes through, which is why the output
/// cap lives here rather than in each formatter: a new tool, or a new field
/// copied verbatim out of a server response, cannot forget to apply it.
pub async fn call_tool(
    backend: &dyn LspBackend,
    name: &str,
    args: Value,
    cancel: &CancellationToken,
) -> ToolOutput {
    let output = tools::call(backend, name, args, cancel).await;
    cap_output(output)
}

/// Clips an answer to [`MAX_OUTPUT_BYTES`], saying so when it does.
///
/// Truncation happens on a character boundary, and the note is appended after
/// the clip so the note itself is always present. An answer that is cut is
/// still a *usable* answer: a model that was told "… and the rest was dropped"
/// can ask for less, whereas a dropped connection tells it nothing.
fn cap_output(mut output: ToolOutput) -> ToolOutput {
    if output.text.len() <= MAX_OUTPUT_BYTES {
        return output;
    }
    let (clipped, _) = format::truncate_bytes(&output.text, MAX_OUTPUT_BYTES);
    output.text = format!(
        "{clipped}\n... (output truncated at {MAX_OUTPUT_BYTES} bytes; ask for less, \
         e.g. a lower `limit`)"
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencraylsp_core::mock::MockBackend;
    use serde_json::json;

    #[test]
    fn the_catalog_is_the_eleven_designed_tools() {
        let names: Vec<String> = tool_defs().into_iter().map(|def| def.name).collect();
        assert_eq!(names, tools::NAMES.to_vec());
    }

    #[tokio::test]
    async fn an_unknown_tool_is_an_argument_error() {
        let backend = MockBackend::new("/ws");
        let out = call_tool(&backend, "lsp_nope", Value::Null, &CancellationToken::new()).await;
        assert!(out.is_error);
        assert!(out.text.starts_with("[invalid_args]"), "{}", out.text);
    }

    #[tokio::test]
    async fn every_advertised_tool_can_be_called_without_panicking() {
        let backend = MockBackend::new("/ws");
        for def in tool_defs() {
            let out = call_tool(&backend, &def.name, json!({}), &CancellationToken::new()).await;
            assert!(
                !out.text.is_empty(),
                "{} returned an empty answer",
                def.name
            );
        }
    }

    // ---- the output cap --------------------------------------
    //
    // The daemon puts one JSON-RPC message on one line, and both ends refuse to
    // buffer more than 4 MiB of it. An answer past that is not a slow answer, it
    // is a dropped connection with nothing in the transcript to explain it — so
    // the tool layer has to refuse to build one, and it has to do that in one
    // place rather than in each formatter.

    fn capped(text: String) -> ToolOutput {
        cap_output(ToolOutput::ok(text))
    }

    /// A server-controlled string copied verbatim into the answer, which is the
    /// shape every unbounded field in this crate has.
    fn giant(bytes: usize) -> ToolOutput {
        capped("x".repeat(bytes))
    }

    #[test]
    fn an_answer_past_the_output_cap_is_clipped_and_says_so() {
        let out = giant(MAX_OUTPUT_BYTES * 2);
        assert!(
            out.text.len() <= MAX_OUTPUT_BYTES + 256,
            "the answer was {} bytes, past the {MAX_OUTPUT_BYTES} cap",
            out.text.len()
        );
        assert!(
            out.text.contains("output truncated at"),
            "a clipped answer must say so, or the model reads a whole answer that is not"
        );
    }

    /// And the clip lands on a character boundary, so the answer is still a
    /// `String` a reader can print. Three-byte scalars, so a byte-index clip
    /// would land mid-character.
    #[test]
    fn a_clipped_answer_is_cut_on_a_character_boundary() {
        let out = capped("\u{65e5}".repeat(MAX_OUTPUT_BYTES / 3 + 64));
        assert!(
            out.text.len() <= MAX_OUTPUT_BYTES + 256,
            "{}",
            out.text.len()
        );
    }

    /// The error answers are clipped too: `LspError::Rpc` carries a raw
    /// server-controlled string, so a failing tool was the easiest way to
    /// overflow the line.
    #[test]
    fn an_error_answer_is_clipped_as_well() {
        let out = cap_output(ToolOutput::error("e".repeat(MAX_OUTPUT_BYTES * 2)));
        assert!(out.is_error, "clipping must not change what the answer is");
        assert!(
            out.text.len() <= MAX_OUTPUT_BYTES + 256,
            "{}",
            out.text.len()
        );
    }

    /// The ordinary case is untouched, so the cap is a ceiling and not a wall.
    #[test]
    fn an_answer_under_the_cap_is_returned_whole() {
        let out = capped("a short answer".to_owned());
        assert_eq!(out.text, "a short answer");
        assert!(!out.is_error);
    }

    /// And it is not a second copy: an answer exactly at the cap is not clipped,
    /// so the common path allocates nothing extra.
    #[test]
    fn an_answer_exactly_at_the_cap_is_not_clipped() {
        let out = capped("y".repeat(MAX_OUTPUT_BYTES));
        assert_eq!(out.text.len(), MAX_OUTPUT_BYTES);
        assert!(!out.text.contains("truncated"));
    }
}
