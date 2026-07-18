//! Trim repeated, low-signal meta from MCP tool results to cut per-call tokens (#752). The MCP
//! server is one process per Claude Code session, so "this agent" = this process; the per-agent
//! throttles below live in-process (a TTL seen-set, like `agent_hook`'s session dedup).
//!
//! Three trims, applied to every non-`memory_*` tool result before rendering:
//! - DRIVE-BY MEMORY DEDUP: a repo memory riding ALONG a result whose exact content this agent
//!   already saw is replaced with a tiny stub — the agent still learns it's relevant here and can
//!   `memory_show` it, without re-reading the body. Keyed by CONTENT, not just id, so a later,
//!   richer view (e.g. `impact_surface full_memories:true` after a compact `symbol_lookup`) is
//!   shown in full rather than stubbed down. (Explicit `memory_*` tools are never trimmed — the
//!   agent asked.)
//! - STATIC CAVEAT THROTTLE: the always-identical disclaimers (`GRAPH_SYNTACTIC_CAVEAT`,
//!   `NO_STATIC_CALLERS_NOTE`) are elided after the agent has seen them within the window; dynamic
//!   caveats (truncation, gap counts) always pass.
//! - REDUNDANT EDGE FLAGS (stateless): per-edge `shown_by_default` / `verified_target_symbol` are
//!   dropped when `true` (interesting only when false), and `edge_confidence` when it equals the
//!   displayed `confidence` (redundant unless an oracle upgrade split them). The struct / CLI stay
//!   fully explicit; only the LLM-facing MCP output is trimmed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

/// How long a surfaced signal (a memory id, a static caveat) stays "recently seen" for this agent
/// before it may ride a drive-by result in full again.
const SEEN_TTL: Duration = Duration::from_secs(30 * 60);
/// Bound on the tracked set so a long session can't grow it without limit.
const SEEN_CAP: usize = 4096;

/// Per-agent (= per MCP process) record of which signals were surfaced in full recently, keyed by
/// an opaque string (a memory id, or a static-caveat text). Shared across service clones via an
/// `Arc`, so every tool call on the session sees the same set.
#[derive(Default)]
pub(crate) struct AgentSeen {
    inner: Mutex<HashMap<String, Instant>>,
}

impl AgentSeen {
    /// Record `key` as surfaced NOW; return whether it was ALREADY surfaced within the TTL (so the
    /// caller elides it). Prunes expired entries opportunistically once over the cap.
    fn seen_then_touch(&self, key: &str) -> bool {
        // Recover from a poisoned lock rather than panic — trimming is best-effort meta, never a
        // reason to fail a tool call.
        let mut map = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();
        let recent = map.get(key).is_some_and(|seen| now.duration_since(*seen) < SEEN_TTL);
        map.insert(key.to_string(), now);
        if map.len() > SEEN_CAP {
            map.retain(|_, seen| now.duration_since(*seen) < SEEN_TTL);
            // Still over cap after dropping expired (a burst of fresh keys): shed arbitrary entries
            // to hold the bound. A shed key just re-surfaces once more later — harmless.
            while map.len() > SEEN_CAP {
                let Some(key) = map.keys().next().cloned() else { break };
                map.remove(&key);
            }
        }
        recent
    }
}

/// Trim a tool-result payload in place: dedup drive-by memories, throttle static caveats, and drop
/// redundant per-edge flags. Idempotent on payloads that carry none of these.
pub(crate) fn trim_result(value: &mut Value, seen: &AgentSeen) {
    // A memory object → replace with a tiny stub if this EXACT content was already surfaced; never
    // descend into it (its bindings carry `memory_id` but no `title`, and a full unseen memory must
    // stay intact). The seen-key folds in a fingerprint of the whole object, so a richer view of a
    // memory whose compact header was already shown gets a fresh key and passes through in full
    // (#753 review) — a plain id key would stub the detail the agent explicitly asked for.
    if let Some((id, title)) = memory_identity(value) {
        let key = format!("{id}:{:016x}", content_fingerprint(value));
        if seen.seen_then_touch(&key) {
            *value = stub(&id, &title);
        }
        return;
    }
    match value {
        Value::Object(map) => {
            drop_redundant_edge_flags(map);
            throttle_static_caveats(map, seen);
            map.values_mut().for_each(|v| trim_result(v, seen));
        },
        Value::Array(items) => items.iter_mut().for_each(|item| trim_result(item, seen)),
        _ => {},
    }
}

/// `(memory_id, title)` when `value` is a memory object — an object with BOTH as non-empty strings.
/// A binding carries `memory_id` but no `title`, so it is not matched. Both the full `RepoMemory`
/// and the compact `CompactRepoMemory` serialize both fields.
fn memory_identity(value: &Value) -> Option<(String, String)> {
    let obj = value.as_object()?;
    let id = obj.get("memory_id")?.as_str()?;
    let title = obj.get("title")?.as_str()?;
    (!id.is_empty() && !title.is_empty()).then(|| (id.to_string(), title.to_string()))
}

/// A process-stable fingerprint of a memory object's SURFACED content, so the dedup key
/// distinguishes views of the same memory (a compact header vs the full body+bindings serialize
/// differently). In-process only — `DefaultHasher` is deterministic within this run, which is all
/// the per-session seen-set needs; the value is never persisted or compared across processes.
fn content_fingerprint(value: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Serde serializes an object's keys in a stable order, so the same view hashes identically
    // across calls while a richer view hashes differently.
    serde_json::to_string(value).unwrap_or_default().hash(&mut hasher);
    hasher.finish()
}

/// The tiny replacement for a re-surfaced memory: enough to recognize it and fetch detail, none of
/// the token-heavy body / bindings / summary.
fn stub(id: &str, title: &str) -> Value {
    json!({
        "memory_id": id,
        "title": title,
        "elided": "already surfaced this session — memory_show for detail",
    })
}

/// Drop always-default / redundant per-edge flags (stateless): the agent reads absence as the
/// default value.
fn drop_redundant_edge_flags(map: &mut Map<String, Value>) {
    // Interesting only when FALSE (a hidden-by-default edge, an unverified target).
    for flag in ["shown_by_default", "verified_target_symbol"] {
        if map.get(flag) == Some(&Value::Bool(true)) {
            map.remove(flag);
        }
    }
    // `edge_confidence` is the underlying heuristic tier; it duplicates the displayed `confidence`
    // unless an oracle upgrade split them (then it stays, showing what tree-sitter alone
    // concluded).
    if let (Some(Value::String(confidence)), Some(Value::String(edge))) =
        (map.get("confidence"), map.get("edge_confidence"))
        && confidence == edge
    {
        map.remove("edge_confidence");
    }
}

/// Throttle KNOWN-STATIC caveats/notes per agent (#752): the same disclaimer every call is elided
/// after the agent has seen it within the window. Dynamic caveats (truncation, gap counts) are not
/// in the static set, so they always pass.
fn throttle_static_caveats(map: &mut Map<String, Value>, seen: &AgentSeen) {
    // `caveats: [String]` (impact_surface): drop static entries seen recently, keep the rest.
    if let Some(Value::Array(caveats)) = map.get_mut("caveats") {
        caveats.retain(|caveat| match caveat.as_str() {
            Some(text) if is_static_caveat(text) => !seen.seen_then_touch(text),
            _ => true,
        });
    }
    // `completeness_note: String` (graph tools): drop when it's the static note and seen recently.
    if let Some(Value::String(note)) = map.get("completeness_note")
        && is_static_caveat(note)
        && seen.seen_then_touch(note)
    {
        map.remove("completeness_note");
    }
}

/// Whether `text` is one of the always-identical disclaimers (as opposed to a dynamic, per-call
/// caveat). Matched against the query layer's own consts so the two can't drift.
fn is_static_caveat(text: &str) -> bool {
    text == rag_rat_query::impact::GRAPH_SYNTACTIC_CAVEAT
        || text == rag_rat_query::graph::NO_STATIC_CALLERS_NOTE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: &str) -> Value {
        json!({
            "memory_id": id, "kind": "Invariant", "title": format!("t-{id}"),
            "body": "a long body ".repeat(50),
            "bindings": [{"memory_id": id, "path": "src/x.rs"}],
        })
    }

    #[test]
    fn first_show_is_full_then_re_show_is_stubbed() {
        let seen = AgentSeen::default();
        let mut result = json!({ "hit": "x", "memories": [memory("m1"), memory("m2")] });
        trim_result(&mut result, &seen);
        assert!(result["memories"][0]["body"].is_string(), "unseen memory keeps its body");
        assert!(result["memories"][1]["body"].is_string());

        let mut again = json!({ "memories": [memory("m1"), memory("m3")] });
        trim_result(&mut again, &seen);
        assert!(again["memories"][0]["body"].is_null(), "re-shown memory drops its body");
        assert_eq!(again["memories"][0]["memory_id"], "m1", "stub keeps the id");
        assert_eq!(again["memories"][0]["title"], "t-m1", "stub keeps the title");
        assert!(again["memories"][0]["elided"].is_string(), "stub explains how to fetch detail");
        assert!(again["memories"][1]["body"].is_string(), "a NEW memory is still full");
    }

    #[test]
    fn a_richer_view_of_a_seen_memory_is_shown_not_stubbed() {
        let seen = AgentSeen::default();
        // A compact drive-by header (symbol_lookup / default impact_surface): no body/bindings.
        let compact =
            || json!({"memory_id": "m1", "kind": "Invariant", "title": "t", "confidence": "high"});
        // The full view (impact_surface `full_memories: true`, or a `memory_*` shape): adds the
        // body.
        let full = || {
            json!({
                "memory_id": "m1", "kind": "Invariant", "title": "t", "confidence": "high",
                "body": "the full prose", "bindings": [{"memory_id": "m1", "path": "src/x.rs"}]
            })
        };

        let mut header = json!({ "memories": [compact()] });
        trim_result(&mut header, &seen);
        assert_eq!(header["memories"][0]["title"], "t", "the compact header surfaces first");

        // A FULLER view of the SAME memory must still surface in full, not be stubbed down to the
        // header the agent already saw (#753 review).
        let mut richer = json!({ "memories": [full()] });
        trim_result(&mut richer, &seen);
        assert_eq!(
            richer["memories"][0]["body"], "the full prose",
            "a richer view of a seen memory is shown, not stubbed",
        );

        // The IDENTICAL full view a second time IS stubbed (same content already surfaced).
        let mut again = json!({ "memories": [full()] });
        trim_result(&mut again, &seen);
        assert!(again["memories"][0]["body"].is_null(), "an identical re-surface is stubbed");
        assert!(again["memories"][0]["elided"].is_string());
    }

    #[test]
    fn a_binding_is_not_mistaken_for_a_memory() {
        let seen = AgentSeen::default();
        let mut result = json!({ "edges": [{"memory_id": "m9", "path": "src/y.rs"}] });
        trim_result(&mut result, &seen);
        assert_eq!(result["edges"][0]["path"], "src/y.rs", "a binding is not stubbed");
        let mut real = json!({ "memories": [memory("m9")] });
        trim_result(&mut real, &seen);
        assert!(real["memories"][0]["body"].is_string(), "the memory m9 surfaces in full");
    }

    #[test]
    fn nested_repo_memories_lanes_are_deduped() {
        let seen = AgentSeen::default();
        let mut first = json!({
            "repo_memories": { "direct": [memory("d1")], "path_crossed": [memory("p1")] }
        });
        trim_result(&mut first, &seen);
        let mut second = json!({ "repo_memories": { "direct": [memory("d1")] } });
        trim_result(&mut second, &seen);
        assert!(
            second["repo_memories"]["direct"][0]["body"].is_null(),
            "nested re-show is stubbed"
        );
    }

    #[test]
    fn static_caveats_throttle_but_dynamic_ones_always_pass() {
        let seen = AgentSeen::default();
        let dynamic = "Sections truncated at limit=50: direct_semantic_callers.";
        let make = || {
            json!({ "completeness_and_caveats": {
                "caveats": [rag_rat_query::impact::GRAPH_SYNTACTIC_CAVEAT, dynamic]
            }})
        };
        // First surface: BOTH caveats present.
        let mut first = make();
        trim_result(&mut first, &seen);
        let c1 = first["completeness_and_caveats"]["caveats"].as_array().unwrap();
        assert_eq!(c1.len(), 2, "first surface keeps both the static and dynamic caveat");

        // Second surface: the static one is dropped, the dynamic one stays.
        let mut second = make();
        trim_result(&mut second, &seen);
        let c2 = second["completeness_and_caveats"]["caveats"].as_array().unwrap();
        assert_eq!(c2.len(), 1, "static caveat throttled on re-show");
        assert_eq!(c2[0], dynamic, "the dynamic caveat always passes");
    }

    #[test]
    fn static_completeness_note_is_throttled() {
        let seen = AgentSeen::default();
        let make = || json!({ "summary": { "completeness_note": rag_rat_query::graph::NO_STATIC_CALLERS_NOTE } });
        let mut first = make();
        trim_result(&mut first, &seen);
        assert!(first["summary"]["completeness_note"].is_string(), "first surface keeps the note");
        let mut second = make();
        trim_result(&mut second, &seen);
        assert!(
            second["summary"]["completeness_note"].is_null(),
            "re-shown static note is dropped"
        );
    }

    #[test]
    fn redundant_edge_flags_are_dropped() {
        let seen = AgentSeen::default();
        let mut result = json!({ "direct_semantic_callers": [{
            "from_symbol": "a", "to_symbol": "b",
            "confidence": "syntactic", "edge_confidence": "syntactic",
            "verified_target_symbol": true, "shown_by_default": true
        }]});
        trim_result(&mut result, &seen);
        let edge = &result["direct_semantic_callers"][0];
        assert!(edge["shown_by_default"].is_null(), "shown_by_default:true is dropped");
        assert!(edge["verified_target_symbol"].is_null(), "verified_target_symbol:true is dropped");
        assert!(edge["edge_confidence"].is_null(), "edge_confidence == confidence is dropped");
        assert_eq!(edge["confidence"], "syntactic", "the displayed confidence stays");
    }

    #[test]
    fn edge_flags_kept_when_informative() {
        let seen = AgentSeen::default();
        // A hidden/unverified edge, and an oracle upgrade that split confidence — all kept.
        let mut result = json!({ "hops": [{
            "confidence": "compiler", "edge_confidence": "syntactic",
            "verified_target_symbol": false, "shown_by_default": false
        }]});
        trim_result(&mut result, &seen);
        let edge = &result["hops"][0];
        assert_eq!(edge["shown_by_default"], false, "false is informative — kept");
        assert_eq!(edge["verified_target_symbol"], false, "false is informative — kept");
        assert_eq!(edge["edge_confidence"], "syntactic", "an upgraded tier keeps edge_confidence");
    }
}
