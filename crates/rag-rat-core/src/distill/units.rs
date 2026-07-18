//! Text-unit segmentation for the distill input substrate (#703).
//!
//! A distilled thread's body and comments are split into numbered UNITS the model cites by id;
//! quotes then materialize MECHANICALLY as the exact source substring of the cited unit's byte span
//! (never re-emitted by the model, so a quote can never drift from the source). Segmentation is
//! block-level via pulldown-cmark's `OffsetIter`, which reports each event with its source byte
//! range — so a fenced code block (which contains blank lines) stays ONE unit instead of shattering
//! the way a naive blank-line split would.
//!
//! This module is deterministic and model-free: it is exercised in Phase 1 to compute the
//! regeneration hash over ordered spans, and reused at drain time (#704) to build the LLM input.

use pulldown_cmark::{Event, Options, Parser};

/// A half-open byte range `[start, end)` into the segmented text. The materialized quote for a unit
/// is exactly `&text[start..end]` — this is the whole point of carrying spans instead of copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    /// The exact source substring this span selects. Callers materialize quotes through here so the
    /// "quote == source substring" invariant holds by construction.
    pub(crate) fn slice(self, text: &str) -> &str {
        &text[self.start..self.end]
    }

    fn len(self) -> usize {
        self.end - self.start
    }
}

/// Split one text blob into block-level units, in source order, dropping whitespace-only gaps.
///
/// Each top-level CommonMark block (paragraph, heading, list, block quote, fenced/indented code,
/// table, …) becomes one unit spanning from its opening event to its closing event. Nested blocks
/// (a list's items, a quote's paragraphs) stay folded into their enclosing top-level unit so a
/// citation targets a coherent chunk, not a fragment. Text with no block structure (or a parser
/// that yields nothing, e.g. empty input) yields no units.
pub(crate) fn segment_blocks(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut depth: u32 = 0;
    let mut open_start: Option<usize> = None;
    // A run of consecutive TOP-LEVEL block-HTML events (`<details>`, HTML tables, generated
    // reports). pulldown-cmark emits raw block HTML as `Event::Html` OUTSIDE any Start/End pair —
    // one event per line — so without this it would contribute no unit, no hash content, and no
    // model-visible evidence. Nested HTML (inside a block quote/list) rides its enclosing block's
    // span, so only depth-0 HTML needs its own unit. Coalesce adjacent lines into one span.
    let mut html_run: Option<(usize, usize)> = None;
    // GFM tables / strikethrough / task lists appear in tracker markdown; enabling them keeps their
    // ranges coherent instead of leaking table pipes into paragraph units.
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        if depth == 0 && matches!(event, Event::Html(_)) {
            html_run = Some((html_run.map_or(range.start, |(start, _)| start), range.end));
            continue;
        }
        // Any other event ends a pending top-level HTML run.
        if let Some((start, end)) = html_run.take() {
            push_trimmed(&mut spans, text, start, end);
        }
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    open_start = Some(range.start);
                }
                depth += 1;
            },
            Event::End(_) => {
                // Saturating: a malformed stream cannot underflow us into a bogus wide span.
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(start) = open_start.take()
                {
                    push_trimmed(&mut spans, text, start, range.end);
                }
            },
            _ => {},
        }
    }
    if let Some((start, end)) = html_run.take() {
        push_trimmed(&mut spans, text, start, end);
    }
    spans
}

/// Record `[start, end)` with surrounding ASCII whitespace trimmed off the span itself, so the
/// materialized quote has no leading/trailing blank lines. Empty-after-trim spans are dropped.
fn push_trimmed(spans: &mut Vec<Span>, text: &str, start: usize, end: usize) {
    let raw = &text[start..end];
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    let lead = raw.len() - raw.trim_start().len();
    let trail = raw.len() - raw.trim_end().len();
    spans.push(Span { start: start + lead, end: end - trail });
}

/// Tail-aware budget: keep whole units from the head and the tail, dropping a contiguous MIDDLE
/// run, until the retained byte total fits `max_total`. Returns the retained unit indices in order
/// plus a `dropped` count. The head+tail bias is deliberate — a thread's framing (the opening
/// report) and its resolution (the closing decision) carry more signal than the middle churn.
///
/// Units already within budget are returned unchanged. Only a SINGLE indivisible over-budget unit
/// is kept past the limit (we never split a unit — that would break the span→quote invariant); any
/// other over-budget set is trimmed, so even a two-unit thread (a short title + a huge body) cannot
/// smuggle an arbitrarily large input past the budget.
pub(crate) fn tail_aware_budget(spans: &[Span], max_total: usize) -> BudgetPlan {
    let total: usize = spans.iter().map(|s| s.len()).sum();
    if total <= max_total {
        return BudgetPlan { kept: (0..spans.len()).collect(), dropped: 0, kept_bytes: total };
    }
    // The first unit (the framing) is ALWAYS retained — even if it alone exceeds the budget — since
    // it carries the problem statement. Then fill the remaining budget from the tail (the
    // resolution) first, extending the head only after the tail can grow no further. On a miss at
    // one end the other end is still tried, so an uneven thread (e.g. `[8, 8, 2]` under a 10-byte
    // budget) keeps head+tail (`[0, 2]`) instead of stalling on the first over-budget middle unit.
    let n = spans.len();
    let mut head = 1usize;
    let mut tail = 0usize;
    let mut kept_bytes = spans[0].len();
    while head + tail < n {
        let head_idx = head;
        let tail_idx = n - 1 - tail;
        if try_grow(spans, tail_idx, &mut kept_bytes, max_total).is_some() {
            tail += 1;
        } else if head_idx != tail_idx
            && try_grow(spans, head_idx, &mut kept_bytes, max_total).is_some()
        {
            head += 1;
        } else {
            break;
        }
    }
    let mut kept: Vec<usize> = (0..head).collect();
    kept.extend((n - tail)..n);
    BudgetPlan { dropped: n - kept.len(), kept_bytes, kept }
}

fn try_grow(spans: &[Span], idx: usize, kept_bytes: &mut usize, max_total: usize) -> Option<()> {
    let next = *kept_bytes + spans[idx].len();
    if next <= max_total {
        *kept_bytes = next;
        Some(())
    } else {
        None
    }
}

/// The retained-unit plan from [`tail_aware_budget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BudgetPlan {
    /// Retained unit indices, in source order (a head prefix followed by a tail suffix).
    pub kept: Vec<usize>,
    /// Number of units dropped from the middle.
    pub dropped: usize,
    /// Total bytes across the retained units.
    pub kept_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::{Span, segment_blocks, tail_aware_budget};

    #[test]
    fn a_fenced_code_block_stays_one_unit_despite_its_blank_lines() {
        let text = "Intro paragraph.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\nClosing note.";
        let spans = segment_blocks(text);
        let quotes: Vec<&str> = spans.iter().map(|s| s.slice(text)).collect();
        assert_eq!(quotes.len(), 3, "intro, whole code block, closing");
        assert_eq!(quotes[0], "Intro paragraph.");
        assert!(quotes[1].contains("fn a()") && quotes[1].contains("fn b()"), "code block whole");
        assert_eq!(quotes[2], "Closing note.");
    }

    #[test]
    fn every_span_slices_back_to_its_exact_source_substring() {
        let text = "First para.\n\n- item one\n- item two\n\n> a quote line";
        let text_bytes = text.as_bytes();
        for span in segment_blocks(text) {
            // The materialized quote is byte-exact and trimmed of surrounding whitespace.
            let quote = span.slice(text);
            assert_eq!(quote.as_bytes(), &text_bytes[span.start..span.end]);
            assert_eq!(quote, quote.trim());
        }
    }

    #[test]
    fn top_level_raw_html_blocks_become_a_unit() {
        // A contiguous raw-HTML block (GitHub `<details>`, HTML tables, generated reports) arrives
        // as raw block HTML with no Start/End pair; its consecutive lines must coalesce into ONE
        // captured unit instead of vanishing.
        let text = "Intro.\n\n<div class=\"report\">\n<p>raw html body</p>\n</div>\n\nOutro.";
        let quotes: Vec<String> =
            segment_blocks(text).iter().map(|s| s.slice(text).to_string()).collect();
        assert!(
            quotes
                .iter()
                .any(|q| q.contains("<div") && q.contains("</div>") && q.contains("raw html body")),
            "the html block is captured whole as one unit: {quotes:?}",
        );
        assert!(quotes.iter().any(|q| q == "Intro."));
        assert!(quotes.iter().any(|q| q == "Outro."));
    }

    #[test]
    fn empty_and_whitespace_only_text_yield_no_units() {
        assert!(segment_blocks("").is_empty());
        assert!(segment_blocks("   \n\n  \t\n").is_empty());
    }

    #[test]
    fn budget_keeps_head_and_tail_dropping_the_middle() {
        // Five 10-byte units; a 25-byte budget keeps two from the ends and drops the middle three.
        let spans: Vec<Span> = (0..5).map(|i| Span { start: i * 10, end: i * 10 + 10 }).collect();
        let plan = tail_aware_budget(&spans, 25);
        assert_eq!(plan.kept_bytes, 20, "two 10-byte units fit under 25");
        assert_eq!(plan.dropped, 3);
        assert_eq!(plan.kept, vec![0, 4], "head unit 0 and tail unit 4 survive");
    }

    #[test]
    fn budget_keeps_head_and_tail_on_an_uneven_thread_instead_of_stalling() {
        // `[8, 8, 2]` under a 10-byte budget: the middle unit (index 1) does not fit after the
        // head, but the 2-byte tail does — head+tail = 10. The old early-exit kept only
        // `[0]`.
        let spans = vec![Span { start: 0, end: 8 }, Span { start: 8, end: 16 }, Span {
            start: 16,
            end: 18,
        }];
        let plan = tail_aware_budget(&spans, 10);
        assert_eq!(plan.kept, vec![0, 2], "head unit 0 and tail unit 2 both fit");
        assert_eq!(plan.kept_bytes, 10);
        assert_eq!(plan.dropped, 1);
    }

    #[test]
    fn budget_trims_a_two_unit_thread_that_exceeds_the_limit() {
        // A short title + a huge body must not slip past the budget just because there are two
        // units: the framing survives, the over-budget body is dropped.
        let spans = vec![Span { start: 0, end: 4 }, Span { start: 4, end: 1000 }];
        let plan = tail_aware_budget(&spans, 10);
        assert_eq!(plan.kept, vec![0], "the framing title is kept; the huge body is dropped");
        assert_eq!(plan.kept_bytes, 4);
        assert_eq!(plan.dropped, 1);
    }

    #[test]
    fn budget_within_limit_keeps_everything_in_order() {
        let spans: Vec<Span> = (0..4).map(|i| Span { start: i * 5, end: i * 5 + 5 }).collect();
        let plan = tail_aware_budget(&spans, 1000);
        assert_eq!(plan.kept, vec![0, 1, 2, 3]);
        assert_eq!(plan.dropped, 0);
    }

    #[test]
    fn budget_keeps_at_least_the_first_unit_when_it_alone_exceeds_the_limit() {
        let spans = vec![Span { start: 0, end: 100 }, Span { start: 100, end: 110 }, Span {
            start: 110,
            end: 120,
        }];
        let plan = tail_aware_budget(&spans, 10);
        assert_eq!(plan.kept, vec![0], "the framing unit always survives");
        assert_eq!(plan.dropped, 2);
    }
}
