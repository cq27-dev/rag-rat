//! The deterministic guards a model verdict must pass before it is recorded: the citation guard
//! (every EVIDENCE line is a pack content line) and the divergence grounding guard (a `diverged`
//! verdict copies a whole note claim and cites an absence row the claim names). Pure text analysis
//! over the rendered pack and the note — no index access.

use super::{ParsedVerdict, Verdict, verify};

/// Minimum normalized (non-whitespace) length for a citation to count — a floor that keeps a bare
/// boilerplate token (`- source`, `verdict`, `note`) from satisfying the guard by matching a
/// content-line substring. Set just below the shortest LEGITIMATE citation the corpus produces (a
/// backticked identifier like `` `thing_two` `` = 11 chars); a higher floor would reject a real
/// short-identifier citation, so the content-line filter — not this length — is the primary
/// defense.
const MIN_CITATION_CHARS: usize = 10;

/// The fabrication guard: EVERY EVIDENCE line must appear in a pack CONTENT line
/// (whitespace-normalized substring) — an identifier-table entry or a bound-file excerpt line, NOT
/// a section header or boilerplate (see [`is_pack_content_line`]) — and be at least
/// [`MIN_CITATION_CHARS`] non-whitespace chars long. There must also be at least one line: an
/// empty-evidence verdict is rejected too, so the model can't skip citing. `content` is
/// [`pack_content_lines`] of the exact string rendered into the prompt. Matching only content lines
/// (not the flattened whole pack) is what stops a citation of pack boilerplate — a header the guard
/// itself emits — from passing.
fn every_citation_matches(content: &[String], parsed: &ParsedVerdict) -> bool {
    if parsed.evidence.is_empty() {
        return false;
    }
    parsed.evidence.iter().all(|line| {
        let cite = normalize_ws(line);
        cite.chars().filter(|c| !c.is_whitespace()).count() >= MIN_CITATION_CHARS
            && !is_bare_locator(&cite)
            && content.iter().any(|c| c.contains(&cite))
    })
}

/// [`every_citation_matches`] against a rendered pack — the citation guard on its own.
#[cfg(test)]
fn verdict_is_cited(pack_text: &str, parsed: &ParsedVerdict) -> bool {
    every_citation_matches(&pack_content_lines(pack_text), parsed)
}

/// The pack's citable content lines, whitespace-normalized — the one set both the citation guard
/// and the divergence guard match citations against.
fn pack_content_lines(pack_text: &str) -> Vec<String> {
    pack_text.lines().filter(|line| is_pack_content_line(line)).map(normalize_ws).collect()
}

/// Minimum normalized length of a copied note claim. This rejects vacuous fragments such as a
/// single identifier while remaining below the shortest useful sentence in the reviewed replay
/// set.
const MIN_CLAIM_CHARS: usize = 20;
const MIN_CLAIM_WORDS: usize = 4;

/// Deterministic grounding guard for a parsed verdict.
///
/// Every verdict still needs real pack citations. A `diverged` verdict additionally needs a
/// substantial `CLAIM:` copied from the note, and it cannot rest solely on `TextPresent`
/// identifier rows. Text presence can support `current`, but by itself it is not proof that a
/// load-bearing note claim is contradicted.
pub(super) fn verdict_is_grounded(
    note_title: &str,
    note_body: &str,
    pack_text: &str,
    parsed: &ParsedVerdict,
) -> bool {
    let content = pack_content_lines(pack_text);
    if !every_citation_matches(&content, parsed) {
        return false;
    }
    if parsed.verdict == Verdict::Current {
        return true;
    }
    let Some(claim) = parsed.claim.as_deref() else {
        return false;
    };
    let claim = normalize_ws(claim.trim().trim_matches('"'));
    // The claim must ground in the title OR the body — never a span spliced across the seam the
    // prompt renders between them (`TITLE: {title}\n{body}`), which would let the model assert a
    // sentence the note never makes.
    if claim.chars().filter(|c| !c.is_whitespace()).count() < MIN_CLAIM_CHARS
        || claim.split_whitespace().count() < MIN_CLAIM_WORDS
        || !(claim_grounds_in_span(&claim, note_title) || claim_grounds_in_span(&claim, note_body))
    {
        return false;
    }

    // The resolver owns the resolution labels; `render_pack` joins each to its identifier with an
    // arrow (`->`).
    let absent_row = format!("-> {}", verify::NOT_FOUND);
    let text_present_symbol_row = format!("-> {}", verify::TEXT_PRESENT_SYMBOL);
    let text_present_file_row = format!("-> {}", verify::TEXT_PRESENT_FILE);
    parsed.evidence.iter().any(|evidence| {
        let cite = normalize_ws(evidence);
        content.iter().any(|line| {
            line.contains(&cite)
                && !line.contains(&text_present_symbol_row)
                && !line.contains(&text_present_file_row)
                && match identifier_from_pack_line(line) {
                    // An identifier row grounds divergence only as ABSENCE evidence the citation
                    // actually names: a `symbol`/`file` PRESENCE row never contradicts a claim on
                    // its own, and a bare resolution-label citation (`NOT FOUND …`) convicts
                    // whatever row it substring-matches without naming an identifier.
                    Some(ident) =>
                        line.contains(&absent_row)
                            && cite.contains(ident)
                            && claim_mentions_identifier(&claim, ident),
                    // Excerpts remain useful model context, but subject overlap cannot
                    // deterministically prove that source contradicts the copied claim.
                    None => false,
                }
        })
    })
}

/// Whether `identifier` occurs case-exactly in `text`, preserving every separator (`/`, `::`,
/// `.`, call punctuation) and delimited from surrounding word characters. Token-only comparison
/// aliases shape-distinct names such as `foo/bar` and `foo::bar`; raw exact shape and boundaries
/// are load-bearing for evidence links.
fn text_mentions_identifier(text: &str, identifier: &str) -> bool {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        return false;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(identifier).any(|(start, _)| {
        let end = start + identifier.len();
        let left_ok = start == 0 || text[..start].chars().next_back().is_none_or(|c| !is_word(c));
        let right_ok = end == text.len() || text[end..].chars().next().is_none_or(|c| !is_word(c));
        left_ok && right_ok
    })
}

/// One item of a text's mixed stream: a word or a comparison/boolean operator, with the
/// adjacency facts the grounding checks need.
#[derive(Clone, Copy)]
enum StreamItem<'a> {
    /// `code` marks words inside a backtick span (identifiers): they match case-EXACTLY, per
    /// occurrence — the same spelling in prose still case-folds. `glued_to_prev` records
    /// zero-whitespace adjacency to the previous stream ITEM (word or operator).
    Word { text: &'a str, code: bool, glued_to_prev: bool },
    /// `glued_to_prev` as above — the matched claim window extends over glued operator chains
    /// (`Vec<T>`, `Option<Vec<T>>`, `!ready`) but never across whitespace into a sibling
    /// sentence.
    Op { symbol: &'a str, glued_to_prev: bool },
}

/// Words and operators in source order. Word boundaries follow the alphanumeric/`_` rule;
/// operators scan two-character forms first (so `<=` is not `<` + `=`, `!=` is not `!` + `=`),
/// then the single-character comparisons `<`/`>`. Syntax arrows (`->`, `=>`) are punctuation,
/// NOT operators. A unary `!` is an operator ONLY glued to a following word (`!ready`) — a
/// prose exclamation (`Warning!`) is punctuation a copying model may drop. Advances by CHAR,
/// not byte — notes are full of em-dashes, and a byte step off a multi-byte boundary panics.
fn mixed_stream(text: &str) -> Vec<StreamItem<'_>> {
    const OPS: &[&str] = &["==", "!=", "<=", ">=", "&&", "||"];
    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    let mut items: Vec<StreamItem<'_>> = Vec::new();
    let mut rest = text;
    let mut in_code = false;
    // Byte offset of `rest` within `text` and the end of the last EMITTED item (glue detection).
    let mut base = 0usize;
    let mut prev_item_end: Option<usize> = None;
    let glued = |start: usize, prev: Option<usize>| prev == Some(start);
    while let Some(c) = rest.chars().next() {
        if c == '`' {
            in_code = !in_code;
            base += 1;
            rest = &rest[1..];
            continue;
        }
        // Syntax arrows are punctuation, not comparison operators.
        if rest.starts_with("->") || rest.starts_with("=>") {
            base += 2;
            rest = &rest[2..];
            continue;
        }
        if let Some(pos) = OPS.iter().position(|op| rest.starts_with(op)) {
            let end = base + 2;
            items.push(StreamItem::Op {
                symbol: OPS[pos],
                glued_to_prev: glued(base, prev_item_end),
            });
            prev_item_end = Some(end);
            base = end;
            rest = &rest[2..];
            continue;
        }
        if c == '<' || c == '>' || (c == '!' && rest[1..].chars().next().is_some_and(is_word_char))
        {
            let symbol = if c == '<' {
                "<"
            } else if c == '>' {
                ">"
            } else {
                "!"
            };
            let end = base + 1;
            items.push(StreamItem::Op { symbol, glued_to_prev: glued(base, prev_item_end) });
            prev_item_end = Some(end);
            base = end;
            rest = &rest[1..];
            continue;
        }
        if is_word_char(c) {
            let end_rel = rest.find(|ch: char| !is_word_char(ch)).unwrap_or(rest.len());
            items.push(StreamItem::Word {
                text: &rest[..end_rel],
                code: in_code,
                glued_to_prev: glued(base, prev_item_end),
            });
            prev_item_end = Some(base + end_rel);
            base += end_rel;
            rest = &rest[end_rel..];
            continue;
        }
        base += c.len_utf8();
        rest = &rest[c.len_utf8()..];
    }
    items
}

/// Whether the WHOLE claim grounds in the span: its words form one contiguous verbatim run
/// (prose case-folded, backticked identifiers case-exact per occurrence), and the claim's
/// operators occur in the same positions as the matched WINDOW's operators — the window extended
/// over operator chains GLUED to its boundary words (`Vec<T>`, `Option<Vec<T>>`, `!ready`) but
/// never across whitespace into a sibling sentence. Flipped, invented, or DROPPED operators
/// (`!ready` → `ready`) all reject. Operators stay out of the word comparison so a model that
/// drops backticks still matches.
fn claim_grounds_in_span(claim: &str, span: &str) -> bool {
    let stream = mixed_stream(span);
    let claim_stream = mixed_stream(claim);
    let claim_words: Vec<&str> = claim_stream
        .iter()
        .filter_map(|item| match item {
            StreamItem::Word { text, .. } => Some(*text),
            StreamItem::Op { .. } => None,
        })
        .collect();
    if claim_words.is_empty() {
        return false;
    }
    let word_positions: Vec<usize> = stream
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| matches!(item, StreamItem::Word { .. }).then_some(idx))
        .collect();
    if word_positions.len() < claim_words.len() {
        return false;
    }
    'windows: for start in 0..=(word_positions.len() - claim_words.len()) {
        for (offset, claim_word) in claim_words.iter().enumerate() {
            let StreamItem::Word { text: note_word, code, .. } =
                stream[word_positions[start + offset]]
            else {
                unreachable!("word_positions indexes only Word items")
            };
            let matches = if code {
                note_word == *claim_word
            } else {
                note_word.eq_ignore_ascii_case(claim_word)
            };
            if !matches {
                continue 'windows;
            }
        }
        let mut first = word_positions[start];
        let mut last = word_positions[start + claim_words.len() - 1];
        // Extend over operators glued to the boundary words (attached unary `!`, generic
        // brackets — including CHAINS like `>>`): they belong to the copied expression even
        // though they sit outside the first/last word.
        while first > 0
            && matches!(stream[first - 1], StreamItem::Op { .. })
            && matches!(
                stream[first],
                StreamItem::Word { glued_to_prev: true, .. }
                    | StreamItem::Op { glued_to_prev: true, .. }
            )
        {
            first -= 1;
        }
        while last + 1 < stream.len()
            && matches!(stream[last + 1], StreamItem::Op { glued_to_prev: true, .. })
        {
            last += 1;
        }
        let window_shape: Vec<Option<&str>> = stream[first..=last]
            .iter()
            .map(|item| match item {
                StreamItem::Op { symbol, .. } => Some(*symbol),
                StreamItem::Word { .. } => None,
            })
            .collect();
        let claim_shape: Vec<Option<&str>> = claim_stream
            .iter()
            .map(|item| match item {
                StreamItem::Op { symbol, .. } => Some(*symbol),
                StreamItem::Word { .. } => None,
            })
            .collect();
        if window_shape == claim_shape {
            return true;
        }
    }
    false
}

/// Whether a cited identifier occurs in the claim with its COMPLETE, case-exact shape — not as a
/// substring (`writer_state` vs `active_writer_stateful`), case variant (`foo` vs `Foo`), or
/// separator variant (`foo/bar` vs `foo::bar`).
fn claim_mentions_identifier(claim: &str, identifier: &str) -> bool {
    text_mentions_identifier(claim, identifier)
}

/// Extract the identifier from a rendered table row (``- `identifier` -> resolution``). Divergence
/// citations to identifier rows must name an identifier that also occurs in the copied claim; this
/// prevents an incidental NOT-FOUND row elsewhere in a long note from back-justifying an unrelated
/// load-bearing claim.
fn identifier_from_pack_line(line: &str) -> Option<&str> {
    line.trim().strip_prefix("- `")?.split_once("` ->").map(|(identifier, _)| identifier)
}

/// Whether a citation is ONLY a `path:line` (or `path:line:`) locator with no source text after it.
/// An excerpt content line renders as `path:line: <code>`, so a bare `src/lib.rs:12` is a substring
/// of it and would satisfy the substring guard without citing any actual code or identifier — a
/// content-free citation. Requiring text BEYOND the locator forces the model to cite the source it
/// claims supports the verdict. A backticked identifier citation (`` `thing_two` ``) has no
/// `:<line>` tail, so it is never a bare locator.
fn is_bare_locator(cite: &str) -> bool {
    let trimmed = cite.trim().trim_end_matches(':');
    // Any whitespace means there is text beyond the locator token — not bare.
    if trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    // A locator ends `<path>:<digits>`; treat that shape (nothing after the line number) as bare.
    match trimmed.rsplit_once(':') {
        Some((path, line)) =>
            !path.is_empty() && !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Whether a rendered pack line is CITABLE CONTENT — an identifier-table entry (`` - `ident` -> …
/// ``) or a bound-file excerpt line (`path:line: text`) — rather than a section header or
/// boilerplate (`IDENTIFIERS (…):`, `BOUND-FILE EXCERPTS (…):`, `- (no identifiers extracted)`,
/// `(no bound-file excerpts)`, or a `path:start-end` range header). The citation guard matches only
/// these, so pack scaffolding can't satisfy a fabricated citation. Mirrors [`super::render_pack`]'s
/// emitted shapes.
fn is_pack_content_line(line: &str) -> bool {
    let line = line.trim();
    // Identifier-table entry: `- ` then a backtick span. The `- (no identifiers extracted)`
    // boilerplate starts with `- (`, not a backtick, so it is excluded.
    if let Some(rest) = line.strip_prefix("- ") {
        return rest.starts_with('`');
    }
    // Bound-file excerpt line: a `path:<digits>: text` locator (the first `": "` is preceded by an
    // ASCII digit). The `path:start-end` range headers use a dash and the section
    // headers/boilerplate carry no such locator, so only real excerpt lines match.
    match line.find(": ") {
        Some(idx) => line[..idx].bytes().next_back().is_some_and(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Collapse every whitespace run (incl. newlines) to a single space and trim — so a citation that
/// differs only in spacing/wrapping still matches a pack line.
pub(super) fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::super::tests::current_citing;
    use super::super::{
        Direction, EvidencePack, ResolutionKind, VERDICT_PROMPT_HEAD, parse_verdict, render_pack,
    };
    use super::*;

    #[test]
    fn citation_guard_accepts_a_real_pack_line_and_rejects_fabrication() {
        let pack = "IDENTIFIERS:\n- `real_symbol` -> symbol src/lib.rs::real_symbol\n";
        let good = parse_verdict(&current_citing("real_symbol")).unwrap();
        assert!(verdict_is_cited(pack, &good), "a citation present in the pack is accepted");

        let fabricated = parse_verdict(&current_citing("ghost_symbol")).unwrap();
        assert!(
            !verdict_is_cited(pack, &fabricated),
            "a citation absent from the pack is rejected"
        );

        let empty =
            parse_verdict("VERDICT: current\nDIRECTION: unknown\nEVIDENCE:\nREASON: none").unwrap();
        assert!(!verdict_is_cited(pack, &empty), "an empty-evidence verdict is rejected too");
    }

    #[test]
    fn citation_guard_rejects_header_and_boilerplate_fragments() {
        // The rendered pack's section headers + boilerplate are NOT citable content: a verdict that
        // cites one (even a long verbatim fragment lifted from the pack) is rejected, so the pack
        // scaffolding the guard itself emits can't satisfy a fabricated citation. Only
        // identifier-table entries and excerpt lines count.
        let pack = render_pack(&EvidencePack {
            memory_id: "m1".to_string(),
            identifiers: vec![verify::IdentifierResolution {
                identifier: "real_symbol".to_string(),
                resolution: "symbol src/lib.rs::real_symbol".to_string(),
                kind: verify::ResolutionKind::Symbol,
            }],
            excerpts: Vec::new(),
            has_live_binding: false,
        });
        let cited = |frag: &str| ParsedVerdict {
            verdict: Verdict::Current,
            direction: Direction::Unknown,
            claim: None,
            evidence: vec![frag.to_string()],
        };
        assert!(
            !verdict_is_cited(
                &pack,
                &cited("IDENTIFIERS (resolved against the active source index):")
            ),
            "the identifier section header is not citable content"
        );
        assert!(
            !verdict_is_cited(&pack, &cited("BOUND-FILE EXCERPTS (current source):")),
            "the excerpts section header is not citable content"
        );
        assert!(
            !verdict_is_cited(&pack, &cited("source")),
            "a bare boilerplate token is below the citation-length floor"
        );
        // The accept path stays green: a genuine identifier-table entry is still cited.
        assert!(
            verdict_is_cited(&pack, &cited("`real_symbol` -> symbol src/lib.rs::real_symbol")),
            "a real identifier-table entry is still accepted"
        );
    }

    #[test]
    fn citation_guard_rejects_a_bare_locator_prefix() {
        // Regression (PR #428): an excerpt renders as `path:line: <code>`, so a bare
        // `path:line` locator is a substring of it and long enough — but cites no actual source.
        // The guard must require text beyond the locator.
        let pack = render_pack(&EvidencePack {
            memory_id: "m1".to_string(),
            identifiers: Vec::new(),
            excerpts: vec![verify::FileExcerpt {
                path: "crates/x/src/lib.rs".to_string(),
                start_line: 12,
                end_line: 12,
                text: "let handle = spawn();".to_string(),
            }],
            has_live_binding: false,
        });
        let cited = |frag: &str| ParsedVerdict {
            verdict: Verdict::Diverged,
            direction: Direction::Unknown,
            claim: Some("the note makes a substantial claim".to_string()),
            evidence: vec![frag.to_string()],
        };
        assert!(
            !verdict_is_cited(&pack, &cited("crates/x/src/lib.rs:12")),
            "a bare path:line locator cites no source and is rejected"
        );
        assert!(
            !verdict_is_cited(&pack, &cited("crates/x/src/lib.rs:12:")),
            "a bare path:line: locator is rejected too"
        );
        // The full excerpt line (locator + code) is real evidence and still accepted.
        assert!(
            verdict_is_cited(&pack, &cited("crates/x/src/lib.rs:12: let handle = spawn();")),
            "the excerpt line with its source text is accepted"
        );
    }

    #[test]
    fn divergence_guard_requires_a_verbatim_note_claim() {
        let note = "The `gone_symbol` function remains available to callers.";
        let pack = "IDENTIFIERS:\n- `gone_symbol` -> NOT FOUND anywhere in the source tree\n";
        let grounded = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `gone_symbol` function remains \
             available to callers.\nEVIDENCE:\n- `gone_symbol` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &grounded),
            "a substantial claim copied from the note plus absence evidence is grounded"
        );
        let quoted = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: \"The `gone_symbol` function \
             remains available to callers.\"\nEVIDENCE:\n- `gone_symbol` -> NOT FOUND anywhere in \
             the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &quoted),
            "harmless outer quotes around a verbatim claim are accepted"
        );

        let missing = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nEVIDENCE:\n- `gone_symbol` -> NOT FOUND \
             anywhere in the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &missing),
            "divergence without a copied note claim is rejected"
        );

        let paraphrased = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: Callers can still use the gone \
             function.\nEVIDENCE:\n- `gone_symbol` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &paraphrased),
            "a plausible paraphrase is not deterministic claim grounding"
        );
    }

    #[test]
    fn divergence_guard_rejects_text_present_only_evidence() {
        let note = "The content_hash column is persisted with each verdict.";
        let pack = "IDENTIFIERS:\n- `content_hash` -> not a defined symbol; appears verbatim as \
                    source text\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: unknown\nCLAIM: The content_hash column is persisted \
             with each verdict.\nEVIDENCE:\n- `content_hash` -> not a defined symbol; appears \
             verbatim as source text\nREASON: not a symbol.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "text presence alone cannot establish a contradiction"
        );
    }

    #[test]
    fn divergence_guard_requires_cited_identifier_to_occur_in_the_claim() {
        let note = "The active writer remains serialized. An old fixture used `gone_helper`.";
        let pack = "IDENTIFIERS:\n- `gone_helper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The active writer remains \
             serialized.\nEVIDENCE:\n- `gone_helper` -> NOT FOUND anywhere in the source \
             tree\nREASON: helper gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "an incidental absence elsewhere in the note cannot back-justify the copied claim"
        );
    }

    #[test]
    fn divergence_guard_rejects_excerpt_only_contradictions() {
        let note = "The `active_writer` guard remains serialized.";
        let pack = "IDENTIFIERS:\n- `active_writer` -> symbol \
                    crates/x/src/lib.rs::active_writer\n\nBOUND-FILE \
                    EXCERPTS:\ncrates/x/src/lib.rs:12: let handle = \
                    spawn();\ncrates/x/src/lib.rs:40: let active_writer = spawn();\n";
        let unrelated = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer` guard remains \
             serialized.\nEVIDENCE:\n- crates/x/src/lib.rs:12: let handle = spawn();\nREASON: \
             contradicts.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &unrelated),
            "an excerpt about an unrelated mechanism is not linked evidence"
        );
        let same_subject = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer` guard remains \
             serialized.\nEVIDENCE:\n- crates/x/src/lib.rs:40: let active_writer = \
             spawn();\nREASON: contradicts.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &same_subject),
            "subject overlap cannot prove that an excerpt contradicts the claim"
        );
    }

    #[test]
    fn divergence_guard_matches_the_cited_identifier_as_a_complete_token() {
        let note = "The `active_writer_stateful` guard remains available. The legacy \
                    `writer_state` was removed.";
        let pack = "IDENTIFIERS:\n- `writer_state` -> NOT FOUND anywhere in the source tree\n- \
                    `active_writer_stateful` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer_stateful` guard \
             remains available.\nEVIDENCE:\n- `writer_state` -> NOT FOUND anywhere in the source \
             tree\nREASON: writer gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a substring of a longer identifier is not the cited identifier"
        );
        let grounded = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer_stateful` guard \
             remains available.\nEVIDENCE:\n- `active_writer_stateful` -> NOT FOUND anywhere in \
             the source tree\nREASON: contradicts the note.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &grounded),
            "the exact identifier in the claim still grounds the verdict"
        );
    }

    #[test]
    fn divergence_guard_preserves_identifier_separators() {
        let note = "The `foo/bar` file remains available; legacy `foo::bar` was removed.";
        let pack = "IDENTIFIERS:\n- `foo::bar` -> NOT FOUND anywhere in the source tree\n- \
                    `foo/bar` -> NOT FOUND anywhere in the source tree\n";
        let wrong_shape = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `foo/bar` file remains \
             available.\nEVIDENCE:\n- `foo::bar` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &wrong_shape),
            "a `foo::bar` absence row does not link to the `foo/bar` file claim"
        );
        let exact = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `foo/bar` file remains \
             available.\nEVIDENCE:\n- `foo/bar` -> NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &exact),
            "the exact separator-preserving identifier still links"
        );
    }

    #[test]
    fn divergence_guard_delimits_punctuation_edged_identifiers() {
        assert!(!text_mentions_identifier("Status.idle", ".idle"));
        assert!(text_mentions_identifier("state is `.idle`", ".idle"));
        assert!(!text_mentions_identifier("foo()suffix", "foo()"));
        assert!(text_mentions_identifier("call `foo()` now", "foo()"));
    }

    #[test]
    fn divergence_guard_rejects_a_generic_word_excerpt_link() {
        // A generic domain word (`error`) shared with an excerpt about something else must not
        // establish the link — only a shared pack identifier does.
        let note = "The `active_writer` guard reports an error and halts.";
        let pack = "IDENTIFIERS:\n- `active_writer` -> symbol \
                    crates/x/src/lib.rs::active_writer\n\nBOUND-FILE EXCERPTS:\nsrc/parser.rs:9: \
                    // parse error recovery\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `active_writer` guard reports \
             an error and halts.\nEVIDENCE:\n- src/parser.rs:9: // parse error recovery\nREASON: \
             changed.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "`error` alone does not link an unrelated excerpt to the claim"
        );
    }

    #[test]
    fn divergence_guard_rejects_an_excerpt_citation_without_its_locator() {
        // Source text can mimic pack rows: an excerpt whose CODE reads like a NOT FOUND row must
        // not ground a citation that omits the `path:line:` locator.
        let note = "The `gone_helper` function remains available.";
        let pack = "IDENTIFIERS:\n- `gone_helper` -> not a defined symbol; appears verbatim as \
                    source text\n\nBOUND-FILE EXCERPTS:\ncrates/x/src/lib.rs:12: // - \
                    `gone_helper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `gone_helper` function remains \
             available.\nEVIDENCE:\n- `gone_helper` -> NOT FOUND anywhere in the source \
             tree\nREASON: spoofed.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "label text mimicked inside excerpt code is not a NOT FOUND row"
        );
    }

    #[test]
    fn divergence_guard_rejects_an_operator_flipped_claim() {
        let note = "The parser rejects input where `limit <= 0` and returns an error.";
        let pack = "IDENTIFIERS:\n- `limit` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             limit >= 0 and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a claim that flips the note's operator asserts the opposite and must not ground"
        );
        let verbatim = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             `limit <= 0` and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in \
             the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &verbatim),
            "a verbatim operator copy still grounds"
        );
    }

    #[test]
    fn divergence_guard_tolerates_non_ascii_note_text() {
        // Regression: the operator scan must not panic on multi-byte characters (em-dash) —
        // real notes are full of them.
        let note = "The writer — serialized globally — rejects `limit <= 0` here.";
        let pack = "IDENTIFIERS:\n- `limit` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The writer — serialized globally — \
             rejects `limit <= 0` here.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "a grounded verdict over non-ASCII note text is accepted, not panicked on"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_single_char_operator_flip() {
        let note = "The parser rejects input where `limit > 0` and returns an error.";
        let pack = "IDENTIFIERS:\n- `limit` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             limit < 0 and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a single-character `>`→`<` flip asserts the opposite and must not ground"
        );
        let verbatim = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             `limit > 0` and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &verbatim),
            "a verbatim copy carrying the same operator still grounds"
        );
    }

    #[test]
    fn divergence_guard_preserves_operator_to_operand_order() {
        let note = "The range requires `low < value && value > high` before continuing.";
        let pack = "IDENTIFIERS:\n- `value` -> NOT FOUND anywhere in the source tree\n";
        let flipped = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The range requires `low > value && \
             value < high` before continuing.\nEVIDENCE:\n- `value` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &flipped),
            "the same operator multiset attached to different operands must not ground"
        );
    }

    #[test]
    fn divergence_guard_does_not_alias_case_distinct_identifiers() {
        // The note says `Foo` remains while legacy `foo` was removed; a claim that LOWERCASES
        // `Foo` to `foo` and cites the `foo` absence row turns a confirmation into a false
        // divergence. Backticked words match case-exactly, so the lowercased claim never grounds.
        let note = "The `Foo` type remains available; legacy `foo` was removed.";
        let pack = "IDENTIFIERS:\n- `foo` -> NOT FOUND anywhere in the source tree\n- `Foo` -> \
                    symbol src/lib.rs::Foo\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The foo type remains available; \
             legacy foo was removed.\nEVIDENCE:\n- `foo` -> NOT FOUND anywhere in the source \
             tree\nREASON: foo gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a case-variant of a backticked identifier is not the note's identifier"
        );
        let exact = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `Foo` type remains available; \
             legacy `foo` was removed.\nEVIDENCE:\n- `foo` -> NOT FOUND anywhere in the source \
             tree\nREASON: foo gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &exact),
            "a case-exact verbatim copy still grounds and links"
        );
    }

    #[test]
    fn divergence_guard_accepts_a_verbatim_leading_negation() {
        // The matched window must extend over the operator glued to its first word, or a
        // verbatim `!ready` claim is wrongly discarded.
        let note = "The gate holds while `!ready` remains false.";
        let pack = "IDENTIFIERS:\n- `ready` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The gate holds while `!ready` \
             remains false.\nEVIDENCE:\n- `ready` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "a verbatim claim beginning with an attached operator grounds"
        );
    }

    #[test]
    fn divergence_guard_ignores_syntax_arrows() {
        // `->` and `=>` are syntax punctuation, not comparison operators. A copied claim may
        // omit them under punctuation-tolerant grounding without changing semantics.
        let note = "The `load` function returns `Result` as `fn load() -> Result`; the mapper \
                    uses `x => y`.";
        let pack = "IDENTIFIERS:\n- `load` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `load` function returns \
             `Result` as fn load Result; the mapper uses x y.\nEVIDENCE:\n- `load` -> NOT FOUND \
             anywhere in the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "dropping syntax arrows does not change the copied claim"
        );
    }

    #[test]
    fn divergence_guard_accepts_nested_generic_closers() {
        // The matched window must extend through the entire glued `>>` chain after its last word.
        let note = "The `value` field stores `Option<Vec<T>>` unchanged.";
        let pack = "IDENTIFIERS:\n- `value` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `value` field stores \
             `Option<Vec<T>>` unchanged.\nEVIDENCE:\n- `value` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "a verbatim nested generic includes every glued closing bracket"
        );
    }

    #[test]
    fn divergence_guard_tolerates_a_dropped_prose_exclamation() {
        // `Warning!` is punctuation, not a unary operator — a model that copies the words but
        // drops the bang still grounds.
        let note = "Warning! The helper remains available.";
        let pack = "IDENTIFIERS:\n- `helper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: Warning! The helper remains \
             available.\nEVIDENCE:\n- `helper` -> NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(verdict_is_grounded("note", note, pack, &parsed), "verbatim with the bang grounds");
        let dropped = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: Warning The helper remains \
             available.\nEVIDENCE:\n- `helper` -> NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &dropped),
            "a dropped prose exclamation still grounds"
        );
    }

    #[test]
    fn divergence_guard_folds_case_for_prose_occurrences_of_a_backticked_word() {
        // The SAME spelling appears backticked (identifier) and bare (prose): only the backticked
        // occurrence is case-exact — a lowercased prose occurrence still grounds.
        let note = "The `Foo` type remains the Foo alias.";
        let pack = "IDENTIFIERS:\n- `Foo` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `Foo` type remains the foo \
             alias.\nEVIDENCE:\n- `Foo` -> NOT FOUND anywhere in the source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &parsed),
            "case is exact for the backticked occurrence, folded for the prose one"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_dropped_unary_negation() {
        let note = "The `publisher` stays enabled when `!ready` is true.";
        let pack = "IDENTIFIERS:\n- `publisher` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `publisher` stays enabled when \
             ready is true.\nEVIDENCE:\n- `publisher` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "dropping the note's unary `!` inverts the claim and must not ground"
        );
        let verbatim = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `publisher` stays enabled when \
             `!ready` is true.\nEVIDENCE:\n- `publisher` -> NOT FOUND anywhere in the source \
             tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            verdict_is_grounded("note", note, pack, &verbatim),
            "a verbatim copy keeping the negation still grounds"
        );
    }

    #[test]
    fn divergence_guard_pools_operators_only_within_the_grounding_span() {
        // An operator elsewhere in the note must not satisfy a flip in the copied sentence:
        // `old != new` in the TITLE does not excuse `<=`→`!=` in a claim copied from the BODY.
        let title = "Migration from `old != new` semantics";
        let body = "The parser rejects input where `limit <= 0` and returns an error.";
        let pack = "IDENTIFIERS:\n- `limit` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The parser rejects input where \
             limit != 0 and returns an error.\nEVIDENCE:\n- `limit` -> NOT FOUND anywhere in the \
             source tree\nREASON: gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded(title, body, pack, &parsed),
            "an unrelated operator elsewhere in the note does not satisfy a flipped claim"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_title_body_splice_claim() {
        let pack = "IDENTIFIERS:\n- `sweeper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: lazy and the sweeper runs \
             hourly.\nEVIDENCE:\n- `sweeper` -> NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded(
                "Cache eviction is lazy",
                "and the sweeper runs hourly.",
                pack,
                &parsed
            ),
            "a span spliced across the title/body seam is a sentence the note never makes"
        );
    }

    #[test]
    fn divergence_guard_rejects_presence_row_and_bare_label_citations() {
        // A `symbol`/`file` PRESENCE row never contradicts a claim on its own, and a bare
        // resolution-label citation (`NOT FOUND …`) names no identifier — both must be rejected
        // even when the copied claim is perfectly grounded.
        let note = "The `gone_symbol` function remains available to callers.";
        let present_pack = "IDENTIFIERS:\n- `gone_symbol` -> symbol src/lib.rs::gone_symbol\n";
        let presence = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `gone_symbol` function remains \
             available to callers.\nEVIDENCE:\n- `gone_symbol` -> symbol \
             src/lib.rs::gone_symbol\nREASON: hallucinated.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, present_pack, &presence),
            "a presence row is not contradiction evidence"
        );
        let absent_pack =
            "IDENTIFIERS:\n- `gone_symbol` -> NOT FOUND anywhere in the source tree\n";
        let bare_label = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The `gone_symbol` function remains \
             available to callers.\nEVIDENCE:\n- NOT FOUND anywhere in the source tree\nREASON: \
             gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, absent_pack, &bare_label),
            "the citation must name the identifier it convicts"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_copied_prefix_with_invented_tail() {
        let note = "The active writer remains serialized.";
        let pack = "IDENTIFIERS:\n- `gone_helper` -> NOT FOUND anywhere in the source tree\n";
        let parsed = parse_verdict(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: The active writer remains \
             serialized and gone_helper remains available\nEVIDENCE:\n- `gone_helper` -> NOT \
             FOUND anywhere in the source tree\nREASON: helper gone.",
        )
        .unwrap();
        assert!(
            !verdict_is_grounded("note", note, pack, &parsed),
            "a verbatim prefix must not ground invented prose appended to the claim"
        );
    }

    #[test]
    fn divergence_guard_rejects_a_bare_long_identifier_as_the_claim() {
        let identifier = "a_tail_failure_leaves_the_old_generation_live";
        let note = format!("The `{identifier}` test documents tail-failure recovery.");
        let pack =
            format!("IDENTIFIERS:\n- `{identifier}` -> NOT FOUND anywhere in the source tree\n");
        let parsed = parse_verdict(&format!(
            "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: \"{identifier}\"\nEVIDENCE:\n- \
             `{identifier}` -> NOT FOUND anywhere in the source tree\nREASON: gone."
        ))
        .unwrap();
        assert!(
            !verdict_is_grounded("note", &note, &pack, &parsed),
            "identifier length alone does not make it a load-bearing claim"
        );
    }

    #[test]
    fn divergence_guard_matches_the_resolution_labels_the_resolver_emits() {
        // The guard recognizes absence and presence rows by the resolver's own labels. The pack is
        // rendered from those consts, so a reworded label fails here instead of silently stopping
        // every `diverged` verdict from being accepted.
        let note = "The `gone_symbol` function remains available to callers.";
        let grounded_under = |resolution: &str, kind| {
            let pack = render_pack(&EvidencePack {
                memory_id: "m1".to_string(),
                identifiers: vec![verify::IdentifierResolution {
                    identifier: "gone_symbol".to_string(),
                    resolution: resolution.to_string(),
                    kind,
                }],
                excerpts: Vec::new(),
                has_live_binding: false,
            });
            let parsed = parse_verdict(&format!(
                "VERDICT: diverged\nDIRECTION: code_ahead\nCLAIM: {note}\nEVIDENCE:\n- \
                 `gone_symbol` -> {resolution}\nREASON: gone."
            ))
            .unwrap();
            verdict_is_grounded("note", note, &pack, &parsed)
        };
        assert!(
            grounded_under(verify::NOT_FOUND, ResolutionKind::Absent),
            "the resolver's absence label grounds a divergence"
        );
        assert!(
            !grounded_under(verify::TEXT_PRESENT_SYMBOL, ResolutionKind::TextPresent),
            "the resolver's symbol text-presence label never grounds a divergence"
        );
        assert!(
            !grounded_under(verify::TEXT_PRESENT_FILE, ResolutionKind::TextPresent),
            "the resolver's file text-presence label never grounds a divergence"
        );
        for label in [verify::NOT_FOUND, verify::TEXT_PRESENT_SYMBOL, verify::TEXT_PRESENT_FILE] {
            assert!(
                VERDICT_PROMPT_HEAD.contains(label),
                "the prompt explains the `{label}` resolution verbatim"
            );
        }
    }
}
