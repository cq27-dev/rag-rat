use std::collections::{BTreeMap, BTreeSet};

use super::super::RefineMember;
use super::types::{MetavarKind, VariationPoint};

/// Render the human-readable template from the anchor's real source (§1.9): a maximal fixed run is
/// the verbatim source slice (collapsing inter-token whitespace via `prev.end .. next.start`); a
/// variation run is `⟨m{id}⟩`; a gapped run is `⟨m{id}?⟩`. `lo_to_hi` maps each occurrence's lo
/// column to its snapped span end. `zero_width_cols` are columns where a MEMBER-ONLY inserted
/// statement attaches: its hole renders AFTER the attach column's fixed token without consuming it
/// (the anchor has no column for it — it is appended source, not a substitution).
pub(super) fn render_template(
    anchor: &RefineMember,
    variation_points: &[VariationPoint],
    lo_to_hi: &BTreeMap<usize, usize>,
    zero_width_cols: &BTreeSet<usize>,
) -> String {
    let spine_len = anchor.node_spans.len();
    struct Occ {
        lo: usize,
        hi: usize,
        label: String,
        gapped: bool,
        /// Member-only insert: render after the attach column's token, don't consume it.
        zero_width: bool,
    }
    let mut occs: Vec<Occ> = Vec::new();
    for vp in variation_points {
        let gapped = vp.kind == MetavarKind::Gapped;
        for &lo in &vp.occurrences {
            let zero_width = zero_width_cols.contains(&lo);
            let hi = if zero_width { lo } else { lo_to_hi.get(&lo).copied().unwrap_or(lo) };
            occs.push(Occ { lo, hi, label: vp.metavar_id.clone(), gapped, zero_width });
        }
    }
    occs.sort_by_key(|o| o.lo);

    let label_of = |o: &Occ| {
        if o.gapped { format!("⟨{}?⟩", o.label) } else { format!("⟨{}⟩", o.label) }
    };

    // Render every zero-width member-only insert whose attach column is in `[lo..=hi]` (after the
    // fixed token, or after a consuming hole that covers those columns), in deterministic
    // sorted-`lo`-then-`metavar_id` order (`occs` is already `lo`-sorted; ties keep emission
    // order). Shared by BOTH the consuming-hole branch and the fixed-column branch so a
    // zero-width VP is NEVER dropped from the template — Fix 4 (#215 Plan 4b Codex round-4).
    let render_zero_width_in = |out: &mut String, lo: usize, hi: usize| {
        for occ in occs.iter().filter(|o| o.zero_width && lo <= o.lo && o.lo <= hi) {
            out.push(' ');
            out.push_str(&label_of(occ));
        }
    };

    let mut out = String::new();
    let mut col = 0usize;
    let mut byte_cursor: Option<usize> = None; // end_byte of the last emitted fixed token
    while col < spine_len {
        // A consuming hole (a substituted/gapped span) starts at this column → emit it and skip the
        // span.
        if let Some(occ) = occs.iter().find(|o| o.lo == col && !o.zero_width) {
            // Emit any pending whitespace between the previous fixed token and this hole.
            let hole_start = anchor.node_spans[occ.lo].start_byte;
            if let Some(prev_end) = byte_cursor
                && let Some(ws) = anchor.text.get(prev_end..hole_start)
            {
                out.push_str(ws);
            }
            out.push_str(&label_of(occ));
            byte_cursor = Some(anchor.node_spans[occ.hi].end_byte);
            // Fix 4: a zero-width member-only insert may share this `lo` column with the consuming
            // hole — or attach anywhere within the consumed span `[occ.lo..=occ.hi]` (one member
            // deletes the first anchor statement while another inserts a leading statement before
            // it). The old code `continue`d straight to `occ.hi + 1`, skipping those columns, so
            // the zero-width VP — still present in `variation_points` JSON — got NO
            // placeholder in the template (a metavar with no rendered hole). Render
            // every zero-width insert across the consumed range here, so every VP in
            // the JSON has a placeholder in the template.
            render_zero_width_in(&mut out, occ.lo, occ.hi);
            col = occ.hi + 1;
            continue;
        }
        // Fixed column: emit only leaf tokens' source verbatim (internal-node tokens are spans of
        // their leaves — emitting them would duplicate). Use the leaf's real source slice + the
        // whitespace before it.
        let span = &anchor.node_spans[col];
        if span.is_leaf {
            if let Some(prev_end) = byte_cursor
                && let Some(ws) = anchor.text.get(prev_end..span.start_byte)
            {
                out.push_str(ws);
            }
            if let Some(src) = anchor.text.get(span.start_byte..span.end_byte) {
                out.push_str(src);
            }
            byte_cursor = Some(span.end_byte);
        }
        // Member-only inserts attached at this column render right after its token (a trailing
        // appended statement), without consuming any anchor column.
        render_zero_width_in(&mut out, col, col);
        col += 1;
    }
    out
}

/// Compute coverage directly from the fixed/variation column mask. `fixed / total`, `1.0` for an
/// empty spine. This is the authoritative coverage; `anti_unify` calls it.
pub(super) fn coverage_from_mask(is_fixed: &[bool]) -> f64 {
    if is_fixed.is_empty() {
        return 1.0;
    }
    let fixed = is_fixed.iter().filter(|f| **f).count();
    fixed as f64 / is_fixed.len() as f64
}
