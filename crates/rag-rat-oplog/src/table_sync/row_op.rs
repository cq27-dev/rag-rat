//! The table→log sync engine's row op + its canonical CBOR wire form.
//!
//! A [`RowOp`] is one replicated row mutation on a syncable table: an `Upsert` (a row identity plus
//! its synced cells) or a `Remove` (a row identity). It mirrors [`super::super::op`]'s discipline —
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

use crate::cbor;

/// Domain tag + version, the envelope's first element. Bump the version to evolve the wire format
/// deliberately (an old binary then rejects the new domain rather than misreading it).
const DOMAIN: &str = "rag-rat/table-op/1";

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible), so every encode step
/// `.expect`s this — mirrors `op`/`content_hash`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

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
    /// resolves insert-vs-update by row existence and merges each cell under per-column LWW.
    Upsert { table: String, pk: Vec<TypedValue>, cells: Vec<Cell> },
    /// Delete the row identified by `pk` (and its LWW clock rows).
    Remove { table: String, pk: Vec<TypedValue> },
}

impl RowOp {
    /// The table this op mutates.
    pub fn table(&self) -> &str {
        match self {
            Self::Upsert { table, .. } | Self::Remove { table, .. } => table,
        }
    }

    /// The op's `pk` cells.
    pub fn pk(&self) -> &[TypedValue] {
        match self {
            Self::Upsert { pk, .. } | Self::Remove { pk, .. } => pk,
        }
    }

    /// The envelope's op-kind tag (element 1). Stable wire tokens — a rename is a format change.
    fn kind_tag(&self) -> &'static str {
        match self {
            Self::Upsert { .. } => "upsert",
            Self::Remove { .. } => "remove",
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

type VecEncoder<'a> = Encoder<&'a mut Vec<u8>>;

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
        RowOp::Upsert { table, pk, cells } => {
            enc.array(3).expect(INFALLIBLE);
            enc.str(table).expect(INFALLIBLE);
            encode_values(enc, pk);
            encode_cells(enc, cells);
        },
        RowOp::Remove { table, pk } => {
            enc.array(2).expect(INFALLIBLE);
            enc.str(table).expect(INFALLIBLE);
            encode_values(enc, pk);
        },
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
            cbor::expect_array(&mut d, 3)?;
            let table = d.str()?.to_string();
            let pk = decode_values(&mut d)?;
            let cells = decode_cells(&mut d)?;
            Some(RowOp::Upsert { table, pk, cells })
        },
        "remove" => {
            cbor::expect_array(&mut d, 2)?;
            let table = d.str()?.to_string();
            let pk = decode_values(&mut d)?;
            Some(RowOp::Remove { table, pk })
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
    let bytes = hex_decode(row_pk)?;
    let mut d = Decoder::new(&bytes);
    let values =
        decode_values(&mut d).map_err(|err| anyhow::anyhow!("row_pk decode failed: {err}"))?;
    anyhow::ensure!(d.position() == bytes.len(), "trailing bytes in row_pk");
    Ok(values)
}

fn hex_decode(text: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(text.len().is_multiple_of(2), "hex string has an odd length");
    (0..text.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&text[i..i + 2], 16).map_err(|err| anyhow::anyhow!("bad hex: {err}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_upsert() -> RowOp {
        RowOp::Upsert {
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

    #[test]
    fn remove_round_trips() {
        let op = RowOp::Remove {
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r".to_string()), TypedValue::I64(7)],
        };
        let bytes = encode(&op);
        assert_eq!(decode(&bytes).unwrap(), DecodedRowOp::Known(op));
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
            enc.array(3).unwrap();
            enc.str("t_demo").unwrap();
            enc.array(0).unwrap(); // empty pk
            enc.array(2).unwrap();
            enc.array(2).unwrap();
            enc.str("a").unwrap();
            enc.i64(1).unwrap();
            enc.array(2).unwrap();
            enc.str("a").unwrap(); // duplicate column
            enc.i64(2).unwrap();
        }
        assert!(decode(&buf).is_err(), "a duplicate column is not canonical");
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
            enc.array(3).unwrap();
            enc.str("t").unwrap();
            enc.array(1_000_000_000).unwrap(); // absurd declared pk length, no elements follow
        }
        assert!(decode(&buf).is_err(), "an oversized array length is capped, not allocated");
    }

    /// Golden vector: the exact canonical bytes of `sample_upsert`. A change here is a wire-format
    /// change — bump `DOMAIN` deliberately, never edit this hex to make the test pass.
    #[test]
    fn upsert_golden_vector() {
        let bytes = encode(&sample_upsert());
        assert_eq!(rag_rat_base::hash::hex_lower(&bytes), GOLDEN_UPSERT_HEX);
    }

    const GOLDEN_UPSERT_HEX: &str = "83727261672d7261742f7461626c652d6f702f31667570736572748366745f64656d6f82617207848265636f756e74038264646f6e65f582646e6f7465f682657469746c65626869";
}
