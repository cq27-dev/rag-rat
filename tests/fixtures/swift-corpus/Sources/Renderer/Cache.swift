import CoreKit

/// Declares `load(id:)` — the SAME bare name as `CoreKit.Store.load(id:)`, in a DIFFERENT module.
/// This is the collision the corpus exists to pin: a resolver working from names alone sees two
/// candidates for `store.load(id:)` in `Renderer.render` and has no honest way to choose. The
/// tree-sitter baseline must therefore leave that call unresolved; SourceKit-LSP (#637) must bind it
/// to `CoreKit.Store.load`.
public struct Cache {
    private var items: [Int: Item]

    public init(items: [Int: Item] = [:]) {
        self.items = items
    }

    public func load(id: Int) -> Item? {
        items[id]
    }
}
