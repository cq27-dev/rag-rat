//! One-time owner↔joiner enrollment protocol (#945).
//!
//! The ticket is routing data plus an opaque nonce; the granted role and label stay in the
//! inviter's durable row, so a joiner cannot escalate by editing the ticket. Redemption consumes
//! the nonce, authors the exact `DeviceAdd`, and catches up stream keys in one IMMEDIATE
//! transaction. Any failure rolls all three effects back.

mod error;
#[cfg(test)]
mod framing_tests;
mod redeem;
mod session;
#[cfg(test)]
mod tests;
mod ticket;
mod wire;

pub use error::InviteError;
pub use redeem::{
    InviteSpec, WriterInviteSpec, mint_invite, mint_writer_invite, redeem_invite,
    redeem_writer_invite,
};
pub use session::{
    ENROLL_PROGRESS_TIMEOUT, EnrollmentAcceptorOutcome, run_enrollment_acceptor,
    run_enrollment_acceptor_with_progress, run_enrollment_dialer,
    run_enrollment_dialer_with_progress, run_writer_grant_dialer,
};
pub(crate) use session::{RESPONSE_ACK, RESPONSE_ACK_TIMEOUT};
pub use ticket::{InviteTicket, InviteTicketKind};
pub use wire::{EnrollmentReceipt, EnrollmentRequest, WriterGrantReceipt, WriterGrantRequest};

pub const ENROLL_ALPN: &[u8] = b"rag-rat/enroll/1";
