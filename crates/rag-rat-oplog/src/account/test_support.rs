//! Fixtures shared by the account subtree's unit tests.

use super::AccountId;
use super::envelope::{AccountEntryHeader, sign_account_entry};
use super::fold::CONTROL_LOG;
use super::id::{AccountEntryHash, OwnerId};
use super::ops::{self as control_ops, AccountOp};
use crate::device::{DeviceSecret, DeviceX25519Secret};
use crate::op::DeviceFingerprint;
use crate::stream::{self, StreamId, StreamSpec, StreamSpecV2};

/// A seed-deterministic test device: its ed25519 signer + fingerprint + the pubkeys a
/// Genesis/DeviceAdd op carries. The x25519 key comes from a distinct seed.
pub(in crate::account) struct Dev {
    pub(in crate::account) secret: DeviceSecret,
    pub(in crate::account) fp: DeviceFingerprint,
    pub(in crate::account) ed: [u8; 32],
    pub(in crate::account) x: [u8; 32],
}

impl Dev {
    pub(in crate::account) fn new(seed: u8) -> Self {
        let secret = DeviceSecret::from_seed(&[seed; 32]);
        let public = secret.public();
        let x = DeviceX25519Secret::from_seed(&[seed.wrapping_add(0x80); 32]).public().to_bytes();
        Dev { fp: public.fingerprint(), ed: public.to_bytes(), x, secret }
    }
}

/// Sign one control-log op by `signer` at `(seq, prev)`, citing `authority_ref`.
pub(in crate::account) fn control_op(
    account: AccountId,
    signer: &Dev,
    seq: u64,
    prev: Option<AccountEntryHash>,
    authority_ref: Option<OwnerId>,
    op: &AccountOp,
) -> (Vec<u8>, AccountEntryHash) {
    let payload = control_ops::encode(op).unwrap();
    let header = AccountEntryHeader {
        account_id: account,
        log_id: CONTROL_LOG,
        device_fingerprint: signer.fp,
        seq,
        prev_hash: prev,
        parent_ref: None,
        entry_type: control_ops::entry_type_of(op),
        op_version: 1,
        crypto_suite: 0,
        auth_len: 1,
        key_id: None,
        authority_ref,
    };
    let signed = sign_account_entry(&signer.secret, &header, &payload).unwrap();
    (signed.signed_bytes, signed.entry_hash)
}

/// A `Private` `/2` StreamOwn for `account` over `repo-a`.
pub(in crate::account) fn stream_own(account: AccountId) -> (StreamId, AccountOp) {
    let spec = StreamSpecV2 {
        owner_account_id: account,
        policy: StreamSpec {
            repo_set: vec!["repo-a".to_string()],
            kind_allow_list: None,
            relation_policy: None,
            node_overrides: Vec::new(),
        },
        access_mode: crate::stream::AccessMode::Private,
    };
    let stream_id = stream::derive_v2(&spec).unwrap();
    let stream_spec_bytes = stream::canonical_spec_v2_bytes(&spec).unwrap();
    (stream_id, AccountOp::StreamOwn { stream_id, stream_spec_bytes })
}
