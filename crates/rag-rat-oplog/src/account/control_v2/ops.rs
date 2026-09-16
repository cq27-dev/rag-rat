//! The v2 payload binds the checkpoint and the exact pre-operation view inside the signature.
//! The existing account envelope supplies the account, control version and operation tag.

use minicbor::{Decoder, Encoder};

use super::super::id;
use super::super::limits::ACCOUNT_ENVELOPE_MAX_BYTES;
use super::super::ops::{self as legacy, AccountOp, DecodedAccountOp};
use crate::cbor::{self, VecEncoderExt};

pub(in crate::account) const CONTROL_VERSION: u32 = 2;
const DOMAIN: &str = "rag-rat/control-op/2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct ControlOp {
    pub checkpoint: [u8; 32],
    /// The digest of the detached manifest naming this revocation's pre-cut view. Only a
    /// revocation nominates one; an ordinary operation needs no historical view at all, so the
    /// slot is absent and an executor never demands evidence it cannot use.
    ///
    /// This is the ONLY credit evidence a cut carries. The manifest names the eligible identities
    /// and every one of them has to be supplied and authenticated before it counts, so there is no
    /// inline list of victim heads a reader could mistake for something the payload enforces.
    pub pre_cut_view: Option<[u8; 32]>,
    pub op: AccountOp,
}

impl ControlOp {
    pub(in crate::account) fn encode(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(!matches!(self.op, AccountOp::AccountGenesis { .. }), "v2 has no genesis");
        let revocation =
            matches!(self.op, AccountOp::DeviceRemove { .. } | AccountOp::OwnerDemote { .. });
        anyhow::ensure!(revocation == self.pre_cut_view.is_some(), "pre-cut view presence");
        let payload = legacy::encode(&self.op)?;
        let mut bytes = Vec::new();
        let mut e = Encoder::new(&mut bytes);
        e.put_array(4);
        e.put_str(DOMAIN);
        e.put_bytes(&self.checkpoint);
        match &self.pre_cut_view {
            Some(view) => {
                e.put_bytes(view);
            },
            None => {
                e.put_null();
            },
        }
        e.put_bytes(&payload);
        // The SIGNED envelope still needs its own exact size check at authoring/verification.
        anyhow::ensure!(bytes.len() <= ACCOUNT_ENVELOPE_MAX_BYTES, "v2 payload too large");
        Ok(bytes)
    }
}

pub(in crate::account) fn decode(entry_type: u32, bytes: &[u8]) -> anyhow::Result<ControlOp> {
    anyhow::ensure!(bytes.len() <= ACCOUNT_ENVELOPE_MAX_BYTES, "v2 payload too large");
    cbor::require_canonical_cbor(bytes)?;
    let mut d = Decoder::new(bytes);
    anyhow::ensure!(d.array()? == Some(4) && d.str()? == DOMAIN, "v2 control grammar");
    let checkpoint = id::fixed(d.bytes()?)?;
    let pre_cut_view = if d.datatype()? == minicbor::data::Type::Null {
        d.null()?;
        None
    } else {
        Some(id::fixed(d.bytes()?)?)
    };
    let DecodedAccountOp::Known(op) = legacy::decode(entry_type, d.bytes()?)? else {
        anyhow::bail!("unsupported v2 control operation");
    };
    let result = ControlOp { checkpoint, pre_cut_view, op };
    anyhow::ensure!(
        d.position() == bytes.len() && result.encode()? == bytes,
        "noncanonical v2 control payload"
    );
    Ok(result)
}
