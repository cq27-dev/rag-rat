//! LSP `(line, character)` ↔ absolute byte conversion, honoring the server's negotiated position
//! encoding. This is the ONE place the encoding matters (everything else is byte space) — and the
//! exact class of silent mis-join the batch reader's `scip.rs` invariant memory documents: every
//! encoding agrees on ASCII, so a UTF-8-assuming shortcut passes every ASCII test and then
//! mis-locates an identifier the moment a multibyte character precedes it on the line.
//!
//! Unlike the batch SCIP reader (which only ever converts occurrence ranges INTO byte space), the
//! live client needs BOTH directions: byte → position to point `textDocument/definition` at a
//! callee, and position → byte to read the returned definition range back. The scalar-walk +
//! code-unit accounting is the same algorithm as `scip::LineColumnToByte`, kept separate here
//! because LSP negotiates its own encoding enum (`utf-8` / `utf-16` / `utf-32`) rather than SCIP's.

/// The position encoding an LSP session resolved during `initialize`. LSP's protocol default is
/// UTF-16; a server may advertise `utf-8` / `utf-32` support and the client picks one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl LspEncoding {
    /// The LSP `PositionEncodingKind` wire string this encoding advertises / is negotiated as.
    pub(crate) fn as_lsp_str(self) -> &'static str {
        match self {
            LspEncoding::Utf8 => "utf-8",
            LspEncoding::Utf16 => "utf-16",
            LspEncoding::Utf32 => "utf-32",
        }
    }

    /// Parse a server's negotiated `PositionEncodingKind`. `None` for an unrecognized value (the
    /// caller falls back to the LSP protocol default, UTF-16).
    pub(crate) fn from_lsp_str(value: &str) -> Option<Self> {
        match value {
            "utf-8" => Some(LspEncoding::Utf8),
            "utf-16" => Some(LspEncoding::Utf16),
            "utf-32" => Some(LspEncoding::Utf32),
            _ => None,
        }
    }
}

/// A zero-based LSP text position: `line` and `character`, the latter counted in code units of the
/// session's [`LspEncoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

/// Precomputes per-line byte starts over a document so `(line, character)` ↔ byte lookups are a
/// single walk from the line start in the declared encoding.
pub struct LineIndex<'a> {
    source: &'a [u8],
    encoding: LspEncoding,
    /// Byte offset of the first byte of each 0-based line.
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(source: &'a [u8], encoding: LspEncoding) -> Self {
        let mut line_starts = vec![0usize];
        for (idx, byte) in source.iter().enumerate() {
            if *byte == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self { source, encoding, line_starts }
    }

    /// The LSP position of an absolute byte offset: its line, and the code-unit count from that
    /// line's start to the offset. `byte` past EOF clamps to the last line's end.
    pub fn position_at_byte(&self, byte: usize) -> LspPosition {
        let byte = byte.min(self.source.len());
        // The line is the last line start at or before `byte`: `partition_point` gives the count of
        // starts `<= byte`, which is `line + 1` (there is always the line-0 start at 0).
        let line = self.line_starts.partition_point(|&start| start <= byte).saturating_sub(1);
        let line_start = self.line_starts[line];
        // Count code units walking scalars from the line start to `byte`.
        let mut units = 0usize;
        let mut cursor = line_start;
        while cursor < byte {
            let ch_len = utf8_char_len(self.source[cursor]);
            let end = (cursor + ch_len).min(byte);
            units += code_units_for(&self.source[cursor..end], self.encoding);
            cursor = end;
        }
        LspPosition {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(units).unwrap_or(u32::MAX),
        }
    }

    /// The absolute byte offset of an LSP position, where `character` counts code units in the
    /// session encoding. `None` when the line is out of range; a `character` past the line's end
    /// clamps to the line end (LSP end positions commonly sit at the trailing column).
    pub fn byte_at_position(&self, pos: LspPosition) -> Option<usize> {
        let line = usize::try_from(pos.line).ok()?;
        let target_units = usize::try_from(pos.character).ok()?;
        let line_start = *self.line_starts.get(line)?;
        // Bound the walk at the next line start (or EOF) so a column overrun can't spill onto the
        // following line.
        let line_end = self.line_starts.get(line + 1).copied().unwrap_or(self.source.len());
        let mut byte = line_start;
        let mut units = 0usize;
        while units < target_units {
            if byte >= line_end {
                return Some(line_end.min(self.source.len()));
            }
            let ch_len = utf8_char_len(self.source[byte]);
            let ch_bytes = self.source.get(byte..byte + ch_len)?;
            units += code_units_for(ch_bytes, self.encoding);
            byte += ch_len;
        }
        Some(byte)
    }
}

/// How many code units a single Unicode scalar (given as its UTF-8 bytes) occupies in `encoding`:
/// UTF-8 → its UTF-8 byte length; UTF-16 → 2 for astral scalars (UTF-8 length 4) else 1; UTF-32 →
/// always 1 (one scalar, one unit).
fn code_units_for(utf8_bytes: &[u8], encoding: LspEncoding) -> usize {
    match encoding {
        LspEncoding::Utf8 => utf8_bytes.len(),
        LspEncoding::Utf16 =>
            if utf8_bytes.len() == 4 {
                2
            } else {
                1
            },
        LspEncoding::Utf32 => 1,
    }
}

/// Byte length of the UTF-8 sequence whose lead byte is `lead`; a continuation/invalid byte counts
/// as 1 so the walk always advances.
fn utf8_char_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_position_round_trips_both_directions() {
        // "fn main() {}\n": the callee-ish token `main` starts at byte 3 → (line 0, char 3) under
        // any encoding (ASCII agrees), and back.
        let src = b"fn main() {}\n";
        for enc in [LspEncoding::Utf8, LspEncoding::Utf16, LspEncoding::Utf32] {
            let idx = LineIndex::new(src, enc);
            let pos = idx.position_at_byte(3);
            assert_eq!(pos, LspPosition { line: 0, character: 3 }, "{enc:?}");
            assert_eq!(idx.byte_at_position(pos), Some(3), "{enc:?} round-trip");
        }
    }

    #[test]
    fn utf16_counts_astral_char_as_two_units_before_the_callee() {
        // A 😀 (U+1F600, 4 UTF-8 bytes, 2 UTF-16 units) precedes `foo` on the line. Under UTF-16
        // the callee's character offset must count the emoji as 2 units, not 1 or 4.
        let src = "let s = \"😀\"; foo();\n".as_bytes();
        let foo_byte = find(src, "foo");
        let idx = LineIndex::new(src, LspEncoding::Utf16);
        let pos = idx.position_at_byte(foo_byte);
        // bytes before foo: `let s = "` (9) + 😀 (4) + `"; ` (3) = 16 bytes; UTF-16 units:
        // 9 + 2 + 3 = 14.
        assert_eq!(pos, LspPosition { line: 0, character: 14 });
        assert_eq!(idx.byte_at_position(pos), Some(foo_byte), "utf-16 round-trip lands on foo");
    }

    #[test]
    fn utf8_counts_astral_char_as_four_units() {
        // Same source under UTF-8: the character offset is the BYTE offset within the line, so the
        // emoji counts as its 4 UTF-8 bytes.
        let src = "let s = \"😀\"; foo();\n".as_bytes();
        let foo_byte = find(src, "foo");
        let idx = LineIndex::new(src, LspEncoding::Utf8);
        let pos = idx.position_at_byte(foo_byte);
        assert_eq!(pos, LspPosition { line: 0, character: 16 });
        assert_eq!(idx.byte_at_position(pos), Some(foo_byte));
    }

    #[test]
    fn utf32_counts_astral_char_as_one_unit() {
        let src = "let s = \"😀\"; foo();\n".as_bytes();
        let foo_byte = find(src, "foo");
        let idx = LineIndex::new(src, LspEncoding::Utf32);
        let pos = idx.position_at_byte(foo_byte);
        // 9 + 1 (one scalar) + 3 = 13.
        assert_eq!(pos, LspPosition { line: 0, character: 13 });
        assert_eq!(idx.byte_at_position(pos), Some(foo_byte));
    }

    #[test]
    fn multi_line_positions_map_to_the_right_line() {
        let src = b"a();\nlet x = 1;\nbar();\n";
        let bar_byte = find(src, "bar");
        let idx = LineIndex::new(src, LspEncoding::Utf16);
        let pos = idx.position_at_byte(bar_byte);
        assert_eq!(pos, LspPosition { line: 2, character: 0 });
        assert_eq!(idx.byte_at_position(pos), Some(bar_byte));
    }

    #[test]
    fn character_past_line_end_clamps_to_line_end() {
        // "ab\ncd\n": (line 0, char 99) clamps to line 0's end — byte 3, the start of line 1, an
        // EXCLUSIVE upper bound that includes line 0's trailing newline but nothing of line 1's
        // content. This is the batch reader's clamp semantics (never spills into the next line's
        // text); a real LSP range never overruns, so this only guards a malformed position.
        let src = b"ab\ncd\n";
        let idx = LineIndex::new(src, LspEncoding::Utf16);
        assert_eq!(idx.byte_at_position(LspPosition { line: 0, character: 99 }), Some(3));
    }

    #[test]
    fn line_out_of_range_is_none() {
        let src = b"only();\n";
        let idx = LineIndex::new(src, LspEncoding::Utf16);
        assert_eq!(idx.byte_at_position(LspPosition { line: 9, character: 0 }), None);
    }

    fn find(src: &[u8], needle: &str) -> usize {
        std::str::from_utf8(src).unwrap().find(needle).unwrap()
    }
}
