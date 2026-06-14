# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.4.0...rag-rat-core-v0.5.0) - 2026-06-14

### Added

- default to TOON output for CLI + MCP, --json opt-out ([#104](https://github.com/cq27-dev/rag-rat/pull/104)) ([#123](https://github.com/cq27-dev/rag-rat/pull/123))
- *(oracle)* scip-clang C/C++ backend ([#71](https://github.com/cq27-dev/rag-rat/pull/71))
- *(oracle)* SCIP phase 1 — reader, occurrence→edge join, heuristic precision/recall eval ([#81](https://github.com/cq27-dev/rag-rat/pull/81))
- *(edges)* persist callee-identifier byte range on graph edges ([#67](https://github.com/cq27-dev/rag-rat/pull/67)) ([#76](https://github.com/cq27-dev/rag-rat/pull/76))
- *(index)* compile and apply real .gitignore in walker + watcher ([#66](https://github.com/cq27-dev/rag-rat/pull/66))

### Fixed

- *(mcp)* serialize logical_symbol_id as a string across serde boundaries ([#130](https://github.com/cq27-dev/rag-rat/pull/130)) ([#131](https://github.com/cq27-dev/rag-rat/pull/131))
- *(memory)* validate path/dir bindings to non-indexed files against the filesystem ([#98](https://github.com/cq27-dev/rag-rat/pull/98)) ([#125](https://github.com/cq27-dev/rag-rat/pull/125))
- *(resolve)* references_type binds only to type definitions in Rust/C/C++ ([#61](https://github.com/cq27-dev/rag-rat/pull/61))
- *(index)* index C/C++ type & function DEFINITIONS, not bare declarations ([#61](https://github.com/cq27-dev/rag-rat/pull/61))
- *(bench)* query_cold opens the realistic production path; stop the false-red gate ([#80](https://github.com/cq27-dev/rag-rat/pull/80))
- *(oracle)* compare logical symbols so C decl-vs-def isn't a false contradiction ([#93](https://github.com/cq27-dev/rag-rat/pull/93))
- *(oracle)* forward tool subprocess stdout to stderr, keep run JSON clean
- *(index)* authoritative full-rebuild clear + stale-overlay self-heal ([#87](https://github.com/cq27-dev/rag-rat/pull/87)) ([#91](https://github.com/cq27-dev/rag-rat/pull/91))
- *(edges)* scope edge resolution to the active checkout ([#89](https://github.com/cq27-dev/rag-rat/pull/89)) ([#90](https://github.com/cq27-dev/rag-rat/pull/90))
- *(memory)* make `memory doctor`→`rebind` resolve cfg-split helpers

### Other

- integrate release-plz with lockstep workspace versioning ([#124](https://github.com/cq27-dev/rag-rat/pull/124))
- Per-package + module-aware import-scope rework ([#61](https://github.com/cq27-dev/rag-rat/pull/61)) ([#106](https://github.com/cq27-dev/rag-rat/pull/106))
- Auto-migrate forward on open — no manual `rag-rat migrate` ([#102](https://github.com/cq27-dev/rag-rat/pull/102)) ([#103](https://github.com/cq27-dev/rag-rat/pull/103))
- Scope-aware + crate-aware edge resolution ([#61](https://github.com/cq27-dev/rag-rat/pull/61)) ([#94](https://github.com/cq27-dev/rag-rat/pull/94))
- *(readme)* SCIP oracle capability, benchmarks section + Bencher badge
- *(query)* prepare_cached the hot per-search edge queries (#79 follow-up)
- Intern repeated edge strings behind an edges compatibility view ([#79](https://github.com/cq27-dev/rag-rat/pull/79)) ([#92](https://github.com/cq27-dev/rag-rat/pull/92))
- oracle run: close the residual mid-subprocess TOCTOU with a pre-spawn files.sha256 snapshot ([#83](https://github.com/cq27-dev/rag-rat/pull/83)) ([#88](https://github.com/cq27-dev/rag-rat/pull/88))
- SCIP phase 3: moniker anchors for repo memories + logical symbols ([#70](https://github.com/cq27-dev/rag-rat/pull/70)) ([#86](https://github.com/cq27-dev/rag-rat/pull/86))
- SCIP phase 2: oracle run, Compiler tier, resolved-external, compare_graph_to_scip ([#69](https://github.com/cq27-dev/rag-rat/pull/69)) ([#82](https://github.com/cq27-dev/rag-rat/pull/82))
- *(readme)* lead with positioning + surface the grep-augmentation proof
- cut idle MCP load (no-op write skip + sweep gating + runtime cap) ([#65](https://github.com/cq27-dev/rag-rat/pull/65))
- *(git-history)* gate the per-pass reload on HEAD/root/shallow change
- *(index)* cut full-index peak RSS and add memory diagnostics
- *(index)* nightly rustfmt the wave-rebuild code
- *(index)* process the full rebuild in waves to bound peak memory
- *(index)* nightly rustfmt the indexing-rework code
