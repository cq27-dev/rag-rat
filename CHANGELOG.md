# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.6.0...rag-rat-core-v0.7.0) - 2026-06-16

### Added

- *(oracle)* C++ corpus (yaml-cpp) + resolve .h headers as C++ under a cpp binding ([#186](https://github.com/cq27-dev/rag-rat/pull/186))
- *(oracle)* scip-typescript backend + ts-ky corpus ([#184](https://github.com/cq27-dev/rag-rat/pull/184))
- *(python)* from-import alias resolution ([#174](https://github.com/cq27-dev/rag-rat/pull/174)) ([#179](https://github.com/cq27-dev/rag-rat/pull/179))
- *(init)* Python root-entrypoint binding + content-aware dir selection ([#173](https://github.com/cq27-dev/rag-rat/pull/173)) ([#181](https://github.com/cq27-dev/rag-rat/pull/181))
- *(python)* prefer a base class when resolving `implements` edges ([#172](https://github.com/cq27-dev/rag-rat/pull/172)) ([#180](https://github.com/cq27-dev/rag-rat/pull/180))
- *(oracle)* unified tier-driven corpus runner + oracle.yml (C3) ([#177](https://github.com/cq27-dev/rag-rat/pull/177))
- *(oracle)* scip-python backend — Python compiler-grade resolution (B6) ([#176](https://github.com/cq27-dev/rag-rat/pull/176))
- *(oracle)* `oracle report --corpus <id>` — run a corpus + emit its C2 resolution report (C2-CLI) ([#175](https://github.com/cq27-dev/rag-rat/pull/175))
- *(lang)* Python language support (symbols, graph edges, embeddings) + AST low-signal ([#167](https://github.com/cq27-dev/rag-rat/pull/167))
- *(oracle)* corpus profiles + health-gate loader (C1) ([#171](https://github.com/cq27-dev/rag-rat/pull/171))
- *(oracle)* live before/after resolution report computation (C2 core) ([#168](https://github.com/cq27-dev/rag-rat/pull/168))
- *(oracle)* resolution-report + corpus-profile schema contract (C0) ([#166](https://github.com/cq27-dev/rag-rat/pull/166))
- *(eval)* gate eval behind a non-default feature + add CI eval job ([#162](https://github.com/cq27-dev/rag-rat/pull/162))
- *(mcp)* nudge the agent to re-anchor stale memories via tool-result content ([#160](https://github.com/cq27-dev/rag-rat/pull/160))

### Fixed

- *(resolve)* stop asserting high confidence on guessed Rust type references ([#192](https://github.com/cq27-dev/rag-rat/pull/192))

### Other

- *(oracle)* normalize small-tier corpora to a comparable ~8k-12k edge scale ([#190](https://github.com/cq27-dev/rag-rat/pull/190))
- list Python in the README's code-graph languages ([#183](https://github.com/cq27-dev/rag-rat/pull/183))

## [0.6.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.5.0...rag-rat-core-v0.6.0) - 2026-06-15

### Added

- *(mcp)* expose memory_doctor; raise memory body cap to 8000 + document caps ([#159](https://github.com/cq27-dev/rag-rat/pull/159))
- opaque sym_<hex> symbol handle; drop ephemeral symbol_id from the wire ([#149](https://github.com/cq27-dev/rag-rat/pull/149)) ([#153](https://github.com/cq27-dev/rag-rat/pull/153))
- lazy-heal symbol_lookup + flag dirty result files (#147, #148) ([#151](https://github.com/cq27-dev/rag-rat/pull/151))
- confidence-aware + SCIP-aware symbol importance ranking ([#108](https://github.com/cq27-dev/rag-rat/pull/108)) ([#142](https://github.com/cq27-dev/rag-rat/pull/142))
- crates.io version check surfaced to agents + operators (opt-out via rag-rat.toml) ([#136](https://github.com/cq27-dev/rag-rat/pull/136))

### Fixed

- *(impact)* signal truncation on the flat impact_surface shape ([#150](https://github.com/cq27-dev/rag-rat/pull/150)) ([#157](https://github.com/cq27-dev/rag-rat/pull/157))
- *(index)* heal a just-added symbol on a zero-hit name lookup ([#152](https://github.com/cq27-dev/rag-rat/pull/152)) ([#158](https://github.com/cq27-dev/rag-rat/pull/158))
- *(memory)* re-derive chunk for live logical-symbol bindings on validate ([#154](https://github.com/cq27-dev/rag-rat/pull/154)) ([#156](https://github.com/cq27-dev/rag-rat/pull/156))
- oracle started_at = run start ([#145](https://github.com/cq27-dev/rag-rat/pull/145)); impact_surface flags truncated sections ([#49](https://github.com/cq27-dev/rag-rat/pull/49)) ([#146](https://github.com/cq27-dev/rag-rat/pull/146))
- *(mcp)* read tools open read-only so a writer can't lock them out ([#143](https://github.com/cq27-dev/rag-rat/pull/143)) ([#144](https://github.com/cq27-dev/rag-rat/pull/144))
- *(grep-augment)* skip pipe-incidental greps + dedup indexed hits (#138, #139) ([#140](https://github.com/cq27-dev/rag-rat/pull/140))

### Other

- de-spine the index + mcp crates — module splits, param structs, naming ([#155](https://github.com/cq27-dev/rag-rat/pull/155))
- rewrite README, add oracle/grep-augmentation docs, fix MCP setup footgun
- *(index)* regression for foreign leaked rows self-healing on full rebuild ([#59](https://github.com/cq27-dev/rag-rat/pull/59)) ([#133](https://github.com/cq27-dev/rag-rat/pull/133))

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
