//! Position encoding: the model speaks 1-based lines and 1-based Unicode
//! scalar columns, while an LSP server speaks 0-based lines and 0-based
//! columns in whatever encoding it negotiated (UTF-8, UTF-16 or UTF-32).
//!
//! Why this file has to exist: `character` in LSP is not a character. It is a
//! code-unit offset, UTF-16 by protocol default. On an ASCII line every
//! encoding agrees and the bug is invisible; the first line that holds CJK text
//! or an emoji makes "column 5" mean three different things, and an agent that
//! is told the wrong column edits the wrong place. A grep-based harness never
//! noticed; a harness that answers "go to definition" must.
//!
//! Both directions live here because the tool needs both: the model's column
//! becomes a server column on the way in, a server column becomes the model's
//! column on the way out. Neither may panic on a column the model or the server
//! got wrong — the line's text is the authority, and a column past the end of
//! the line clamps to the line's end, which is what an editor does.

use opencraylsp_core::backend::PositionEncoding;

/// The number of encoding code units the first `scalar_index` Unicode scalars of
/// `line` occupy — the value an LSP `character` field carries.
///
/// An index past the end of the line clamps to the end instead of panicking:
/// the model asking for column 500 of a 20-column line means "the end of it",
/// not a crash.
pub fn units_from_scalar(line: &str, scalar_index: usize, encoding: PositionEncoding) -> u32 {
    match encoding {
        // UTF-32 counts Unicode scalars, so the index *is* the offset.
        PositionEncoding::Utf32 => scalar_index.min(line.chars().count()) as u32,
        // UTF-8 counts bytes; walk to the byte offset where the wanted scalar
        // starts, and to the line's length when the index is at or past the end.
        PositionEncoding::Utf8 => {
            let wanted = scalar_index.min(line.chars().count());
            line.char_indices()
                .nth(wanted)
                .map(|(byte, _)| byte)
                .unwrap_or(line.len()) as u32
        }
        // UTF-16 counts code units: a BMP scalar is one, an astral one (an
        // emoji) is two. `take` clamps for free when the index is past the end.
        PositionEncoding::Utf16 => line
            .chars()
            .take(scalar_index)
            .map(char::len_utf16)
            .sum::<usize>() as u32,
    }
}

/// The 0-based Unicode scalar index of the scalar that starts at `units` — the
/// inverse of [`units_from_scalar`].
///
/// An offset that lands *inside* a multi-unit scalar (a byte in the middle of a
/// UTF-8 sequence, the low surrogate of an astral UTF-16 pair) resolves to that
/// scalar's start rather than splitting it: a position the model can act on is
/// always a whole character, and a server that reports a split one is the
/// outlier. An offset at or past the end of the line clamps to the line's end.
pub fn scalar_from_units(line: &str, units: u32, encoding: PositionEncoding) -> u32 {
    match encoding {
        PositionEncoding::Utf32 => units.min(line.chars().count() as u32),
        PositionEncoding::Utf8 => {
            let mut index = 0usize;
            for (byte, ch) in line.char_indices() {
                if byte + ch.len_utf8() <= units as usize {
                    index += 1;
                } else {
                    break;
                }
            }
            index as u32
        }
        PositionEncoding::Utf16 => {
            let mut seen = 0u32;
            let mut index = 0u32;
            for ch in line.chars() {
                let width = char::len_utf16(ch) as u32;
                if seen + width > units {
                    break;
                }
                seen += width;
                index += 1;
            }
            index
        }
    }
}

/// The model's 1-based column on `line` as the server's `character` field.
///
/// A column of 0 (the model never counts from 0, so this is a caller bug) is
/// treated as 1; a column past the line's end clamps to the end.
pub fn to_server_column(line: &str, column: u32, encoding: PositionEncoding) -> u32 {
    units_from_scalar(line, column.saturating_sub(1) as usize, encoding)
}

/// The server's 0-based `character` field on `line` as the model's 1-based
/// column.
pub fn to_editor_column(line: &str, character: u32, encoding: PositionEncoding) -> u32 {
    scalar_from_units(line, character, encoding) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    // Written as escapes, not literals, so this file stays ASCII: the CJK gate
    // (`make english`) forbids Han characters in harness source, and the point
    // of the test is the byte/code-unit width, which an escape spells out
    // exactly. `CJK` is three BMP scalars, three bytes each in UTF-8 and one
    // code unit each in UTF-16. `EMOJI` is one astral scalar: four UTF-8 bytes
    // and *two* UTF-16 code units (a surrogate pair).
    const CJK: &str = "\u{65e5}\u{672c}\u{8a9e}";
    const EMOJI: &str = "\u{1f600}";

    #[test]
    fn ascii_agrees_across_every_encoding() {
        let line = "let x = 1;";
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            assert_eq!(to_server_column(line, 1, encoding), 0);
            assert_eq!(to_server_column(line, 5, encoding), 4);
            assert_eq!(to_editor_column(line, 4, encoding), 5);
        }
    }

    #[test]
    fn cjk_columns_differ_between_utf8_and_utf16() {
        // "CJK" is 9 UTF-8 bytes but only 3 UTF-16 units.
        assert_eq!(units_from_scalar(CJK, 1, PositionEncoding::Utf8), 3);
        assert_eq!(units_from_scalar(CJK, 2, PositionEncoding::Utf8), 6);
        assert_eq!(units_from_scalar(CJK, 1, PositionEncoding::Utf16), 1);
        assert_eq!(units_from_scalar(CJK, 2, PositionEncoding::Utf32), 2);

        // The model's 2nd column is the 2nd scalar in every encoding, even
        // though the byte offset and the unit offset disagree.
        assert_eq!(to_server_column(CJK, 2, PositionEncoding::Utf8), 3);
        assert_eq!(to_server_column(CJK, 2, PositionEncoding::Utf16), 1);
        assert_eq!(to_editor_column(CJK, 1, PositionEncoding::Utf16), 2);
        assert_eq!(to_editor_column(CJK, 3, PositionEncoding::Utf8), 2);
    }

    #[test]
    fn an_astral_scalar_is_two_utf16_units() {
        let line = format!("a{EMOJI}b");
        assert_eq!(line.chars().count(), 3);
        assert_eq!(units_from_scalar(&line, 2, PositionEncoding::Utf16), 3);
        assert_eq!(units_from_scalar(&line, 2, PositionEncoding::Utf8), 5);
        assert_eq!(units_from_scalar(&line, 2, PositionEncoding::Utf32), 2);

        // The model's 3rd column is `b` in every encoding.
        assert_eq!(to_server_column(&line, 3, PositionEncoding::Utf16), 3);
        assert_eq!(to_editor_column(&line, 3, PositionEncoding::Utf16), 3);
        assert_eq!(to_editor_column(&line, 5, PositionEncoding::Utf8), 3);
    }

    #[test]
    fn offsets_inside_a_scalar_snap_to_its_start() {
        let line = format!("a{EMOJI}b");
        // Byte 2 and unit 2 both land inside the emoji; both resolve to the
        // emoji's own scalar index (1), never to a split character.
        assert_eq!(scalar_from_units(&line, 2, PositionEncoding::Utf8), 1);
        assert_eq!(scalar_from_units(&line, 2, PositionEncoding::Utf16), 1);
        // The high surrogate boundary is exact and included.
        assert_eq!(scalar_from_units(&line, 1, PositionEncoding::Utf16), 1);
        // Unit 3 is the start of `b` (units 0 = a, 1-2 = the emoji, 3 = b) and
        // unit 4 is the end of the line.
        assert_eq!(scalar_from_units(&line, 3, PositionEncoding::Utf16), 2);
        assert_eq!(scalar_from_units(&line, 4, PositionEncoding::Utf16), 3);
    }

    #[test]
    fn columns_past_the_end_clamp_to_the_end() {
        let line = "ab";
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            assert_eq!(to_server_column(line, 500, encoding), 2);
            assert_eq!(to_editor_column(line, 500, encoding), 3);
        }
        assert_eq!(to_editor_column(CJK, 999, PositionEncoding::Utf16), 4);
        assert_eq!(to_editor_column(CJK, 999, PositionEncoding::Utf8), 4);
    }

    #[test]
    fn an_empty_line_has_a_single_valid_column() {
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            assert_eq!(to_server_column("", 1, encoding), 0);
            assert_eq!(to_server_column("", 7, encoding), 0);
            assert_eq!(to_editor_column("", 0, encoding), 1);
            assert_eq!(to_editor_column("", 9, encoding), 1);
        }
    }

    #[test]
    fn column_zero_is_treated_as_the_first_column() {
        // The model counts from 1; 0 is a caller bug that must not underflow.
        assert_eq!(to_server_column("abc", 0, PositionEncoding::Utf16), 0);
    }

    #[test]
    fn every_scalar_boundary_round_trips() {
        let line = format!("x{CJK}y{EMOJI}z");
        let scalars = line.chars().count();
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            for index in 0..=scalars {
                let units = units_from_scalar(&line, index, encoding);
                assert_eq!(
                    scalar_from_units(&line, units, encoding) as usize,
                    index,
                    "scalar {index} did not survive {encoding:?}"
                );
            }
        }
    }
}
