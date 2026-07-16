//! Path rendering shared across subsystems.

use std::path::Path;

/// The canonical `/`-separated rendering used everywhere a path is persisted or compared against
/// the `files` table — Windows separators normalize so the same file hashes/joins identically on
/// every platform.
pub fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
