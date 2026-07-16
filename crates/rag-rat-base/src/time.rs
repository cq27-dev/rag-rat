//! Wall-clock access for code without an injected clock.

/// Current Unix time in milliseconds. Prefer an injected/persisted timestamp where one exists;
/// this is the single wall-clock read for paths that genuinely need "now".
pub fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}
