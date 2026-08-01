//! Frozen wire shapes for the repo-scoped `/5` table-sync transport.

use std::collections::BTreeMap;

use minicbor::{Decoder, Encoder};

/// Dedicated ALPN for table-stream sync. Existing account/content/enrollment ALPNs do not move.
pub const TABLE_SYNC_ALPN: &[u8] = b"rag-rat/table-sync/1";

const FRAME_DOMAIN: &str = "rag-rat/table-sync-frame/1";
pub const MAX_MANIFEST_ITEMS: usize = 1024;
pub const MAX_REPO_ID_BYTES: usize = 1024;
pub const MAX_SCOPE_ID_BYTES: usize = 128;
pub const MAX_TABLE_INVENTORY_HASHES: usize = 65_536;
pub const MAX_TABLE_ENTRIES_PER_PAGE: usize = 32;
pub const MAX_TABLE_ENTRY_BYTES: usize = rag_rat_oplog::TABLE_SYNC_ENTRY_MAX_BYTES;

type Hash = [u8; 32];

/// One stream route advertised after mutual account authorization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestItem {
    pub repo_id: String,
    pub incarnation_ref: Hash,
    pub scope_id: String,
    pub stream_id: Hash,
}

/// Canonical manifest: sorted, exactly deduplicated, and conflict-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest(Vec<ManifestItem>);

impl Manifest {
    pub fn new(mut items: Vec<ManifestItem>) -> Result<Self, TableWireError> {
        let mut routes = BTreeMap::new();
        let mut identities = BTreeMap::new();
        for item in &items {
            validate_text("repo_id", &item.repo_id, MAX_REPO_ID_BYTES)?;
            validate_text("scope_id", &item.scope_id, MAX_SCOPE_ID_BYTES)?;
            if routes
                .insert((item.repo_id.as_str(), item.scope_id.as_str()), item)
                .is_some_and(|existing| existing != item)
                || identities.insert(item.stream_id, item).is_some_and(|existing| existing != item)
            {
                return Err(TableWireError::Malformed(
                    "manifest contains conflicting routes for one stream".into(),
                ));
            }
        }
        drop((routes, identities));
        items.sort();
        items.dedup();
        if items.len() > MAX_MANIFEST_ITEMS {
            return Err(TableWireError::OverCap(format!(
                "manifest items {} > {MAX_MANIFEST_ITEMS}",
                items.len()
            )));
        }
        Ok(Self(items))
    }

    pub fn items(&self) -> &[ManifestItem] {
        &self.0
    }
}

/// Table-session frames. Every inventory and entry page is explicitly stream-qualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableFrame {
    Manifest(Manifest),
    Inventory { stream_id: Hash, have: Vec<Hash> },
    Entries { stream_id: Hash, entries: Vec<Vec<u8>>, more: bool },
    StreamDone { stream_id: Hash },
    Done,
    Ack,
}

mod tag {
    pub const MANIFEST: u8 = 0;
    pub const INVENTORY: u8 = 1;
    pub const ENTRIES: u8 = 2;
    pub const STREAM_DONE: u8 = 3;
    pub const DONE: u8 = 4;
    pub const ACK: u8 = 5;
}

#[derive(Debug)]
pub enum TableWireError {
    Malformed(String),
    OverCap(String),
}

impl std::fmt::Display for TableWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(message) => write!(f, "malformed table-sync frame: {message}"),
            Self::OverCap(message) => write!(f, "table-sync frame over cap: {message}"),
        }
    }
}

impl std::error::Error for TableWireError {}

impl TableFrame {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut enc = Encoder::new(&mut bytes);
        match self {
            Self::Manifest(manifest) => {
                enc.array(3).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::MANIFEST).expect(INFALLIBLE);
                enc.array(manifest.items().len() as u64).expect(INFALLIBLE);
                for item in manifest.items() {
                    enc.array(4).expect(INFALLIBLE);
                    enc.str(&item.repo_id).expect(INFALLIBLE);
                    enc.bytes(&item.incarnation_ref).expect(INFALLIBLE);
                    enc.str(&item.scope_id).expect(INFALLIBLE);
                    enc.bytes(&item.stream_id).expect(INFALLIBLE);
                }
            },
            Self::Inventory { stream_id, have } => {
                enc.array(4).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::INVENTORY).expect(INFALLIBLE);
                enc.bytes(stream_id).expect(INFALLIBLE);
                enc.array(have.len() as u64).expect(INFALLIBLE);
                for hash in have {
                    enc.bytes(hash).expect(INFALLIBLE);
                }
            },
            Self::Entries { stream_id, entries, more } => {
                enc.array(5).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::ENTRIES).expect(INFALLIBLE);
                enc.bytes(stream_id).expect(INFALLIBLE);
                enc.array(entries.len() as u64).expect(INFALLIBLE);
                for entry in entries {
                    enc.bytes(entry).expect(INFALLIBLE);
                }
                enc.bool(*more).expect(INFALLIBLE);
            },
            Self::StreamDone { stream_id } => {
                enc.array(3).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::STREAM_DONE).expect(INFALLIBLE);
                enc.bytes(stream_id).expect(INFALLIBLE);
            },
            Self::Done => {
                enc.array(2).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::DONE).expect(INFALLIBLE);
            },
            Self::Ack => {
                enc.array(2).expect(INFALLIBLE);
                enc.str(FRAME_DOMAIN).expect(INFALLIBLE);
                enc.u8(tag::ACK).expect(INFALLIBLE);
            },
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, TableWireError> {
        let mut dec = Decoder::new(bytes);
        let outer = expect_len(dec.array().map_err(m)?, "frame")?;
        let domain = dec.str().map_err(m)?;
        if domain != FRAME_DOMAIN {
            return Err(TableWireError::Malformed(format!("unknown frame domain {domain:?}")));
        }
        let tag = dec.u8().map_err(m)?;
        let expected = match tag {
            tag::MANIFEST => 3,
            tag::INVENTORY => 4,
            tag::ENTRIES => 5,
            tag::STREAM_DONE => 3,
            tag::DONE | tag::ACK => 2,
            other => return Err(TableWireError::Malformed(format!("unknown frame tag {other}"))),
        };
        if outer != expected {
            return Err(TableWireError::Malformed(format!(
                "frame tag {tag} arity {outer}, expected {expected}"
            )));
        }
        let frame = match tag {
            tag::MANIFEST => Self::Manifest(decode_manifest(&mut dec)?),
            tag::INVENTORY => {
                let stream_id = fixed32(dec.bytes().map_err(m)?, "stream_id")?;
                let count = bounded_count(
                    dec.array().map_err(m)?,
                    "inventory hashes",
                    MAX_TABLE_INVENTORY_HASHES,
                )?;
                let mut have = Vec::with_capacity(count);
                for _ in 0..count {
                    have.push(fixed32(dec.bytes().map_err(m)?, "inventory hash")?);
                }
                Self::Inventory { stream_id, have }
            },
            tag::ENTRIES => {
                let stream_id = fixed32(dec.bytes().map_err(m)?, "stream_id")?;
                let count =
                    bounded_count(dec.array().map_err(m)?, "entries", MAX_TABLE_ENTRIES_PER_PAGE)?;
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let entry = dec.bytes().map_err(m)?;
                    if entry.len() > MAX_TABLE_ENTRY_BYTES {
                        return Err(TableWireError::OverCap(format!(
                            "entry bytes {} > {MAX_TABLE_ENTRY_BYTES}",
                            entry.len()
                        )));
                    }
                    entries.push(entry.to_vec());
                }
                Self::Entries { stream_id, entries, more: dec.bool().map_err(m)? }
            },
            tag::STREAM_DONE =>
                Self::StreamDone { stream_id: fixed32(dec.bytes().map_err(m)?, "stream_id")? },
            tag::DONE => Self::Done,
            tag::ACK => Self::Ack,
            _ => unreachable!(),
        };
        if dec.position() != bytes.len() {
            return Err(TableWireError::Malformed("trailing bytes after frame".into()));
        }
        Ok(frame)
    }
}

fn decode_manifest(dec: &mut Decoder<'_>) -> Result<Manifest, TableWireError> {
    let count = bounded_count(dec.array().map_err(m)?, "manifest items", MAX_MANIFEST_ITEMS)?;
    let mut raw = Vec::with_capacity(count);
    for _ in 0..count {
        if dec.array().map_err(m)? != Some(4) {
            return Err(TableWireError::Malformed("manifest item arity".into()));
        }
        let repo_id = dec.str().map_err(m)?;
        validate_text("repo_id", repo_id, MAX_REPO_ID_BYTES)?;
        let incarnation_ref = fixed32(dec.bytes().map_err(m)?, "incarnation_ref")?;
        let scope_id = dec.str().map_err(m)?;
        validate_text("scope_id", scope_id, MAX_SCOPE_ID_BYTES)?;
        let stream_id = fixed32(dec.bytes().map_err(m)?, "stream_id")?;
        raw.push(ManifestItem {
            repo_id: repo_id.to_owned(),
            incarnation_ref,
            scope_id: scope_id.to_owned(),
            stream_id,
        });
    }
    let canonical = Manifest::new(raw.clone())?;
    if canonical.items() != raw {
        return Err(TableWireError::Malformed(
            "manifest items are not in canonical sorted/deduplicated order".into(),
        ));
    }
    Ok(canonical)
}

fn validate_text(field: &str, value: &str, max: usize) -> Result<(), TableWireError> {
    if value.is_empty() {
        return Err(TableWireError::Malformed(format!("empty {field}")));
    }
    if value.len() > max {
        return Err(TableWireError::OverCap(format!("{field} bytes {} > {max}", value.len())));
    }
    Ok(())
}

fn bounded_count(count: Option<u64>, field: &str, max: usize) -> Result<usize, TableWireError> {
    let count = expect_len(count, field)?;
    if count > max as u64 {
        return Err(TableWireError::OverCap(format!("{field} {count} > {max}")));
    }
    Ok(count as usize)
}

fn expect_len(count: Option<u64>, field: &str) -> Result<u64, TableWireError> {
    count.ok_or_else(|| TableWireError::Malformed(format!("indefinite-length {field} array")))
}

fn fixed32(bytes: &[u8], field: &str) -> Result<Hash, TableWireError> {
    bytes.try_into().map_err(|_| {
        TableWireError::Malformed(format!("{field} must be 32 bytes, got {}", bytes.len()))
    })
}

fn m(error: minicbor::decode::Error) -> TableWireError {
    TableWireError::Malformed(error.to_string())
}

const INFALLIBLE: &str = "encoding into an owned Vec cannot fail";

#[cfg(test)]
mod tests {
    use super::*;

    fn item(repo: &str, incarnation: u8, scope: &str, stream: u8) -> ManifestItem {
        ManifestItem {
            repo_id: repo.into(),
            incarnation_ref: [incarnation; 32],
            scope_id: scope.into(),
            stream_id: [stream; 32],
        }
    }

    #[test]
    fn manifest_sorts_and_collapses_exact_duplicates_but_rejects_conflicts() {
        let a = item("repo-a", 1, "anchors/1", 2);
        let b = item("repo-b", 3, "anchors/1", 4);
        let manifest = Manifest::new(vec![b.clone(), a.clone(), a.clone()]).unwrap();
        assert_eq!(manifest.items(), &[a.clone(), b]);

        let mut conflict = a.clone();
        conflict.stream_id = [9; 32];
        assert!(matches!(Manifest::new(vec![a, conflict]), Err(TableWireError::Malformed(_))));
    }

    #[test]
    fn all_frames_roundtrip_and_new_wire_bytes_are_frozen() {
        let manifest = Manifest::new(vec![item("r", 1, "s", 2)]).unwrap();
        let frames = [
            TableFrame::Manifest(manifest.clone()),
            TableFrame::Inventory { stream_id: [2; 32], have: vec![[3; 32]] },
            TableFrame::Entries { stream_id: [2; 32], entries: vec![vec![4, 5]], more: false },
            TableFrame::StreamDone { stream_id: [2; 32] },
            TableFrame::Done,
            TableFrame::Ack,
        ];
        for frame in frames {
            assert_eq!(TableFrame::decode(&frame.encode()).unwrap(), frame);
        }
        assert_eq!(TableFrame::Done.encode(), [
            0x82, 0x78, 0x1a, b'r', b'a', b'g', b'-', b'r', b'a', b't', b'/', b't', b'a', b'b',
            b'l', b'e', b'-', b's', b'y', b'n', b'c', b'-', b'f', b'r', b'a', b'm', b'e', b'/',
            b'1', 0x04,
        ]);
        assert_eq!(TABLE_SYNC_ALPN, b"rag-rat/table-sync/1");
    }

    #[test]
    fn every_new_frame_layout_has_one_combined_golden_digest() {
        use sha2::{Digest, Sha256};

        let frames = [
            TableFrame::Manifest(Manifest::new(vec![item("r", 1, "s", 2)]).unwrap()),
            TableFrame::Inventory { stream_id: [2; 32], have: vec![[3; 32]] },
            TableFrame::Entries { stream_id: [2; 32], entries: vec![vec![4, 5]], more: false },
            TableFrame::StreamDone { stream_id: [2; 32] },
            TableFrame::Done,
            TableFrame::Ack,
        ];
        let mut golden = Vec::new();
        for frame in frames {
            let bytes = frame.encode();
            golden.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            golden.extend_from_slice(&bytes);
        }
        assert_eq!(Sha256::digest(golden).as_slice(), &[
            127, 65, 177, 170, 94, 248, 249, 42, 4, 84, 175, 14, 138, 115, 103, 182, 228, 64, 61,
            105, 158, 189, 214, 59, 224, 146, 228, 176, 152, 213, 72, 222,
        ]);
    }

    #[test]
    fn malformed_domain_arity_and_trailing_bytes_are_rejected() {
        let mut wrong_domain = TableFrame::Done.encode();
        wrong_domain[3] = b'x';
        assert!(matches!(TableFrame::decode(&wrong_domain), Err(TableWireError::Malformed(_))));

        let mut wrong_arity = Vec::new();
        let mut enc = Encoder::new(&mut wrong_arity);
        enc.array(3).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(tag::DONE).unwrap();
        enc.u8(0).unwrap();
        assert!(matches!(TableFrame::decode(&wrong_arity), Err(TableWireError::Malformed(_))));

        let mut trailing = TableFrame::Done.encode();
        trailing.push(0);
        assert!(matches!(TableFrame::decode(&trailing), Err(TableWireError::Malformed(_))));
    }

    #[test]
    fn manifest_item_count_and_string_widths_are_bounded_before_body_allocation() {
        let mut over_count = Vec::new();
        let mut enc = Encoder::new(&mut over_count);
        enc.array(3).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(tag::MANIFEST).unwrap();
        enc.array((MAX_MANIFEST_ITEMS + 1) as u64).unwrap();
        assert!(matches!(TableFrame::decode(&over_count), Err(TableWireError::OverCap(_))));

        let over_repo = item(&"r".repeat(MAX_REPO_ID_BYTES + 1), 1, "s", 2);
        assert!(matches!(Manifest::new(vec![over_repo]), Err(TableWireError::OverCap(_))));
        let over_scope = item("r", 1, &"s".repeat(MAX_SCOPE_ID_BYTES + 1), 2);
        assert!(matches!(Manifest::new(vec![over_scope]), Err(TableWireError::OverCap(_))));
    }

    #[test]
    fn inventory_entry_count_and_entry_width_are_bounded_from_declared_lengths() {
        let mut inventory = Vec::new();
        let mut enc = Encoder::new(&mut inventory);
        enc.array(4).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(tag::INVENTORY).unwrap();
        enc.bytes(&[1; 32]).unwrap();
        enc.array((MAX_TABLE_INVENTORY_HASHES + 1) as u64).unwrap();
        assert!(matches!(TableFrame::decode(&inventory), Err(TableWireError::OverCap(_))));

        let mut entries = Vec::new();
        let mut enc = Encoder::new(&mut entries);
        enc.array(5).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(tag::ENTRIES).unwrap();
        enc.bytes(&[1; 32]).unwrap();
        enc.array((MAX_TABLE_ENTRIES_PER_PAGE + 1) as u64).unwrap();
        assert!(matches!(TableFrame::decode(&entries), Err(TableWireError::OverCap(_))));

        let oversized = TableFrame::Entries {
            stream_id: [1; 32],
            entries: vec![vec![0; MAX_TABLE_ENTRY_BYTES + 1]],
            more: false,
        }
        .encode();
        assert!(matches!(TableFrame::decode(&oversized), Err(TableWireError::OverCap(_))));
    }

    #[test]
    fn malformed_manifest_item_arity_is_rejected() {
        let mut bytes = Vec::new();
        let mut enc = Encoder::new(&mut bytes);
        enc.array(3).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(tag::MANIFEST).unwrap();
        enc.array(1).unwrap();
        enc.array(3).unwrap();
        assert!(matches!(TableFrame::decode(&bytes), Err(TableWireError::Malformed(_))));
    }

    #[test]
    fn noncanonical_wire_manifest_is_rejected() {
        let manifest = Manifest::new(vec![
            item("repo-a", 1, "anchors/1", 2),
            item("repo-b", 3, "anchors/1", 4),
        ])
        .unwrap();
        let mut reversed = manifest.items().to_vec();
        reversed.reverse();
        // Bypass `Manifest::new` only inside this module to handcraft noncanonical wire.
        let bytes = TableFrame::Manifest(Manifest(reversed)).encode();
        assert!(matches!(TableFrame::decode(&bytes), Err(TableWireError::Malformed(_))));
    }

    #[test]
    fn remaining_manifest_and_decoder_bounds_are_rejected() {
        let items = (0..=MAX_MANIFEST_ITEMS)
            .map(|index| {
                let mut stream_id = [0; 32];
                stream_id[..8].copy_from_slice(&(index as u64).to_be_bytes());
                ManifestItem {
                    repo_id: format!("repo-{index}"),
                    incarnation_ref: [1; 32],
                    scope_id: "anchors/1".into(),
                    stream_id,
                }
            })
            .collect();
        let over_cap = Manifest::new(items).unwrap_err();
        assert!(over_cap.to_string().contains("over cap"));
        assert!(Manifest::new(vec![item("", 1, "s", 2)]).is_err());

        let malformed = TableFrame::decode(&[0xff]).unwrap_err();
        assert!(malformed.to_string().contains("malformed"));

        let mut unknown_tag = Vec::new();
        let mut enc = Encoder::new(&mut unknown_tag);
        enc.array(2).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(99).unwrap();
        assert!(TableFrame::decode(&unknown_tag).is_err());

        let mut short_stream = Vec::new();
        let mut enc = Encoder::new(&mut short_stream);
        enc.array(3).unwrap();
        enc.str(FRAME_DOMAIN).unwrap();
        enc.u8(tag::STREAM_DONE).unwrap();
        enc.bytes(&[0]).unwrap();
        assert!(TableFrame::decode(&short_stream).is_err());
    }
}
