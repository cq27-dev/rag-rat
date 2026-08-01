use rag_rat_base::hash::hex_sha256;

#[derive(Debug, Clone)]
pub struct ChunkAnchor {
    pub version: i64,
    pub normalized_hash: String,
    pub start_boundary_hash: String,
    pub end_boundary_hash: String,
    pub start_context_hash: String,
    pub end_context_hash: String,
    pub context_radius: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorStatus {
    Exact,
    Relocated { start_line: usize, end_line: usize, text: String },
    Stale,
}

pub fn anchor_for_text(
    text: &str,
    start_line: usize,
    end_line: usize,
    full_text: &str,
) -> ChunkAnchor {
    anchor_for_lines(text, start_line, end_line, &full_text.lines().collect::<Vec<_>>())
}

/// Like [`anchor_for_text`] but takes the file's lines pre-split. Hot path: when anchoring every
/// chunk of a file, the caller collects `full_text.lines()` once and reuses the slice, instead of
/// re-splitting the whole file per chunk (O(file_size × chunks)).
pub fn anchor_for_lines(
    text: &str,
    start_line: usize,
    end_line: usize,
    lines: &[&str],
) -> ChunkAnchor {
    let radius = 2;
    ChunkAnchor {
        version: 1,
        normalized_hash: hash_normalized(text),
        start_boundary_hash: boundary_hash(lines, start_line),
        end_boundary_hash: boundary_hash(lines, end_line),
        start_context_hash: context_hash(lines, start_line, radius),
        end_context_hash: context_hash(lines, end_line, radius),
        context_radius: i64::try_from(radius).unwrap_or(2),
    }
}

pub fn validate(
    stored_text: &str,
    start_line: usize,
    end_line: usize,
    anchor: &ChunkAnchor,
    current_text: &str,
) -> AnchorStatus {
    let Some(current_slice) = slice_lines(current_text, start_line, end_line) else {
        return relocate(stored_text, anchor, current_text).unwrap_or(AnchorStatus::Stale);
    };
    if hash_normalized(&current_slice) == anchor.normalized_hash {
        return AnchorStatus::Exact;
    }
    relocate(stored_text, anchor, current_text).unwrap_or(AnchorStatus::Stale)
}

pub fn hash_normalized(text: &str) -> String {
    let normalized =
        text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>();
    hex_sha256(normalized.join("\n").as_bytes())
}

fn relocate(stored_text: &str, anchor: &ChunkAnchor, current_text: &str) -> Option<AnchorStatus> {
    let wanted_hash = if anchor.normalized_hash.is_empty() {
        hash_normalized(stored_text)
    } else {
        anchor.normalized_hash.clone()
    };
    let wanted_lines = stored_text.lines().count().max(1);
    let lines = current_text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    for start in 1..=lines.len() {
        let end = (start + wanted_lines - 1).min(lines.len());
        let Some(candidate) = slice_lines(current_text, start, end) else {
            continue;
        };
        if hash_normalized(&candidate) == wanted_hash {
            let start_boundary = boundary_hash(&lines, start);
            let end_boundary = boundary_hash(&lines, end);
            let start_context =
                context_hash(&lines, start, usize::try_from(anchor.context_radius).unwrap_or(2));
            let end_context =
                context_hash(&lines, end, usize::try_from(anchor.context_radius).unwrap_or(2));
            if start_boundary == anchor.start_boundary_hash
                || end_boundary == anchor.end_boundary_hash
                || start_context == anchor.start_context_hash
                || end_context == anchor.end_context_hash
            {
                return Some(AnchorStatus::Relocated {
                    start_line: start,
                    end_line: end,
                    text: candidate,
                });
            }
        }
    }
    None
}

pub(crate) fn slice_lines(text: &str, start_line: usize, end_line: usize) -> Option<String> {
    if start_line == 0 || end_line < start_line {
        return None;
    }
    let selected = text
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let line_no = idx + 1;
            (line_no >= start_line && line_no <= end_line).then_some(line)
        })
        .collect::<Vec<_>>();
    (!selected.is_empty()).then(|| {
        let mut text = selected.join("\n");
        text.push('\n');
        text
    })
}

fn boundary_hash(lines: &[&str], line: usize) -> String {
    if line == 0 {
        return hex_sha256(b"");
    }
    lines
        .get(line - 1)
        .map(|line| hex_sha256(line.trim().as_bytes()))
        .unwrap_or_else(|| hex_sha256(b""))
}

fn context_hash(lines: &[&str], line: usize, radius: usize) -> String {
    if lines.is_empty() {
        return hex_sha256(b"");
    }
    let line = line.saturating_sub(1);
    let start = line.saturating_sub(radius);
    let end = (line + radius + 1).min(lines.len());
    let normalized =
        lines[start..end].iter().map(|line| line.trim()).collect::<Vec<_>>().join("\n");
    hex_sha256(normalized.as_bytes())
}
