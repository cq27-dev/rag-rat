use std::path::Path;

use rag_rat_base::language::Language;

use crate::index::parser;

pub const MAX_STRUCTURAL_PARSE_BYTES: usize = 512_000;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub kind: &'static str,
    pub symbol_path: Option<String>,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

pub fn chunks_for_file(path: &Path, language: Language, text: &str) -> Vec<Chunk> {
    if text.len() > MAX_STRUCTURAL_PARSE_BYTES && language != Language::Markdown {
        return split_text_chunks(path, "code", text, 160);
    }
    match language {
        Language::Markdown => markdown_chunks(text),
        _ => code_chunks(path, language, text).unwrap_or_else(|_| whole_file_chunk(path, text)),
    }
}

pub fn generated_chunks_for_file(path: &Path, text: &str) -> Vec<Chunk> {
    split_text_chunks(path, "generated", text, 160)
}

fn markdown_chunks(text: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current_heading = Vec::<String>::new();
    let mut start_line = 1;
    let mut start_byte = 0;
    let mut buffer = String::new();
    let mut byte = 0;

    for (idx, raw) in text.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches('\n').trim_end_matches('\r');
        if line.starts_with('#') && !buffer.trim().is_empty() {
            chunks.push(make_chunk(
                "markdown",
                Some(current_heading.join(" > ")),
                start_byte,
                byte,
                start_line,
                idx,
                std::mem::take(&mut buffer),
            ));
            start_line = idx + 1;
            start_byte = byte;
        }
        if let Some(heading) = heading_text(line) {
            current_heading.push(heading);
        }
        buffer.push_str(line);
        buffer.push('\n');
        byte += raw.len();
    }

    if !buffer.trim().is_empty() {
        chunks.push(make_chunk(
            "markdown",
            Some(current_heading.join(" > ")),
            start_byte,
            text.len(),
            start_line,
            text.lines().count().max(start_line),
            buffer,
        ));
    }
    chunks
}

fn code_chunks(path: &Path, language: Language, text: &str) -> anyhow::Result<Vec<Chunk>> {
    Ok(code_chunks_for_symbols(path, text, &parser::parse_symbols(path, language, text)?))
}

/// Build code chunks from already-parsed symbols — lets the full-rebuild prepare phase reuse the
/// single shared parse instead of `code_chunks` re-parsing the file via `parse_symbols`.
pub fn code_chunks_for_symbols(
    path: &Path,
    text: &str,
    symbols: &[parser::ParsedSymbol],
) -> Vec<Chunk> {
    // One line-offset table per file: every symbol's line range becomes an O(1) byte-range lookup
    // instead of a from-byte-0 rescan per symbol (#517).
    let lines = LineOffsets::new(text);
    let mut chunks = Vec::new();
    for symbol in symbols {
        let Some(range) = lines.byte_range(symbol.start_line, symbol.end_line) else {
            continue;
        };
        let span_start = range.start;
        let span_text = &text[range];
        if span_text.trim().is_empty() {
            continue;
        }
        for (part_idx, part) in
            split_symbol(span_text, span_start, symbol.start_line, 120).into_iter().enumerate()
        {
            chunks.push(make_chunk(
                "code",
                Some(if part_idx == 0 {
                    symbol.qualified_name.clone()
                } else {
                    format!("{}#{part_idx}", symbol.qualified_name)
                }),
                part.start_byte,
                part.end_byte,
                part.start_line,
                part.end_line,
                part.text,
            ));
        }
    }
    chunks.extend(uncovered_code_chunks(path, text, symbols, &lines));
    chunks.sort_by_key(|chunk| (chunk.start_byte, chunk.end_byte));
    if chunks.is_empty() { whole_file_chunk(path, text) } else { chunks }
}

fn uncovered_code_chunks(
    path: &Path,
    text: &str,
    symbols: &[parser::ParsedSymbol],
    lines: &LineOffsets,
) -> Vec<Chunk> {
    let line_count = lines.line_count().max(1);
    let mut covered = vec![false; line_count + 1];
    for symbol in symbols {
        let start = symbol.start_line.max(1);
        let end = symbol.end_line.min(line_count);
        for is_covered in covered.iter_mut().take(end + 1).skip(start) {
            *is_covered = true;
        }
    }

    let mut chunks = Vec::new();
    let mut start_line = None;
    for (line, is_covered) in covered.iter().enumerate().take(line_count + 1).skip(1) {
        if !*is_covered {
            start_line.get_or_insert(line);
            continue;
        }
        if let Some(start) = start_line.take() {
            push_uncovered_chunk(
                path,
                text,
                lines,
                start,
                line.saturating_sub(1),
                chunks.len(),
                &mut chunks,
            );
        }
    }
    if let Some(start) = start_line {
        push_uncovered_chunk(path, text, lines, start, line_count, chunks.len(), &mut chunks);
    }
    chunks
}

#[allow(clippy::too_many_arguments)]
fn push_uncovered_chunk(
    path: &Path,
    text: &str,
    lines: &LineOffsets,
    start_line: usize,
    end_line: usize,
    context_index: usize,
    chunks: &mut Vec<Chunk>,
) {
    let Some(range) = lines.byte_range(start_line, end_line) else {
        return;
    };
    let span_start = range.start;
    let span_text = &text[range];
    if span_text.trim().is_empty() {
        return;
    }
    for (part_idx, part) in
        split_symbol(span_text, span_start, start_line, 80).into_iter().enumerate()
    {
        chunks.push(make_chunk(
            "code",
            Some(format!(
                "{}::#context-{}{}",
                path.to_string_lossy().replace('\\', "/"),
                context_index + 1,
                if part_idx == 0 { String::new() } else { format!("-{part_idx}") }
            )),
            part.start_byte,
            part.end_byte,
            part.start_line,
            part.end_line,
            part.text,
        ));
    }
}

/// Byte offsets of each line start, computed once per file so a line range becomes an O(1)
/// byte-range lookup — replacing the per-symbol from-byte-0 rescan that made chunking
/// O(symbols × file bytes) (#517). The range slices the raw on-disk bytes (incl. any CRLF), so a
/// span's length equals its real byte length; `split_symbol` re-normalizes each line when it
/// builds chunk text, and advances its byte cursor over these raw lengths so code-chunk offsets
/// stay aligned with the tree-sitter symbol offsets `query::graph_meta` joins against.
pub(crate) struct LineOffsets {
    /// `starts[i]` is the byte offset where 1-based line `i + 1` begins. A trailing `\n` closes
    /// the last line rather than opening a phantom one (`split_inclusive` line semantics).
    starts: Vec<usize>,
    text_len: usize,
}

impl LineOffsets {
    pub(crate) fn new(text: &str) -> Self {
        let mut starts = Vec::new();
        if !text.is_empty() {
            starts.push(0);
        }
        for (idx, byte) in text.bytes().enumerate() {
            if byte == b'\n' && idx + 1 < text.len() {
                starts.push(idx + 1);
            }
        }
        Self { starts, text_len: text.len() }
    }

    pub(crate) fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// The byte range covering whole lines `start_line..=end_line` (1-based; `end_line` clamps to
    /// EOF), or `None` when the range is empty or starts past the last line.
    pub(crate) fn byte_range(
        &self,
        start_line: usize,
        end_line: usize,
    ) -> Option<std::ops::Range<usize>> {
        if start_line == 0 || end_line < start_line || start_line > self.starts.len() {
            return None;
        }
        let start = self.starts[start_line - 1];
        let end = self.starts.get(end_line).copied().unwrap_or(self.text_len);
        Some(start..end)
    }
}

fn whole_file_chunk(path: &Path, text: &str) -> Vec<Chunk> {
    vec![make_chunk(
        "code",
        path.file_name().map(|name| name.to_string_lossy().to_string()),
        0,
        text.len(),
        1,
        text.lines().count().max(1),
        text.to_string(),
    )]
}

fn split_text_chunks(path: &Path, kind: &'static str, text: &str, max_lines: usize) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut start_line = 1;
    let mut start_byte = 0;
    let mut byte = 0;
    let mut buffer = String::new();
    for (idx, raw) in text.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches('\n').trim_end_matches('\r');
        buffer.push_str(line);
        buffer.push('\n');
        byte += raw.len();
        let line_no = idx + 1;
        if line_no - start_line + 1 >= max_lines {
            chunks.push(make_chunk(
                kind,
                path.file_name().map(|name| name.to_string_lossy().to_string()),
                start_byte,
                byte,
                start_line,
                line_no,
                std::mem::take(&mut buffer),
            ));
            start_byte = byte;
            start_line = line_no + 1;
        }
    }
    if !buffer.trim().is_empty() {
        chunks.push(make_chunk(
            kind,
            path.file_name().map(|name| name.to_string_lossy().to_string()),
            start_byte,
            text.len(),
            start_line,
            text.lines().count().max(start_line),
            buffer,
        ));
    }
    chunks
}

fn make_chunk(
    kind: &'static str,
    symbol_path: Option<String>,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
    text: String,
) -> Chunk {
    Chunk {
        kind,
        symbol_path: symbol_path.filter(|s| !s.is_empty()),
        start_byte,
        end_byte,
        start_line,
        end_line,
        text,
    }
}

fn heading_text(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let level_end = trimmed.chars().take_while(|c| *c == '#').count();
    if level_end == 0 {
        return None;
    }
    Some(trimmed[level_end..].trim().to_string())
}

#[derive(Debug)]
struct ChunkPart {
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
    text: String,
}

fn split_symbol(
    text: &str,
    base_byte: usize,
    base_line: usize,
    max_lines: usize,
) -> Vec<ChunkPart> {
    let mut parts = Vec::new();
    let mut start_byte = base_byte;
    let mut start_line = base_line;
    let mut byte = base_byte;
    let mut buffer = String::new();
    for (idx, raw) in text.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches('\n').trim_end_matches('\r');
        buffer.push_str(line);
        buffer.push('\n');
        byte += raw.len();
        let line_no = base_line + idx;
        if line_no - start_line + 1 >= max_lines {
            parts.push(ChunkPart {
                start_byte,
                end_byte: byte,
                start_line,
                end_line: line_no,
                text: std::mem::take(&mut buffer),
            });
            start_byte = byte;
            start_line = line_no + 1;
        }
    }
    if !buffer.trim().is_empty() {
        parts.push(ChunkPart {
            start_byte,
            end_byte: base_byte + text.len(),
            start_line,
            end_line: base_line + text.lines().count().saturating_sub(1),
            text: buffer,
        });
    }
    parts
}
