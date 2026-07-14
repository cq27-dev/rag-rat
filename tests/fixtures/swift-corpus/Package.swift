// swift-tools-version:6.0
// The SwiftPM corpus (#636): a real, buildable package — not loose .swift files — because the
// SourceKit-LSP oracle (#637) resolves against a BUILT module graph. `Renderer` depends on
// `CoreKit`, so cross-MODULE calls exist for the oracle to resolve and for the tree-sitter baseline
// to (correctly) leave unresolved.
import PackageDescription

let package = Package(
    name: "SwiftCorpus",
    targets: [
        .target(name: "CoreKit"),
        .target(name: "Renderer", dependencies: ["CoreKit"]),
        .testTarget(name: "CoreKitTests", dependencies: ["CoreKit"]),
    ]
)
