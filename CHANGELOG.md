# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.10.0...rag-rat-core-v0.11.0) - 2026-07-01

### Added

- *(embed)* content-address the vector cache so embeddings survive reindex ([#357](https://github.com/cq27-dev/rag-rat/pull/357)) ([#358](https://github.com/cq27-dev/rag-rat/pull/358))
- *(embed)* benchmark-embedding CLI behind the eval feature (Phase 3) ([#354](https://github.com/cq27-dev/rag-rat/pull/354))
- *(embed)* embed light/incremental reconciles against the local query_endpoint ([#356](https://github.com/cq27-dev/rag-rat/pull/356))
- *(embed)* model context-awareness — warn short-context models truncate long code, steer to a long-context model ([#351](https://github.com/cq27-dev/rag-rat/pull/351))
- *(cookbook)* infinity + vLLM ephemeral recipes over OpenAI /v1/embeddings (Phase 2) ([#349](https://github.com/cq27-dev/rag-rat/pull/349))
- *(embed)* centralize remote embedding on OpenAI /v1/embeddings + backend selector (Phase 1) ([#348](https://github.com/cq27-dev/rag-rat/pull/348))
- *(embed)* auto-tune ephemeral remote embedding concurrency ([#342](https://github.com/cq27-dev/rag-rat/pull/342))
- *(init)* add extensible ratatui wizard ([#337](https://github.com/cq27-dev/rag-rat/pull/337))
- *(embed)* user-selectable GPU for ephemeral cookbook provisioning ([llm.embedding.remote] gpu) ([#335](https://github.com/cq27-dev/rag-rat/pull/335))
- *(embed)* remote Ollama embedding — connect + ephemeral cookbook, hardened (#317/#318) ([#330](https://github.com/cq27-dev/rag-rat/pull/330))
- *(embed)* wire Ollama end-to-end — connect mode (#317 task 5+6) ([#326](https://github.com/cq27-dev/rag-rat/pull/326))
- *(embed)* OllamaEmbedder backend (#317 task 4) ([#325](https://github.com/cq27-dev/rag-rat/pull/325))
- *(embed)* [embedding.remote] config block (#317 task 3) ([#324](https://github.com/cq27-dev/rag-rat/pull/324))
- *(embed)* register the ollama backend — Backend::Ollama + registry row ([#317](https://github.com/cq27-dev/rag-rat/pull/317)) ([#322](https://github.com/cq27-dev/rag-rat/pull/322))

### Fixed

- *(ollama)* handle local embedding limits ([#340](https://github.com/cq27-dev/rag-rat/pull/340))
- *(watch)* honor .gitignore in watch placement — stop exhausting inotify watches ([#331](https://github.com/cq27-dev/rag-rat/pull/331)) ([#332](https://github.com/cq27-dev/rag-rat/pull/332))
- *(embed)* restore the public index::ai::MODEL2VEC_HF_REPO path (#320 review) ([#323](https://github.com/cq27-dev/rag-rat/pull/323))

### Other

- *(readme)* reframe as an agent-workflow conversion path ([#359](https://github.com/cq27-dev/rag-rat/pull/359))
- *(embed)* unlock GPU-backend throughput — sweep visibility, higher cap, backend-aware provision timeout ([#350](https://github.com/cq27-dev/rag-rat/pull/350))
- *(embed)* parallelize remote ollama reconcile ([#341](https://github.com/cq27-dev/rag-rat/pull/341))
- *(embed)* extract index/ai/providers/ — single resolution chokepoint ([#317](https://github.com/cq27-dev/rag-rat/pull/317)) ([#320](https://github.com/cq27-dev/rag-rat/pull/320))

## [0.10.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.9.0...rag-rat-core-v0.10.0) - 2026-06-25

### Added

- *(eval)* track commit-replay search recall in Bencher on main ([#315](https://github.com/cq27-dev/rag-rat/pull/315))
- *(embed)* re-encode existing f32 embedding blobs to int8 ([#312](https://github.com/cq27-dev/rag-rat/pull/312)) ([#313](https://github.com/cq27-dev/rag-rat/pull/313))
- *(embed)* int8 scalar quantization for chunk_embeddings — ~4x smaller, neutral quality ([#112](https://github.com/cq27-dev/rag-rat/pull/112)) ([#311](https://github.com/cq27-dev/rag-rat/pull/311))
- *(eval)* recall@3 + recall@returned ceiling metrics; graded-git rerank (off) — #109 spike ([#310](https://github.com/cq27-dev/rag-rat/pull/310))
- *(eval)* commit-replay retrieval eval harness ([#120](https://github.com/cq27-dev/rag-rat/pull/120)) ([#303](https://github.com/cq27-dev/rag-rat/pull/303))

### Fixed

- *(eval)* suppress git hooks on the replay's throwaway worktrees ([#306](https://github.com/cq27-dev/rag-rat/pull/306))

### Other

- *(embed)* centralize the embedding-model registry; BGE + jina as selectable options ([#112](https://github.com/cq27-dev/rag-rat/pull/112)) ([#309](https://github.com/cq27-dev/rag-rat/pull/309))

## [0.9.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.8.0...rag-rat-core-v0.9.0) - 2026-06-24

### Added

- *(clones)* name why a resolved symbol is clone-ineligible (#274 item 3a) ([#300](https://github.com/cq27-dev/rag-rat/pull/300))
- *(clones)* exclude tests from the write-time clone check + raise its precision ([#292](https://github.com/cq27-dev/rag-rat/pull/292)) ([#293](https://github.com/cq27-dev/rag-rat/pull/293))
- *(doctor)* flag stale clone fingerprints + stop listing migrations twice ([#291](https://github.com/cq27-dev/rag-rat/pull/291))
- *(clones)* write-time clone check — warn agents when they're duplicating existing code ([#287](https://github.com/cq27-dev/rag-rat/pull/287)) ([#289](https://github.com/cq27-dev/rag-rat/pull/289))
- *(clones)* precompute the clone-edge graph in the background so find_clones scales ([#286](https://github.com/cq27-dev/rag-rat/pull/286)) ([#288](https://github.com/cq27-dev/rag-rat/pull/288))
- *(clones)* global cross-class refine cell budget ([#272](https://github.com/cq27-dev/rag-rat/pull/272)) ([#281](https://github.com/cq27-dev/rag-rat/pull/281))
- *(clones)* clone measurement infrastructure — perf microbench + recall signature ([#279](https://github.com/cq27-dev/rag-rat/pull/279)) ([#280](https://github.com/cq27-dev/rag-rat/pull/280))
- *(dream)* deterministic memory-maintenance worklist v1 ([#122](https://github.com/cq27-dev/rag-rat/pull/122)) ([#260](https://github.com/cq27-dev/rag-rat/pull/260))
- *(clones)* multi-language correctness — comments, literals, TS function-valued declarators, generated-skip ([#232](https://github.com/cq27-dev/rag-rat/pull/232)) ([#252](https://github.com/cq27-dev/rag-rat/pull/252))
- *(clones)* anti-unification refine engine — template + variation points + signature + clones --explain (#215 Plan 4b) ([#243](https://github.com/cq27-dev/rag-rat/pull/243))
- cross-platform support — macOS + Windows ([#244](https://github.com/cq27-dev/rag-rat/pull/244))
- clone refine 4a — coherence split + LCS confidence + refactorability ROI + cache (#215 Plan 4a) ([#236](https://github.com/cq27-dev/rag-rat/pull/236))
- clone-detection query surface — find_clones + clones_for_symbol (#215 Plan 2) ([#234](https://github.com/cq27-dev/rag-rat/pull/234))
- *(index)* clone-detection fingerprint substrate — SourcererCC inverted index ([#215](https://github.com/cq27-dev/rag-rat/pull/215)) ([#229](https://github.com/cq27-dev/rag-rat/pull/229))

### Fixed

- *(clones)* drop stale theta arg from coherence_split's #259 budget test — unbreak main ([#301](https://github.com/cq27-dev/rag-rat/pull/301))
- *(clones)* dampen un-refined member_count factor so refine-failed classes can't masquerade as high-ROI ([#259](https://github.com/cq27-dev/rag-rat/pull/259)) ([#299](https://github.com/cq27-dev/rag-rat/pull/299))
- *(clones)* covering-subset for coherence_split's budget-tripped tail ([#282](https://github.com/cq27-dev/rag-rat/pull/282)) ([#283](https://github.com/cq27-dev/rag-rat/pull/283))
- *(clones)* close Kotlin boolean/null + C/C++ char-value normalize recall gaps ([#253](https://github.com/cq27-dev/rag-rat/pull/253)) ([#278](https://github.com/cq27-dev/rag-rat/pull/278))
- *(clones)* Typedness::Structural for pure-closure signatures (#274 item 10) ([#277](https://github.com/cq27-dev/rag-rat/pull/277))
- *(clones)* widen string-hole template cosmetics (#254, #274 item 16) ([#276](https://github.com/cq27-dev/rag-rat/pull/276))
- *(clones)* close two #235 follow-ups — discriminator test + scoped callee reopen ([#273](https://github.com/cq27-dev/rag-rat/pull/273))
- *(clones)* #256 follow-ups — de-flake dense-clique test + seed split by similarity (R-A) ([#265](https://github.com/cq27-dev/rag-rat/pull/265))
- *(clones)* split giant over-merged components + coverage-gated ROI ([#256](https://github.com/cq27-dev/rag-rat/pull/256)) ([#257](https://github.com/cq27-dev/rag-rat/pull/257))
- *(oracle)* edge_oracle survives reindex (content-anchored verdicts) + enforcing regression guards ([#248](https://github.com/cq27-dev/rag-rat/pull/248)) ([#249](https://github.com/cq27-dev/rag-rat/pull/249))

### Other

- *(clones)* coherence_split GROW checks pre-verified edge adjacency, not recomputed similarity ([#258](https://github.com/cq27-dev/rag-rat/pull/258)) ([#298](https://github.com/cq27-dev/rag-rat/pull/298))
- *(clones)* cap non-discriminating hot-token postings in candidate generation ([#271](https://github.com/cq27-dev/rag-rat/pull/271)) ([#297](https://github.com/cq27-dev/rag-rat/pull/297))
- *(clones)* consolidate the three test-path detectors into one canonical helper ([#294](https://github.com/cq27-dev/rag-rat/pull/294)) ([#295](https://github.com/cq27-dev/rag-rat/pull/295))
- *(status)* db.status no longer runs the per-chunk embedding reconcile plan (~200s → ms) ([#285](https://github.com/cq27-dev/rag-rat/pull/285))
- *(clones)* parallelize candidate-gen (sub_block 3.8×) + uncapped --recall-symbols (#282 follow-ups) ([#284](https://github.com/cq27-dev/rag-rat/pull/284))
- code sweep — clone/god-module cleanup + ship #251/#220/#267/#222 ([#268](https://github.com/cq27-dev/rag-rat/pull/268))
- *(clones)* total_cmp seed sort + document greedy cover seed-order dependence (#256 adversary polish) ([#266](https://github.com/cq27-dev/rag-rat/pull/266))
- *(clones)* BLOB-pack the token bag — drop per-token symbol_token_postings ([#231](https://github.com/cq27-dev/rag-rat/pull/231)) ([#250](https://github.com/cq27-dev/rag-rat/pull/250))

## [0.8.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.7.0...rag-rat-core-v0.8.0) - 2026-06-19

### Added

- *(index)* intern symbol qualified_name into the shared name_strings pool ([#224](https://github.com/cq27-dev/rag-rat/pull/224)) ([#227](https://github.com/cq27-dev/rag-rat/pull/227))
- *(index)* dictionary-zstd chunk-text compression + drop chunks.text (#77 Phase 2) ([#225](https://github.com/cq27-dev/rag-rat/pull/225))
- *(serve)* worktree-aware serving — linked git worktrees as branch overlays ([#219](https://github.com/cq27-dev/rag-rat/pull/219))
- *(git)* move all runtime git operations to gix (gitoxide) ([#212](https://github.com/cq27-dev/rag-rat/pull/212)) ([#213](https://github.com/cq27-dev/rag-rat/pull/213))
- *(graph)* synthesize message-dispatch (actor-channel / enum) edges ([#200](https://github.com/cq27-dev/rag-rat/pull/200)) ([#206](https://github.com/cq27-dev/rag-rat/pull/206))
- *(graph)* zero-caller find_callers is never low completeness ([#200](https://github.com/cq27-dev/rag-rat/pull/200)) ([#205](https://github.com/cq27-dev/rag-rat/pull/205))
- *(symbol)* exclude generated bindings from symbol search by default ([#202](https://github.com/cq27-dev/rag-rat/pull/202)) ([#204](https://github.com/cq27-dev/rag-rat/pull/204))
- *(oracle)* add py-django corpus + tolerate diagnostic exit codes (#182 groundwork) ([#198](https://github.com/cq27-dev/rag-rat/pull/198))
- *(oracle)* opt-in external-resolution health floor for npm-style corpora ([#185](https://github.com/cq27-dev/rag-rat/pull/185)) ([#196](https://github.com/cq27-dev/rag-rat/pull/196))
- *(oracle)* scip-java (Kotlin) backend ([#193](https://github.com/cq27-dev/rag-rat/pull/193))
- *(impact)* compact repo-memory view by default; full bodies on request ([#37](https://github.com/cq27-dev/rag-rat/pull/37)) ([#194](https://github.com/cq27-dev/rag-rat/pull/194))

### Fixed

- *(parser)* bound tree-sitter parse with a wall-clock budget ([#210](https://github.com/cq27-dev/rag-rat/pull/210)) ([#211](https://github.com/cq27-dev/rag-rat/pull/211))
- *(graph)* synthesize message-dispatch (actor-channel / enum) edges ([#200](https://github.com/cq27-dev/rag-rat/pull/200)) ([#208](https://github.com/cq27-dev/rag-rat/pull/208))
- *(symbol)* accept a sym_<hex> handle in the ref slot ([#201](https://github.com/cq27-dev/rag-rat/pull/201)) ([#203](https://github.com/cq27-dev/rag-rat/pull/203))

### Other

- *(impact)* stop scanning chunks.text in impact_surface (#77 Phase 1) ([#223](https://github.com/cq27-dev/rag-rat/pull/223))
- *(oracle)* pin indexer dep tree + lock corpus installs (#185 items 2-3) ([#197](https://github.com/cq27-dev/rag-rat/pull/197))

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
