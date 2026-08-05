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
//!
//! **Two derivations coexist (sync phase C).** [`owner_stream`] / [`derive`] produce the original
//! `rag-rat/stream/1` id — LOCAL-only, what the live phase-B authoring path mints. [`derive_v2`]
//! produces the owner-bound `rag-rat/stream/2` id (§14), which commits the owning `AccountId`
//! inside the hash so a synced `/3` content entry transitively names its owner. C1 adds `/2`
//! ALONGSIDE `/1` (byte-unchanged); nothing switches the live path until C3 adoption.

use minicbor::data::Type;
use minicbor::{Decoder, Encoder};
use sha2::{Digest, Sha256};

use super::account::AccountId;
use super::cbor;

/// Domain tag + version for the stream-identity derivation. Bump only when the canonical rule
/// itself changes — never when a kind/relation token is added.
const STREAM_DOMAIN: &str = "rag-rat/stream/1";

/// Domain tag for the OWNER-BOUND stream identity (sync phase C, §14). `/2` commits the owning
/// `AccountId` inside the hashed identity, so a `/3` content entry that names the stream
/// transitively names its owner — two accounts claiming one stream is cryptographically impossible.
/// `/1` stays local-only (never on the wire); `/2` is what sync mints.
const STREAM_V2_DOMAIN: &str = "rag-rat/stream/2";

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible) — mirrors `super::op`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// A stream's immutable identity: `sha256` of the canonical policy tuple. The scoping key for
/// chains, watermarks, fork detection, and the shadow projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId([u8; 32]);

impl StreamId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// By value — `StreamId` is `Copy` (clippy `wrong_self_convention` flags `to_*` on `&self`).
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Whether a per-node override pulls a node INTO the view or drops it OUT — the per-node
/// refinement on top of the per-kind allow-list defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NodeOverrideAction {
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
pub struct NodeOverride {
    pub node_id: String,
    pub action: NodeOverrideAction,
}

/// The visibility policy a stream identity is derived from. Collections may arrive in any order
/// with duplicates — [`derive`] canonicalizes (byte-sort + dedup) before encoding, so caller order
/// never perturbs the identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSpec {
    /// The repos this stream carries. Non-empty.
    pub repo_set: Vec<String>,
    /// `None` = the UNFILTERED view (every kind, present and future); `Some` = the enumerated
    /// per-kind allow-list (`Some(vec![])` is the valid nothing-visible degenerate).
    pub kind_allow_list: Option<Vec<String>>,
    /// `None` = all relations; `Some` = the enumerated relation allow-list.
    pub relation_policy: Option<Vec<String>>,
    /// Per-node overrides on top of the kind defaults. One action per `node_id`.
    pub node_overrides: Vec<NodeOverride>,
}

/// The full-visibility policy a repo's own authored log lives on: one repo, no kind or relation
/// filtering, no overrides. The `/1` [`owner_stream`] and the `/2` [`owner_stream_v2`] derivations
/// share this ONE literal so the two owner-stream identities can never drift in their visibility
/// rule — they differ only in the domain tag (and the owner prefix `/2` adds).
fn owner_stream_spec(repo_id: &str) -> StreamSpec {
    StreamSpec {
        repo_set: vec![repo_id.to_string()],
        kind_allow_list: None,
        relation_policy: None,
        node_overrides: Vec::new(),
    }
}

/// The default full-visibility stream a repo's own authored log lives on: one repo, no kind or
/// relation filtering, no overrides. The only stream shape mintable this increment.
pub fn owner_stream(repo_id: &str) -> anyhow::Result<StreamId> {
    derive(&owner_stream_spec(repo_id))
}

/// Derive the immutable `stream_id` for a visibility policy. Canonicalizes the spec (sort + dedup)
/// first; rejects a policy that has no coherent identity — an empty/blank `repo_set`, or two
/// overrides that give one `node_id` conflicting actions.
pub fn derive(spec: &StreamSpec) -> anyhow::Result<StreamId> {
    let bytes = canonical_spec_bytes(spec)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(&bytes));
    Ok(StreamId(out))
}

/// The canonical CBOR tuple the `/1` identity hashes — `[domain, repo_set, kind_allow_list | null,
/// relation_policy | null, override_pairs]`, definite lengths + minimal headers throughout.
fn canonical_spec_bytes(spec: &StreamSpec) -> anyhow::Result<Vec<u8>> {
    let policy = canonical_policy(spec)?;
    let mut buf = Vec::with_capacity(96);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(5).expect(INFALLIBLE);
        enc.str(STREAM_DOMAIN).expect(INFALLIBLE);
        encode_policy(&mut enc, &policy);
    }
    Ok(buf)
}

/// The validated + canonicalized policy fields shared by the `/1` and `/2` derivations. Extracting
/// them keeps the two identities byte-identical in everything but the domain tag + owner prefix.
struct CanonicalPolicy {
    repo_set: Vec<String>,
    kind_allow_list: Option<Vec<String>>,
    relation_policy: Option<Vec<String>>,
    overrides: Vec<NodeOverride>,
}

/// Validate + canonicalize a stream's policy (sort + dedup; reject an empty/blank `repo_set` or a
/// node with conflicting overrides) — the identity-defining rules, independent of the domain
/// version.
fn canonical_policy(spec: &StreamSpec) -> anyhow::Result<CanonicalPolicy> {
    let repo_set = sorted_deduped(&spec.repo_set);
    if repo_set.is_empty() {
        anyhow::bail!("a stream must carry at least one repo");
    }
    if repo_set.iter().any(|repo| repo.is_empty()) {
        anyhow::bail!("a stream repo id must be non-empty");
    }
    Ok(CanonicalPolicy {
        repo_set,
        kind_allow_list: spec.kind_allow_list.as_deref().map(sorted_deduped),
        relation_policy: spec.relation_policy.as_deref().map(sorted_deduped),
        overrides: canonical_overrides(&spec.node_overrides)?,
    })
}

/// Append the four policy fields (`repo_set, kind_allow_list|null, relation_policy|null,
/// override_pairs`) to `enc` in the frozen order. The BYTES are identical whether they follow the
/// `/1` header or the `/2` header+owner — the shared tail of both identities.
fn encode_policy(enc: &mut Encoder<&mut Vec<u8>>, policy: &CanonicalPolicy) {
    encode_str_array(enc, &policy.repo_set);
    encode_optional_str_array(enc, policy.kind_allow_list.as_deref());
    encode_optional_str_array(enc, policy.relation_policy.as_deref());
    enc.array(policy.overrides.len() as u64).expect(INFALLIBLE);
    for entry in &policy.overrides {
        enc.array(2).expect(INFALLIBLE);
        enc.str(&entry.node_id).expect(INFALLIBLE);
        enc.str(entry.action.as_wire_str()).expect(INFALLIBLE);
    }
}

/// Who may PULL a stream (#407). The mode is committed INSIDE the hashed `/2` identity (see
/// [`canonical_spec_v2_bytes`]), so it is fixed at stream creation and cannot be flipped: a
/// different mode is a different `stream_id`, which is exactly the "mode conversion is a deliberate
/// re-publication, never a flag flip" rule. `Private` is the default and the pre-#407 behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessMode {
    /// Only roster/grant-authorized peers of the owning account may pull. Every pre-#407 stream.
    #[default]
    Private,
    /// Anyone may pull the stream's signed-plaintext (`crypto_suite = 0`) entries; writes stay
    /// grant-gated. The public-knowledge-base mode.
    PublicRead,
}

impl AccessMode {
    /// The canonical wire tag for the mode, appended as the 7th `/2` spec element for a non-default
    /// mode. `Private` has no tag — it is encoded by OMISSION (the 6-element form), so every
    /// private stream keeps its exact pre-#407 bytes and id.
    fn wire_tag(self) -> Option<u64> {
        match self {
            AccessMode::Private => None,
            AccessMode::PublicRead => Some(1),
        }
    }

    /// The mode named by a 7th-element wire tag, or an error for an unknown / non-canonical tag
    /// (tag 0 would be `Private`, which must be encoded by omission, so it is rejected here).
    fn from_wire_tag(tag: u64) -> anyhow::Result<Self> {
        match tag {
            1 => Ok(AccessMode::PublicRead),
            other => anyhow::bail!("unknown or non-canonical stream/2 access-mode tag {other}"),
        }
    }
}

/// The owner-bound stream policy (`/2`, §14): the `/1` visibility policy plus the owning account.
/// The owner is committed INSIDE the hashed identity, so ownership is self-certifying and
/// immutable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSpecV2 {
    /// The account that owns this stream — all its authorization lives in this account's logs.
    pub owner_account_id: AccountId,
    /// The visibility policy (repos, kind/relation filters, per-node overrides), same shape as
    /// `/1`.
    pub policy: StreamSpec,
    /// Who may pull this stream (#407). Committed into the identity; `Private` for every pre-#407
    /// stream and encoded by omission so their ids never move.
    pub access_mode: AccessMode,
}

/// The owner-bound (`/2`) counterpart of [`owner_stream`]: the same full-visibility policy, wrapped
/// with the owning `account_id` so the identity self-certifies its owner (§14). Reusing
/// [`owner_stream_spec`] with the `/1` path keeps the two owner-stream identities byte-identical in
/// their visibility rule. C3.4 authors a `StreamOwn` over the returned spec to publish ownership;
/// nothing switches the live authoring path here.
pub fn owner_stream_v2(repo_id: &str, account_id: AccountId) -> StreamSpecV2 {
    StreamSpecV2 {
        owner_account_id: account_id,
        policy: owner_stream_spec(repo_id),
        access_mode: AccessMode::Private,
    }
}

/// Derive the owner-bound `stream_id` (`/2`, §14): `sha256(cbor(["rag-rat/stream/2",
/// owner_account_id (b32), repo_set, kind_allow_list|null, relation_policy|null, override_set]))`.
/// C1 ships this ALONGSIDE [`owner_stream`] / [`derive`] (`/1`), which the live phase-B authoring
/// path keeps using until C3 adoption — nothing switches the live path here.
pub fn derive_v2(spec: &StreamSpecV2) -> anyhow::Result<StreamId> {
    let bytes = canonical_spec_v2_bytes(spec)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(&bytes));
    Ok(StreamId(out))
}

/// The canonical CBOR tuple the `/2` identity hashes — `[domain, owner_account_id (b32), repo_set,
/// kind_allow_list | null, relation_policy | null, override_pairs]`. The owner sits between the
/// domain and the shared policy tail.
pub fn canonical_spec_v2_bytes(spec: &StreamSpecV2) -> anyhow::Result<Vec<u8>> {
    let policy = canonical_policy(&spec.policy)?;
    let mut buf = Vec::with_capacity(128);
    {
        let mut enc = Encoder::new(&mut buf);
        // `Private` is encoded by OMISSION (6 elements, byte-identical to pre-#407) so private
        // stream ids never move; a non-default mode appends a 7th tag element.
        let mode_tag = spec.access_mode.wire_tag();
        enc.array(if mode_tag.is_some() { 7 } else { 6 }).expect(INFALLIBLE);
        enc.str(STREAM_V2_DOMAIN).expect(INFALLIBLE);
        enc.bytes(&spec.owner_account_id.to_bytes()).expect(INFALLIBLE);
        encode_policy(&mut enc, &policy);
        if let Some(tag) = mode_tag {
            enc.u64(tag).expect(INFALLIBLE);
        }
    }
    Ok(buf)
}

/// Decode and validate a canonical owner-bound `/2` stream specification. `StreamOwn` carries
/// these bytes as a self-certifying preimage, so accepting merely a matching SHA-256 digest is not
/// enough: the tuple must have the frozen shape, contain a valid policy, and already be in its
/// unique canonical encoding.
pub fn decode_spec_v2(bytes: &[u8]) -> anyhow::Result<StreamSpecV2> {
    cbor::require_canonical_cbor(bytes)?;
    let mut dec = Decoder::new(bytes);
    // 6 elements = the pre-#407 form (`Private` by omission); 7 = a non-default access mode
    // appended.
    let arity = dec.array()?;
    anyhow::ensure!(
        matches!(arity, Some(6) | Some(7)),
        "stream/2 spec must be a 6- or 7-element array"
    );
    anyhow::ensure!(dec.str()? == STREAM_V2_DOMAIN, "unknown stream spec domain");
    let owner_account_id = AccountId::from_bytes(decode_fixed::<32>(&mut dec, "owner account")?);
    let policy = StreamSpec {
        repo_set: decode_str_array(&mut dec)?,
        kind_allow_list: decode_optional_str_array(&mut dec)?,
        relation_policy: decode_optional_str_array(&mut dec)?,
        node_overrides: decode_overrides(&mut dec)?,
    };
    let access_mode = if arity == Some(7) {
        // `from_wire_tag` rejects tag 0 (`Private`), so an explicit `Private` in 7-element form is
        // refused here; the re-encode check below independently enforces the same canonical rule.
        AccessMode::from_wire_tag(dec.u64()?)?
    } else {
        AccessMode::Private
    };
    anyhow::ensure!(dec.position() == bytes.len(), "trailing bytes in stream/2 spec");
    let spec = StreamSpecV2 { owner_account_id, policy, access_mode };
    anyhow::ensure!(
        canonical_spec_v2_bytes(&spec)? == bytes,
        "stream/2 spec is not in canonical set order"
    );
    Ok(spec)
}

fn decode_fixed<const N: usize>(
    dec: &mut Decoder<'_>,
    field: &'static str,
) -> anyhow::Result<[u8; N]> {
    let value = dec.bytes()?;
    value.try_into().map_err(|_| anyhow::anyhow!("{field} must be {N} bytes"))
}

fn decode_str_array(dec: &mut Decoder<'_>) -> anyhow::Result<Vec<String>> {
    let len = dec.array()?.ok_or_else(|| anyhow::anyhow!("indefinite arrays are not canonical"))?;
    let mut values = Vec::with_capacity(len as usize);
    for _ in 0..len {
        values.push(dec.str()?.to_string());
    }
    Ok(values)
}

fn decode_optional_str_array(dec: &mut Decoder<'_>) -> anyhow::Result<Option<Vec<String>>> {
    if dec.datatype()? == Type::Null {
        dec.null()?;
        Ok(None)
    } else {
        Ok(Some(decode_str_array(dec)?))
    }
}

fn decode_overrides(dec: &mut Decoder<'_>) -> anyhow::Result<Vec<NodeOverride>> {
    let len = dec.array()?.ok_or_else(|| anyhow::anyhow!("indefinite arrays are not canonical"))?;
    let mut overrides = Vec::with_capacity(len as usize);
    for _ in 0..len {
        anyhow::ensure!(dec.array()? == Some(2), "stream override must be a 2-element array");
        let node_id = dec.str()?.to_string();
        let action = match dec.str()? {
            "include" => NodeOverrideAction::Include,
            "exclude" => NodeOverrideAction::Exclude,
            other => anyhow::bail!("unknown stream override action `{other}`"),
        };
        overrides.push(NodeOverride { node_id, action });
    }
    Ok(overrides)
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
    use crate::cbor;

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

    fn owner() -> AccountId {
        AccountId::from_bytes([0x11; 32])
    }

    fn spec_v2() -> StreamSpecV2 {
        StreamSpecV2 {
            owner_account_id: owner(),
            policy: filtered_spec(),
            access_mode: AccessMode::Private,
        }
    }

    #[test]
    fn stream_v2_pins_the_owner_bound_identity() {
        // The `/2` derivation is a frozen primitive (StreamOwn validation + `/3` bodies build on
        // it); a canonical-rule change must break this and force a deliberate
        // `rag-rat/stream/2` bump.
        let id = derive_v2(&spec_v2()).unwrap();
        assert_eq!(
            hex(&id.to_bytes()),
            "d998110a767b415646b18c09745d6586b2ae6afdeb3144fe5f0907d2761557e3",
            "stream/2 golden",
        );
    }

    #[test]
    fn owner_stream_v2_golden_pins_the_default_owned_stream() {
        // The `/2` owner stream `ensure_owned_stream_v2_in_tx` authors a `StreamOwn` over is a
        // frozen primitive (its id lands in signed `/3` bodies): a canonical-rule drift must break
        // this and force a deliberate `rag-rat/stream/2` bump. Also proves the constructor reuses
        // the full-visibility policy — same bytes as the `/1` owner stream plus the owner prefix.
        let spec = owner_stream_v2("repo-a", AccountId::from_bytes([0x11; 32]));
        assert_eq!(
            spec.policy,
            owner_stream_spec("repo-a"),
            "owner_stream_v2 wraps the shared /1 owner policy verbatim",
        );
        let id = derive_v2(&spec).unwrap();
        assert_eq!(
            hex(&id.to_bytes()),
            "d1fee7eec725d5004c7f3827788cada29545315484ccf073663536a3172f5223",
            "owner_stream_v2 golden",
        );
    }

    #[test]
    fn stream_v2_canonical_bytes_have_the_expected_structure() {
        // The owner b32 sits between the domain and the shared policy tail.
        let bytes = canonical_spec_v2_bytes(&StreamSpecV2 {
            owner_account_id: AccountId::from_bytes([0x11; 32]),
            policy: StreamSpec {
                repo_set: vec!["repo-a".to_string()],
                kind_allow_list: None,
                relation_policy: None,
                node_overrides: Vec::new(),
            },
            access_mode: AccessMode::Private,
        })
        .unwrap();
        assert_eq!(bytes[0], 0x86, "6-element array header");
        assert_eq!(bytes[1], 0x70, "text header, len 16");
        assert_eq!(&bytes[2..18], b"rag-rat/stream/2");
        assert_eq!(&bytes[18..20], &[0x58, 0x20], "owner_account_id bstr header, len 32");
        assert_eq!(&bytes[20..52], &[0x11; 32], "owner_account_id bytes");
        assert_eq!(bytes[52], 0x81, "repo_set: 1-element array");
        assert_eq!(bytes[53], 0x66, "text header, len 6");
        assert_eq!(&bytes[54..60], b"repo-a");
        assert_eq!(bytes[60], 0xf6, "unfiltered kind_allow_list is CBOR null");
        assert_eq!(bytes[61], 0xf6, "all-relations policy is CBOR null");
        assert_eq!(bytes[62], 0x80, "empty override set");
        assert_eq!(bytes.len(), 63);
        cbor::require_canonical_cbor(&bytes).expect("stream/2 tuple is canonical CBOR");
        assert_eq!(decode_spec_v2(&bytes).unwrap(), StreamSpecV2 {
            owner_account_id: AccountId::from_bytes([0x11; 32]),
            policy: StreamSpec {
                repo_set: vec!["repo-a".to_string()],
                kind_allow_list: None,
                relation_policy: None,
                node_overrides: Vec::new(),
            },
            access_mode: AccessMode::Private,
        });
    }

    #[test]
    fn stream_v2_decoder_round_trips_every_policy_dimension() {
        let expected = spec_v2();
        let bytes = canonical_spec_v2_bytes(&expected).unwrap();
        let decoded = decode_spec_v2(&bytes).unwrap();
        assert_eq!(decoded.owner_account_id, expected.owner_account_id);
        assert_eq!(decoded.policy.repo_set, ["repo-a", "repo-b"]);
        assert_eq!(
            decoded.policy.kind_allow_list.as_deref(),
            Some(["Decision".to_string(), "Invariant".to_string()].as_slice()),
        );
        assert_eq!(
            decoded.policy.relation_policy.as_deref(),
            Some(["tracks".to_string()].as_slice())
        );
        assert_eq!(decoded.policy.node_overrides, expected.policy.node_overrides);
        assert_eq!(canonical_spec_v2_bytes(&decoded).unwrap(), bytes);
    }

    fn raw_v2(domain: &str, owner: &[u8], repos: &[&str], overrides: &[(&str, &str)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut enc = Encoder::new(&mut bytes);
        enc.array(6).unwrap();
        enc.str(domain).unwrap();
        enc.bytes(owner).unwrap();
        enc.array(repos.len() as u64).unwrap();
        for repo in repos {
            enc.str(repo).unwrap();
        }
        enc.null().unwrap();
        enc.null().unwrap();
        enc.array(overrides.len() as u64).unwrap();
        for (node, action) in overrides {
            enc.array(2).unwrap();
            enc.str(node).unwrap();
            enc.str(action).unwrap();
        }
        bytes
    }

    #[test]
    fn stream_v2_decoder_rejects_malformed_self_certifying_preimages() {
        let owner = owner().to_bytes();
        let mut trailing = raw_v2(STREAM_V2_DOMAIN, &owner, &["repo-a"], &[]);
        trailing.push(0x00);
        let cases = [
            ("wrong domain", raw_v2(STREAM_DOMAIN, &owner, &["repo-a"], &[])),
            ("short owner", raw_v2(STREAM_V2_DOMAIN, &[0x11; 31], &["repo-a"], &[])),
            (
                "unknown override action",
                raw_v2(STREAM_V2_DOMAIN, &owner, &["repo-a"], &[("mem-1", "unknown")]),
            ),
            (
                "conflicting override",
                raw_v2(STREAM_V2_DOMAIN, &owner, &["repo-a"], &[
                    ("mem-1", "exclude"),
                    ("mem-1", "include"),
                ]),
            ),
            ("trailing bytes", trailing),
        ];
        for (label, bytes) in cases {
            assert!(decode_spec_v2(&bytes).is_err(), "accepted {label}");
        }
    }

    #[test]
    fn stream_v2_decoder_rejects_structural_cbor_that_is_not_in_canonical_set_order() {
        let mut bytes = canonical_spec_v2_bytes(&StreamSpecV2 {
            owner_account_id: owner(),
            policy: StreamSpec {
                repo_set: vec!["repo-a".to_string(), "repo-b".to_string()],
                kind_allow_list: None,
                relation_policy: None,
                node_overrides: Vec::new(),
            },
            access_mode: AccessMode::Private,
        })
        .unwrap();
        let a = bytes.windows(6).position(|window| window == b"repo-a").unwrap();
        let b = bytes.windows(6).position(|window| window == b"repo-b").unwrap();
        let repo_a = bytes[a..a + 6].to_vec();
        let repo_b = bytes[b..b + 6].to_vec();
        bytes[a..a + 6].copy_from_slice(&repo_b);
        bytes[b..b + 6].copy_from_slice(&repo_a);

        cbor::require_canonical_cbor(&bytes)
            .expect("the CBOR itself remains structurally canonical");
        assert!(
            decode_spec_v2(&bytes).is_err(),
            "semantic set ordering is part of the stream identity's canonical preimage",
        );
    }

    #[test]
    fn two_owners_cannot_derive_the_same_stream_id() {
        // Same policy, different owner ⇒ different stream — ownership is committed inside the id,
        // so two accounts claiming one stream is impossible.
        let a = derive_v2(&StreamSpecV2 {
            owner_account_id: AccountId::from_bytes([0x11; 32]),
            policy: filtered_spec(),
            access_mode: AccessMode::Private,
        })
        .unwrap();
        let b = derive_v2(&StreamSpecV2 {
            owner_account_id: AccountId::from_bytes([0x22; 32]),
            policy: filtered_spec(),
            access_mode: AccessMode::Private,
        })
        .unwrap();
        assert_ne!(a, b, "a different owner must derive a different stream_id");
    }

    #[test]
    fn public_read_round_trips_and_derives_a_distinct_identity() {
        // A public_read stream encodes a 7th element, round-trips, and — because the mode is folded
        // into the id — derives a DIFFERENT stream_id than the same policy/owner as Private. That
        // is the "mode conversion is a new stream, never a flag flip" guarantee, by
        // construction.
        let private = spec_v2();
        let public = StreamSpecV2 { access_mode: AccessMode::PublicRead, ..spec_v2() };
        let bytes = canonical_spec_v2_bytes(&public).unwrap();
        assert_eq!(bytes[0], 0x87, "public_read is a 7-element array");
        // `filtered_spec()` is deliberately unsorted, so the decode canonicalizes the policy order;
        // round-trip at the canonical-bytes level (like the sibling round-trip test) and confirm
        // the mode survived.
        let decoded = decode_spec_v2(&bytes).unwrap();
        assert_eq!(decoded.access_mode, AccessMode::PublicRead, "the mode round-trips");
        assert_eq!(
            canonical_spec_v2_bytes(&decoded).unwrap(),
            bytes,
            "public_read round-trips canonically",
        );
        assert_ne!(
            derive_v2(&private).unwrap(),
            derive_v2(&public).unwrap(),
            "flipping the access mode is a different stream_id, not a mutation",
        );
    }

    #[test]
    fn private_is_encoded_by_omission_so_its_bytes_and_id_never_move() {
        // Private MUST stay byte-identical to the pre-#407 6-element form (the goldens above pin
        // the ids). An explicit Private can only be the 6-element form.
        let private = spec_v2();
        let bytes = canonical_spec_v2_bytes(&private).unwrap();
        assert_eq!(bytes[0], 0x86, "Private stays a 6-element array (mode omitted)");
        assert_eq!(decode_spec_v2(&bytes).unwrap().access_mode, AccessMode::Private);
    }

    #[test]
    fn a_seven_element_spec_carrying_private_is_rejected_as_non_canonical() {
        // Tag 0 (Private) in 7-element form is refused: Private must be encoded by omission, so
        // `from_wire_tag(0)` bails before the decode completes (the re-encode canonicity check
        // would reject it too, but the tag check fires first).
        let public = StreamSpecV2 { access_mode: AccessMode::PublicRead, ..spec_v2() };
        let mut bytes = canonical_spec_v2_bytes(&public).unwrap();
        // Rewrite the trailing mode tag from 1 (PublicRead) to 0 (Private) in place.
        let last = *bytes.last().unwrap();
        assert_eq!(last, 0x01, "the trailing PublicRead tag is a CBOR unsigned 1");
        *bytes.last_mut().unwrap() = 0x00;
        assert!(decode_spec_v2(&bytes).is_err(), "explicit Private in 7-element form is rejected");
    }

    #[test]
    fn an_unknown_access_mode_tag_is_rejected() {
        let public = StreamSpecV2 { access_mode: AccessMode::PublicRead, ..spec_v2() };
        let mut bytes = canonical_spec_v2_bytes(&public).unwrap();
        *bytes.last_mut().unwrap() = 0x09; // an unknown mode tag
        assert!(decode_spec_v2(&bytes).is_err(), "an unknown access-mode tag fails closed");
    }

    #[test]
    fn stream_v2_is_distinct_from_the_legacy_v1_identity() {
        // The same visibility policy under `/2` (owner-bound) must not collide with its `/1` id.
        assert_ne!(derive(&filtered_spec()).unwrap(), derive_v2(&spec_v2()).unwrap());
    }
}
