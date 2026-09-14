//! The table→log sync engine's row op + its canonical CBOR wire form.
//!
//! A [`RowOp`] is one replicated row mutation on a syncable table: an `Upsert` (a row identity plus
//! its synced cells), a `Remove` (a row identity), or a `Restate` (a batch of earlier deletes at
//! their original identities, re-carried so the entries that first stated them can be reclaimed).
//! It mirrors [`super::super::op`]'s discipline —
//! a domain-tagged, definite-length, deterministic envelope `[domain, op-kind, payload]`, versioned
//! (`"rag-rat/table-op/1"`) so a future format can never collide — but its vocabulary is
//! **self-describing**: the op carries the table name, the column names, and each value's type
//! (through CBOR's own type system), because the applier's registry lives only in code and never
//! travels. That is what lets a peer store-and-relay an op for a table it doesn't know.
//!
//! [`decode`] returns a [`DecodedRowOp`]: a recognized op-kind is `Known`; a future op-kind this
//! binary doesn't recognize decodes to `Unknown` with its raw bytes retained (never applied) so a
//! later binary can re-fold it — the same forward-compat seam `op` uses. A known op has exactly one
//! accepted encoding (`encode(decode(bytes)) == bytes`); an unknown op must still be exactly one
//! canonical CBOR item. Structurally-corrupt bytes are a hard error, distinct from the seam.

use minicbor::Encoder;
use minicbor::data::Type;
use minicbor::decode::{Decoder, Error as CborError};

use crate::cbor::{self, INFALLIBLE, VecEncoder};
use crate::op::DeviceFingerprint;

/// Domain tag + version, the envelope's first element. Bump the version to evolve the wire format
/// deliberately (an old binary then rejects the new domain rather than misreading it).
const DOMAIN: &str = "rag-rat/table-op/1";

/// One replicated cell: a column name and its typed value. Cells within an op are ordered by column
/// name and unique — the canonical form the wire pins.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub column: String,
    pub value: TypedValue,
}

/// A self-describing cell value. The variant IS the wire type (through CBOR's own type system), so
/// the applier can check it against the column's registry-declared type and quarantine a mismatch.
/// `I64` covers every SQLite `INTEGER`; `Text`/`Blob`/`Bool`/`Null` map to `TEXT`/`BLOB`/a 0/1
/// integer flag / SQL `NULL`.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedValue {
    Null,
    Bool(bool),
    I64(i64),
    Text(String),
    Blob(Vec<u8>),
}

/// A row mutation on a syncable table. The op-log entry that carries it supplies the ordering
/// metadata (lamport + device); these bytes freeze only the mutation itself, as `op` does.
#[derive(Debug, Clone, PartialEq)]
pub enum RowOp {
    /// Insert-or-update the row identified by `pk` with `cells` (the synced columns). The applier
    /// resolves insert-vs-update by row existence, and the WHOLE row moves as a unit under its
    /// write clock — there is no per-column merge.
    ///
    /// `spec_version` states which synced column set the author wrote against, so a receiver can
    /// tell an OLDER producer's complete row (fill the columns it predates from their declared
    /// defaults) from a NEWER producer's partial one (park until this binary catches up).
    Upsert { table: String, spec_version: u32, pk: Vec<TypedValue>, cells: Vec<Cell> },
    /// Delete the row identified by `pk` (and its LWW clock rows).
    ///
    /// `spec_version` is carried for wire symmetry and diagnostics but NOT acted on: a remove names
    /// only the row identity, so no column set is involved and no default can apply. Gating a
    /// deletion on a version skew would delay it for no benefit.
    Remove { table: String, spec_version: u32, pk: Vec<TypedValue> },
    /// Re-state a batch of earlier deletes at their ORIGINAL identities (#1295). The signer asserts
    /// nothing a `Remove` at its tail could not, and strictly less: each delete settles under LWW
    /// exactly as the entry that first stated it did, so a newer write to the row still wins.
    /// What the restatement changes is delivery — the signer's chain now carries these deletes at
    /// this entry, so the entries that carried them before can be compacted away.
    ///
    /// `spec_version` is carried like `Remove`'s and never gated on. `deletes` is canonical:
    /// sorted by row identity, unique, non-empty; every `lamport` must be strictly below the
    /// carrying entry's own, which the applier checks (the wire cannot).
    Restate { table: String, spec_version: u32, deletes: Vec<StatedDelete> },
}

/// One delete a [`RowOp::Restate`] re-carries: the row identity and the `(device, lamport)` the
/// delete was originally signed at, which is the identity it competes under.
#[derive(Debug, Clone, PartialEq)]
pub struct StatedDelete {
    pub pk: Vec<TypedValue>,
    pub device: DeviceFingerprint,
    pub lamport: u64,
}

impl RowOp {
    /// The table this op mutates.
    pub fn table(&self) -> &str {
        match self {
            Self::Upsert { table, .. }
            | Self::Remove { table, .. }
            | Self::Restate { table, .. } => table,
        }
    }

    /// Every row identity the op names: one for an `Upsert` or `Remove`, each stated delete's for
    /// a `Restate`.
    pub fn pks(&self) -> impl Iterator<Item = &[TypedValue]> {
        let (one, many): (Option<&[TypedValue]>, &[StatedDelete]) = match self {
            Self::Upsert { pk, .. } | Self::Remove { pk, .. } => (Some(pk), &[]),
            Self::Restate { deletes, .. } => (None, deletes),
        };
        one.into_iter().chain(many.iter().map(|delete| delete.pk.as_slice()))
    }

    /// The synced column set this op was authored against.
    pub fn spec_version(&self) -> u32 {
        match self {
            Self::Upsert { spec_version, .. }
            | Self::Remove { spec_version, .. }
            | Self::Restate { spec_version, .. } => *spec_version,
        }
    }

    /// The envelope's op-kind tag (element 1). Stable wire tokens — a rename is a format change.
    fn kind_tag(&self) -> &'static str {
        match self {
            Self::Upsert { .. } => "upsert",
            Self::Remove { .. } => "remove",
            Self::Restate { .. } => "restate",
        }
    }
}

/// The outcome of decoding one row-op envelope. `Unknown` is the forward-compat seam: an op-kind
/// this binary doesn't recognize is kept opaque (raw bytes RETAINED) rather than dropped or
/// applied, so a later binary can re-fold the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedRowOp {
    Known(RowOp),
    Unknown { tag: String, raw: Vec<u8> },
}

/// Encode one row op to canonical CBOR: `[domain, op-kind, payload]`, definite lengths throughout,
/// deterministic. Cells are emitted sorted by column name so the bytes are stable regardless of the
/// producer's in-memory order.
pub fn encode(op: &RowOp) -> Vec<u8> {
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
fn encode_payload(enc: &mut VecEncoder<'_>, op: &RowOp) {
    match op {
        RowOp::Upsert { table, spec_version, pk, cells } => {
            enc.array(4).expect(INFALLIBLE);
            enc.str(table).expect(INFALLIBLE);
            enc.u32(*spec_version).expect(INFALLIBLE);
            encode_values(enc, pk);
            encode_cells(enc, cells);
        },
        RowOp::Remove { table, spec_version, pk } => {
            enc.array(3).expect(INFALLIBLE);
            enc.str(table).expect(INFALLIBLE);
            enc.u32(*spec_version).expect(INFALLIBLE);
            encode_values(enc, pk);
        },
        RowOp::Restate { table, spec_version, deletes } => {
            enc.array(3).expect(INFALLIBLE);
            enc.str(table).expect(INFALLIBLE);
            enc.u32(*spec_version).expect(INFALLIBLE);
            encode_deletes(enc, deletes);
        },
    }
}

/// Encode stated deletes sorted by row identity (the canonical order), each as a
/// `[pk, device, lamport]` triple. Sorted by the identity's canonical bytes — the same order
/// [`row_pk_string`] induces — so the bytes are stable regardless of the producer's order.
fn encode_deletes(enc: &mut VecEncoder<'_>, deletes: &[StatedDelete]) {
    let mut sorted: Vec<&StatedDelete> = deletes.iter().collect();
    sorted.sort_by_cached_key(|delete| row_pk_string(&delete.pk));
    enc.array(sorted.len() as u64).expect(INFALLIBLE);
    for delete in sorted {
        enc.array(3).expect(INFALLIBLE);
        encode_values(enc, &delete.pk);
        enc.bytes(&delete.device.to_bytes()).expect(INFALLIBLE);
        enc.u64(delete.lamport).expect(INFALLIBLE);
    }
}

fn encode_values(enc: &mut VecEncoder<'_>, values: &[TypedValue]) {
    enc.array(values.len() as u64).expect(INFALLIBLE);
    for value in values {
        encode_value(enc, value);
    }
}

/// Encode cells sorted by column name (the canonical order), each as a `[column, value]` pair.
fn encode_cells(enc: &mut VecEncoder<'_>, cells: &[Cell]) {
    let mut sorted: Vec<&Cell> = cells.iter().collect();
    sorted.sort_by(|a, b| a.column.cmp(&b.column));
    enc.array(sorted.len() as u64).expect(INFALLIBLE);
    for cell in sorted {
        enc.array(2).expect(INFALLIBLE);
        enc.str(&cell.column).expect(INFALLIBLE);
        encode_value(enc, &cell.value);
    }
}

/// Encode one typed value as its natural CBOR type — the value's type is self-describing on the
/// wire.
fn encode_value(enc: &mut VecEncoder<'_>, value: &TypedValue) {
    match value {
        TypedValue::Null => {
            enc.null().expect(INFALLIBLE);
        },
        TypedValue::Bool(b) => {
            enc.bool(*b).expect(INFALLIBLE);
        },
        TypedValue::I64(n) => {
            enc.i64(*n).expect(INFALLIBLE);
        },
        TypedValue::Text(s) => {
            enc.str(s).expect(INFALLIBLE);
        },
        TypedValue::Blob(b) => {
            enc.bytes(b).expect(INFALLIBLE);
        },
    }
}

/// Decode one row-op envelope. A recognized op → `Known`; a future op-kind → `Unknown` (raw bytes
/// retained); structurally-invalid CBOR or a wrong/absent domain tag → `Err`.
pub fn decode(bytes: &[u8]) -> anyhow::Result<DecodedRowOp> {
    decode_envelope(bytes).map_err(|err| anyhow::anyhow!("row-op decode failed: {err}"))
}

fn decode_envelope(bytes: &[u8]) -> Result<DecodedRowOp, CborError> {
    let mut d = Decoder::new(bytes);
    cbor::expect_array(&mut d, 3)?;
    cbor::expect_domain(&mut d, DOMAIN)?;
    let kind = d.str()?.to_string();
    let known = match kind.as_str() {
        "upsert" => {
            cbor::expect_array(&mut d, 4)?;
            let table = d.str()?.to_string();
            let spec_version = d.u32()?;
            let pk = decode_values(&mut d)?;
            let cells = decode_cells(&mut d)?;
            Some(RowOp::Upsert { table, spec_version, pk, cells })
        },
        "remove" => {
            cbor::expect_array(&mut d, 3)?;
            let table = d.str()?.to_string();
            let spec_version = d.u32()?;
            let pk = decode_values(&mut d)?;
            Some(RowOp::Remove { table, spec_version, pk })
        },
        "restate" => {
            cbor::expect_array(&mut d, 3)?;
            let table = d.str()?.to_string();
            let spec_version = d.u32()?;
            let deletes = decode_deletes(&mut d)?;
            Some(RowOp::Restate { table, spec_version, deletes })
        },
        // A future op-kind this binary doesn't know — retained opaque, canonicity checked below.
        _ => None,
    };
    match known {
        Some(op) => {
            // Byte-canonical identity: a known op has exactly ONE accepted encoding (sorted cells,
            // minimal headers, definite lengths, no trailing). Re-encoding and demanding equality
            // rejects every alternate representation, so a later signature over these bytes is
            // unambiguous — the same rule `op::decode` enforces.
            if encode(&op) != bytes {
                return Err(CborError::message("non-canonical row-op encoding"));
            }
            Ok(DecodedRowOp::Known(op))
        },
        None => {
            // An unknown op is retained opaque (we can't re-encode it), but must still be exactly
            // one canonical CBOR item with no trailing bytes, or a future binary that
            // learns the kind could see two wire forms of one op.
            cbor::require_canonical_cbor(bytes)?;
            Ok(DecodedRowOp::Unknown { tag: kind, raw: bytes.to_vec() })
        },
    }
}

/// Upper bound on a row op's pk-value / cell count. A real row identity is a handful of columns and
/// a real row is dozens; a larger declared count is malformed or hostile. Enforced BEFORE any
/// capacity allocation so a tiny signed payload with an enormous CBOR array header cannot OOM the
/// process (the length header is attacker-controlled and pre-sizing on it is the vector).
const MAX_ROW_OP_ELEMENTS: u64 = 4096;

/// Read a definite array length, rejecting one above the protocol cap before it is used to size an
/// allocation.
fn capped_len(d: &mut Decoder<'_>) -> Result<usize, CborError> {
    let len = cbor::expect_definite_len(d)?;
    if len > MAX_ROW_OP_ELEMENTS {
        return Err(CborError::message("row-op array length exceeds the protocol cap"));
    }
    Ok(len as usize)
}

fn decode_values(d: &mut Decoder<'_>) -> Result<Vec<TypedValue>, CborError> {
    let len = capped_len(d)?;
    let mut values = Vec::with_capacity(len);
    for _ in 0..len {
        values.push(decode_value(d)?);
    }
    Ok(values)
}

/// Decode cells, enforcing strictly-ascending unique column names (the canonical order). Rejecting
/// `<=` the previous column catches both unsorted input and a duplicate column in one check.
fn decode_cells(d: &mut Decoder<'_>) -> Result<Vec<Cell>, CborError> {
    let len = capped_len(d)?;
    let mut cells = Vec::with_capacity(len);
    let mut prev: Option<String> = None;
    for _ in 0..len {
        cbor::expect_array(d, 2)?;
        let column = d.str()?.to_string();
        if prev.as_ref().is_some_and(|p| &column <= p) {
            return Err(CborError::message("row-op cells not sorted or duplicated"));
        }
        let value = decode_value(d)?;
        prev = Some(column.clone());
        cells.push(Cell { column, value });
    }
    Ok(cells)
}

/// Decode stated deletes, enforcing strictly-ascending unique row identities (the canonical
/// order) and a non-empty batch: an empty restatement states nothing and has no reason to exist
/// on a chain. The batch length and each identity's arity are both under the element cap.
fn decode_deletes(d: &mut Decoder<'_>) -> Result<Vec<StatedDelete>, CborError> {
    let len = capped_len(d)?;
    if len == 0 {
        return Err(CborError::message("a restate batch names no deletes"));
    }
    let mut deletes = Vec::with_capacity(len);
    let mut prev: Option<String> = None;
    for _ in 0..len {
        cbor::expect_array(d, 3)?;
        let pk = decode_values(d)?;
        let identity = row_pk_string(&pk);
        if prev.as_ref().is_some_and(|p| &identity <= p) {
            return Err(CborError::message("restate deletes not sorted or duplicated"));
        }
        let device = DeviceFingerprint::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "device")?);
        let lamport = d.u64()?;
        prev = Some(identity);
        deletes.push(StatedDelete { pk, device, lamport });
    }
    Ok(deletes)
}

fn decode_value(d: &mut Decoder<'_>) -> Result<TypedValue, CborError> {
    match d.datatype()? {
        Type::Null => {
            d.null()?;
            Ok(TypedValue::Null)
        },
        Type::Bool => Ok(TypedValue::Bool(d.bool()?)),
        Type::U8
        | Type::U16
        | Type::U32
        | Type::U64
        | Type::I8
        | Type::I16
        | Type::I32
        | Type::I64 => Ok(TypedValue::I64(d.i64()?)),
        Type::Bytes => Ok(TypedValue::Blob(d.bytes()?.to_vec())),
        Type::String => Ok(TypedValue::Text(d.str()?.to_string())),
        other =>
            Err(CborError::message(format!("unexpected CBOR type for a row-op value: {other:?}"))),
    }
}

impl StatedDelete {
    /// The bytes this delete adds to a `Restate` payload: its `[pk, device, lamport]` triple.
    fn encoded_len(&self) -> usize {
        let mut buf = Vec::with_capacity(64);
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).expect(INFALLIBLE);
            encode_values(&mut enc, &self.pk);
            enc.bytes(&self.device.to_bytes()).expect(INFALLIBLE);
            enc.u64(self.lamport).expect(INFALLIBLE);
        }
        buf.len()
    }
}

/// `deletes` packed, in the order given, into the fewest `Restate` ops on `table` whose encoded
/// payload stays within `payload_max` bytes and [`MAX_ROW_OP_ELEMENTS`] deletes each: greedy, in
/// order, so a prefix of the input fills the leading batches. Each batch carries the input
/// indices it holds; a delete that does not fit a batch alone is left out and reported, so it can
/// leave its pin standing without stalling the rest.
pub(crate) struct PackedRestates {
    pub batches: Vec<(RowOp, Vec<usize>)>,
    pub unfit: Vec<usize>,
}

pub(crate) fn pack_restates(
    table: &str,
    spec_version: u32,
    deletes: &[StatedDelete],
    payload_max: usize,
) -> PackedRestates {
    let mut packer = RestatePacker::new(table, spec_version, payload_max);
    let mut packed = PackedRestates { batches: Vec::new(), unfit: Vec::new() };
    let mut batch: Vec<usize> = Vec::new();
    let flush = |batch: &mut Vec<usize>, packed: &mut PackedRestates| {
        if batch.is_empty() {
            return;
        }
        let op = RowOp::Restate {
            table: table.to_string(),
            spec_version,
            deletes: batch.iter().map(|&i| deletes[i].clone()).collect(),
        };
        packed.batches.push((op, std::mem::take(batch)));
    };
    for (index, delete) in deletes.iter().enumerate() {
        match packer.push(delete) {
            Placed::Unfit => packed.unfit.push(index),
            Placed::NewBatch => {
                flush(&mut batch, &mut packed);
                batch.push(index);
            },
            Placed::SameBatch => batch.push(index),
        }
    }
    flush(&mut batch, &mut packed);
    packed
}

/// Where the packer put a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placed {
    /// Into the batch being filled.
    SameBatch,
    /// The batch being filled was closed and a new one opened for it.
    NewBatch,
    /// It does not fit a batch alone; left out.
    Unfit,
}

/// The arithmetic behind [`pack_restates`], usable on its own to COUNT the batches a sequence of
/// deletes would need without building them — what the compaction economics ask, once per prefix.
pub(crate) struct RestatePacker {
    /// Everything but the deletes array's own header and elements, which the per-batch
    /// arithmetic adds back: the header grows with the element count (1, 2 or 3 bytes).
    base: usize,
    payload_max: usize,
    batches: usize,
    batch_len: usize,
    batch_bytes: usize,
}

impl RestatePacker {
    pub(crate) fn new(table: &str, spec_version: u32, payload_max: usize) -> Self {
        let empty = RowOp::Restate { table: table.to_string(), spec_version, deletes: vec![] };
        Self {
            base: encode(&empty).len() - 1,
            payload_max,
            batches: 0,
            batch_len: 0,
            batch_bytes: 0,
        }
    }

    fn array_header(n: usize) -> usize {
        if n < 24 {
            1
        } else if n < 256 {
            2
        } else {
            3
        }
    }

    pub(crate) fn push(&mut self, delete: &StatedDelete) -> Placed {
        let len = delete.encoded_len();
        if self.base + Self::array_header(1) + len > self.payload_max {
            return Placed::Unfit;
        }
        let would_be = self.base + Self::array_header(self.batch_len + 1) + self.batch_bytes + len;
        let placed = if self.batch_len == 0 {
            self.batches += 1;
            Placed::NewBatch
        } else if would_be > self.payload_max || self.batch_len as u64 >= MAX_ROW_OP_ELEMENTS {
            self.batches += 1;
            self.batch_len = 0;
            self.batch_bytes = 0;
            Placed::NewBatch
        } else {
            Placed::SameBatch
        };
        self.batch_len += 1;
        self.batch_bytes += len;
        placed
    }

    /// Batches opened so far.
    pub(crate) fn batches(&self) -> usize {
        self.batches
    }
}

/// A stable, opaque string identity for a row's PK tuple — the hex of the canonical CBOR encoding
/// of the `pk` values. Used as the `row_pk` TEXT key in the LWW clock and published-row tables, so
/// two devices agree on a row's identity without sharing local rowids.
pub fn row_pk_string(pk: &[TypedValue]) -> String {
    let mut buf = Vec::with_capacity(32);
    {
        let mut enc = Encoder::new(&mut buf);
        encode_values(&mut enc, pk);
    }
    rag_rat_base::hash::hex_lower(&buf)
}

/// The anti-echo identity of a row: the hex `sha256` of the canonical CBOR of its synced cells
/// (sorted by column). The applier records this after writing a row and the producer recomputes it
/// from the current row — an equal hash means the row already carries the synced state, so it is
/// not re-emitted (the echo-republish guard). Covers ONLY the passed cells (synced columns), so
/// local re-resolution of other columns can never perturb it.
pub(crate) fn cells_hash(cells: &[Cell]) -> String {
    let mut buf = Vec::with_capacity(64);
    {
        let mut enc = Encoder::new(&mut buf);
        encode_cells(&mut enc, cells);
    }
    rag_rat_base::hash::hex_lower(&cbor::sha256(&buf))
}

/// Recover the pk values from a `row_pk` produced by [`row_pk_string`] — the inverse, so the
/// producer can reconstruct a deleted row's identity (present in the published-rows table, absent
/// from the table) to emit a `Remove`.
pub(crate) fn row_pk_values(row_pk: &str) -> anyhow::Result<Vec<TypedValue>> {
    let bytes = rag_rat_base::hash::hex_decode(row_pk)
        .ok_or_else(|| anyhow::anyhow!("bad hex in row_pk"))?;
    let mut d = Decoder::new(&bytes);
    let values =
        decode_values(&mut d).map_err(|err| anyhow::anyhow!("row_pk decode failed: {err}"))?;
    anyhow::ensure!(d.position() == bytes.len(), "trailing bytes in row_pk");
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_upsert() -> RowOp {
        RowOp::Upsert {
            spec_version: 1,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r".to_string()), TypedValue::I64(7)],
            // Deliberately out of column order — encode must canonicalize.
            cells: vec![
                Cell { column: "title".to_string(), value: TypedValue::Text("hi".to_string()) },
                Cell { column: "count".to_string(), value: TypedValue::I64(3) },
                Cell { column: "done".to_string(), value: TypedValue::Bool(true) },
                Cell { column: "note".to_string(), value: TypedValue::Null },
            ],
        }
    }

    #[test]
    fn upsert_round_trips_and_sorts_cells() {
        let op = sample_upsert();
        let bytes = encode(&op);
        let DecodedRowOp::Known(decoded) = decode(&bytes).unwrap() else {
            panic!("known op");
        };
        // Decoded cells are in canonical (sorted) order.
        let RowOp::Upsert { cells, .. } = &decoded else { panic!("upsert") };
        let columns: Vec<&str> = cells.iter().map(|c| c.column.as_str()).collect();
        assert_eq!(columns, ["count", "done", "note", "title"], "cells decode sorted");
        // Re-encoding the decoded op reproduces the exact bytes (canonical identity).
        assert_eq!(encode(&decoded), bytes);
    }

    fn sample_remove() -> RowOp {
        // `spec_version` deliberately differs from the array arity and from the upsert sample, so a
        // field-order swap or a hardcoded version cannot round-trip or match the golden bytes.
        RowOp::Remove {
            spec_version: 7,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r".to_string()), TypedValue::I64(7)],
        }
    }

    #[test]
    fn remove_round_trips() {
        let op = sample_remove();
        let bytes = encode(&op);
        assert_eq!(decode(&bytes).unwrap(), DecodedRowOp::Known(op));
    }

    /// Golden vector for `Remove`, held to the same discipline as [`upsert_golden_vector`].
    ///
    /// A round-trip alone pins NOTHING about the format: `decode(encode(op)) == op` holds under any
    /// symmetric change — reordering the payload, dropping `spec_version`, retyping it — so half
    /// the wire was unprotected while the other half was pinned. #1002 changed this payload's
    /// arity from 2 to 3 and inserted an element in the MIDDLE, which is exactly the kind of
    /// change a round-trip cannot see.
    #[test]
    fn remove_golden_vector() {
        let bytes = encode(&sample_remove());
        assert_eq!(rag_rat_base::hash::hex_lower(&bytes), GOLDEN_REMOVE_HEX);
    }

    fn sample_restate() -> RowOp {
        // Deliberately out of identity order — encode must canonicalize.
        RowOp::Restate {
            spec_version: 3,
            table: "t_demo".to_string(),
            deletes: vec![
                StatedDelete {
                    pk: vec![TypedValue::Text("r".to_string()), TypedValue::I64(9)],
                    device: DeviceFingerprint::from_bytes([0x22; 32]),
                    lamport: 41,
                },
                StatedDelete {
                    pk: vec![TypedValue::Text("r".to_string()), TypedValue::I64(7)],
                    device: DeviceFingerprint::from_bytes([0x11; 32]),
                    lamport: 40,
                },
            ],
        }
    }

    #[test]
    fn restate_round_trips_canonically() {
        let op = sample_restate();
        let bytes = encode(&op);
        let DecodedRowOp::Known(decoded) = decode(&bytes).unwrap() else {
            panic!("known op");
        };
        let RowOp::Restate { deletes, .. } = &decoded else { panic!("restate") };
        let lamports: Vec<u64> = deletes.iter().map(|d| d.lamport).collect();
        assert_eq!(lamports, [40, 41], "deletes decode in identity order");
        assert_eq!(encode(&decoded), bytes, "canonical identity");
        assert_eq!(decoded.pks().count(), 2, "every stated identity is a pk of the op");
    }

    /// Golden vector for `Restate`, held to the same discipline as [`upsert_golden_vector`].
    #[test]
    fn restate_golden_vector() {
        let bytes = encode(&sample_restate());
        assert_eq!(rag_rat_base::hash::hex_lower(&bytes), GOLDEN_RESTATE_HEX);
    }

    /// Hand-encode a restate envelope with `deletes` given as pre-built `[pk, device, lamport]`
    /// triples, past the encoder's sort.
    fn restate_envelope(
        triples: &[(&[TypedValue], u8, u64)],
        declared_len: Option<u64>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("restate").unwrap();
            enc.array(3).unwrap();
            enc.str("t_demo").unwrap();
            enc.u32(1).unwrap();
            enc.array(declared_len.unwrap_or(triples.len() as u64)).unwrap();
            for (pk, device, lamport) in triples {
                enc.array(3).unwrap();
                encode_values(&mut enc, pk);
                enc.bytes(&[*device; 32]).unwrap();
                enc.u64(*lamport).unwrap();
            }
        }
        buf
    }

    #[test]
    fn an_empty_duplicate_or_unsorted_restate_is_rejected() {
        let seven = [TypedValue::I64(7)];
        let nine = [TypedValue::I64(9)];
        let err = decode(&restate_envelope(&[], None)).unwrap_err().to_string();
        assert!(err.contains("names no deletes"), "{err}");
        let err = decode(&restate_envelope(&[(&seven, 1, 1), (&seven, 2, 2)], None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not sorted or duplicated"), "{err}");
        let err = decode(&restate_envelope(&[(&nine, 1, 1), (&seven, 2, 2)], None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not sorted or duplicated"), "{err}");
    }

    #[test]
    fn restate_obeys_the_element_caps_for_the_batch_and_each_pk() {
        // The batch header is capped before it sizes an allocation …
        let err =
            decode(&restate_envelope(&[], Some(MAX_ROW_OP_ELEMENTS + 1))).unwrap_err().to_string();
        assert!(err.contains("exceeds the protocol cap"), "{err}");
        // … and so is each identity's header inside the batch.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("restate").unwrap();
            enc.array(3).unwrap();
            enc.str("t_demo").unwrap();
            enc.u32(1).unwrap();
            enc.array(1).unwrap();
            enc.array(3).unwrap();
            enc.array(MAX_ROW_OP_ELEMENTS + 1).unwrap();
        }
        let err = decode(&buf).unwrap_err().to_string();
        assert!(err.contains("exceeds the protocol cap"), "{err}");
    }

    /// Batches fill greedily in input order under the byte budget; a key that does not fit alone
    /// is reported, not silently dropped or forced.
    #[test]
    fn restate_packing_obeys_the_signed_byte_limit() {
        let key = |n: usize| StatedDelete {
            pk: vec![TypedValue::Text("k".repeat(n))],
            device: DeviceFingerprint::from_bytes([1; 32]),
            lamport: 1,
        };
        let one = key(10).encoded_len();
        let base =
            encode(&RowOp::Restate { table: "t".to_string(), spec_version: 1, deletes: vec![] })
                .len();
        // Room for exactly two ten-byte keys per batch.
        let budget = base + 2 * one;
        let deletes = vec![key(10), key(10), key(10), key(10), key(10), key(200)];
        let packed = pack_restates("t", 1, &deletes, budget);
        let sizes: Vec<usize> = packed.batches.iter().map(|(_, members)| members.len()).collect();
        assert_eq!(sizes, [2, 2, 1]);
        assert_eq!(packed.unfit, [5], "the oversized key is left out");
        for (op, _) in &packed.batches {
            assert!(encode(op).len() <= budget, "every batch fits the budget");
        }
        assert_eq!(packed.batches[0].1, [0, 1], "batches keep input order");
    }

    #[test]
    fn a_duplicate_or_unsorted_column_is_rejected() {
        // Two cells with the same column name — hand-encode past the encoder's sort so the bytes
        // reach the decoder unsorted/duplicated.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("upsert").unwrap();
            enc.array(4).unwrap();
            enc.str("t_demo").unwrap();
            enc.u32(1).unwrap(); // spec_version
            enc.array(0).unwrap(); // empty pk
            enc.array(2).unwrap();
            enc.array(2).unwrap();
            enc.str("a").unwrap();
            enc.i64(1).unwrap();
            enc.array(2).unwrap();
            enc.str("a").unwrap(); // duplicate column
            enc.i64(2).unwrap();
        }
        // Assert the REASON, not merely `is_err()`. A hand-built fixture drifts out of shape
        // whenever the payload arity changes, and a bare `is_err()` then passes on the arity gate
        // while the rule it names goes untested — which is what happened when #1002 widened this
        // payload from 3 elements to 4.
        let err = decode(&buf).expect_err("a duplicate column is not canonical").to_string();
        assert!(err.contains("not sorted or duplicated"), "rejected for the stated reason: {err}");
    }

    #[test]
    fn an_unknown_kind_is_retained_opaque() {
        // A canonical envelope with a future op-kind decodes to Unknown, bytes retained.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("wholeset").unwrap(); // a future kind this binary doesn't know
            enc.array(0).unwrap();
        }
        assert_eq!(decode(&buf).unwrap(), DecodedRowOp::Unknown {
            tag: "wholeset".to_string(),
            raw: buf.clone()
        },);
    }

    #[test]
    fn a_wrong_domain_is_a_hard_error() {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.str("rag-rat/op/1").unwrap(); // the memory-content domain, not ours
            enc.str("upsert").unwrap();
            enc.array(0).unwrap();
        }
        assert!(decode(&buf).is_err(), "a foreign domain tag is rejected, never misread");
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode(&sample_upsert());
        bytes.push(0x00);
        assert!(decode(&bytes).is_err(), "trailing bytes break canonical identity");
    }

    #[test]
    fn row_pk_string_is_stable_and_distinguishes() {
        let a = row_pk_string(&[TypedValue::Text("r".to_string()), TypedValue::I64(7)]);
        let b = row_pk_string(&[TypedValue::Text("r".to_string()), TypedValue::I64(8)]);
        assert_eq!(a, row_pk_string(&[TypedValue::Text("r".to_string()), TypedValue::I64(7)]));
        assert_ne!(a, b, "a different pk yields a different identity");
    }

    #[test]
    fn an_oversized_array_header_is_rejected_without_allocating() {
        // A pk array header claiming a billion elements with no payload must be rejected by the
        // cap, never pre-sized into an allocation.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("upsert").unwrap();
            enc.array(4).unwrap();
            enc.str("t").unwrap();
            enc.u32(1).unwrap(); // spec_version
            enc.array(1_000_000_000).unwrap(); // absurd declared pk length, no elements follow
        }
        // Assert the CAP is what rejected it. A bare `is_err()` here proves nothing: the array is
        // truncated, so decoding would fail on end-of-input even with no cap at all — the test
        // passed with `MAX_ROW_OP_ELEMENTS` raised to `u64::MAX`. The whole point is that the
        // attacker-controlled length header is refused BEFORE it sizes an allocation.
        let err = decode(&buf).expect_err("an oversized array length is refused").to_string();
        assert!(err.contains("exceeds the protocol cap"), "refused by the cap, not by EOF: {err}");
    }

    #[test]
    fn an_array_length_at_the_cap_is_not_refused_by_the_cap() {
        // The other side of the boundary: the cap must reject only what is ABOVE it, or a
        // legitimate op near the limit would be unparseable. Declared length == the cap,
        // still truncated, so it must fail on the CONTENT rather than the header.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.str(DOMAIN).unwrap();
            enc.str("upsert").unwrap();
            enc.array(4).unwrap();
            enc.str("t").unwrap();
            enc.u32(1).unwrap();
            enc.array(MAX_ROW_OP_ELEMENTS).unwrap();
        }
        let err = decode(&buf).expect_err("still truncated, so it cannot decode").to_string();
        assert!(!err.contains("exceeds the protocol cap"), "the cap is exclusive: {err}");
    }

    /// Golden vector: the exact canonical bytes of `sample_upsert`. A change here is a wire-format
    /// change — bump `DOMAIN` deliberately, never edit this hex to make the test pass.
    ///
    /// This hex WAS edited once, when #1002 added `spec_version` to the payload, WITHOUT a domain
    /// bump. That was sound exactly once and is not a precedent: at the time no released binary
    /// could ever have produced or read a table op — `SYNCABLE_TABLES` had been empty since the
    /// engine was written and no transport carried the format — so `/1` had never existed on a wire
    /// or in a store, and bumping would have asserted a compatibility generation that never was.
    /// That is no longer true. Any FURTHER change to these bytes bumps the domain.
    #[test]
    fn upsert_golden_vector() {
        let bytes = encode(&sample_upsert());
        assert_eq!(rag_rat_base::hash::hex_lower(&bytes), GOLDEN_UPSERT_HEX);
    }

    const GOLDEN_RESTATE_HEX: &str = "83727261672d7261742f7461626c652d6f702f3167726573746174658366745f64656d6f038283826172075820111111111111111111111111111111111111111111111111111111111111111118288382617209582022222222222222222222222222222222222222222222222222222222222222221829";

    const GOLDEN_REMOVE_HEX: &str =
        "83727261672d7261742f7461626c652d6f702f316672656d6f76658366745f64656d6f0782617207";

    const GOLDEN_UPSERT_HEX: &str = "83727261672d7261742f7461626c652d6f702f31667570736572748466745f64656d6f0182617207848265636f756e74038264646f6e65f582646e6f7465f682657469746c65626869";
}
