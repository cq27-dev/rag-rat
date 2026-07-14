import Testing
import XCTest

@testable import CoreKit

/// XCTest: recognized by inheriting `XCTestCase`. `makeItem` is a HELPER, not a `test*` method — it
/// is still test scaffolding, and must not be indexed as production source.
final class StoreTests: XCTestCase {
    func testLoadsSeed() async throws {
        let store = Store(seed: Item(id: 1, title: "seed"))
        let item = try await store.load(id: 0)
        XCTAssertEqual(item.title, "seed")
    }

    func makeItem() -> Item {
        Item(id: 2, title: "helper")
    }
}

/// swift-testing: recognized by the `@Test` / `@Suite` ATTRIBUTE rather than by inheritance.
@Suite struct StatusSuite {
    @Test func idleIsNotRunning() {
        #expect(Status.idle != Status.running)
    }
}

@Test func cachedDefaultsToLoad() async throws {
    let store = Store(seed: Item(id: 7, title: "cached"))
    let item = try await store.cached(id: 0)
    #expect(item.title == "cached")
}
