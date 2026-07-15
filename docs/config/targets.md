# Index targets (`[target_bindings]` / `[[target]]`)

Part of the [config reference](../config.md).

Simple bindings map a language to directories:

```toml
[target_bindings]
rust = ["crates/app/src"]
typescript = ["apps/mobile/src"]
kotlin = ["apps/wear-bridge/src"]
cpp = ["include", "src"]
markdown = ["docs"]
```

A simple binding indexes each language's default extensions in the listed directories
(`rust` → `.rs`, `typescript` → `.ts`/`.tsx`, `python` → `.py`/`.pyi`, `c` → `.c`/`.h`, etc.).
The one ambiguous case is the `.h` header: with no binding it is detected as **C** (the safe
default), but an explicit `cpp` binding also claims `.h` in its directories and indexes those headers
as **C++**. This is what lets a C++ library whose API lives in `.h` files (most of them) get header
symbols, so calls resolve to their definitions instead of going unresolved. A `.c` file is never
treated as C++.

Expanded targets add name, kind, include, and exclude metadata:

```toml
[[target]]
name = "generated-bindings"
language = "typescript"
directories = ["packages/app/src/generated"]
kind = "generated"
include = ["**/*.ts"]
exclude = ["**/*.map"]
```

Supported languages are `rust`, `typescript`, `kotlin`, `c`, `cpp`, `python`, `swift`, and
`markdown`. Rust, TypeScript/TSX, Kotlin, C, C++, Python, and Swift source use tree-sitter structural
indexing when files are under the parser size cap.
Markdown uses heading-section chunking and does not use tree-sitter. Supported target kinds are
`source`, `generated`, `docs`, and `tests`; generated targets are indexed with coarse chunks and
still obey `include_generated` filtering.

Parser grammar dependencies are exact-pinned in `Cargo.toml`: `tree-sitter` 0.26.9,
`tree-sitter-rust` 0.24.2, `tree-sitter-typescript` 0.23.2, `tree-sitter-kotlin-ng` 1.1.0,
`tree-sitter-c` 0.24.2, `tree-sitter-cpp` 0.23.4, `tree-sitter-python` 0.25.0, and
`tree-sitter-swift` 0.7.3.
