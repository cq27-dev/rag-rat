import Foundation

/// Non-ASCII source. LSP speaks UTF-16 code units while the index speaks BYTES, and the two diverge
/// here on purpose:
///
///   - `é` is 2 bytes / 1 UTF-16 unit,
///   - `☕` is 3 bytes / 1 UTF-16 unit,
///   - `🚀` is 4 bytes / **2** UTF-16 units (a surrogate pair),
///   - `データ` is 3 bytes per character / 1 unit each.
///
/// So a callee that sits AFTER these on its line has a byte offset that is not its LSP `character`
/// offset, and any position-conversion bug shows up as an off-by-N instead of hiding. `résumé` is an
/// accented IDENTIFIER, so the symbol's own name spans multi-byte text too.
public let café = "naïve 🚀 データ ☕"

public func résumé() -> String {
    café.uppercased()
}

/// The call to `résumé()` is preceded on its line by ASCII only, so its byte offset and character
/// offset agree — the control.
public func plainCall() -> String {
    return résumé()
}

/// The call to `résumé()` is preceded on its line by `"🚀☕"`, so its byte offset and its UTF-16
/// character offset DIVERGE — the case that catches a byte-vs-UTF-16 mixup.
public func offsetCall() -> String {
    let prefix = "🚀☕"; return prefix + résumé()
}
