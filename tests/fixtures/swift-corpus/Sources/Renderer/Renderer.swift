import CoreKit

public struct Renderer {
    private let store: Store<Item>
    private let cache: Cache

    public init(store: Store<Item>, cache: Cache) {
        self.store = store
        self.cache = cache
    }

    /// The cross-MODULE call. `store.load(id:)` is declared in CoreKit; `Cache.load(id:)` in THIS
    /// module shares its bare name. Optional chaining (`?.title`) on the cache result keeps the two
    /// call shapes distinct in the AST.
    public func render(id: Int) async throws -> String {
        let item = try await store.load(id: id)
        let cached = cache.load(id: id)?.title
        return "\(item.title) \(cached ?? "-")"
    }

    /// A generic cross-module method reached through a TRAILING closure.
    public func titles() async -> [String] {
        await store.mapped { element in
            element.title
        }
    }

    /// Enum cases across the module boundary, in both the qualified (`Status.running`) and the
    /// shorthand (`.idle`) shape — only the shorthand is bindable by bare name.
    public func status(for item: Item?) -> Status {
        guard item != nil else {
            return .idle
        }
        return Status.running
    }

    /// OVERLOADED cross-module calls: which `fetch` each reaches is a type question, not a name
    /// question.
    public func described() -> String {
        let service = Service()
        return service.fetch(1).title + service.fetch("name").title + service.describe()
    }
}
