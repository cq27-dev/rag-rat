# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.21.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-base-v0.20.0...rag-rat-base-v0.21.0) - 2026-07-24

### Added

- *(init)* add Papertrail (issue tracker) and Distillation wizard steps ([#887](https://github.com/cq27-dev/rag-rat/pull/887))
- *(distill)* default to the validated 30B ephemeral box, with distillation docs ([#876](https://github.com/cq27-dev/rag-rat/pull/876))
- *(distill)* drain prepared snapshots through the configured chat model ([#704](https://github.com/cq27-dev/rag-rat/pull/704)) ([#799](https://github.com/cq27-dev/rag-rat/pull/799))

### Other

- *(watch)* quiet-window overlay skip; reuse repo handles and the recorded delta in the probe ([#857](https://github.com/cq27-dev/rag-rat/pull/857))
- *(watch)* add a minimum inter-pass cooldown to the watcher event loop ([#847](https://github.com/cq27-dev/rag-rat/pull/847))

## [0.20.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-base-v0.19.0...rag-rat-base-v0.20.0) - 2026-07-20

### Added

- *(cli)* rag-rat rm <path> — remove a repo from the global index (purge + VACUUM), config, and hooks ([#778](https://github.com/cq27-dev/rag-rat/pull/778))
- *(plugin)* opencode plugin bundle — @rag-rat/plugin-opencode (MCP + hooks) ([#785](https://github.com/cq27-dev/rag-rat/pull/785))
- *(config)* add [llm.distill] config with a model-size-aware provision timeout ([#779](https://github.com/cq27-dev/rag-rat/pull/779))
- *(agent-hook)* augment the Read tool with file/dir memories + load-bearing symbols ([#756](https://github.com/cq27-dev/rag-rat/pull/756)) ([#761](https://github.com/cq27-dev/rag-rat/pull/761))
- *(agent-hook)* PostToolUse edit trigger — scoped reindex, watcher-aware, detached ([#738](https://github.com/cq27-dev/rag-rat/pull/738))
- *(papertrail)* closing-edge substrate — provider-neutral schema, gated text tier, item/comment ref mining ([#702](https://github.com/cq27-dev/rag-rat/pull/702)) ([#722](https://github.com/cq27-dev/rag-rat/pull/722))

### Fixed

- *(tests)* route test scratch through a shared self-healing helper (fixes #726) ([#732](https://github.com/cq27-dev/rag-rat/pull/732))

### Other

- *(readme)* link the rag-rat.cq27.dev site ([#749](https://github.com/cq27-dev/rag-rat/pull/749))
- *(locks)* shared content-carrying single-flight coalescing primitive ([#736](https://github.com/cq27-dev/rag-rat/pull/736))
- *(workspace)* [**breaking**] extract rag-rat-query — the read layer: graph/impact/symbol/tree queries, memory reads + evidence, pagerank (#706 phase 6) ([#719](https://github.com/cq27-dev/rag-rat/pull/719))
- *(workspace)* [**breaking**] extract the rag-rat-clones crate (#706 phase 4) ([#717](https://github.com/cq27-dev/rag-rat/pull/717))
- *(workspace)* [**breaking**] extract rag-rat-papertrail — mirror, providers, transport, evidence (#706 phase 2) ([#715](https://github.com/cq27-dev/rag-rat/pull/715))
- *(workspace)* [**breaking**] extract the rag-rat-db database layer with an explicit MigrationHooks seam (#706 phase 1) ([#714](https://github.com/cq27-dev/rag-rat/pull/714))

## [0.19.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.18.0...rag-rat-core-v0.19.0) - 2026-07-16

### Added

- *(index)* scoped reconcile over an explicit path set (index --paths) ([#687](https://github.com/cq27-dev/rag-rat/pull/687))
- *(oplog)* content-key crypto primitives — content keys, key_id, X25519 sealed-box wrap (C4.1) ([#709](https://github.com/cq27-dev/rag-rat/pull/709))
- *(evals)* regenerate memory-compaction verify-packs corpus + add a committed generator ([#695](https://github.com/cq27-dev/rag-rat/pull/695)) ([#700](https://github.com/cq27-dev/rag-rat/pull/700))
- *(oplog)* defer the /3 content-ingest refold + exclude local authoring from the remote budget ([#699](https://github.com/cq27-dev/rag-rat/pull/699))
- *(memory)* author the live memory path onto owner-bound /2//3 streams ([#681](https://github.com/cq27-dev/rag-rat/pull/681))
- *(watch)* surface silently-dropped filesystem watches in index_status ([#670](https://github.com/cq27-dev/rag-rat/pull/670))
- *(account)* add the in-tx owner-stream ownership ensure seam ([#677](https://github.com/cq27-dev/rag-rat/pull/677))
- *(account)* add the /3 local-content authoring seam + accepted→memory projection ([#668](https://github.com/cq27-dev/rag-rat/pull/668))
- *(papertrail)* native GitLab provider — namespaced ids, parallel list legs, events comment lane ([#654](https://github.com/cq27-dev/rag-rat/pull/654))
- *(account)* mint the store's local account (C3.4a bootstrap) ([#666](https://github.com/cq27-dev/rag-rat/pull/666))
- *(account)* wire the /3 acceptance refold into ingest and the account fold ([#653](https://github.com/cq27-dev/rag-rat/pull/653))
- *(swift)* add the SwiftPM corpus, and fix the calls it exposed ([#650](https://github.com/cq27-dev/rag-rat/pull/650))
- *(papertrail)* automatic sync orchestration — watcher deadline, hook trigger, coalesced single-flight ([#646](https://github.com/cq27-dev/rag-rat/pull/646))

### Fixed

- Android/Termux support — flock via libc, static libc++ in the prebuilt ([#710](https://github.com/cq27-dev/rag-rat/pull/710))
- *(dream)* cut divergence false positives — memory-id cross-refs, verbatim, documented removals ([#686](https://github.com/cq27-dev/rag-rat/pull/686))
- *(fts)* detect FTS5 shadow corruption at the query layer and self-heal from durable sources ([#675](https://github.com/cq27-dev/rag-rat/pull/675))
- *(account)* guard the ancestry contiguity check against a u64::MAX seq ([#657](https://github.com/cq27-dev/rag-rat/pull/657))
- *(swift)* recover force-unwrapped receiver method calls ([#656](https://github.com/cq27-dev/rag-rat/pull/656))

### Other

- *(memory)* pin the read path unchanged under /3 + mark the foreign read projection phase-D ([#693](https://github.com/cq27-dev/rag-rat/pull/693))
- *(graph)* index-seed the remaining edges-view readers — grep_augment + impact_surface ([#692](https://github.com/cq27-dev/rag-rat/pull/692)) ([#694](https://github.com/cq27-dev/rag-rat/pull/694))
- *(graph)* seed find_callers/trace_callees on indexed edge id columns ([#682](https://github.com/cq27-dev/rag-rat/pull/682)) ([#684](https://github.com/cq27-dev/rag-rat/pull/684))
- neutral wording for the sync feature in repo_identity docs ([#674](https://github.com/cq27-dev/rag-rat/pull/674))
- split config.md into per-topic pages under docs/config/ ([#669](https://github.com/cq27-dev/rag-rat/pull/669))

## [0.18.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.17.0...rag-rat-core-v0.18.0) - 2026-07-14

### Added

- *(account)* add /3 candidate ancestry and branch selection ([#647](https://github.com/cq27-dev/rag-rat/pull/647))
- *(lang)* add Swift baseline support ([#639](https://github.com/cq27-dev/rag-rat/pull/639))
- *(account)* add pure content acceptance evaluator; split auth_len freshness out of the authority seam ([#645](https://github.com/cq27-dev/rag-rat/pull/645))
- *(papertrail)* add automatic sync scheduling core ([#640](https://github.com/cq27-dev/rag-rat/pull/640))
- *(account)* add signed content entry envelope ([#643](https://github.com/cq27-dev/rag-rat/pull/643))
- *(account)* finish C1 authority projection and query seams ([#604](https://github.com/cq27-dev/rag-rat/pull/604)) ([#641](https://github.com/cq27-dev/rag-rat/pull/641))
- *(papertrail)* add native GitHub project mirror ([#638](https://github.com/cq27-dev/rag-rat/pull/638))
- *(papertrail)* add shared provider transport ([#633](https://github.com/cq27-dev/rag-rat/pull/633))
- *(papertrail)* add tracker config and provider ref grammar ([#631](https://github.com/cq27-dev/rag-rat/pull/631))
- *(papertrail)* add provider-neutral schema ([#632](https://github.com/cq27-dev/rag-rat/pull/632))
- *(account)* C1 candidate-DAG storage — ingest, refold + branch selection (V059) ([#627](https://github.com/cq27-dev/rag-rat/pull/627))

### Fixed

- *(account)* preserve bounded historical authority ([#642](https://github.com/cq27-dev/rag-rat/pull/642))
- *(account)* bound candidate ingest work ([#634](https://github.com/cq27-dev/rag-rat/pull/634))

### Other

- add content candidate DAG storage ([#644](https://github.com/cq27-dev/rag-rat/pull/644))
- tiered god-module sweep (watch, config, mcp, embed_loop) ([#630](https://github.com/cq27-dev/rag-rat/pull/630))
- explain Codex MCP approval for reviews ([#628](https://github.com/cq27-dev/rag-rat/pull/628))

## [0.17.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.16.0...rag-rat-core-v0.17.0) - 2026-07-11

### Added

- *(plugin)* one-step plugin for Claude Code + Codex — MCP via npx, harness-neutral hooks ([#569](https://github.com/cq27-dev/rag-rat/pull/569))
- *(account)* the stratified control-log fold (§11–§12) ([#622](https://github.com/cq27-dev/rag-rat/pull/622))
- *(dist)* ship an android/Termux binary + make npx @rag-rat/bin work there ([#616](https://github.com/cq27-dev/rag-rat/pull/616)) ([#621](https://github.com/cq27-dev/rag-rat/pull/621))
- *(account)* sync phase C1 wire/crypto layer + fold foundation ([#618](https://github.com/cq27-dev/rag-rat/pull/618))
- *(doctor)* reclaim freelist dead space with `doctor --vacuum` ([#574](https://github.com/cq27-dev/rag-rat/pull/574)) ([#613](https://github.com/cq27-dev/rag-rat/pull/613))

### Fixed

- *(mcp)* boot a dormant server outside a rag-rat repo instead of dying ([#603](https://github.com/cq27-dev/rag-rat/pull/603)) ([#611](https://github.com/cq27-dev/rag-rat/pull/611))

## [0.16.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.15.0...rag-rat-core-v0.16.0) - 2026-07-10

### Added

- *(papertrail)* PapertrailClient substrate — async provider trait, normalized DTOs, index/github → index/papertrail ([#600](https://github.com/cq27-dev/rag-rat/pull/600))
- *(reconcile)* heal op-log ghosts at index reconcile — idle-repo backstop ([#583](https://github.com/cq27-dev/rag-rat/pull/583)) ([#584](https://github.com/cq27-dev/rag-rat/pull/584))
- *(oracle)* check_library_usage — external-dependency contracts + deprecation from SCIP external_symbols ([#114](https://github.com/cq27-dev/rag-rat/pull/114)) ([#580](https://github.com/cq27-dev/rag-rat/pull/580))
- *(impact)* windowed file-pair change-coupling signal (V056) ([#570](https://github.com/cq27-dev/rag-rat/pull/570))

### Fixed

- *(schema)* don't let dev/test builds silently migrate the shared global DB ([#585](https://github.com/cq27-dev/rag-rat/pull/585)) ([#601](https://github.com/cq27-dev/rag-rat/pull/601))
- *(clones)* meter delta hydration against a work budget + memoize posting lists across bags ([#598](https://github.com/cq27-dev/rag-rat/pull/598)) ([#599](https://github.com/cq27-dev/rag-rat/pull/599))
- *(oplog)* self-healing per-node reconcile so no memory row is a permanent ghost ([#541](https://github.com/cq27-dev/rag-rat/pull/541)) ([#576](https://github.com/cq27-dev/rag-rat/pull/576))
- *(index)* hoist incremental reads off the SQLite write lock; commit authored writes durably ([#560](https://github.com/cq27-dev/rag-rat/pull/560)) ([#561](https://github.com/cq27-dev/rag-rat/pull/561))
- *(dream)* four-tier identifier resolution to kill memory_divergence false positives ([#559](https://github.com/cq27-dev/rag-rat/pull/559))

### Other

- *(watch)* event-scoped worktree-overlay refresh with a per-worktree diff basis ([#579](https://github.com/cq27-dev/rag-rat/pull/579))
- *(reconcile)* cover the fast-path freshness invariants left open by #530 ([#578](https://github.com/cq27-dev/rag-rat/pull/578))
- *(reconcile)* serve the skip-summary from a version-stamped column ([#575](https://github.com/cq27-dev/rag-rat/pull/575))
- *(search)* materialize the FTS scope view once per candidate query ([#568](https://github.com/cq27-dev/rag-rat/pull/568))
- *(reconcile)* classify skip-summary low-signal from one shared parse per file ([#572](https://github.com/cq27-dev/rag-rat/pull/572))
- *(benchmarks)* refresh kernel-index + SCIP-oracle numbers to v0.15.0 ([#563](https://github.com/cq27-dev/rag-rat/pull/563))

## [0.15.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.14.0...rag-rat-core-v0.15.0) - 2026-07-09

### Added

- *(oplog)* wire authoring into the live memory write path (phase B) ([#538](https://github.com/cq27-dev/rag-rat/pull/538))
- *(oplog)* op-authoring helper + full backfill of existing memories (phase B, unwired) ([#526](https://github.com/cq27-dev/rag-rat/pull/526))
- *(oracle)* live LSP client substrate for the incremental resolution path ([#531](https://github.com/cq27-dev/rag-rat/pull/531))
- *(index)* realign logical-symbol references on key-derivation drift ([#493](https://github.com/cq27-dev/rag-rat/pull/493)) ([#525](https://github.com/cq27-dev/rag-rat/pull/525))
- *(memory)* two-observation downgrade hysteresis for anchor status ([#492](https://github.com/cq27-dev/rag-rat/pull/492)) ([#528](https://github.com/cq27-dev/rag-rat/pull/528))
- *(clones)* scip refine mode — moniker-collapse same-symbol callees to Type-2 ([#512](https://github.com/cq27-dev/rag-rat/pull/512))
- *(dream)* expose dream + dream_review MCP tools ([#263](https://github.com/cq27-dev/rag-rat/pull/263)) ([#514](https://github.com/cq27-dev/rag-rat/pull/514))
- *(oplog)* persisted local device identity — CSPRNG keygen + single-row store (phase B) ([#523](https://github.com/cq27-dev/rag-rat/pull/523))
- *(oplog)* immutable stream identity — signed stream binding, stream-scoped store, fork quarantine (phase B S2) ([#511](https://github.com/cq27-dev/rag-rat/pull/511))
- *(memory)* default the memory surface to summary; extend it to the memory-query tools ([#508](https://github.com/cq27-dev/rag-rat/pull/508))
- *(oplog)* durable storage — layer-1 signed log + full-replay shadow projection (phase B C4) ([#504](https://github.com/cq27-dev/rag-rat/pull/504))
- *(oplog)* signed hash-chained entry envelope + ed25519 device keys (phase B C4 layer-1) ([#500](https://github.com/cq27-dev/rag-rat/pull/500))
- *(memory)* pending anchor status for in-flight worktree branches ([#496](https://github.com/cq27-dev/rag-rat/pull/496))
- *(oplog)* memory op model + deterministic projection fold (phase B §5.4) ([#495](https://github.com/cq27-dev/rag-rat/pull/495))
- *(clones)* per-generation df snapshot (clone_df_epoch) frees the live df to move ([#490](https://github.com/cq27-dev/rag-rat/pull/490))
- *(memory)* canonical content_hash primitive (phase B §5.5) ([#480](https://github.com/cq27-dev/rag-rat/pull/480))
- *(schema)* actionable version-skew messaging for the newer-schema refusal ([#487](https://github.com/cq27-dev/rag-rat/pull/487))
- *(index)* quiet-pass WAL checkpointing + doctor file-health warnings ([#486](https://github.com/cq27-dev/rag-rat/pull/486))
- *(locks)* per-database sync-session lock for the device-wide iroh endpoint ([#485](https://github.com/cq27-dev/rag-rat/pull/485))
- *(clones)* incremental delta maintenance of the persisted clone graph ([#477](https://github.com/cq27-dev/rag-rat/pull/477))
- *(memory)* typed cross-repo node edges (repo_node_edges) ([#476](https://github.com/cq27-dev/rag-rat/pull/476))
- *(memory)* polymorphic node payload + Task/Concept kinds ([#471](https://github.com/cq27-dev/rag-rat/pull/471))
- *(memory)* allow unanchored nodes (Concept / standalone Task) ([#466](https://github.com/cq27-dev/rag-rat/pull/466))

### Fixed

- *(oracle)* surface real I/O errors from the tool-output read ([#556](https://github.com/cq27-dev/rag-rat/pull/556))
- *(parser)* grow the stack for recursive tree-descent helpers + tripwire enforcing it ([#551](https://github.com/cq27-dev/rag-rat/pull/551))
- *(dream)* rank coverage_gap by scoped PageRank, not unscoped caller in-degree ([#261](https://github.com/cq27-dev/rag-rat/pull/261)) ([#515](https://github.com/cq27-dev/rag-rat/pull/515))
- *(watch)* run maintenance passes on a worker thread so events and the fleet trigger stay live ([#510](https://github.com/cq27-dev/rag-rat/pull/510))
- *(index)* carry retained committed rows across a HEAD move instead of re-deriving the repo ([#505](https://github.com/cq27-dev/rag-rat/pull/505))
- *(schema)* forward-migrate replay no longer stamps the dirty marker ([#501](https://github.com/cq27-dev/rag-rat/pull/501))
- *(memory)* relocation prefers the discriminator-matching twin, not plan order ([#494](https://github.com/cq27-dev/rag-rat/pull/494))
- *(index)* stop the gix status walk from descending into gitignored directories ([#481](https://github.com/cq27-dev/rag-rat/pull/481))
- *(watch)* gate the background clone-graph rebuild on a content quiet window ([#475](https://github.com/cq27-dev/rag-rat/pull/475))
- *(watch)* gate the linux-only drain_until_quiet test helper behind cfg ([#468](https://github.com/cq27-dev/rag-rat/pull/468))

### Other

- *(index)* route the heal path through the shared single-parse core ([#552](https://github.com/cq27-dev/rag-rat/pull/552))
- *(parser)* iterative depth-safe tree walks for symbol and edge extraction ([#540](https://github.com/cq27-dev/rag-rat/pull/540))
- *(init)* print the MCP connect command instead of auto-registering ([#542](https://github.com/cq27-dev/rag-rat/pull/542))
- complete the MCP tool catalog, trim the README, make the skills MCP-native ([#539](https://github.com/cq27-dev/rag-rat/pull/539))
- *(edges)* linearize contains_edges and containing_symbol; dedup the grammar match ([#533](https://github.com/cq27-dev/rag-rat/pull/533))
- *(reconcile)* skip the policy re-parse for current chunks on the backlog gate ([#529](https://github.com/cq27-dev/rag-rat/pull/529))
- *(index)* shared-parse low-signal classification + O(1) chunker line spans ([#527](https://github.com/cq27-dev/rag-rat/pull/527))

## [0.14.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.13.0...rag-rat-core-v0.14.0) - 2026-07-07

### Added

- *(index)* make explicit-config adoption loud and refuse empty indexes ([#458](https://github.com/cq27-dev/rag-rat/pull/458))
- *(memory)* apply [memory] surface="summary" to every drive-by renderer ([#426](https://github.com/cq27-dev/rag-rat/pull/426)) ([#453](https://github.com/cq27-dev/rag-rat/pull/453))

### Fixed

- reduce incremental index churn ([#460](https://github.com/cq27-dev/rag-rat/pull/460))
- *(watch)* prune linked worktree watches ([#454](https://github.com/cq27-dev/rag-rat/pull/454))
- *(cross-platform)* full request drain in the ollama-probe and embed stubs ([#446](https://github.com/cq27-dev/rag-rat/pull/446)) ([#452](https://github.com/cq27-dev/rag-rat/pull/452))
- *(cross-platform)* TOML path escaping, cookbook slash, clone teardown, Windows clippy ([#446](https://github.com/cq27-dev/rag-rat/pull/446)) ([#449](https://github.com/cq27-dev/rag-rat/pull/449))
- *(dream)* persist failed model attempts ([#450](https://github.com/cq27-dev/rag-rat/pull/450))

### Other

- derive enum strings with strum ([#457](https://github.com/cq27-dev/rag-rat/pull/457))
- Split oracle tests and antiunify module ([#456](https://github.com/cq27-dev/rag-rat/pull/456))
- *(watch)* cover watcher state helpers ([#455](https://github.com/cq27-dev/rag-rat/pull/455))

## [0.13.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.12.0...rag-rat-core-v0.13.0) - 2026-07-05

### Added

- *(dream)* human review surface — dream <id> --accept|--dismiss|--reset ([#262](https://github.com/cq27-dev/rag-rat/pull/262)) ([#440](https://github.com/cq27-dev/rag-rat/pull/440))
- *(dream)* run the verdict/compaction model on an ephemeral remote GPU (`[llm.dream.remote]`) ([#438](https://github.com/cq27-dev/rag-rat/pull/438))
- *(dream)* v2 memory passes — reality verdicts + compact summaries ([#428](https://github.com/cq27-dev/rag-rat/pull/428))
- *(index)* global database by default, rag-rat consolidate importer ([#402](https://github.com/cq27-dev/rag-rat/pull/402)) ([#419](https://github.com/cq27-dev/rag-rat/pull/419))
- *(index)* generation-staged full rebuild, per-repo write locks (V043) ([#416](https://github.com/cq27-dev/rag-rat/pull/416))
- *(index)* repo-scoped clones, oracle, reconcile, memories (V042) ([#415](https://github.com/cq27-dev/rag-rat/pull/415))
- *(search)* repo-scoped FTS and papertrail queries (V041) ([#414](https://github.com/cq27-dev/rag-rat/pull/414))
- *(index)* V040 — repo_id scoping on core tables, scope view, gc (#398 phase A3) ([#413](https://github.com/cq27-dev/rag-rat/pull/413))
- *(schema)* V039 — move per-repo meta singletons to repo_meta (#397 phase A2) ([#412](https://github.com/cq27-dev/rag-rat/pull/412))
- *(schema)* V038 repos registry, repo identity, and data_dir helper (#396 phase A1) ([#408](https://github.com/cq27-dev/rag-rat/pull/408))

### Fixed

- *(locks)* release flock explicitly on drop — close alone races fork-inherited fds ([#409](https://github.com/cq27-dev/rag-rat/pull/409)) ([#410](https://github.com/cq27-dev/rag-rat/pull/410))

## [0.12.0](https://github.com/cq27-dev/rag-rat/compare/rag-rat-core-v0.11.0...rag-rat-core-v0.12.0) - 2026-07-02

### Added

- *(clones)* make the write-time clone-check size guard mode-aware (#296 phase 4) ([#393](https://github.com/cq27-dev/rag-rat/pull/393))
- *(clones)* bounded postings fast path in the write-time clone check (#296 phase 3) ([#392](https://github.com/cq27-dev/rag-rat/pull/392))
- *(clones)* populate clone_subblock_postings in the generation-staged precompute (#296 phase 2) ([#391](https://github.com/cq27-dev/rag-rat/pull/391))
- *(schema)* V037 — clone_subblock_postings + postings_written gate (#296 phase 1) ([#390](https://github.com/cq27-dev/rag-rat/pull/390))
- *(log)* config-gated tracing debug log + hook-embedding repro harness ([#377](https://github.com/cq27-dev/rag-rat/pull/377))

### Fixed

- *(embed)* a fresh index adopts the configured embedding model, not the hash fallback ([#394](https://github.com/cq27-dev/rag-rat/pull/394)) ([#395](https://github.com/cq27-dev/rag-rat/pull/395))

### Other

- *(clones)* BFS the subject's component in clones_for_symbol on the live path ([#270](https://github.com/cq27-dev/rag-rat/pull/270)) ([#384](https://github.com/cq27-dev/rag-rat/pull/384))
- *(store)* rewrap two doc comments to satisfy nightly rustfmt ([#385](https://github.com/cq27-dev/rag-rat/pull/385))
- *(reconcile)* stream estimated_reconcile_jobs — the last materialize-everything site ([#64](https://github.com/cq27-dev/rag-rat/pull/64)) ([#383](https://github.com/cq27-dev/rag-rat/pull/383))
- *(reconcile)* stream embedding_reconcile_plan candidate materialization ([#379](https://github.com/cq27-dev/rag-rat/pull/379)) ([#382](https://github.com/cq27-dev/rag-rat/pull/382))
- *(embed)* split index/ai/reconcile.rs god-module into cohesive siblings ([#375](https://github.com/cq27-dev/rag-rat/pull/375))
- *(clones)* split god-module query_api/clones.rs into cohesive siblings ([#368](https://github.com/cq27-dev/rag-rat/pull/368))
- extract oversized inline test modules into sibling files ([#366](https://github.com/cq27-dev/rag-rat/pull/366))
- dedup credential and row-count helpers ([#364](https://github.com/cq27-dev/rag-rat/pull/364))

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
