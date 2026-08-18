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

use super::cbor;

/// Domain tag + version, the envelope's first element. Bump the version to evolve the wire format
/// deliberately (an old binary then rejects the new domain rather than misreading it).
const DOMAIN: &str = "rag-rat/op/1";

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible), so every encode step
/// `.expect`s this — mirrors `content_hash`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

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
}

impl FromStr for DeviceFingerprint {
    type Err = ParseDeviceFingerprintError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(ParseDeviceFingerprintError::Length { actual: value.len() });
        }
        let mut bytes = [0u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = rag_rat_base::hash::hex_nibble(pair[0])
                .ok_or(ParseDeviceFingerprintError::InvalidHex { index: index * 2 })?;
            let low = rag_rat_base::hash::hex_nibble(pair[1])
                .ok_or(ParseDeviceFingerprintError::InvalidHex { index: index * 2 + 1 })?;
            bytes[index] = high << 4 | low;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for DeviceFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A memory-node lifecycle status — the validated `repo_memories.status` set, mirrored as a closed
/// enum so the fold can carry it typed. The db tokens are pinned by test against
/// `query::memory::validate_status`; do not add a token without that gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        match self {
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Obsolete => "obsolete",
            Self::Rejected => "rejected",
        }
    }

    /// `None` for an unrecognized token — the caller treats that as a forward-compat status this
    /// binary can't project (→ [`DecodedOp::Unknown`]), not a decode error.
    pub fn from_db_str(value: &str) -> Option<Self> {
        Some(match value {
            "active" => Self::Active,
            "stale" => Self::Stale,
            "obsolete" => Self::Obsolete,
            "rejected" => Self::Rejected,
            _ => return None,
        })
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
    fn identity(&self) -> (&str, &str) {
        (self.binding_kind.as_str(), self.binding_id.as_str())
    }
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
        MemoryOp::NodeAnchors { anchors, .. } => {
            if anchors.len() > MAX_ANCHORS_PER_OP {
                return false;
            }
            let mut identities: Vec<(&str, &str)> =
                anchors.iter().map(PortableAnchor::identity).collect();
            identities.sort_unstable();
            identities.windows(2).all(|pair| pair[0] != pair[1])
        },
        // Listed rather than wildcarded ON PURPOSE: this seam's contract is "reject exactly what
        // `decode` rejects", so the next op kind that grows a count cap or an ordering rule must
        // fail to compile here instead of silently answering `true` — the same under-approximation
        // this function was added to close.
        MemoryOp::NodeCreate { .. }
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

/// A `minicbor` encoder writing into an owned `Vec` — the concrete, infallible target every encode
/// helper shares.
type VecEncoder<'a> = Encoder<&'a mut Vec<u8>>;

/// Encode one op to canonical CBOR: `[domain, op-kind, payload]`, definite lengths throughout,
/// deterministic. The op's METADATA (`OpMeta`) is NOT encoded here — it belongs to the signed
/// envelope (a later increment); these bytes freeze the op wire format the golden vectors pin.
pub fn encode(op: &MemoryOp) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(3).expect(INFALLIBLE);
        enc.str(DOMAIN).expect(INFALLIBLE);
        enc.str(op.kind_tag()).expect(INFALLIBLE);
        encode_payload(&mut enc, op);
    }
    buf
}

/// Write the op-specific payload as exactly ONE CBOR item (the envelope's element 2).
fn encode_payload(enc: &mut VecEncoder<'_>, op: &MemoryOp) {
    match op {
        MemoryOp::NodeCreate { node_id, content } | MemoryOp::NodeUpdate { node_id, content } => {
            enc.array(2).expect(INFALLIBLE);
            enc.str(node_id.as_str()).expect(INFALLIBLE);
            encode_content(enc, content);
        },
        MemoryOp::NodeStatus { node_id, status } => {
            enc.array(2).expect(INFALLIBLE);
            enc.str(node_id.as_str()).expect(INFALLIBLE);
            enc.str(status.as_db_str()).expect(INFALLIBLE);
        },
        MemoryOp::EdgeAdd { edge } => encode_edge_spec(enc, edge),
        MemoryOp::EdgeRemove { edge_key } => {
            enc.str(edge_key.as_str()).expect(INFALLIBLE);
        },
        MemoryOp::Rebind { edge_key, resolved } => {
            enc.array(2).expect(INFALLIBLE);
            enc.str(edge_key.as_str()).expect(INFALLIBLE);
            encode_resolved(enc, resolved);
        },
        MemoryOp::NodeAnchors { node_id, anchors } => {
            enc.array(2).expect(INFALLIBLE);
            enc.str(node_id.as_str()).expect(INFALLIBLE);
            encode_anchors(enc, anchors);
        },
        MemoryOp::Snapshot => {
            // Inert boundary marker: a strictly-null payload. A future snapshot that carries a
            // coverage manifest (§5.4/C4) is a NEW op kind — NOT a non-null payload under this kind
            // — so an old binary retains it through the unknown-KIND seam (uniform forward-compat),
            // while `snapshot` stays null-only and its decode rejects any non-null payload.
            enc.null().expect(INFALLIBLE);
        },
    }
}

fn encode_content(enc: &mut VecEncoder<'_>, content: &NodeContent) {
    enc.array(7).expect(INFALLIBLE);
    enc.str(&content.kind).expect(INFALLIBLE);
    enc.str(&content.title).expect(INFALLIBLE);
    enc.str(&content.body).expect(INFALLIBLE);
    enc.str(&content.confidence).expect(INFALLIBLE);
    enc.str(&content.source).expect(INFALLIBLE);
    // Tags are a SET: sort AND dedup before encoding so neither order nor duplicates perturb the
    // canonical bytes. `NodeContent::canonicalize` applies the SAME rule to stored content, so the
    // wire and the projected state agree.
    let mut tags: Vec<&str> = content.tags.iter().map(String::as_str).collect();
    tags.sort_unstable();
    tags.dedup();
    enc.array(tags.len() as u64).expect(INFALLIBLE);
    for tag in tags {
        enc.str(tag).expect(INFALLIBLE);
    }
    encode_opt_str(enc, content.payload.as_deref());
}

fn encode_edge_spec(enc: &mut VecEncoder<'_>, edge: &EdgeSpec) {
    enc.array(6).expect(INFALLIBLE);
    enc.str(edge.source_node_id.as_str()).expect(INFALLIBLE);
    enc.str(edge.relation.as_db_str()).expect(INFALLIBLE);
    enc.str(&edge.target_repo_id).expect(INFALLIBLE);
    enc.str(&edge.target_kind).expect(INFALLIBLE);
    enc.str(&edge.target_anchor).expect(INFALLIBLE);
    enc.str(&edge.owner_repo_id).expect(INFALLIBLE);
}

fn encode_resolved(enc: &mut VecEncoder<'_>, resolved: &ResolvedAnchor) {
    enc.array(3).expect(INFALLIBLE);
    enc.str(&resolved.target_repo_id).expect(INFALLIBLE);
    encode_opt_str(enc, resolved.target_node_id.as_deref());
    enc.str(&resolved.anchor_status).expect(INFALLIBLE);
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
fn encode_anchors(enc: &mut VecEncoder<'_>, anchors: &[PortableAnchor]) {
    let mut ordered: Vec<&PortableAnchor> = anchors.iter().collect();
    ordered.sort_by(|a, b| a.identity().cmp(&b.identity()));
    enc.array(ordered.len() as u64).expect(INFALLIBLE);
    for anchor in ordered {
        encode_anchor(enc, anchor);
    }
}

fn encode_anchor(enc: &mut VecEncoder<'_>, anchor: &PortableAnchor) {
    enc.array(14).expect(INFALLIBLE);
    enc.str(&anchor.binding_kind).expect(INFALLIBLE);
    enc.str(&anchor.binding_id).expect(INFALLIBLE);
    encode_opt_str(enc, anchor.path.as_deref());
    encode_opt_i64(enc, anchor.start_line);
    encode_opt_i64(enc, anchor.end_line);
    encode_opt_str(enc, anchor.commit_hash.as_deref());
    encode_opt_str(enc, anchor.tracker.as_deref());
    encode_opt_str(enc, anchor.project.as_deref());
    encode_opt_str(enc, anchor.item_key.as_deref());
    enc.i64(anchor.created_at_ms).expect(INFALLIBLE);
    encode_opt_str(enc, anchor.symbol_kind.as_deref());
    encode_opt_str(enc, anchor.signature_hash.as_deref());
    encode_opt_str(enc, anchor.moniker_tool.as_deref());
    encode_opt_str(enc, anchor.moniker_tool_version.as_deref());
}

/// Encode an optional integer as an integer item or CBOR `null` — the `encode_opt_str` rule for the
/// nullable INTEGER columns (`start_line` / `end_line`), which a tracker binding leaves unset.
fn encode_opt_i64(enc: &mut VecEncoder<'_>, value: Option<i64>) {
    match value {
        Some(number) => enc.i64(number).expect(INFALLIBLE),
        None => enc.null().expect(INFALLIBLE),
    };
}

/// Encode an optional string as a text item or CBOR `null` — a distinct, unambiguous absent marker.
fn encode_opt_str(enc: &mut VecEncoder<'_>, value: Option<&str>) {
    match value {
        Some(text) => enc.str(text).expect(INFALLIBLE),
        None => enc.null().expect(INFALLIBLE),
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
    let tags = decode_str_array(d)?;
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

fn decode_anchors(d: &mut Decoder<'_>) -> Result<Vec<PortableAnchor>, CborError> {
    let len = cbor::expect_definite_len(d)?;
    // Judge the COUNT from the header before decoding a single element. The length is
    // attacker-controlled, so this both bounds the work and stays clear of trusting it enough to
    // preallocate — the `decode_str_array` rule.
    if len > MAX_ANCHORS_PER_OP as u64 {
        return Err(CborError::message(format!(
            "node_anchors carries {len} anchors, over the {MAX_ANCHORS_PER_OP} limit"
        )));
    }
    let mut out: Vec<PortableAnchor> = Vec::new();
    for _ in 0..len {
        let anchor = decode_anchor(d)?;
        // Canonical SET order: strictly increasing by identity. One comparison rejects both an
        // unsorted payload and a duplicated row identity — the latter names one row twice, and
        // nothing in the op says which of the two should win.
        if let Some(previous) = out.last()
            && previous.identity() >= anchor.identity()
        {
            return Err(CborError::message(
                "node_anchors must be strictly increasing by (binding_kind, binding_id)",
            ));
        }
        out.push(anchor);
    }
    Ok(out)
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

fn decode_str_array(d: &mut Decoder<'_>) -> Result<Vec<String>, CborError> {
    let len = cbor::expect_definite_len(d)?;
    // Do NOT preallocate `len`: it is an attacker-controllable CBOR array header, so a bogus huge
    // count would OOM before the (short) body is even read. Grow as elements are actually decoded —
    // a truncated array errors at the first missing element, bounding work by real input size.
    let mut out = Vec::new();
    for _ in 0..len {
        out.push(d.str()?.to_string());
    }
    Ok(out)
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
mod tests {
    use super::*;

    #[test]
    fn device_fingerprint_hex_is_exact_and_canonical() {
        let expected = DeviceFingerprint::from_bytes([0xab; 32]);
        assert_eq!(expected.to_string(), "ab".repeat(32));
        assert_eq!("AB".repeat(32).parse::<DeviceFingerprint>().unwrap(), expected);

        let short = "ab".repeat(31).parse::<DeviceFingerprint>().unwrap_err();
        assert!(short.to_string().contains("exactly 64 hexadecimal characters"));
        let malformed = format!("{}az", "ab".repeat(31));
        let malformed = malformed.parse::<DeviceFingerprint>().unwrap_err();
        assert!(malformed.to_string().contains("non-hexadecimal character at byte 63"));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Hand-roll a raw CBOR envelope, scoping the encoder so its borrow on the buffer ends before
    /// the bytes are returned — the fixture builder for the forward-compat / corruption tests.
    fn raw_envelope(write: impl FnOnce(&mut VecEncoder<'_>)) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            write(&mut enc);
        }
        buf
    }

    fn content() -> NodeContent {
        NodeContent {
            kind: "Invariant".to_string(),
            title: "title".to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            source: "agent".to_string(),
            // Already sorted — encode canonicalizes, so a round-trip yields sorted tags.
            tags: vec!["a".to_string(), "b".to_string()],
            payload: Some(r#"{"schema_version":1}"#.to_string()),
        }
    }

    fn edge_spec() -> EdgeSpec {
        EdgeSpec {
            source_node_id: NodeId::from("mem_src"),
            relation: EdgeRelation::DependsOn,
            target_repo_id: "repo_t".to_string(),
            target_kind: "node".to_string(),
            target_anchor: "mem_dst".to_string(),
            owner_repo_id: "repo_o".to_string(),
        }
    }

    fn resolved() -> ResolvedAnchor {
        ResolvedAnchor {
            target_repo_id: "repo_t".to_string(),
            target_node_id: Some("mem_dst".to_string()),
            anchor_status: "current".to_string(),
        }
    }

    /// A symbol binding and a tracker binding: between them every nullable column is exercised in
    /// both states, since neither shape populates the other's columns. Already in identity order —
    /// encode canonicalizes, so a round-trip yields the sorted set.
    fn anchors() -> Vec<PortableAnchor> {
        vec![
            PortableAnchor {
                binding_kind: "symbol".to_string(),
                binding_id: "crates/x/src/lib.rs::run".to_string(),
                path: Some("crates/x/src/lib.rs".to_string()),
                start_line: Some(10),
                end_line: Some(20),
                commit_hash: Some("c0ffee".to_string()),
                tracker: None,
                project: None,
                item_key: None,
                created_at_ms: 1_700_000_000_000,
                symbol_kind: Some("function".to_string()),
                signature_hash: Some("5ig".to_string()),
                moniker_tool: Some("scip-rust".to_string()),
                moniker_tool_version: Some("0.3".to_string()),
            },
            PortableAnchor {
                binding_kind: "tracker".to_string(),
                binding_id: "github:owner/repo#7".to_string(),
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                tracker: Some("github".to_string()),
                project: Some("owner/repo".to_string()),
                item_key: Some("7".to_string()),
                created_at_ms: 1_700_000_000_001,
                symbol_kind: None,
                signature_hash: None,
                moniker_tool: None,
                moniker_tool_version: None,
            },
        ]
    }

    /// Write one anchor into a hand-rolled envelope — the fixture builder for the canonical-order
    /// rejections, which need wire forms `encode` would never produce.
    fn raw_anchor(enc: &mut VecEncoder<'_>, kind: &str, id: &str) {
        enc.array(14).unwrap();
        enc.str(kind).unwrap();
        enc.str(id).unwrap();
        enc.null().unwrap(); // path
        enc.null().unwrap(); // start_line
        enc.null().unwrap(); // end_line
        enc.null().unwrap(); // commit_hash
        enc.null().unwrap(); // tracker
        enc.null().unwrap(); // project
        enc.null().unwrap(); // item_key
        enc.i64(1).unwrap(); // created_at_ms
        enc.null().unwrap(); // symbol_kind
        enc.null().unwrap(); // signature_hash
        enc.null().unwrap(); // moniker_tool
        enc.null().unwrap(); // moniker_tool_version
    }

    /// One representative op per variant — the golden + round-trip fixtures.
    fn every_variant() -> Vec<(&'static str, MemoryOp)> {
        vec![
            ("node_create", MemoryOp::NodeCreate {
                node_id: NodeId::from("mem_1"),
                content: content(),
            }),
            ("node_update", MemoryOp::NodeUpdate {
                node_id: NodeId::from("mem_1"),
                content: content(),
            }),
            ("node_status", MemoryOp::NodeStatus {
                node_id: NodeId::from("mem_1"),
                status: NodeStatus::Obsolete,
            }),
            ("edge_add", MemoryOp::EdgeAdd { edge: edge_spec() }),
            ("edge_remove", MemoryOp::EdgeRemove { edge_key: EdgeKey::from("edgekey_1") }),
            ("rebind", MemoryOp::Rebind {
                edge_key: EdgeKey::from("edgekey_1"),
                resolved: resolved(),
            }),
            ("node_anchors", MemoryOp::NodeAnchors {
                node_id: NodeId::from("mem_1"),
                anchors: anchors(),
            }),
            ("snapshot", MemoryOp::Snapshot),
        ]
    }

    #[test]
    fn golden_vectors_pin_the_op_wire_format() {
        // The op wire format is a frozen primitive: a signed envelope, the fold, and (later) the
        // content-addressed identity all build on these exact bytes. Any change to the canonical
        // rule must break this test and force a deliberate `rag-rat/op/1` version bump.
        let got: Vec<(&str, String)> =
            every_variant().iter().map(|(name, op)| (*name, hex(&encode(op)))).collect();
        let want: Vec<(&str, &str)> = vec![
            (
                "node_create",
                "836c7261672d7261742f6f702f316b6e6f64655f63726561746582656d656d5f318769496e76617269616e74657469746c6564626f64796468696768656167656e748261616162747b22736368656d615f76657273696f6e223a317d",
            ),
            (
                "node_update",
                "836c7261672d7261742f6f702f316b6e6f64655f75706461746582656d656d5f318769496e76617269616e74657469746c6564626f64796468696768656167656e748261616162747b22736368656d615f76657273696f6e223a317d",
            ),
            ("node_status", "836c7261672d7261742f6f702f316b6e6f64655f73746174757382656d656d5f31686f62736f6c657465"),
            (
                "edge_add",
                "836c7261672d7261742f6f702f3168656467655f61646486676d656d5f7372636a646570656e64735f6f6e667265706f5f74646e6f6465676d656d5f647374667265706f5f6f",
            ),
            ("edge_remove", "836c7261672d7261742f6f702f316b656467655f72656d6f766569656467656b65795f31"),
            (
                "rebind",
                "836c7261672d7261742f6f702f3166726562696e648269656467656b65795f3183667265706f5f74676d656d5f6473746763757272656e74",
            ),
            (
                "node_anchors",
                "836c7261672d7261742f6f702f316c6e6f64655f616e63686f727382656d656d5f31828e6673796d626f6c78186372617465732f782f7372632f6c69622e72733a3a72756e736372617465732f782f7372632f6c69622e72730a1466633066666565f6f6f61b0000018bcfe568006866756e6374696f6e6335696769736369702d7275737463302e338e67747261636b6572736769746875623a6f776e65722f7265706f2337f6f6f6f6666769746875626a6f776e65722f7265706f61371b0000018bcfe56801f6f6f6f6",
            ),
            ("snapshot", "836c7261672d7261742f6f702f3168736e617073686f74f6"),
        ];
        let got_refs: Vec<(&str, &str)> =
            got.iter().map(|(name, bytes)| (*name, bytes.as_str())).collect();
        assert_eq!(got_refs, want);
    }

    #[test]
    fn every_variant_round_trips() {
        for (name, op) in every_variant() {
            let bytes = encode(&op);
            match decode(&bytes).unwrap() {
                DecodedOp::Known(decoded) => {
                    assert_eq!(decoded, op, "{name} must round-trip through encode/decode");
                },
                DecodedOp::Unknown { tag, .. } => {
                    panic!("{name} decoded as Unknown(tag={tag}), expected Known");
                },
            }
        }
    }

    /// An anchor set is a SET: the wire bytes must not depend on the order the caller assembled it
    /// in, or two devices holding the same bindings would author byte-different ops.
    #[test]
    fn node_anchors_encodes_in_identity_order_whatever_the_input_order() {
        let sorted = MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors: anchors() };
        let mut reversed_anchors = anchors();
        reversed_anchors.reverse();
        let reversed =
            MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors: reversed_anchors };
        assert_eq!(encode(&sorted), encode(&reversed));
    }

    /// Two anchors naming ONE row: the op cannot say which wins, so it is rejected outright rather
    /// than silently deduped. Encode does not dedup either, so such an op cannot round-trip.
    #[test]
    fn node_anchors_rejects_a_duplicate_row_identity() {
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("node_anchors").unwrap();
            enc.array(2).unwrap();
            enc.str("mem_1").unwrap();
            enc.array(2).unwrap();
            raw_anchor(enc, "symbol", "same");
            raw_anchor(enc, "symbol", "same");
        });

        let err = decode(&buf).unwrap_err().to_string();
        assert!(err.contains("strictly increasing"), "{err}");
    }

    /// The round-trip direction the hand-rolled duplicate test cannot reach: an op BUILT with two
    /// anchors for one row must not survive its own encode. Pins that `decode`'s strict ordering is
    /// what rejects it — the `encode == bytes` check cannot, since the stable sort re-encodes such
    /// a payload to the very bytes it came from.
    #[test]
    fn an_op_carrying_a_duplicate_identity_cannot_round_trip() {
        let mut duplicated = anchors();
        duplicated.push(duplicated[0].clone());
        let op = MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors: duplicated };

        let err = decode(&encode(&op)).unwrap_err().to_string();
        assert!(err.contains("strictly increasing"), "{err}");
    }

    /// The authoring-side twin: such an op is refused BEFORE it is signed, so it never becomes a
    /// permanent entry whose anchors every peer silently drops at projection.
    #[test]
    fn wire_limits_reject_what_decode_cannot_read_back() {
        let mut duplicated = anchors();
        duplicated.push(duplicated[0].clone());
        assert!(!within_wire_limits(&MemoryOp::NodeAnchors {
            node_id: NodeId::from("mem_1"),
            anchors: duplicated,
        }));

        let over_cap: Vec<PortableAnchor> = (0..=MAX_ANCHORS_PER_OP)
            .map(|index| PortableAnchor {
                binding_id: format!("id_{index:03}"),
                ..anchors()[0].clone()
            })
            .collect();
        assert!(!within_wire_limits(&MemoryOp::NodeAnchors {
            node_id: NodeId::from("mem_1"),
            anchors: over_cap,
        }));

        assert!(within_wire_limits(&MemoryOp::NodeAnchors {
            node_id: NodeId::from("mem_1"),
            anchors: anchors(),
        }));
        assert!(every_variant().iter().all(|(_, op)| within_wire_limits(op)));
    }

    #[test]
    fn node_anchors_rejects_an_unsorted_set() {
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("node_anchors").unwrap();
            enc.array(2).unwrap();
            enc.str("mem_1").unwrap();
            enc.array(2).unwrap();
            raw_anchor(enc, "tracker", "b");
            raw_anchor(enc, "symbol", "a");
        });

        let err = decode(&buf).unwrap_err().to_string();
        assert!(err.contains("strictly increasing"), "{err}");
    }

    /// The cap is judged from the array HEADER, before a single element is decoded — an
    /// attacker-controlled count must not buy work (or an allocation) proportional to itself. The
    /// envelope below declares an over-cap count and then carries NOTHING, so only a header-first
    /// check can produce the cap error rather than a truncation error.
    #[test]
    fn node_anchors_rejects_an_over_cap_count_from_the_header() {
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("node_anchors").unwrap();
            enc.array(2).unwrap();
            enc.str("mem_1").unwrap();
            enc.array(MAX_ANCHORS_PER_OP as u64 + 1).unwrap();
        });

        let err = decode(&buf).unwrap_err().to_string();
        assert!(err.contains(&format!("over the {MAX_ANCHORS_PER_OP} limit")), "{err}");
    }

    /// The off-by-one guard the sibling `row_op` cap carries: a set exactly AT the limit is legal,
    /// so the cap can never be read as "fewer than".
    #[test]
    fn an_anchor_count_at_the_cap_is_not_refused_by_the_cap() {
        let anchors: Vec<PortableAnchor> = (0..MAX_ANCHORS_PER_OP)
            .map(|index| PortableAnchor {
                binding_kind: "symbol".to_string(),
                // Zero-padded so identity order matches numeric order — an unpadded `10` would sort
                // before `9` and trip the strictly-increasing check for reasons unrelated to the
                // cap.
                binding_id: format!("id_{index:03}"),
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                tracker: None,
                project: None,
                item_key: None,
                created_at_ms: 1,
                symbol_kind: None,
                signature_hash: None,
                moniker_tool: None,
                moniker_tool_version: None,
            })
            .collect();
        let op = MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors };
        assert_eq!(decode(&encode(&op)).unwrap(), DecodedOp::Known(op));
    }

    /// An anchor set is legitimately empty for an unanchored memory; that must be a valid op, not a
    /// degenerate one, so the drain can distinguish "no bindings" from "no snapshot".
    #[test]
    fn node_anchors_accepts_an_empty_set() {
        let op = MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors: Vec::new() };
        let decoded = decode(&encode(&op)).unwrap();
        assert_eq!(decoded, DecodedOp::Known(op));
    }

    #[test]
    fn unknown_op_kind_is_retained_not_projected() {
        // A future op kind: a well-formed `[domain, "future_op", payload]` envelope. It must decode
        // to `Unknown` with the tag + the ORIGINAL bytes retained (re-foldable after an upgrade),
        // never an error and never a silent drop.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("future_op").unwrap();
            enc.u64(42).unwrap();
        });

        match decode(&buf).unwrap() {
            DecodedOp::Unknown { tag, raw } => {
                assert_eq!(tag, "future_op");
                assert_eq!(raw, buf, "the raw bytes are retained verbatim for re-fold");
            },
            DecodedOp::Known(op) => panic!("expected Unknown, got Known({op:?})"),
        }
    }

    #[test]
    fn unknown_relation_token_decodes_to_unknown() {
        // A future edge relation inside an otherwise-valid `edge_add` → the whole op is kept
        // opaque.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("edge_add").unwrap();
            enc.array(6).unwrap();
            enc.str("mem_src").unwrap();
            enc.str("mentors").unwrap(); // not a known EdgeRelation token
            enc.str("repo_t").unwrap();
            enc.str("node").unwrap();
            enc.str("mem_dst").unwrap();
            enc.str("repo_o").unwrap();
        });

        match decode(&buf).unwrap() {
            DecodedOp::Unknown { tag, raw } => {
                assert_eq!(tag, "edge_add");
                assert_eq!(raw, buf);
            },
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn unknown_status_token_decodes_to_unknown() {
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("node_status").unwrap();
            enc.array(2).unwrap();
            enc.str("mem_1").unwrap();
            enc.str("archived").unwrap(); // not a known NodeStatus token
        });

        match decode(&buf).unwrap() {
            DecodedOp::Unknown { tag, .. } => assert_eq!(tag, "node_status"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn wrong_domain_tag_is_a_hard_error() {
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str("rag-rat/op/2").unwrap();
            enc.str("snapshot").unwrap();
            enc.null().unwrap();
        });
        assert!(decode(&buf).is_err(), "a bumped domain version must not silently decode");
    }

    #[test]
    fn structurally_malformed_bytes_are_a_hard_error() {
        // A known kind whose payload array has the wrong arity is corruption, not forward-compat.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("node_status").unwrap();
            enc.array(1).unwrap(); // node_status wants a 2-element payload
            enc.str("mem_1").unwrap();
        });
        assert!(decode(&buf).is_err());
        // Not even a CBOR array.
        assert!(decode(&[0x00]).is_err());
    }

    #[test]
    fn trailing_bytes_after_an_op_are_rejected() {
        // A complete, valid op followed by extra CBOR is not a canonical envelope — accepting it
        // would make the retained bytes differ from what `encode` produces (wire-identity drift).
        let mut buf = encode(&MemoryOp::Snapshot);
        buf.push(0x00); // a stray trailing CBOR unsigned 0
        assert!(decode(&buf).is_err(), "trailing bytes must be rejected");
    }

    #[test]
    fn truncated_unknown_kind_payload_is_rejected() {
        // A future op kind whose declared payload array is short is corruption, not a retainable
        // opaque op — the skip-and-verify path rejects it.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("future_op").unwrap();
            enc.array(3).unwrap(); // claims three elements...
            enc.str("only_one").unwrap(); // ...supplies one
        });
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn truncated_edge_add_is_rejected_even_with_an_unknown_relation() {
        // The full payload is read before the relation is judged, so a short `edge_add` hard-errors
        // rather than being silently accepted as Unknown.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("edge_add").unwrap();
            enc.array(6).unwrap(); // claims six...
            enc.str("mem_src").unwrap();
            enc.str("mentors").unwrap(); // unknown relation
            enc.str("repo_t").unwrap(); // ...supplies three
        });
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn non_canonical_tag_order_is_rejected() {
        // Tags out of canonical (sorted) order re-encode differently → rejected. Otherwise the same
        // logical op would have two accepted wire representations under one signature.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("node_create").unwrap();
            enc.array(2).unwrap();
            enc.str("mem_1").unwrap();
            enc.array(7).unwrap();
            enc.str("Invariant").unwrap();
            enc.str("title").unwrap();
            enc.str("body").unwrap();
            enc.str("high").unwrap();
            enc.str("agent").unwrap();
            enc.array(2).unwrap();
            enc.str("b").unwrap(); // out of sorted order
            enc.str("a").unwrap();
            enc.null().unwrap(); // payload
        });
        assert!(decode(&buf).is_err(), "unsorted tags are non-canonical");
    }

    #[test]
    fn duplicate_tags_are_deduped_and_rejected_on_the_wire() {
        // Tags are a SET: encode drops duplicates, so a dup and its deduped form encode
        // identically.
        let mut dup = content();
        dup.tags = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let mut deduped = content();
        deduped.tags = vec!["a".to_string(), "b".to_string()];
        assert_eq!(encode(&node_create(dup)), encode(&node_create(deduped)));
        // And a hand-built dup-tag envelope is non-canonical (re-encode differs) → rejected.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("node_create").unwrap();
            enc.array(2).unwrap();
            enc.str("mem_1").unwrap();
            enc.array(7).unwrap();
            enc.str("Invariant").unwrap();
            enc.str("title").unwrap();
            enc.str("body").unwrap();
            enc.str("high").unwrap();
            enc.str("agent").unwrap();
            enc.array(2).unwrap();
            enc.str("a").unwrap();
            enc.str("a").unwrap(); // duplicate
            enc.null().unwrap();
        });
        assert!(decode(&buf).is_err(), "duplicate tags are non-canonical on the wire");
    }

    #[test]
    fn overlong_length_header_is_rejected() {
        // Splice the canonical snapshot's inline domain-length header (`0x6c`, len 12) into the
        // non-minimal 1-byte-length form (`0x78 0x0c`) — same string, non-canonical CBOR. `encode`
        // only ever emits the minimal header, so `decode` must reject the overlong input.
        let canonical = encode(&MemoryOp::Snapshot);
        assert_eq!(canonical[1], 0x6c, "domain length header is the inline minimal form");
        let mut overlong = vec![canonical[0], 0x78, 0x0c];
        overlong.extend_from_slice(&canonical[2..]);
        assert!(decode(&overlong).is_err(), "an overlong length header is non-canonical");
    }

    #[test]
    fn a_huge_declared_tag_count_does_not_preallocate() {
        // A tiny payload declaring an enormous tag-array length must return a decode error, never
        // OOM or panic — the decoder grows with real bytes, so it errors at the first missing tag.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("node_create").unwrap();
            enc.array(2).unwrap();
            enc.str("mem_1").unwrap();
            enc.array(7).unwrap();
            enc.str("Invariant").unwrap();
            enc.str("title").unwrap();
            enc.str("body").unwrap();
            enc.str("high").unwrap();
            enc.str("agent").unwrap();
            enc.array(u64::MAX).unwrap(); // absurd declared tag count, with no elements following
        });
        assert!(decode(&buf).is_err(), "a bogus tag count must error, not allocate");
    }

    #[test]
    fn non_canonical_unknown_op_bytes_are_rejected() {
        // A future op KIND whose payload uses a non-minimal length header is non-canonical and must
        // be rejected even though the kind is unknown — every RETAINED op is one canonical wire
        // form.
        let mut overlong = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("future_op").unwrap();
        });
        // Payload = text "x" with a non-minimal 1-byte length header (`0x78 0x01`) not inline
        // `0x61`.
        overlong.extend_from_slice(&[0x78, 0x01, b'x']);
        assert!(decode(&overlong).is_err(), "a non-canonical unknown payload is rejected");

        // The canonical (inline) form of the same unknown op IS retained.
        let mut canonical = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("future_op").unwrap();
        });
        canonical.extend_from_slice(&[0x61, b'x']);
        assert!(matches!(decode(&canonical).unwrap(), DecodedOp::Unknown { .. }));
    }

    #[test]
    fn deeply_nested_unknown_op_is_rejected() {
        // Unbounded recursion in the canonical validator would overflow the stack; a pathologically
        // nested payload must return a decode error instead.
        let mut buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("future_op").unwrap();
        });
        // Payload = MAX_CBOR_DEPTH+2 nested single-element arrays (0x81) around a uint-0 leaf.
        buf.extend(std::iter::repeat_n(0x81u8, cbor::MAX_CBOR_DEPTH + 2));
        buf.push(0x00);
        assert!(decode(&buf).is_err(), "excessive CBOR nesting is rejected, not overflowed");
    }

    #[test]
    fn invalid_utf8_text_in_unknown_op_is_rejected() {
        // A future op whose payload TEXT string is not valid UTF-8 (`0x61 0xff`) must be rejected:
        // a later decoder reads it with `d.str()` (UTF-8 required), so it is not
        // re-foldable and can't be retained as "canonical".
        let mut buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("future_op").unwrap();
        });
        buf.extend_from_slice(&[0x61, 0xff]); // text, length 1, content byte 0xff (invalid UTF-8)
        assert!(decode(&buf).is_err(), "invalid UTF-8 text in an unknown op is rejected");
    }

    #[test]
    fn indefinite_length_unknown_op_is_rejected() {
        // An indefinite-length payload (`0x9f … 0xff`) is never canonical CBOR.
        let mut buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("future_op").unwrap();
        });
        buf.extend_from_slice(&[0x9f, 0xff]); // indefinite-length array, immediately closed
        assert!(decode(&buf).is_err());
    }

    /// Wrap raw `payload` CBOR bytes as the 3rd element of an unknown-KIND op envelope, so the
    /// retention path's canonical-CBOR validator is what judges `payload`.
    fn unknown_op_with_payload(payload: &[u8]) -> Vec<u8> {
        let mut buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("future_op").unwrap();
        });
        buf.extend_from_slice(payload);
        buf
    }

    #[test]
    fn canonical_validator_accepts_every_canonical_cbor_shape() {
        // One canonical item per CBOR major type / integer width → each retained as Unknown.
        let shapes: &[&[u8]] = &[
            &[0x41, 0x00],                                           // byte string, len 1
            &[0xa1, 0x61, b'a', 0x00],                               // map {"a": 0} (single key)
            &[0xc0, 0x00],                                           // tag(0) wrapping uint 0
            &[0xf5],                                                 // simple value: true
            &[0x19, 0x01, 0x00],                                     // uint 256 (minimal 2-byte)
            &[0x1a, 0x00, 0x01, 0x00, 0x00],                         // uint 65536 (minimal 4-byte)
            &[0x1b, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00], // uint 2^32 (minimal 8-byte)
        ];
        for payload in shapes {
            let bytes = unknown_op_with_payload(payload);
            assert!(
                matches!(decode(&bytes).unwrap(), DecodedOp::Unknown { .. }),
                "canonical payload {payload:02x?} should be retained as Unknown",
            );
        }
    }

    #[test]
    fn canonical_validator_rejects_every_non_canonical_cbor_shape() {
        let shapes: &[&[u8]] = &[
            &[0xa2, 0x61, b'b', 0x00, 0x61, b'a', 0x00], // map keys OUT of order ("b" then "a")
            &[0xa2, 0x61, b'a', 0x00, 0x61, b'a', 0x01], // DUPLICATE map key "a"
            &[0x19, 0x00, 0xff],                         // uint 255 in a non-minimal 2-byte header
            &[0x1a, 0x00, 0x00, 0x00, 0xff],             // uint 255 in a non-minimal 4-byte header
            &[0x1b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff], // …non-minimal 8-byte header
            &[0x62, b'a'],                               /* text claims length 2, only 1 byte
                                                          * present */
        ];
        for payload in shapes {
            let bytes = unknown_op_with_payload(payload);
            assert!(
                decode(&bytes).is_err(),
                "non-canonical payload {payload:02x?} should be rejected"
            );
        }
    }

    #[test]
    fn trailing_bytes_after_an_unknown_op_are_rejected() {
        // The Known path rejects trailing bytes via `encode == bytes`; the Unknown path must reject
        // them via the canonical validator's own no-trailing-bytes check.
        let mut buf = unknown_op_with_payload(&[0x00]); // canonical unknown op (payload = uint 0)
        buf.push(0x00); // a stray trailing CBOR byte
        assert!(decode(&buf).is_err(), "trailing bytes after an unknown op are rejected");
    }

    #[test]
    fn a_non_null_snapshot_payload_is_rejected() {
        // `snapshot` is strictly null; a future manifest-carrying snapshot is a NEW kind, not a
        // non-null payload here — an old binary must reject, never misread.
        let buf = raw_envelope(|enc| {
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("snapshot").unwrap();
            enc.u64(1).unwrap(); // non-null payload
        });
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn node_status_tokens_match_the_validated_memory_status_set() {
        // The op-log status tokens ARE the persisted `repo_memories.status` set — pin them against
        // the write-path validator so the two can never drift, and pin the exact db strings.
        for (status, token) in [
            (NodeStatus::Active, "active"),
            (NodeStatus::Stale, "stale"),
            (NodeStatus::Obsolete, "obsolete"),
            (NodeStatus::Rejected, "rejected"),
        ] {
            assert_eq!(status.as_db_str(), token);
            assert_eq!(NodeStatus::from_db_str(token), Some(status));
            memory::validate_status(token)
                .unwrap_or_else(|_| panic!("`{token}` must be a valid memory status"));
        }
        assert_eq!(NodeStatus::from_db_str("archived"), None);
        assert_eq!(NodeStatus::default(), NodeStatus::Active);
    }

    #[test]
    fn edge_key_matches_the_live_edge_table_derivation() {
        // The op-log derives `edge_key` through the same helper the live table uses, so an add via
        // the op-log and a direct insert content-address to the SAME key.
        let spec = edge_spec();
        let expected = memory::edge_key(
            spec.source_node_id.as_str(),
            spec.relation.as_db_str(),
            &spec.target_kind,
            &spec.target_anchor,
        );
        assert_eq!(spec.edge_key().as_str(), expected);
    }

    #[test]
    fn payload_absent_differs_from_payload_present() {
        // The `null` vs text encoding keeps a no-payload node distinct from one with a payload.
        let mut without = content();
        without.payload = None;
        assert_ne!(encode(&node_create(without)), encode(&node_create(content())));
    }

    fn node_create(content: NodeContent) -> MemoryOp {
        MemoryOp::NodeCreate { node_id: NodeId::from("mem_1"), content }
    }
}
