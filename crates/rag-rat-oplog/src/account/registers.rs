//! The three revocation register families (§11) — the extend-only watermarks the fold derives from
//! cut ops and then uses to condemn entries.
//!
//! Every register is a `(key, cut)`: a [`RegisterKey`] identifying WHICH chain it bounds, and a
//! [`Cut`] giving the valid-prefix watermark. Three families, each minted by a different cut op:
//! - **device-level** `(account, log, device)` — from `DeviceRemove`; scopes ALL entries on that
//!   device's chain.
//! - **owner-incarnation** `(account, log, device, owner_id)` — from `OwnerDemote`; scopes only the
//!   entries on that chain whose `authority_ref` cites `owner_id` (the ops authored under exactly
//!   that promotion).
//! - **grant-level** `(stream, grantee, grant_id, device)` — from `StreamRevoke`; scopes `/3`
//!   CONTENT entries only. C1 has no content chains, so a grant register never scopes an
//!   account-log entry here — it is recorded for the C2 acceptance predicate to consume.
//!
//! Re-grant / re-promotion mints a FRESH incarnation (a new `owner_id` / `grant_id`), hence a fresh
//! register key with no watermark — which is why a re-authorized device resumes (§11).

use super::AccountId;
use super::cut::Cut;
use super::envelope::AccountEntryHeader;
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

/// Which chain a register bounds (§11). The variant selects the family; the fields are the family's
/// key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum RegisterKey {
    /// From `DeviceRemove` — the whole `(account, log, device)` chain.
    Device { account: AccountId, log: u8, device: DeviceFingerprint },
    /// From `OwnerDemote` — entries on `(account, log, device)` whose `authority_ref` cites
    /// `owner_id`.
    OwnerIncarnation { account: AccountId, log: u8, device: DeviceFingerprint, owner_id: [u8; 32] },
    /// From `StreamRevoke` — `/3` content on `(stream, grantee, grant_id, device)` (C2 consumes
    /// it).
    Grant { stream: StreamId, grantee: AccountId, grant_id: [u8; 32], device: DeviceFingerprint },
}

/// An extend-only revocation register: a chain key + its valid-prefix cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Register {
    pub(super) key: RegisterKey,
    pub(super) cut: Cut,
}

impl RegisterKey {
    /// Whether this register scopes `header` — i.e. the entry lies on the chain the register
    /// bounds. (Whether a scoped entry is CONDEMNED is a further `beyond` / off-branch test,
    /// §11.)
    pub(super) fn scopes(&self, header: &AccountEntryHeader) -> bool {
        match self {
            RegisterKey::Device { account, log, device } =>
                header.account_id == *account
                    && header.log_id == *log
                    && header.device_fingerprint == *device,
            RegisterKey::OwnerIncarnation { account, log, device, owner_id } =>
                header.account_id == *account
                    && header.log_id == *log
                    && header.device_fingerprint == *device
                    && header.authority_ref == Some(*owner_id),
            // Grant registers bound `/3` content chains, which do not exist in C1; an account-log
            // entry is never in a grant register's scope.
            RegisterKey::Grant { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A control-log header on `(account 0xaa, log 0, device 0xbb)` citing `authority_ref`.
    fn header(
        account: u8,
        log: u8,
        device: u8,
        authority_ref: Option<[u8; 32]>,
    ) -> AccountEntryHeader {
        AccountEntryHeader {
            account_id: AccountId::from_bytes([account; 32]),
            log_id: log,
            device_fingerprint: DeviceFingerprint::from_bytes([device; 32]),
            seq: 4,
            prev_hash: Some([0x01; 32]),
            parent_ref: Some([0x02; 32]),
            entry_type: 3,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 2,
            key_id: None,
            authority_ref,
        }
    }

    #[test]
    fn device_register_scopes_the_whole_chain() {
        let key = RegisterKey::Device {
            account: AccountId::from_bytes([0xaa; 32]),
            log: 0,
            device: DeviceFingerprint::from_bytes([0xbb; 32]),
        };
        assert!(key.scopes(&header(0xaa, 0, 0xbb, None)), "same chain, any authority_ref");
        assert!(
            key.scopes(&header(0xaa, 0, 0xbb, Some([0x77; 32]))),
            "same chain, any authority_ref"
        );
        assert!(!key.scopes(&header(0xaa, 0, 0xcc, None)), "different device");
        assert!(!key.scopes(&header(0xdd, 0, 0xbb, None)), "different account");
        assert!(!key.scopes(&header(0xaa, 1, 0xbb, None)), "different log");
    }

    #[test]
    fn owner_incarnation_register_scopes_only_citing_entries() {
        let owner_id = [0x77; 32];
        let key = RegisterKey::OwnerIncarnation {
            account: AccountId::from_bytes([0xaa; 32]),
            log: 0,
            device: DeviceFingerprint::from_bytes([0xbb; 32]),
            owner_id,
        };
        assert!(key.scopes(&header(0xaa, 0, 0xbb, Some(owner_id))), "cites this incarnation");
        assert!(
            !key.scopes(&header(0xaa, 0, 0xbb, Some([0x88; 32]))),
            "cites a different incarnation"
        );
        assert!(!key.scopes(&header(0xaa, 0, 0xbb, None)), "cites nothing (genesis-scope)");
        assert!(!key.scopes(&header(0xaa, 0, 0xcc, Some(owner_id))), "different device");
    }

    #[test]
    fn grant_register_never_scopes_an_account_log_entry() {
        let key = RegisterKey::Grant {
            stream: StreamId::from_bytes([0x33; 32]),
            grantee: AccountId::from_bytes([0x99; 32]),
            grant_id: [0xaa; 32],
            device: DeviceFingerprint::from_bytes([0xbb; 32]),
        };
        assert!(
            !key.scopes(&header(0xaa, 0, 0xbb, Some([0xaa; 32]))),
            "grant scopes content, not control"
        );
    }
}
