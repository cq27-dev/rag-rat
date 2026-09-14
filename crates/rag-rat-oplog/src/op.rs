//! The memory op model + its canonical CBOR wire form (phase B op-log, §5.4/§6.3).
//!
//! An [`Entry`] is one op (`op`) plus its ordering metadata (`meta`): a per-stream Lamport counter
//! and a device fingerprint. The Lamport/device pair is DEFINED here as the op-log's total order —
//! neither exists in the schema yet (it arrives with the signed envelope in a later increment). The
//! op set itself is frozen from design §5.4 / contract §6.3.
//!
//! Every op serializes to CANONICAL, deterministic CBOR: a definite-length envelope
//! `[domain, op-kind, payload]`, domain-tagged + versioned (`"rag-rat/op/1"`) so a future op format
//! can never collide — the same discipline `crate::canonical` / `content_hash` use for
//! `"rag-rat/content-hash/1"`. **Structural** canonicity only (definite lengths, minimal-length
//! headers, deterministic field order): unlike `crate::canonical`, strings are serialized VERBATIM,
//! NOT NFC-normalized. An op is a per-author record whose bytes are fixed at authoring and carried
//! opaque thereafter — its determinism is byte-for-byte reproducibility of the SAME op, not
//! cross-author convergence of the same content. Content normalization (NFC + `trim`) is the write
//! path's job at author time (so the stored content and the separately-NFC-normalizing
//! `content_hash` agree); the wire serializer does not re-normalize. [`decode`] returns a
//! [`DecodedOp`]: a recognized op is `Known`; an
//! op whose KIND — or whose relation/status TOKEN — this binary doesn't know decodes to `Unknown`,
//! its raw bytes RETAINED (never projected) so a binary upgrade can re-fold it (the layer-1 opaque
//! seam, §5.4). Structurally-corrupt bytes are a hard error, distinct from the forward-compat seam.
//!
//! Closed-token reuse: the edge relation is `rag_rat_query::memory::EdgeRelation` (the persisted
//! `repo_node_edges.relation` set) and [`NodeStatus`] mirrors the validated memory-status set
//! (`active`/`stale`/`obsolete`/`rejected`) — this module invents NO new status/relation tokens.
//! `edge_key` is derived through the same `query::memory::edge_key` helper the live edge table
//! uses, and is treated as an opaque identity here (its canonical-CBOR form is a separate §5.5
//! increment).

use std::fmt;
use std::str::FromStr;

use minicbor::Encoder;
use minicbor::data::Type;
use minicbor::decode::{Decoder, Error as CborError};
use rag_rat_query::memory::{self, EdgeRelation};

use super::cbor::{self, VecEncoder, VecEncoderExt};

/// Domain tag + version, the envelope's first element. Bump the version to evolve the wire format
/// deliberately (an old binary then rejects the new domain rather than misreading it).
const DOMAIN: &str = "rag-rat/op/1";

/// A globally-unique memory/graph-node id (the `repo_memories.id` / `source_node_id` shape). Owned
/// and `Ord` so it keys the projected `nodes` map.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

impl NodeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NodeId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// The stable, content-addressed edge identity (`repo_node_edges.edge_key`). Opaque here: derived
/// via [`EdgeSpec::edge_key`] for an add, carried verbatim by a remove/rebind, and used only as a
/// map key by the fold. `Ord` so it keys the projected `edges` map.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeKey(String);

impl EdgeKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for EdgeKey {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for EdgeKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A 32-byte opaque device identity — the total-order tie-break under equal Lamport counters. Kept
/// opaque this increment (an ed25519 pubkey hash once the signed envelope lands); `Ord` compares
/// the raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceFingerprint([u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseDeviceFingerprintError {
    Length { actual: usize },
    InvalidHex { index: usize },
}

impl fmt::Display for ParseDeviceFingerprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { actual } => write!(
                f,
                "invalid device fingerprint: expected exactly 64 hexadecimal characters, got \
                 {actual}"
            ),
            Self::InvalidHex { index } =>
                write!(f, "invalid device fingerprint: non-hexadecimal character at byte {index}"),
        }
    }
}

impl std::error::Error for ParseDeviceFingerprintError {}

impl DeviceFingerprint {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw 32 bytes — the signed entry body encodes the fingerprint verbatim (`super::entry`).
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// A stored `device_fingerprint` BLOB, or an error naming the column when it is not 32 bytes.
    pub(crate) fn try_from_sql(bytes: Vec<u8>) -> anyhow::Result<Self> {
        cbor::sql_fixed(bytes, "device_fingerprint").map(Self)
    }
}

impl FromStr for DeviceFingerprint {
    type Err = ParseDeviceFingerprintError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(ParseDeviceFingerprintError::Length { actual: value.len() });
        }
        let mut bytes = [0u8; 32];
        for (index, &[hi, lo]) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = rag_rat_base::hash::hex_nibble(hi)
                .ok_or(ParseDeviceFingerprintError::InvalidHex { index: index * 2 })?;
            let low = rag_rat_base::hash::hex_nibble(lo)
                .ok_or(ParseDeviceFingerprintError::InvalidHex { index: index * 2 + 1 })?;
            bytes[index] = high << 4 | low;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for DeviceFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&rag_rat_base::hash::hex_lower(&self.0))
    }
}

/// A memory-node lifecycle status — the validated `repo_memories.status` set, mirrored as a closed
/// enum so the fold can carry it typed. The db tokens are pinned by test against
/// `query::memory::validate_status`; do not add a token without that gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum NodeStatus {
    Active,
    Stale,
    Obsolete,
    Rejected,
}

impl Default for NodeStatus {
    /// A node with no status op projects as `active` (the create-time default).
    fn default() -> Self {
        Self::Active
    }
}

impl NodeStatus {
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// `None` for an unrecognized token — the caller treats that as a forward-compat status this
    /// binary can't project (→ [`DecodedOp::Unknown`]), not a decode error.
    pub fn from_db_str(value: &str) -> Option<Self> {
        value.parse().ok()
    }
}

/// The content dimension of a node — the mapped `repo_memories` content columns (+ sibling
/// `repo_memory_tags`). Identity/bookkeeping columns (`content_hash`/`input_hash`/`repo_id`/
/// timestamps) are NOT op payload. `kind`/`confidence`/`source` are carried verbatim as strings
/// (their closed-set validation is the write path's job, not the wire's).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeContent {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub confidence: String,
    pub source: String,
    /// A SET: canonically sorted + deduped (via [`NodeContent::canonicalize`] / at encode time),
    /// so neither order nor duplicates perturb the wire bytes or the projected state.
    pub tags: Vec<String>,
    /// Opaque `schema_version`-tagged JSON payload for a polymorphic node; carried verbatim.
    pub payload: Option<String>,
}

impl NodeContent {
    /// Put `tags` in canonical (sorted + deduplicated) SET order. The wire encoder applies the same
    /// rule, and the fold applies this before STORING content, so an in-memory op built with
    /// unsorted/duplicate tags projects identically to the same op round-tripped through the wire.
    pub fn canonicalize(&mut self) {
        self.tags.sort_unstable();
        self.tags.dedup();
    }
}

/// The presence dimension of an edge — mirrors `repo_node_edges`: source, relation, target
/// `(repo, kind, anchor)`, and the owner repo. `edge_key` is DERIVED from
/// `(source, relation, target_kind, target_anchor)` (not the repo ids), matching the live table.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeSpec {
    pub source_node_id: NodeId,
    pub relation: EdgeRelation,
    pub target_repo_id: String,
    pub target_kind: String,
    pub target_anchor: String,
    pub owner_repo_id: String,
}

impl EdgeSpec {
    /// Derive the stable `edge_key` through the SAME helper the live edge table uses, so an op-log
    /// add and a direct insert content-address identically.
    pub fn edge_key(&self) -> EdgeKey {
        EdgeKey::from(memory::edge_key(
            self.source_node_id.as_str(),
            self.relation.as_db_str(),
            &self.target_kind,
            &self.target_anchor,
        ))
    }
}

/// The re-resolved local anchor a [`MemoryOp::Rebind`] carries — mirrors the resolution triple the
/// edge table recomputes on read (`target_repo_id`, resolved local `target_node_id`,
/// `anchor_status`). `anchor_status` is carried verbatim (opaque resolution state, not a wire token
/// this module owns).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAnchor {
    pub target_repo_id: String,
    pub target_node_id: Option<String>,
    pub anchor_status: String,
}

/// The portable half of one `repo_memory_bindings` row: the PK tail plus every column the
/// `anchors/1` table scope replicates, and nothing checkout-local. The `(repo_id, memory_id)` half
/// of that PK is context rather than payload — the repo being drained, and the op's own `node_id` —
/// so it is not carried per anchor.
///
/// Field order mirrors the `anchors/1` spec: the PK tail, then its synced columns. Keeping the two
/// readable against each other is the point; a column added to that scope has to be added here as a
/// new op kind, since widening this one would break the byte-canonical identity below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableAnchor {
    pub binding_kind: String,
    pub binding_id: String,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub commit_hash: Option<String>,
    pub tracker: Option<String>,
    pub project: Option<String>,
    pub item_key: Option<String>,
    pub created_at_ms: i64,
    pub symbol_kind: Option<String>,
    pub signature_hash: Option<String>,
    pub moniker_tool: Option<String>,
    pub moniker_tool_version: Option<String>,
}

impl PortableAnchor {
    /// The row this anchor names — the half of the binding PK an anchor set is ordered and
    /// deduplicated by. Deliberately NOT a derived `Ord` over the whole struct: two anchors sharing
    /// an identity are a conflict to reject, not two distinct members to order by their payloads.
    pub(crate) fn identity(&self) -> (&str, &str) {
        (self.binding_kind.as_str(), self.binding_id.as_str())
    }
}

/// The scope of one symbol anchor's target, hashed — the part of a symbol's identity the anchor
/// leaves out. An anchor carries a symbol's path, qualified name, kind and signature; its
/// enclosing scope is the fifth component of the index's logical key, and the only one that tells
/// apart two symbols agreeing on the other four — in Rust, two impls of different traits for one
/// type with the trait on a later line (`Twin as Alpha` / `Twin as Beta`), whose kind and captured
/// signature are both `impl` (#1276).
///
/// Keyed by the row it describes, `(binding_kind, binding_id)`, so it rides beside a `node_anchors`
/// set rather than inside it: `PortableAnchor` is a byte-canonical fixed-arity array, and widening
/// it would cost every older binary the anchors themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorScope {
    pub binding_kind: String,
    pub binding_id: String,
    /// `hex_sha256` of the target's scope path, as the author's index derives it.
    pub scope_hash: String,
}

impl AnchorScope {
    /// The anchor this scope describes — the same identity a `node_anchors` set is keyed by.
    pub(crate) fn identity(&self) -> (&str, &str) {
        (self.binding_kind.as_str(), self.binding_id.as_str())
    }
}

/// A row in a wire set is keyed by binding identity, never by its payload.
trait IdentifiedRow {
    fn identity(&self) -> (&str, &str);
}

impl IdentifiedRow for PortableAnchor {
    fn identity(&self) -> (&str, &str) {
        self.identity()
    }
}

impl IdentifiedRow for AnchorScope {
    fn identity(&self) -> (&str, &str) {
        self.identity()
    }
}

fn identities_unique<T: IdentifiedRow>(items: &[T]) -> bool {
    if items.len() > MAX_ANCHORS_PER_OP {
        return false;
    }
    let mut identities: Vec<_> = items.iter().map(IdentifiedRow::identity).collect();
    identities.sort_unstable();
    identities.windows(2).all(|pair| pair[0] != pair[1])
}

/// The most anchors one `node_anchors` op may carry. A memory holds a handful of bindings in
/// practice (its own, plus an auto-moniker), so this is a generous structural bound rather than a
/// budget.
///
/// RAISING IT IS A FORMAT CHANGE, and the failure it causes is SILENT. `/3` ingest never decodes op
/// bytes (they may be sealed), so an over-cap op from a newer peer is accepted, retained, and
/// forwarded; the projection then treats an undecodable body as a local skip, not an acceptance
/// failure, so the memory's anchors simply never appear on the older peer and nothing reports it. A
/// larger limit ships as a NEW op kind, the same discipline `snapshot` documents for its payload.
pub const MAX_ANCHORS_PER_OP: usize = 64;

/// The `PortableAnchor` fields, in wire order. Pinned against the `anchors/1` spec by a test in
/// that scope's own module, so adding a column there fails loudly instead of silently seeding NULL
/// across the account boundary.
pub(crate) const PORTABLE_ANCHOR_FIELDS: &[&str] = &[
    "binding_kind",
    "binding_id",
    "path",
    "start_line",
    "end_line",
    "commit_hash",
    "tracker",
    "project",
    "item_key",
    "created_at_ms",
    "symbol_kind",
    "signature_hash",
    "moniker_tool",
    "moniker_tool_version",
];

/// Whether `op` satisfies the structural limits `decode` enforces — the guard that keeps an op from
/// being signed and replicated in a shape NO binary can read back, its author included.
///
/// Byte size is bounded separately, by the content-entry caps. What this catches is the shapes that
/// are *small* and still undecodable: an over-cap anchor count, and a duplicated row identity. Both
/// encode happily, and `/3` would accept, retain and forward them, so without this gate they become
/// permanent entries whose anchors every peer silently drops at projection.
pub fn within_wire_limits(op: &MemoryOp) -> bool {
    match op {
        MemoryOp::NodeAnchors { anchors, .. } => identities_unique(anchors),
        MemoryOp::NodeAnchorScopes { scopes, .. } => identities_unique(scopes),
        // Listed rather than wildcarded ON PURPOSE: this seam's contract is "reject exactly what
        // `decode` rejects", so the next op kind that grows a count cap or an ordering rule must
        // fail to compile here instead of silently answering `true` — the same under-approximation
        // this function was added to close.
        MemoryOp::NodeSourceHash { .. }
        | MemoryOp::NodeCreate { .. }
        | MemoryOp::NodeUpdate { .. }
        | MemoryOp::NodeStatus { .. }
        | MemoryOp::EdgeAdd { .. }
        | MemoryOp::EdgeRemove { .. }
        | MemoryOp::Rebind { .. }
        | MemoryOp::Snapshot => true,
    }
}

/// The frozen op set (§5.4 / §6.3). Each op mutates exactly one LWW register of one node/edge (see
/// the fold), except `NodeCreate`, which also establishes existence.
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryOp {
    /// Establish a node and set its content register.
    NodeCreate { node_id: NodeId, content: NodeContent },
    /// FULL content replacement for an existing node.
    NodeUpdate { node_id: NodeId, content: NodeContent },
    /// The status/lifecycle dimension.
    NodeStatus { node_id: NodeId, status: NodeStatus },
    /// Edge presence; the `edge_key` is derivable from the spec.
    EdgeAdd { edge: EdgeSpec },
    /// Tombstone an edge by its stable `edge_key`.
    EdgeRemove { edge_key: EdgeKey },
    /// Re-resolve an edge's local anchor; NEVER mutates the `edge_key` or presence.
    Rebind { edge_key: EdgeKey, resolved: ResolvedAnchor },
    /// A node's portable anchor set — a FULL-SET snapshot, never a delta.
    NodeAnchors { node_id: NodeId, anchors: Vec<PortableAnchor> },
    /// The hash of the source text a node's author anchored to, so a receiver can tell whether its
    /// own checkout has drifted from what that author meant.
    ///
    /// A SIBLING of `node_anchors` rather than a field on it: `PortableAnchor` is a fixed-arity
    /// byte-canonical array, so carrying this there would be a new op kind anyway — and an old
    /// binary retains an unknown kind opaque, which under that shape would cost it the ANCHORS.
    /// Split, the same binary keeps its anchors and loses only the staleness marking.
    NodeSourceHash { node_id: NodeId, source_text_hash: String },
    /// The scope of each symbol anchor's target in a node's anchor set (see [`AnchorScope`]) — a
    /// FULL-SET statement, keyed by anchor identity; an empty set retracts.
    ///
    /// A sibling of `node_anchors` for the same reason `node_source_hash` is, and paired with it
    /// the same way: the fold takes a node's scopes from the device that wrote the winning
    /// anchor set, so an older binary that never publishes them contributes none, and an older
    /// receiver retains this kind opaque and keeps the anchors it already understands.
    NodeAnchorScopes { node_id: NodeId, scopes: Vec<AnchorScope> },
    /// A converged-state boundary marker; inert in the fold this increment (§5.4/C4).
    Snapshot,
}

impl MemoryOp {
    /// The envelope's op-kind tag (element 1). Stable wire tokens — a rename is a format change.
    fn kind_tag(&self) -> &'static str {
        match self {
            Self::NodeCreate { .. } => "node_create",
            Self::NodeUpdate { .. } => "node_update",
            Self::NodeStatus { .. } => "node_status",
            Self::EdgeAdd { .. } => "edge_add",
            Self::EdgeRemove { .. } => "edge_remove",
            Self::Rebind { .. } => "rebind",
            Self::NodeAnchors { .. } => "node_anchors",
            Self::NodeSourceHash { .. } => "node_source_hash",
            Self::NodeAnchorScopes { .. } => "node_anchor_scopes",
            Self::Snapshot => "snapshot",
        }
    }
}

/// One op plus its total-order metadata — the unit the fold consumes.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub meta: OpMeta,
    pub op: MemoryOp,
}

/// The op-log's ordering key: a per-stream Lamport counter + the authoring device. Total order is
/// `(lamport, device)` ascending, device bytes breaking a Lamport tie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpMeta {
    pub lamport: u64,
    pub device: DeviceFingerprint,
}

/// The outcome of decoding one op envelope. `Unknown` is the forward-compat seam: an op kind — or a
/// relation/status token — this binary doesn't recognize is kept opaque (raw bytes RETAINED) rather
/// than dropped or projected, so a later binary can re-fold the stream (§5.4).
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedOp {
    Known(MemoryOp),
    Unknown { tag: String, raw: Vec<u8> },
}

/// Encode one op to canonical CBOR: `[domain, op-kind, payload]`, definite lengths throughout,
/// deterministic. The op's METADATA (`OpMeta`) is NOT encoded here — it belongs to the signed
/// envelope (a later increment); these bytes freeze the op wire format the golden vectors pin.
pub fn encode(op: &MemoryOp) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.put_array(3);
        enc.put_str(DOMAIN);
        enc.put_str(op.kind_tag());
        encode_payload(&mut enc, op);
    }
    buf
}

/// Write the op-specific payload as exactly ONE CBOR item (the envelope's element 2).
fn encode_payload(enc: &mut VecEncoder<'_>, op: &MemoryOp) {
    match op {
        MemoryOp::NodeCreate { node_id, content } | MemoryOp::NodeUpdate { node_id, content } => {
            enc.put_array(2);
            enc.put_str(node_id.as_str());
            encode_content(enc, content);
        },
        MemoryOp::NodeStatus { node_id, status } => {
            enc.put_array(2);
            enc.put_str(node_id.as_str());
            enc.put_str(status.as_db_str());
        },
        MemoryOp::EdgeAdd { edge } => encode_edge_spec(enc, edge),
        MemoryOp::EdgeRemove { edge_key } => {
            enc.put_str(edge_key.as_str());
        },
        MemoryOp::Rebind { edge_key, resolved } => {
            enc.put_array(2);
            enc.put_str(edge_key.as_str());
            encode_resolved(enc, resolved);
        },
        MemoryOp::NodeAnchors { node_id, anchors } => {
            enc.put_array(2);
            enc.put_str(node_id.as_str());
            encode_anchors(enc, anchors);
        },
        MemoryOp::NodeSourceHash { node_id, source_text_hash } => {
            enc.put_array(2);
            enc.put_str(node_id.as_str());
            enc.put_str(source_text_hash);
        },
        MemoryOp::NodeAnchorScopes { node_id, scopes } => {
            enc.put_array(2);
            enc.put_str(node_id.as_str());
            encode_anchor_scopes(enc, scopes);
        },
        MemoryOp::Snapshot => {
            // Inert boundary marker: a strictly-null payload. A future snapshot that carries a
            // coverage manifest (§5.4/C4) is a NEW op kind — NOT a non-null payload under this kind
            // — so an old binary retains it through the unknown-KIND seam (uniform forward-compat),
            // while `snapshot` stays null-only and its decode rejects any non-null payload.
            enc.put_null();
        },
    }
}

fn encode_content(enc: &mut VecEncoder<'_>, content: &NodeContent) {
    enc.put_array(7);
    enc.put_str(&content.kind);
    enc.put_str(&content.title);
    enc.put_str(&content.body);
    enc.put_str(&content.confidence);
    enc.put_str(&content.source);
    // Tags are a SET: sort AND dedup before encoding so neither order nor duplicates perturb the
    // canonical bytes. `NodeContent::canonicalize` applies the SAME rule to stored content, so the
    // wire and the projected state agree.
    let mut tags: Vec<&str> = content.tags.iter().map(String::as_str).collect();
    tags.sort_unstable();
    tags.dedup();
    enc.put_array(tags.len() as u64);
    for tag in tags {
        enc.put_str(tag);
    }
    encode_opt_str(enc, content.payload.as_deref());
}

fn encode_edge_spec(enc: &mut VecEncoder<'_>, edge: &EdgeSpec) {
    enc.put_array(6);
    enc.put_str(edge.source_node_id.as_str());
    enc.put_str(edge.relation.as_db_str());
    enc.put_str(&edge.target_repo_id);
    enc.put_str(&edge.target_kind);
    enc.put_str(&edge.target_anchor);
    enc.put_str(&edge.owner_repo_id);
}

fn encode_resolved(enc: &mut VecEncoder<'_>, resolved: &ResolvedAnchor) {
    enc.put_array(3);
    enc.put_str(&resolved.target_repo_id);
    encode_opt_str(enc, resolved.target_node_id.as_deref());
    enc.put_str(&resolved.anchor_status);
}

/// Encode the anchor SET, ordered by identity so neither the caller's insertion sequence nor the
/// query that produced it perturbs the canonical bytes — the rule `tags` already follows.
///
/// Duplicates are deliberately NOT deduped here. Two anchors sharing `(binding_kind, binding_id)`
/// name one row twice, and the payload cannot say which of them wins, so `decode`'s
/// strictly-increasing check rejects them.
///
/// That check is the SOLE rejector — the `encode == bytes` identity check is not a second net here,
/// and must not be mistaken for one. It is unreachable (decode errors first), and it would pass
/// anyway: this sort is stable, so a duplicate-carrying payload re-encodes to the bytes it came
/// from. Relaxing the `>=` to `>` would let duplicates straight through.
fn encode_identity_set<T: IdentifiedRow>(
    enc: &mut VecEncoder<'_>,
    items: &[T],
    write: impl Fn(&mut VecEncoder<'_>, &T),
) {
    let mut ordered: Vec<&T> = items.iter().collect();
    ordered.sort_by(|a, b| a.identity().cmp(&b.identity()));
    enc.put_array(ordered.len() as u64);
    for item in ordered {
        write(enc, item);
    }
}

fn encode_anchors(enc: &mut VecEncoder<'_>, anchors: &[PortableAnchor]) {
    encode_identity_set(enc, anchors, encode_anchor);
}

fn encode_anchor_scopes(enc: &mut VecEncoder<'_>, scopes: &[AnchorScope]) {
    encode_identity_set(enc, scopes, encode_anchor_scope);
}

fn encode_anchor_scope(enc: &mut VecEncoder<'_>, scope: &AnchorScope) {
    enc.put_array(3);
    enc.put_str(&scope.binding_kind);
    enc.put_str(&scope.binding_id);
    enc.put_str(&scope.scope_hash);
}

fn encode_anchor(enc: &mut VecEncoder<'_>, anchor: &PortableAnchor) {
    enc.put_array(14);
    enc.put_str(&anchor.binding_kind);
    enc.put_str(&anchor.binding_id);
    encode_opt_str(enc, anchor.path.as_deref());
    encode_opt_i64(enc, anchor.start_line);
    encode_opt_i64(enc, anchor.end_line);
    encode_opt_str(enc, anchor.commit_hash.as_deref());
    encode_opt_str(enc, anchor.tracker.as_deref());
    encode_opt_str(enc, anchor.project.as_deref());
    encode_opt_str(enc, anchor.item_key.as_deref());
    enc.put_i64(anchor.created_at_ms);
    encode_opt_str(enc, anchor.symbol_kind.as_deref());
    encode_opt_str(enc, anchor.signature_hash.as_deref());
    encode_opt_str(enc, anchor.moniker_tool.as_deref());
    encode_opt_str(enc, anchor.moniker_tool_version.as_deref());
}

/// Encode an optional integer as an integer item or CBOR `null` — the `encode_opt_str` rule for the
/// nullable INTEGER columns (`start_line` / `end_line`), which a tracker binding leaves unset.
fn encode_opt_i64(enc: &mut VecEncoder<'_>, value: Option<i64>) {
    match value {
        Some(number) => enc.put_i64(number),
        None => enc.put_null(),
    };
}

/// Encode an optional string as a text item or CBOR `null` — a distinct, unambiguous absent marker.
fn encode_opt_str(enc: &mut VecEncoder<'_>, value: Option<&str>) {
    match value {
        Some(text) => enc.put_str(text),
        None => enc.put_null(),
    };
}

/// Decode one op envelope. A recognized op → `Known`; a future op kind / relation / status token →
/// `Unknown` (raw bytes retained); structurally-invalid CBOR or a wrong/absent domain tag → `Err`.
pub fn decode(bytes: &[u8]) -> anyhow::Result<DecodedOp> {
    decode_envelope(bytes).map_err(|err| anyhow::anyhow!("op decode failed: {err}"))
}

fn decode_envelope(bytes: &[u8]) -> Result<DecodedOp, CborError> {
    let mut d = Decoder::new(bytes);
    cbor::expect_array(&mut d, 3)?;
    let domain = d.str()?;
    if domain != DOMAIN {
        // A wrong/absent domain tag is a foreign or corrupt object, NOT a forward-compat op — a
        // future format bumps the version and an old binary must reject rather than misread it.
        return Err(CborError::message(format!(
            "unknown op domain tag `{domain}` (expected `{DOMAIN}`)"
        )));
    }
    let kind = d.str()?.to_string();
    // `None` from a decode helper means "a token this binary doesn't know" → the whole op is kept
    // opaque as `Unknown`. A hard `Err` (propagated by `?`) means the bytes are structurally wrong.
    let known = match kind.as_str() {
        "node_create" => {
            let (node_id, content) = decode_node_content(&mut d)?;
            Some(MemoryOp::NodeCreate { node_id, content })
        },
        "node_update" => {
            let (node_id, content) = decode_node_content(&mut d)?;
            Some(MemoryOp::NodeUpdate { node_id, content })
        },
        "node_status" => decode_node_status(&mut d)?,
        "edge_add" => decode_edge_spec(&mut d)?.map(|edge| MemoryOp::EdgeAdd { edge }),
        "edge_remove" => Some(MemoryOp::EdgeRemove { edge_key: EdgeKey::from(d.str()?) }),
        "rebind" => {
            let (edge_key, resolved) = decode_rebind(&mut d)?;
            Some(MemoryOp::Rebind { edge_key, resolved })
        },
        "node_anchors" => {
            let (node_id, anchors) = decode_node_anchors(&mut d)?;
            Some(MemoryOp::NodeAnchors { node_id, anchors })
        },
        "node_source_hash" => {
            cbor::expect_array(&mut d, 2)?;
            let node_id = NodeId::from(d.str()?);
            Some(MemoryOp::NodeSourceHash { node_id, source_text_hash: d.str()?.to_string() })
        },
        "node_anchor_scopes" => {
            cbor::expect_array(&mut d, 2)?;
            let node_id = NodeId::from(d.str()?);
            let scopes = decode_anchor_scopes(&mut d)?;
            Some(MemoryOp::NodeAnchorScopes { node_id, scopes })
        },
        "snapshot" => {
            d.null()?;
            Some(MemoryOp::Snapshot)
        },
        // A future op KIND this binary doesn't know — its payload is not read here; the raw bytes
        // are validated for canonical CBOR on the `None` arm below.
        _ => None,
    };
    match known {
        Some(op) => {
            // Byte-CANONICAL identity: a known op has exactly ONE accepted encoding — the one
            // `encode` produces (minimal headers, definite lengths, sorted+deduped tags, NO
            // trailing bytes). `minicbor`'s decoder otherwise accepts
            // structurally-valid but non-canonical input; re-encoding and demanding
            // equality rejects every alternate representation, so a later signature /
            // content-address over these bytes is unambiguous.
            if encode(&op) != bytes {
                return Err(CborError::message("non-canonical op encoding"));
            }
            Ok(DecodedOp::Known(op))
        },
        None => {
            // An UNKNOWN op is retained opaque (we can't re-encode it), but it must STILL be
            // exactly one canonical CBOR item with no trailing bytes — otherwise a
            // future binary that learns the kind could see two wire forms of one
            // logical op (and its `encode == bytes` check would then reject an entry an
            // older peer accepted + forwarded). Validate the raw bytes.
            cbor::require_canonical_cbor(bytes)?;
            Ok(DecodedOp::Unknown { tag: kind, raw: bytes.to_vec() })
        },
    }
}

fn decode_node_content(d: &mut Decoder<'_>) -> Result<(NodeId, NodeContent), CborError> {
    cbor::expect_array(d, 2)?;
    let node_id = NodeId::from(d.str()?);
    let content = decode_content(d)?;
    Ok((node_id, content))
}

fn decode_content(d: &mut Decoder<'_>) -> Result<NodeContent, CborError> {
    cbor::expect_array(d, 7)?;
    let kind = d.str()?.to_string();
    let title = d.str()?.to_string();
    let body = d.str()?.to_string();
    let confidence = d.str()?.to_string();
    let source = d.str()?.to_string();
    let tags = cbor::decode_str_array(d)?;
    let payload = decode_opt_str(d)?;
    Ok(NodeContent { kind, title, body, confidence, source, tags, payload })
}

/// Decode a node-status op, or `None` for a forward-compat status token this binary can't project.
fn decode_node_status(d: &mut Decoder<'_>) -> Result<Option<MemoryOp>, CborError> {
    cbor::expect_array(d, 2)?;
    let node_id = NodeId::from(d.str()?);
    let token = d.str()?;
    Ok(NodeStatus::from_db_str(token).map(|status| MemoryOp::NodeStatus { node_id, status }))
}

/// Decode an edge spec, or `None` for a forward-compat relation token this binary can't project.
fn decode_edge_spec(d: &mut Decoder<'_>) -> Result<Option<EdgeSpec>, CborError> {
    cbor::expect_array(d, 6)?;
    let source_node_id = NodeId::from(d.str()?);
    let token = d.str()?.to_string();
    // Read the WHOLE payload before judging the relation token, so a TRUNCATED `edge_add` is a hard
    // (structural) error even when the relation is unknown — an unknown relation must still be a
    // complete, well-formed op to be retained opaquely.
    let target_repo_id = d.str()?.to_string();
    let target_kind = d.str()?.to_string();
    let target_anchor = d.str()?.to_string();
    let owner_repo_id = d.str()?.to_string();
    let Ok(relation) = EdgeRelation::from_db_str(&token) else {
        // A relation this binary doesn't know → not projectable; kept opaque as `Unknown`.
        return Ok(None);
    };
    Ok(Some(EdgeSpec {
        source_node_id,
        relation,
        target_repo_id,
        target_kind,
        target_anchor,
        owner_repo_id,
    }))
}

fn decode_rebind(d: &mut Decoder<'_>) -> Result<(EdgeKey, ResolvedAnchor), CborError> {
    cbor::expect_array(d, 2)?;
    let edge_key = EdgeKey::from(d.str()?);
    let resolved = decode_resolved(d)?;
    Ok((edge_key, resolved))
}

fn decode_node_anchors(d: &mut Decoder<'_>) -> Result<(NodeId, Vec<PortableAnchor>), CborError> {
    cbor::expect_array(d, 2)?;
    let node_id = NodeId::from(d.str()?);
    let anchors = decode_anchors(d)?;
    Ok((node_id, anchors))
}

fn decode_identity_set<T: IdentifiedRow>(
    d: &mut Decoder<'_>,
    op: &str,
    noun: &str,
    read: impl Fn(&mut Decoder<'_>) -> Result<T, CborError>,
) -> Result<Vec<T>, CborError> {
    let len = cbor::expect_definite_len(d)?;
    // Judge the attacker-controlled count before reading or preallocating any elements.
    if len > MAX_ANCHORS_PER_OP as u64 {
        return Err(CborError::message(format!(
            "{op} carries {len} {noun}, over the {MAX_ANCHORS_PER_OP} limit"
        )));
    }
    let mut out: Vec<T> = Vec::new();
    for _ in 0..len {
        let item = read(d)?;
        // One strict comparison rejects both unsorted payloads and duplicated identities.
        if let Some(previous) = out.last()
            && previous.identity() >= item.identity()
        {
            return Err(CborError::message(format!(
                "{op} must be strictly increasing by (binding_kind, binding_id)"
            )));
        }
        out.push(item);
    }
    Ok(out)
}

fn decode_anchors(d: &mut Decoder<'_>) -> Result<Vec<PortableAnchor>, CborError> {
    decode_identity_set(d, "node_anchors", "anchors", decode_anchor)
}

fn decode_anchor_scopes(d: &mut Decoder<'_>) -> Result<Vec<AnchorScope>, CborError> {
    decode_identity_set(d, "node_anchor_scopes", "scopes", decode_anchor_scope)
}

fn decode_anchor_scope(d: &mut Decoder<'_>) -> Result<AnchorScope, CborError> {
    cbor::expect_array(d, 3)?;
    Ok(AnchorScope {
        binding_kind: d.str()?.to_string(),
        binding_id: d.str()?.to_string(),
        scope_hash: d.str()?.to_string(),
    })
}

fn decode_anchor(d: &mut Decoder<'_>) -> Result<PortableAnchor, CborError> {
    cbor::expect_array(d, 14)?;
    // Field order is the wire order; struct-literal fields evaluate top to bottom, so this reads
    // the array in the sequence `encode_anchor` wrote it.
    Ok(PortableAnchor {
        binding_kind: d.str()?.to_string(),
        binding_id: d.str()?.to_string(),
        path: decode_opt_str(d)?,
        start_line: decode_opt_i64(d)?,
        end_line: decode_opt_i64(d)?,
        commit_hash: decode_opt_str(d)?,
        tracker: decode_opt_str(d)?,
        project: decode_opt_str(d)?,
        item_key: decode_opt_str(d)?,
        created_at_ms: d.i64()?,
        symbol_kind: decode_opt_str(d)?,
        signature_hash: decode_opt_str(d)?,
        moniker_tool: decode_opt_str(d)?,
        moniker_tool_version: decode_opt_str(d)?,
    })
}

fn decode_opt_i64(d: &mut Decoder<'_>) -> Result<Option<i64>, CborError> {
    if d.datatype()? == Type::Null {
        d.null()?;
        Ok(None)
    } else {
        Ok(Some(d.i64()?))
    }
}

fn decode_resolved(d: &mut Decoder<'_>) -> Result<ResolvedAnchor, CborError> {
    cbor::expect_array(d, 3)?;
    let target_repo_id = d.str()?.to_string();
    let target_node_id = decode_opt_str(d)?;
    let anchor_status = d.str()?.to_string();
    Ok(ResolvedAnchor { target_repo_id, target_node_id, anchor_status })
}

fn decode_opt_str(d: &mut Decoder<'_>) -> Result<Option<String>, CborError> {
    if d.datatype()? == Type::Null {
        d.null()?;
        Ok(None)
    } else {
        Ok(Some(d.str()?.to_string()))
    }
}

#[cfg(test)]
#[path = "op_tests.rs"]
mod tests;
