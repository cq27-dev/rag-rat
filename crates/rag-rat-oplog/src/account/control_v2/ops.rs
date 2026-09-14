//! The v2 payload binds the checkpoint and the exact pre-operation view inside the signature.
//! The existing account envelope supplies the account, control version and operation tag.

use minicbor::{Decoder, Encoder};

use super::super::id;
use super::super::limits::ACCOUNT_ENVELOPE_MAX_BYTES;
use super::super::ops::{self as legacy, AccountOp, DecodedAccountOp, DeviceCut};
use crate::cbor::{self, VecEncoderExt};
use crate::op::DeviceFingerprint;

pub(in crate::account) const CONTROL_VERSION: u32 = 2;
const DOMAIN: &str = "rag-rat/control-op/2";
pub(in crate::account) const FRONTIER_MAX: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct ControlOp {
    pub checkpoint: [u8; 32],
    pub pre_cut_view: [u8; 32],
    pub op: AccountOp,
    /// Present even when empty for a revocation; absent for every other operation.
    pub credit_frontier: Option<Vec<DeviceCut>>,
}

impl ControlOp {
    pub(in crate::account) fn encode(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(!matches!(self.op, AccountOp::AccountGenesis { .. }), "v2 has no genesis");
        let revocation =
            matches!(self.op, AccountOp::DeviceRemove { .. } | AccountOp::OwnerDemote { .. });
        anyhow::ensure!(revocation == self.credit_frontier.is_some(), "credit frontier presence");
        let mut frontier = self.credit_frontier.clone();
        if let Some(frontier) = &mut frontier {
            anyhow::ensure!(frontier.len() <= FRONTIER_MAX, "credit frontier too large");
            frontier.sort_unstable_by_key(|head| head.device_fingerprint.to_bytes());
            anyhow::ensure!(
                frontier.windows(2).all(|p| p[0].device_fingerprint != p[1].device_fingerprint),
                "duplicate frontier device"
            );
            anyhow::ensure!(
                frontier.iter().all(|head| i64::try_from(head.seq).is_ok()),
                "frontier sequence exceeds storage range"
            );
        }
        let payload = legacy::encode(&self.op)?;
        let mut bytes = Vec::new();
        let mut e = Encoder::new(&mut bytes);
        e.put_array(5);
        e.put_str(DOMAIN);
        e.put_bytes(&self.checkpoint);
        e.put_bytes(&self.pre_cut_view);
        e.put_bytes(&payload);
        if let Some(frontier) = frontier {
            e.put_array(frontier.len() as u64);
            for head in frontier {
                e.put_array(3);
                e.put_bytes(&head.device_fingerprint.to_bytes());
                e.put_u64(head.seq);
                e.put_bytes(head.hash.as_slice());
            }
        } else {
            e.put_null();
        }
        // The SIGNED envelope still needs its own exact size check at authoring/verification.
        anyhow::ensure!(bytes.len() <= ACCOUNT_ENVELOPE_MAX_BYTES, "v2 payload too large");
        Ok(bytes)
    }
}

pub(in crate::account) fn decode(entry_type: u32, bytes: &[u8]) -> anyhow::Result<ControlOp> {
    anyhow::ensure!(bytes.len() <= ACCOUNT_ENVELOPE_MAX_BYTES, "v2 payload too large");
    cbor::require_canonical_cbor(bytes)?;
    let mut d = Decoder::new(bytes);
    anyhow::ensure!(d.array()? == Some(5) && d.str()? == DOMAIN, "v2 control grammar");
    let checkpoint = id::fixed(d.bytes()?)?;
    let pre_cut_view = id::fixed(d.bytes()?)?;
    let DecodedAccountOp::Known(op) = legacy::decode(entry_type, d.bytes()?)? else {
        anyhow::bail!("unsupported v2 control operation");
    };
    let credit_frontier = if d.datatype()? == minicbor::data::Type::Null {
        d.null()?;
        None
    } else {
        let n = d.array()?.ok_or_else(|| anyhow::anyhow!("indefinite frontier"))?;
        anyhow::ensure!(n <= FRONTIER_MAX as u64, "credit frontier too large");
        let mut heads = Vec::with_capacity(n as usize);
        for _ in 0..n {
            anyhow::ensure!(d.array()? == Some(3), "frontier head grammar");
            heads.push(DeviceCut {
                device_fingerprint: DeviceFingerprint::from_bytes(id::fixed(d.bytes()?)?),
                seq: d.u64()?,
                hash: id::fixed::<32>(d.bytes()?)?.into(),
            });
        }
        Some(heads)
    };
    let result = ControlOp { checkpoint, pre_cut_view, op, credit_frontier };
    anyhow::ensure!(
        d.position() == bytes.len() && result.encode()? == bytes,
        "noncanonical v2 control payload"
    );
    Ok(result)
}
