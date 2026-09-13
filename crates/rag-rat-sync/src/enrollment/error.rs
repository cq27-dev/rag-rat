//! The enrollment protocol's error type.

//! The enrollment protocol's error type.

#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    #[error("malformed enrollment data: {0}")]
    Malformed(String),
    #[error("enrollment invite expired")]
    Expired,
    #[error("enrollment invite was already used")]
    Used,
    #[error("enrollment invite is unknown")]
    Unknown,
    #[error("join request transport node does not match the connection")]
    WrongNode,
    /// The request's expected account differs from the account bound to its one-time nonce.
    #[error("enrollment invite belongs to a different account")]
    AccountMismatch,
    /// The exact-request replay found the acknowledged DeviceAdd no longer roster-effective —
    /// the owner removed the device inside the replay window, so the stored receipt (and its
    /// stream-key wraps) must not be released again.
    #[error("the enrolled device was removed from the roster")]
    Revoked,
    /// The exact receipt does not fit the admission budget the joiner declared. Refused BEFORE
    /// the nonce is consumed: candidate capacity is grow-only, so committing the redemption
    /// would burn the enrollment on a receipt the joiner can never hold.
    #[error("the enrollment receipt does not fit the joiner's declared capacity")]
    JoinerCapacity,
    /// The joiner claims a candidate the owner's authenticated snapshot does not hold. Refused
    /// BEFORE the nonce is consumed: adopting the receipt into the union with that unreconciled
    /// history could make the acknowledged DeviceAdd ineffective, and every exact replay would
    /// fail identically.
    #[error("the joiner holds account candidates the inviter's snapshot cannot reconcile")]
    HeldStateConflict,
    #[error("enrollment storage: {0}")]
    Storage(anyhow::Error),
    #[error("enrollment stream: {0}")]
    Io(std::io::Error),
}

impl From<anyhow::Error> for InviteError {
    fn from(value: anyhow::Error) -> Self {
        Self::Storage(value)
    }
}

impl From<std::io::Error> for InviteError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
