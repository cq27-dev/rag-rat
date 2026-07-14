import Foundation

/// A protocol with an associated type and an async requirement, plus a protocol EXTENSION whose
/// default method calls back into the requirement — the shape that gives extension members a
/// container scope path (`Repository::cached`) rather than a bare one.
public protocol Repository: Sendable {
    associatedtype Item: Sendable

    func load(id: Int) async throws -> Item
}

extension Repository {
    public func cached(id: Int) async throws -> Item {
        try await load(id: id)
    }
}

public enum Status: Sendable, Equatable {
    case idle
    case failed(String)
    case running
}

public struct Item: Sendable, Equatable {
    public let id: Int
    public let title: String

    public init(id: Int, title: String) {
        self.id = id
        self.title = title
    }
}

public class BaseService {
    public init() {}

    public func describe() -> String {
        "base"
    }
}

/// Inheritance plus OVERLOADS: `fetch(_:)` differs only by parameter type, so a resolver working
/// from names alone cannot say which one a `service.fetch(…)` call reaches. That is the point of
/// the fixture — the tree-sitter baseline must not pretend to know.
public final class Service: BaseService {
    public func fetch(_ id: Int) -> Item {
        Item(id: id, title: "by-id")
    }

    public func fetch(_ name: String) -> Item {
        Item(id: 0, title: name)
    }

    public override func describe() -> String {
        "service"
    }
}
