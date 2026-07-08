//! Immutable, content-derived stream identity (phase B, #509).
//!
//! A stream is the unit of replication: one signed, per-device hash chain and one watermark exist
//! PER stream, and a filtered view is its own stream — never a lens over another one. The
//! identity is derived from the visibility policy itself, so it is immutable by construction:
//!
//! ```text
//! stream_id = sha256( cbor([ "rag-rat/stream/1",
//!                            repo_set,                  -- byte-sorted, deduped, non-empty
//!                            kind_allow_list | null,    -- null = UNFILTERED (see below)
//!                            relation_policy | null,    -- null = ALL RELATIONS
//!                            per_node_override_set ]))  -- [node_id, action] pairs, node_id-sorted
//! ```
//!
//! Changing the policy (repo set, allow-list, overrides) cannot mutate a stream — it derives a NEW
//! `stream_id` (a later increment announces the transition with a signed snapshot entry), so
//! watermark/fork continuity is never claimed across different visibility rules.
//!
//! **`null` is NOT an empty list.** The unfiltered (owner) view is encoded as CBOR `null`, a
//! distinguished marker — deliberately not "every kind known today" enumerated: adding a kind or
//! relation must stay *data + a projector* (the unknown-op retention seam), and an enumerated full
//! list would re-mint the owner stream on every vocabulary addition. An EMPTY list is the
//! degenerate nothing-visible filter and hashes differently.
//!
//! Tokens (repo ids, kind/relation tokens, node ids) are machine identifiers and are encoded
//! VERBATIM — no NFC/trim (the same "structural canonicity only" rule as [`super::op`]).

use minicbor::Encoder;
use sha2::{Digest, Sha256};

/// Domain tag + version for the stream-identity derivation. Bump only when the canonical rule
/// itself changes — never when a kind/relation token is added.
const STREAM_DOMAIN: &str = "rag-rat/stream/1";

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible) — mirrors `super::op`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// A stream's immutable identity: `sha256` of the canonical policy tuple. The scoping key for
/// chains, watermarks, fork detection, and the shadow projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct StreamId([u8; 32]);

impl StreamId {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// By value — `StreamId` is `Copy` (clippy `wrong_self_convention` flags `to_*` on `&self`).
    pub(crate) fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Whether a per-node override pulls a node INTO the view or drops it OUT — the per-node
/// refinement on top of the per-kind allow-list defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NodeOverrideAction {
    Include,
    Exclude,
}

impl NodeOverrideAction {
    /// The frozen wire token — a rename is a format change.
    fn as_wire_str(self) -> &'static str {
        match self {
            Self::Include => "include",
            Self::Exclude => "exclude",
        }
    }
}

/// One per-node visibility override: `node_id` + the action applied to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeOverride {
    pub(crate) node_id: String,
    pub(crate) action: NodeOverrideAction,
}

/// The visibility policy a stream identity is derived from. Collections may arrive in any order
/// with duplicates — [`derive`] canonicalizes (byte-sort + dedup) before encoding, so caller order
/// never perturbs the identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamSpec {
    /// The repos this stream carries. Non-empty.
    pub(crate) repo_set: Vec<String>,
    /// `None` = the UNFILTERED view (every kind, present and future); `Some` = the enumerated
    /// per-kind allow-list (`Some(vec![])` is the valid nothing-visible degenerate).
    pub(crate) kind_allow_list: Option<Vec<String>>,
    /// `None` = all relations; `Some` = the enumerated relation allow-list.
    pub(crate) relation_policy: Option<Vec<String>>,
    /// Per-node overrides on top of the kind defaults. One action per `node_id`.
    pub(crate) node_overrides: Vec<NodeOverride>,
}

/// The default full-visibility stream a repo's own authored log lives on: one repo, no kind or
/// relation filtering, no overrides. The only stream shape mintable this increment.
pub(crate) fn owner_stream(repo_id: &str) -> anyhow::Result<StreamId> {
    derive(&StreamSpec {
        repo_set: vec![repo_id.to_string()],
        kind_allow_list: None,
        relation_policy: None,
        node_overrides: Vec::new(),
    })
}

/// Derive the immutable `stream_id` for a visibility policy. Canonicalizes the spec (sort + dedup)
/// first; rejects a policy that has no coherent identity — an empty/blank `repo_set`, or two
/// overrides that give one `node_id` conflicting actions.
pub(crate) fn derive(spec: &StreamSpec) -> anyhow::Result<StreamId> {
    let bytes = canonical_spec_bytes(spec)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(&bytes));
    Ok(StreamId(out))
}

/// The canonical CBOR tuple the identity hashes — `[domain, repo_set, kind_allow_list | null,
/// relation_policy | null, override_pairs]`, definite lengths + minimal headers throughout.
fn canonical_spec_bytes(spec: &StreamSpec) -> anyhow::Result<Vec<u8>> {
    let repo_set = sorted_deduped(&spec.repo_set);
    if repo_set.is_empty() {
        anyhow::bail!("a stream must carry at least one repo");
    }
    if repo_set.iter().any(|repo| repo.is_empty()) {
        anyhow::bail!("a stream repo id must be non-empty");
    }
    let kind_allow_list = spec.kind_allow_list.as_deref().map(sorted_deduped);
    let relation_policy = spec.relation_policy.as_deref().map(sorted_deduped);
    let overrides = canonical_overrides(&spec.node_overrides)?;

    let mut buf = Vec::with_capacity(96);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(5).expect(INFALLIBLE);
        enc.str(STREAM_DOMAIN).expect(INFALLIBLE);
        encode_str_array(&mut enc, &repo_set);
        encode_optional_str_array(&mut enc, kind_allow_list.as_deref());
        encode_optional_str_array(&mut enc, relation_policy.as_deref());
        enc.array(overrides.len() as u64).expect(INFALLIBLE);
        for entry in &overrides {
            enc.array(2).expect(INFALLIBLE);
            enc.str(&entry.node_id).expect(INFALLIBLE);
            enc.str(entry.action.as_wire_str()).expect(INFALLIBLE);
        }
    }
    Ok(buf)
}

/// Byte-sort + dedup a token list into canonical SET order (the same rule as `NodeContent` tags).
fn sorted_deduped(values: &[String]) -> Vec<String> {
    let mut out = values.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

/// Canonicalize the override set: sorted by `(node_id, action)`, exact duplicates collapsed, and a
/// `node_id` carrying BOTH actions rejected — a node cannot be simultaneously included and
/// excluded, and silently picking one would make two authors derive different "same" streams.
fn canonical_overrides(overrides: &[NodeOverride]) -> anyhow::Result<Vec<NodeOverride>> {
    let mut out = overrides.to_vec();
    out.sort_unstable_by(|a, b| (&a.node_id, a.action).cmp(&(&b.node_id, b.action)));
    out.dedup();
    for pair in out.windows(2) {
        if pair[0].node_id == pair[1].node_id {
            anyhow::bail!("node `{}` has conflicting stream overrides", pair[0].node_id);
        }
    }
    Ok(out)
}

fn encode_str_array(enc: &mut Encoder<&mut Vec<u8>>, values: &[String]) {
    enc.array(values.len() as u64).expect(INFALLIBLE);
    for value in values {
        enc.str(value).expect(INFALLIBLE);
    }
}

/// `None` → CBOR `null` (the distinguished UNFILTERED marker); `Some` → the enumerated list. The
/// two must stay distinguishable on the wire — see the module docs.
fn encode_optional_str_array(enc: &mut Encoder<&mut Vec<u8>>, values: Option<&[String]>) {
    match values {
        Some(values) => encode_str_array(enc, values),
        None => {
            enc.null().expect(INFALLIBLE);
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::cbor;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn filtered_spec() -> StreamSpec {
        StreamSpec {
            repo_set: vec!["repo-b".to_string(), "repo-a".to_string()],
            kind_allow_list: Some(vec!["Invariant".to_string(), "Decision".to_string()]),
            relation_policy: Some(vec!["tracks".to_string()]),
            node_overrides: vec![NodeOverride {
                node_id: "mem_1".to_string(),
                action: NodeOverrideAction::Exclude,
            }],
        }
    }

    #[test]
    fn golden_vectors_pin_the_stream_identity() {
        // The derivation is a frozen primitive: signed entry bodies embed these 32 bytes, so a
        // canonical-rule change must break this test and force a deliberate `rag-rat/stream/1`
        // domain bump.
        let owner = owner_stream("repo-a").unwrap();
        assert_eq!(
            hex(&owner.to_bytes()),
            "4d011fdcf0fad7d9483f70a9bb797f75262758656fe8f3ce88ff8d17ed480c4f",
            "owner stream golden",
        );
        let filtered = derive(&filtered_spec()).unwrap();
        assert_eq!(
            hex(&filtered.to_bytes()),
            "4ae73564c5647cd84e7cc8ca8f8ff680e2c06cb85bbfcfc981c0f8a276d3db06",
            "filtered stream golden",
        );
    }

    #[test]
    fn canonical_spec_bytes_have_the_expected_structure() {
        // Structure-level proof of the frozen shape the golden vector pins.
        let bytes = canonical_spec_bytes(&StreamSpec {
            repo_set: vec!["repo-a".to_string()],
            kind_allow_list: None,
            relation_policy: None,
            node_overrides: Vec::new(),
        })
        .unwrap();
        assert_eq!(bytes[0], 0x85, "5-element array header");
        assert_eq!(bytes[1], 0x70, "text header, len 16");
        assert_eq!(&bytes[2..18], b"rag-rat/stream/1");
        assert_eq!(bytes[18], 0x81, "repo_set: 1-element array");
        assert_eq!(bytes[19], 0x66, "text header, len 6");
        assert_eq!(&bytes[20..26], b"repo-a");
        assert_eq!(bytes[26], 0xf6, "unfiltered kind_allow_list is CBOR null");
        assert_eq!(bytes[27], 0xf6, "all-relations policy is CBOR null");
        assert_eq!(bytes[28], 0x80, "empty override set");
        assert_eq!(bytes.len(), 29);
        // And the tuple obeys the module-wide canonical-CBOR floor.
        cbor::require_canonical_cbor(&bytes).expect("spec tuple is canonical CBOR");
    }

    #[test]
    fn caller_order_and_duplicates_never_perturb_the_identity() {
        let baseline = derive(&filtered_spec()).unwrap();
        let shuffled = StreamSpec {
            repo_set: vec!["repo-a".to_string(), "repo-b".to_string(), "repo-a".to_string()],
            kind_allow_list: Some(vec![
                "Decision".to_string(),
                "Invariant".to_string(),
                "Decision".to_string(),
            ]),
            relation_policy: Some(vec!["tracks".to_string(), "tracks".to_string()]),
            node_overrides: vec![
                NodeOverride { node_id: "mem_1".to_string(), action: NodeOverrideAction::Exclude },
                NodeOverride { node_id: "mem_1".to_string(), action: NodeOverrideAction::Exclude },
            ],
        };
        assert_eq!(derive(&shuffled).unwrap(), baseline);
    }

    #[test]
    fn unfiltered_null_is_distinct_from_an_empty_allow_list() {
        // `None` (every kind, forever) and `Some(vec![])` (nothing visible) are different
        // policies and MUST derive different identities.
        let unfiltered = owner_stream("repo-a").unwrap();
        let nothing_visible = derive(&StreamSpec {
            repo_set: vec!["repo-a".to_string()],
            kind_allow_list: Some(Vec::new()),
            relation_policy: None,
            node_overrides: Vec::new(),
        })
        .unwrap();
        assert_ne!(unfiltered, nothing_visible);
    }

    #[test]
    fn every_policy_dimension_feeds_the_identity() {
        let base = filtered_spec();
        let baseline = derive(&base).unwrap();
        let repo_changed = StreamSpec { repo_set: vec!["repo-c".to_string()], ..base.clone() };
        let kinds_changed =
            StreamSpec { kind_allow_list: Some(vec!["Risk".to_string()]), ..base.clone() };
        let relations_changed = StreamSpec { relation_policy: None, ..base.clone() };
        let overrides_changed = StreamSpec {
            node_overrides: vec![NodeOverride {
                node_id: "mem_1".to_string(),
                action: NodeOverrideAction::Include,
            }],
            ..base
        };
        for changed in [repo_changed, kinds_changed, relations_changed, overrides_changed] {
            assert_ne!(derive(&changed).unwrap(), baseline, "changed policy, same id: {changed:?}");
        }
    }

    #[test]
    fn an_empty_or_blank_repo_set_is_rejected() {
        assert!(
            derive(&StreamSpec {
                repo_set: Vec::new(),
                kind_allow_list: None,
                relation_policy: None,
                node_overrides: Vec::new(),
            })
            .is_err(),
            "empty repo_set",
        );
        assert!(owner_stream("").is_err(), "blank repo id");
    }

    #[test]
    fn conflicting_node_overrides_are_rejected() {
        let spec = StreamSpec {
            repo_set: vec!["repo-a".to_string()],
            kind_allow_list: None,
            relation_policy: None,
            node_overrides: vec![
                NodeOverride { node_id: "mem_1".to_string(), action: NodeOverrideAction::Include },
                NodeOverride { node_id: "mem_1".to_string(), action: NodeOverrideAction::Exclude },
            ],
        };
        assert!(derive(&spec).is_err(), "one node cannot be both included and excluded");
    }
}
