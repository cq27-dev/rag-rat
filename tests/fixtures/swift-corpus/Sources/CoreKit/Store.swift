import Foundation

/// An actor (a Swift-only nominal kind), generic over its element, conforming to `Repository`.
/// Its `load(id:)` shares a bare name with `Renderer.Cache.load(id:)` in the OTHER module — the
/// collision a name-only resolver cannot settle.
public actor Store<Element: Sendable>: Repository {
    public typealias Item = Element

    private var elements: [Element]

    public var count: Int {
        elements.count
    }

    public init(seed: Element) {
        self.elements = [seed]
    }

    public func load(id: Int) async throws -> Element {
        elements[id]
    }

    /// Generic method + a closure parameter, called with a TRAILING closure from `Renderer`.
    public func mapped<Output>(_ transform: (Element) -> Output) -> [Output] {
        elements.map(transform)
    }

    public func append(_ element: Element) {
        elements.append(element)
    }

    public subscript(index: Int) -> Element {
        elements[index]
    }
}
