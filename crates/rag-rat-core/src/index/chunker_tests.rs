//! CRLF byte-offset correctness for the chunker (#241).
//!
//! `text.lines()` strips `\n` and `\r\n` alike, so the old `byte += line.len() + 1` advance
//! under-counted CRLF terminators by one byte per line — drifting `start_byte`/`end_byte` behind
//! the real on-disk positions that `query::graph_meta` joins against tree-sitter symbol offsets
//! (which count raw bytes, CR included). These tests pin chunk byte ranges to the actual on-disk
//! bytes across all three chunker entry points (markdown, generated/split-text, and the
//! tree-sitter code path) and — crucially — exercise the MULTI-chunk tiling path, where the running
//! byte accumulator (the value the bug corrupted) becomes an intermediate chunk boundary. The
//! single-chunk cases pin their `end_byte` to `text.len()` regardless, so an in-range assertion
//! over them would pass even on the buggy code; the intermediate-boundary assertions below would
//! not.

use std::path::Path;

use crate::index::chunker::{self, Chunk};
use crate::language::Language;

/// A chunk's `[start_byte, end_byte)` must slice the real on-disk bytes whose CRLF-normalized form
/// reproduces the chunk's stored (LF-normalized) text. Trailing newlines are compared loosely (a
/// chunk's final line may gain a synthetic `\n`, or the `whole_file` fallback may keep raw bytes);
/// the exact-boundary assertions in each test are what discriminate the #241 bug.
fn assert_chunk_bytes_match_disk(chunks: &[Chunk], text: &str) {
    assert!(!chunks.is_empty(), "expected at least one chunk");
    for c in chunks {
        assert!(c.start_byte <= c.end_byte, "start>end: {c:?}");
        assert!(c.end_byte <= text.len(), "end {} past EOF {}: {c:?}", c.end_byte, text.len());
        let disk = text[c.start_byte..c.end_byte].replace("\r\n", "\n");
        let stored = c.text.replace("\r\n", "\n");
        assert_eq!(
            stored.trim_end_matches('\n'),
            disk.trim_end_matches('\n'),
            "chunk [{}..{}] stored text != on-disk bytes",
            c.start_byte,
            c.end_byte,
        );
    }
}

/// Generated/markdown chunks tile the file contiguously from 0 to EOF with no gap or overlap.
fn assert_contiguous(chunks: &[Chunk], text: &str) {
    for pair in chunks.windows(2) {
        assert_eq!(pair[0].end_byte, pair[1].start_byte, "gap/overlap between chunks");
    }
    assert_eq!(chunks.first().unwrap().start_byte, 0, "first chunk must start at 0");
    assert_eq!(chunks.last().unwrap().end_byte, text.len(), "last chunk must end at EOF");
}

#[test]
fn crlf_generated_single_chunk_matches_disk() {
    let lf = "line one\nline two\nline three\n";
    let crlf = "line one\r\nline two\r\nline three\r\n";
    let p = Path::new("x.txt");
    let lf_chunks = chunker::generated_chunks_for_file(p, lf);
    let crlf_chunks = chunker::generated_chunks_for_file(p, crlf);
    assert_chunk_bytes_match_disk(&lf_chunks, lf);
    assert_chunk_bytes_match_disk(&crlf_chunks, crlf);
    assert_contiguous(&crlf_chunks, crlf);
}

#[test]
fn crlf_generated_multi_chunk_boundaries_are_on_disk_exact() {
    // split_text_chunks flushes a chunk every 160 lines (generated_chunks_for_file). With >160 CRLF
    // lines the flushed chunk's end_byte IS the running accumulator — exactly what the
    // `line.len() + 1` bug corrupted. Uniform 6-byte lines ("xxxx\r\n") make the true boundary
    // obvious: line k ends at on-disk byte 6*k (the bug would put it at 5*k).
    let crlf = "xxxx\r\n".repeat(400);
    let chunks = chunker::generated_chunks_for_file(Path::new("big.txt"), &crlf);
    assert!(chunks.len() >= 3, "expected multiple chunks, got {}", chunks.len());
    assert_chunk_bytes_match_disk(&chunks, &crlf);
    assert_contiguous(&chunks, &crlf);
    assert_eq!(chunks[0].end_byte, 160 * 6, "first flush must land on the real on-disk byte");
    assert_eq!(chunks[1].start_byte, 160 * 6, "next chunk must abut the on-disk boundary");
}

#[test]
fn crlf_markdown_intermediate_boundary_is_on_disk_exact() {
    let crlf = "# Title\r\n\r\nbody text\r\n\r\n## Section\r\n\r\nmore body\r\n";
    let chunks = chunker::chunks_for_file(Path::new("d.md"), Language::Markdown, crlf);
    assert_chunk_bytes_match_disk(&chunks, crlf);
    assert_contiguous(&chunks, crlf);
    // The heading split must land on the real on-disk offset of "## Section", not the
    // CR-undercounted position the bug produced.
    let section = crlf.find("## Section").unwrap();
    assert!(
        chunks.iter().any(|c| c.start_byte == section),
        "no chunk starts at the on-disk '## Section' offset {section}: {:?}",
        chunks.iter().map(|c| (c.start_byte, c.end_byte)).collect::<Vec<_>>(),
    );
}

#[test]
fn crlf_code_path_chunk_offsets_match_disk() {
    // The tree-sitter code path (code_chunks_for_symbols -> LineOffsets -> split_symbol) is the
    // path whose offsets graph_meta joins against tree-sitter symbol byte offsets. The line-offset
    // table yields the raw on-disk span so split_symbol's end_byte stays byte-aligned with the raw
    // file on CRLF input (the pre-#241-completion code computed end_byte in LF-normalized space
    // and drifted short).
    let crlf = "fn alpha() {\r\n    let x = 1;\r\n    let y = 2;\r\n}\r\n\r\nfn beta() {\r\n    \
                alpha();\r\n}\r\n";
    let chunks = chunker::chunks_for_file(Path::new("x.rs"), Language::Rust, crlf);
    assert_chunk_bytes_match_disk(&chunks, crlf);
    // alpha's chunk must end at the real on-disk end of its closing brace line (byte after the
    // first "}\r\n"), not short by the swallowed CR bytes.
    let alpha_end = crlf.find("}\r\n").unwrap() + "}\r\n".len();
    let alpha = chunks.iter().find(|c| c.start_byte == 0).expect("a chunk starting at byte 0");
    assert_eq!(alpha.end_byte, alpha_end, "alpha code-chunk end_byte must be on-disk exact");
    // beta's chunk must start at beta's real on-disk offset.
    let beta = crlf.find("fn beta").unwrap();
    assert!(
        chunks.iter().any(|c| c.start_byte == beta),
        "no code chunk starts at the on-disk 'fn beta' offset {beta}: {:?}",
        chunks
            .iter()
            .map(|c| (c.start_byte, c.end_byte, c.symbol_path.clone()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn crlf_no_trailing_newline_has_no_phantom_terminator() {
    let crlf = "a\r\nb\r\nc";
    let chunks = chunker::generated_chunks_for_file(Path::new("x.txt"), crlf);
    assert_chunk_bytes_match_disk(&chunks, crlf);
    assert_eq!(chunks.last().unwrap().end_byte, crlf.len(), "must end exactly at EOF");
}

#[test]
fn lf_offsets_unchanged_regression() {
    // The common case must stay byte-identical to pre-fix behaviour.
    let lf = "alpha\nbeta\ngamma\n";
    let chunks = chunker::generated_chunks_for_file(Path::new("x.txt"), lf);
    assert_chunk_bytes_match_disk(&chunks, lf);
    assert_contiguous(&chunks, lf);
}

/// The per-file line-offset table replacing `line_span`'s from-byte-0 rescan (#517). These pin the
/// exact contract the old scan had, so the O(1) lookups stay byte-identical: raw on-disk bytes
/// (CRLF included), 1-based whole-line ranges, `None` on empty/invalid ranges, EOF clamping.
mod line_offsets {
    use crate::index::chunker::LineOffsets;

    #[test]
    fn middle_range_slices_whole_lines_including_terminators() {
        let text = "one\ntwo\nthree\nfour\n";
        let lines = LineOffsets::new(text);
        let range = lines.byte_range(2, 3).expect("valid range");
        assert_eq!(&text[range], "two\nthree\n");
    }

    #[test]
    fn crlf_bytes_are_preserved_in_the_span() {
        // The span must be the raw on-disk bytes — chunk offsets join against tree-sitter symbol
        // offsets, which count CR bytes.
        let text = "a\r\nb\r\nc\r\n";
        let lines = LineOffsets::new(text);
        let range = lines.byte_range(2, 2).expect("valid range");
        assert_eq!(&text[range], "b\r\n");
    }

    #[test]
    fn zero_start_line_and_inverted_range_are_none() {
        let lines = LineOffsets::new("a\nb\n");
        assert!(lines.byte_range(0, 1).is_none(), "lines are 1-based");
        assert!(lines.byte_range(2, 1).is_none(), "end before start");
    }

    #[test]
    fn start_line_past_eof_is_none() {
        let lines = LineOffsets::new("a\nb");
        assert!(lines.byte_range(3, 5).is_none());
    }

    #[test]
    fn end_line_past_eof_clamps_to_text_end() {
        let text = "a\nb\nc";
        let lines = LineOffsets::new(text);
        let range = lines.byte_range(2, 99).expect("valid range");
        assert_eq!(&text[range], "b\nc");
    }

    #[test]
    fn unterminated_last_line_ends_exactly_at_eof() {
        let text = "a\nb\nc";
        let lines = LineOffsets::new(text);
        let range = lines.byte_range(3, 3).expect("valid range");
        assert_eq!(&text[range], "c");
        assert_eq!(lines.line_count(), 3);
    }

    #[test]
    fn trailing_newline_does_not_create_a_phantom_line() {
        let lines = LineOffsets::new("a\nb\n");
        assert_eq!(lines.line_count(), 2, "split_inclusive semantics: no trailing empty line");
        assert!(lines.byte_range(3, 3).is_none());
    }

    #[test]
    fn empty_text_has_no_lines() {
        let lines = LineOffsets::new("");
        assert_eq!(lines.line_count(), 0);
        assert!(lines.byte_range(1, 1).is_none());
    }
}
