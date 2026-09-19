# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.23.3](https://github.com/cq27-dev/rag-rat/compare/v0.23.2...v0.23.3) - 2026-09-19

### Added

- *(sync)* persist and report unresolved table-row causes ([#1344](https://github.com/cq27-dev/rag-rat/pull/1344))
- *(sync)* key memory summaries by memory so a regeneration syncs as one upsert ([#1320](https://github.com/cq27-dev/rag-rat/pull/1320))
- *(oplog)* persist permanent account pins and block unsupported authority ([#1352](https://github.com/cq27-dev/rag-rat/pull/1352))
- *(sync)* restate a writer's own deletes so compaction can reclaim the entries that first stated them
- *(oplog)* make a control v2 cut's pre-cut manifest durable ([#1364](https://github.com/cq27-dev/rag-rat/pull/1364))
- *(oplog)* fold control v2 register effects into a pinned account's authority history ([#1311](https://github.com/cq27-dev/rag-rat/pull/1311)) ([#1363](https://github.com/cq27-dev/rag-rat/pull/1363))
- *(oplog)* fold a pinned account from its checkpoint instead of retracting it ([#1361](https://github.com/cq27-dev/rag-rat/pull/1361))
- *(oplog)* execute owner-authorized control v2 cuts over a pinned checkpoint ([#1355](https://github.com/cq27-dev/rag-rat/pull/1355))
- *(oplog)* add isolated control v2 grammar and replay planning ([#1351](https://github.com/cq27-dev/rag-rat/pull/1351))
- *(oplog)* verify externally pinned legacy checkpoints for control v2 ([#1350](https://github.com/cq27-dev/rag-rat/pull/1350))

### Fixed

- *(oracle)* reject an unknown corpus tool or tier when the profiles load
- *(oplog)* judge invite-reservation expiry by the wall clock ([#1365](https://github.com/cq27-dev/rag-rat/pull/1365))
- *(oplog)* retract a pinned account's projections and park its table entries once ([#1360](https://github.com/cq27-dev/rag-rat/pull/1360))
- *(sync)* pad discovery announcements to hide roster size ([#1349](https://github.com/cq27-dev/rag-rat/pull/1349))
- *(core)* advance migration replay test pin to schema v129 ([#1347](https://github.com/cq27-dev/rag-rat/pull/1347))
- *(sync)* retain outstanding suffix tips after floor adoption ([#1346](https://github.com/cq27-dev/rag-rat/pull/1346))
- *(core)* reject malformed local distill source tokens during hydration
- *(core)* raise rebind's durability guard after the backfill it runs
- *(core)* omit full error details from watcher test diagnostics
- *(core)* roll back failed overlay refresh commits
- *(db)* discard derived verification rows during late repo merge ([#1335](https://github.com/cq27-dev/rag-rat/pull/1335))
- *(papertrail)* read a drifted error-class token as an unknown failure
- *(oracle)* gate indexed definition documents on index-vs-disk drift
- *(query)* recognize graph seed extensions from the language registry
- *(clones)* stop charging the template-lane cell budget for skipped members
- *(oplog)* keep a chain closed once a register join cannot decide it ([#1368](https://github.com/cq27-dev/rag-rat/pull/1368)) ([#1374](https://github.com/cq27-dev/rag-rat/pull/1374))
- *(oplog)* correct stale control v2 composition docs and pin a forked revocation ([#1366](https://github.com/cq27-dev/rag-rat/pull/1366))
- *(oplog)* park a control v2 cut whose cited mint was not supplied ([#1358](https://github.com/cq27-dev/rag-rat/pull/1358))
- *(oplog)* identify malformed stored ids by their actual field
- *(sync)* bound an entries page by bytes as well as entry count ([#1371](https://github.com/cq27-dev/rag-rat/pull/1371)) ([#1373](https://github.com/cq27-dev/rag-rat/pull/1373))
- *(sync)* refuse a writer nonce presented to the pairing flow as unknown
- *(sync)* report enrollment transport failures as transport, not storage
- *(sync)* report a stalled session peer as a timeout, not a protocol violation
- *(sync)* refuse an over-cap account-lane frame before writing it

### Other

- *(cli)* share command parsing, locking and maintenance reports ([#1336](https://github.com/cq27-dev/rag-rat/pull/1336))
- *(oplog)* share account authority operations and type identifiers ([#1338](https://github.com/cq27-dev/rag-rat/pull/1338))
- *(core)* type clone delta report status tokens
- *(sync)* make SyncAlpn the crate's ALPN type for dialers and dispatchers
- *(mcp)* correct test references that name things that no longer exist
- *(base)* finish moving base's fixtures onto ScratchDir and test_git
- *(base)* keep the embedding registry to one row per model
- *(base)* route every hex encoding through base::hash
- *(base)* name the global and per-worktree listener lock families
- *(base)* settle the config enums on strum tokens and one config parser
- *(base)* back Language tokens with strum and rename as_str to as_db_str
- *(oracle)* pass the drift-gate snapshots as one ShaSnapshots bundle
- *(core)* share index pipeline contracts and finalization ([#1341](https://github.com/cq27-dev/rag-rat/pull/1341))
- *(core)* clarify query and embedding API boundaries ([#1340](https://github.com/cq27-dev/rag-rat/pull/1340))
- *(core)* share graph extraction rules and clarify healing phases ([#1339](https://github.com/cq27-dev/rag-rat/pull/1339))
- *(core)* avoid dumping sync effects in assertion failures
- *(core)* move the remaining inline test tails into #[path] sibling files
- *(core)* read sync_driver's kv timestamps through typed meta accessors
- *(core)* split grep-augment's compose into its three lanes
- *(core)* carry the discovery advertisement's identity as one AdvertisementIdentity
- *(core)* reattach drain_synced_memory's doc and drop prompts.rs's module-wide dead_code allow
- *(core)* give the stream seal-policy and access-mode intents a token surface
- *(core)* route every lexical search entry point through LexicalQuery
- *(core)* share eval's recall predicates and pass its search knobs as SearchTuning
- *(core)* remove the unused search::hybrid and search::semantic modules
- *(core)* merge eval expectation lanes through one accessor pair
- *(core)* build signed node content and edge specs from the rows that carry them
- *(core)* split sync_driver's reconcile and resident start into named phases
- *(core)* share distill source tokens in the thread identity module
- *(core)* run distill extract and drain through one transaction helper
- *(core)* colocate watcher tests with their modules
- *(core)* split consolidation by import responsibility
- *(core)* name watcher pass inputs and lifecycle stages
- *(core)* document overlay transaction failure policies
- *(core)* share connection scope keys and view columns
- *(core)* report watcher overlay failures through tracing
- *(core)* pair on-open repairs with their read-only gates
- *(core)* bump overlay revisions in their matching change arms
- *(core)* name consolidation child slices and metadata sides
- *(core)* carry checkout keys through overlay scopes
- *(core)* enforce poison tripwire coverage from the schema registry
- *(core)* describe poison coverage against the current schema
- *(core)* align lifecycle documentation with its functions
- *(db)* move EdgeConfidence below the read layer and hang its ladders off it
- *(query)* type the persisted memory kind, status, confidence, source and relocation reason
- *(db)* tighten the digest lane and chunk-text decoder primitives
- *(dream)* type finding status and expose a typed kind on worklist and review rows
- *(llm)* cover endpoint redaction in the crate that owns it
- *(base)* name the workspace dir, legacy database, and imported marker
- *(oracle)* type the persisted run status as RunStatus
- *(papertrail)* isolate the reference-driven sync lane in ref_sync
- *(papertrail)* type the report's pause reason and sync-error status
- *(mcp)* drive tool schema checks from a complete per-tool table
- *(mcp)* turn the hook listener's task body into an ordinary function
- *(mcp)* export only the catalog's contract from the tools module
- *(mcp)* declare the resurface window once for both dedup lanes
- *(mcp)* route the clone file lens through file_lens
- *(mcp)* share the lens discovery record and serve options between serve paths
- *(mcp)* escalate graph completeness risk on the typed report
- *(mcp)* build the graph tools' traversal options in one place
- *(mcp)* declare the symbol selector and handle encoding once
- *(mcp)* stop advertising search knobs the plain full-text tools discard
- *(base)* share the config test fixtures instead of copying them
- *(base)* honour the crate's own clock, version, and import seams
- *(base)* resolve [index] root through one helper in Config::load
- *(base)* derive the retention sweep's owned log names from Role
- *(db)* split migrations into ladder infrastructure and per-era step modules
- *(db)* move the migration ladder's interleaved test modules into their own files
- *(db)* render PurgeIdSet's live subquery and temp capture from one descriptor
- *(db)* name the registration outcomes in a Registration enum
- *(db)* extract the adoption re-point phase into repoint_scoped_rows
- *(db)* route sqlite_master existence probes through one helper
- *(db)* pin the late-merge periphery list to its A5 prefix and fix stale coverage prose
- *(db)* retire the stale migration-registration doc and tests that cannot fail
- *(db)* carry each migration's ledger-atomic and refold flags in the roster
- *(db)* build SchemaStatus through one constructor
- *(db)* correct stale module headers and unresolvable cross-crate doc links
- *(papertrail)* split the mirror runner's test tail by concern
- *(papertrail)* route ref grammars on one reported MatchedShape
- *(papertrail)* fold the configless legacy-grammar fallback into parse_tracker_refs
- *(papertrail)* declare the transport's GitHub quota quirks as ProviderQuirks
- *(papertrail)* bind delete_item's item identity once
- *(papertrail)* bind the remaining enum tokens in SQL instead of spelling literals
- *(papertrail)* state the ref-kind claim rank once on RefKind
- *(papertrail)* type the mirror cursor's processed item kind as ItemKind
- *(papertrail)* derive the error-class tokens and bind the pause class in SQL
- *(papertrail)* name the persisted-health, symbol-span and issue-target shapes
- *(papertrail)* key evidence coalescing on RecordKey
- *(papertrail)* share the previous-civil-day rule between mirror and GitLab
- *(papertrail)* reattach the commit-closer doc block to its function
- *(papertrail)* route the tag and token fingerprints through hex_lower
- *(llm)* make lib.rs an index of the crate's surface
- *(llm)* correct provisioning visibility and stale module paths
- *(llm)* name the sweep's failure-breaker budget once
- *(llm)* build the tuner's and the benchmark's probe workload in one place
- *(llm)* put the cookbook's platform process handling behind named shims
- *(llm)* derive the embedding provision deadline from provision_deadline
- *(llm)* type CookbookInput's backend and capability
- *(llm)* fold the embedder's BuildParams into ProvisionedEmbedderParams
- *(llm)* build every Authorization header in the shared http transport
- *(oracle)* split backend tests by subject and narrow the lsp dead-code allow
- *(oracle)* drop the stale phase-1 module headers and index every module
- *(oracle)* collect store.rs row iterators instead of hand-rolled loops
- *(oracle)* split the live pass's per-definition verdict write out of resolve_one_file
- *(oracle)* give check_library_usage's cost-ordered phases their own functions
- *(oracle)* spell the edge_oracle.kind SQL lists from OracleResolutionKind
- *(oracle)* declare each live backend's moniker source on its registry entry
- *(oracle)* share the edge-join-candidate SELECT and row mapper
- *(oracle)* pin the corpus profile hash
- *(oracle)* record an oracle run from a named OracleRunRecord
- *(oracle)* express every drift gate through one pinning predicate
- *(query)* share persisted memory test fixtures
- *(query)* move memory tests into sibling modules
- *(query)* curate graph and impact exports and remove row collectors
- *(query)* make scoped_weighted_fan_in delegate to its batched sibling
- *(query)* one short_name helper, and distinct names for the two qualified-symbol rules
- *(query)* keep ImpactCategory typed through ImpactSurface
- *(query)* read import/export dependents through one shared query
- *(query)* share the traversal hop SELECT and summary counts between directions
- *(query)* split important_symbols into its load, graph, seed and hydrate phases
- *(query)* compose forward_visibility_filter from its three clauses
- *(query)* name graph_meta's call-edge set and count/list predicates
- *(query)* read binding resolution shadows through one named fragment
- *(query)* name the live-memory status predicate once
- *(dream)* match divergence evidence on the resolver's own resolution labels
- *(dream)* keep the finding builders and imports in reading order
- *(dream)* move the verdict grounding guards into their own module
- *(dream)* write memory_reality through one UPSERT for both outcomes
- *(dream)* build the pack content-line set once per grounding check
- *(dream)* share each model pass's scope and failure-stamp preamble
- *(dream)* name the unassigned repo sentinel through LEGACY_REPO_ID
- *(clones)* share the RefineMember fixtures and import test names directly
- *(clones)* pass the anti-unify class as one ClassView
- *(clones)* drop the redundant per-group cancel poll in the maximality pass
- *(clones)* split coherence_split_cancellable into named stages
- *(clones)* gather member-run tokens and fold agreement in one place each
- *(clones)* share one annotation-type scan
- *(clones)* drive both LCS lanes through the budgeted path
- *(clones)* type SigParam::type_source as an enum
- *(clones)* name the v1 confidence thresholds and derive band steps
- *(clones)* list str_escaped_char in the shared string-body kinds
- *(clones)* restore the opening sentence of the MemberStatement doc
- *(clones)* drop the crate-visible ClassAlignment and OccSpan re-export
- *(clones)* write the star align's skip-and-sample arm once
- *(clones)* make CellBudget own its spent/exhausted state
- *(oplog)* pin the last two rows — absent-verdict default and the reservation headroom refusal ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1392](https://github.com/cq27-dev/rag-rat/pull/1392))
- *(oplog)* a condemned removal must not tombstone the device it names ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1390](https://github.com/cq27-dev/rag-rat/pull/1390))
- *(oplog)* pin the mint gate a demote's peer-supplied owner_id reaches ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1388](https://github.com/cq27-dev/rag-rat/pull/1388))
- *(oplog)* make every plan_replay bound and refusal attributable ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1386](https://github.com/cq27-dev/rag-rat/pull/1386))
- *(oplog)* pin every conjunct of the v2 candidate admission check ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1385](https://github.com/cq27-dev/rag-rat/pull/1385))
- *(oplog)* pin the two structural refusals in the replay planner ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1384](https://github.com/cq27-dev/rag-rat/pull/1384))
- *(oplog)* compose two applied cuts so the ordering and epoch rows are reachable ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1383](https://github.com/cq27-dev/rag-rat/pull/1383))
- *(oplog)* complete both sides of the v2 register preconditions ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1382](https://github.com/cq27-dev/rag-rat/pull/1382))
- *(oplog)* let the determinism fixtures reach two of the fold's sort components ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1381](https://github.com/cq27-dev/rag-rat/pull/1381))
- *(oplog)* route the executor's two verdict maps through pure functions ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1380](https://github.com/cq27-dev/rag-rat/pull/1380))
- *(oplog)* pin the three preconditions a v2 revocation must satisfy ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1379](https://github.com/cq27-dev/rag-rat/pull/1379))
- *(oplog)* pin the control v2 ancestry walk's step conditions ([#1369](https://github.com/cq27-dev/rag-rat/pull/1369)) ([#1376](https://github.com/cq27-dev/rag-rat/pull/1376))
- *(oplog)* trim the checkpoint legacy trace to what execution reads ([#1359](https://github.com/cq27-dev/rag-rat/pull/1359))
- *(sync)* pin v1 revocation credit and plan explicit account upgrades ([#1343](https://github.com/cq27-dev/rag-rat/pull/1343))
- *(oplog)* move large test modules to sibling files
- *(oplog)* retain typed pending reasons in ingest outcomes
- *(oplog)* share the identity-keyed wire set rules
- *(oplog)* carry row keys and clocks through merge bookkeeping
- *(oplog)* name projection ordering and assembly phases
- *(oplog)* name the persisted pre-context stream placeholder
- *(oplog)* name the (lamport, entry hash) chain cursor
- *(oplog)* add field-aware constructors for stored fixed-width ids
- *(oplog)* type the /5 scope id as ScopeId
- *(oplog)* derive the node-status and override-action wire tokens with strum
- *(oplog)* write the signed and stream-identity preimages through VecEncoderExt
- *(oplog)* retire the stale crate header, dead-code allows and mangled comments
- *(sync)* pin the two peer-facing caps a session cannot do without ([#1372](https://github.com/cq27-dev/rag-rat/pull/1372)) ([#1378](https://github.com/cq27-dev/rag-rat/pull/1378))
- *(sync)* move the large inline test tails into tests modules and share the in-memory stores
- *(sync)* split endpoint.rs into a module directory by concern
- *(sync)* type the sync_invites role column as a StoredInviteKind
- *(sync)* share the redeem-under-writer-lock scaffold between both invite kinds
- *(sync)* expose transport error sources and derive discovery errors
- *(sync)* share one bounded dial between the two enrollment dialers
- *(sync)* bound every lane's frame IO through one deadline helper
- *(sync)* route every length-prefixed framing through one reader and writer
- *(sync)* carry the table session's idle timeout in its limits and pass capabilities, not bools
- *(sync)* generate the op-log stores' NodeAuth impls from one macro
- *(sync)* name lanes without ALPN-like suffixes and map every module in the crate docs
- *(sync)* pin framing bytes and refusal boundaries before consolidation

## [0.23.2](https://github.com/cq27-dev/rag-rat/compare/v0.23.1...v0.23.2) - 2026-09-13

### Added

- *(sync)* publish each symbol anchor's scope so a rebind between twin impls follows ([#1303](https://github.com/cq27-dev/rag-rat/pull/1303))
- *(sync)* relay contributors' logs and memories through the owner's sessions ([#1293](https://github.com/cq27-dev/rag-rat/pull/1293))
- *(sync)* admit a grantor's relayed grantee entries in its sessions ([#1292](https://github.com/cq27-dev/rag-rat/pull/1292))

### Fixed

- *(sync)* converge a returning memory's held rows when no baseline was parked ([#1309](https://github.com/cq27-dev/rag-rat/pull/1309))
- *(sync)* keep a binding's authored anchor on the wire and resolve it locally ([#1306](https://github.com/cq27-dev/rag-rat/pull/1306))
- *(sync)* re-apply a synced memory's anchors and source hash when its author republishes ([#1275](https://github.com/cq27-dev/rag-rat/pull/1275))
- *(oplog)* let a revoking cut vouch for the ops authored concurrently with it ([#1313](https://github.com/cq27-dev/rag-rat/pull/1313))
- *(sync)* keep a removed synced memory's bindings and park its anchor baseline ([#1305](https://github.com/cq27-dev/rag-rat/pull/1305))
- *(sync)* keep one foreign account's pull failure from stopping the rest ([#1300](https://github.com/cq27-dev/rag-rat/pull/1300))
- *(consolidate)* leave other accounts' memories out of the legacy import ([#1299](https://github.com/cq27-dev/rag-rat/pull/1299))
- *(oplog)* measure content freshness against the held control log ([#1296](https://github.com/cq27-dev/rag-rat/pull/1296))
- *(sync)* clear the drain watermarks when a repository is purged ([#1289](https://github.com/cq27-dev/rag-rat/pull/1289))
- *(oplog)* credit a revoking cut with the control ops it condemns ([#1302](https://github.com/cq27-dev/rag-rat/pull/1302))
- *(oplog)* never compact away an entry that still carries a row ([#1291](https://github.com/cq27-dev/rag-rat/pull/1291))
- *(sync)* count only unaccepted candidates against the content receive budgets ([#1288](https://github.com/cq27-dev/rag-rat/pull/1288))
- *(sync)* bound the account/content session's writes by the idle timeout ([#1287](https://github.com/cq27-dev/rag-rat/pull/1287))

### Other

- *(core)* type the reconcile report status as ReconcileStatus
- *(cli)* render the Embedding step's pickers through one cursor_list widget
- *(cli)* type the maintenance-hook report status and lift the long sync notes
- *(cli)* dispatch sync commands through one exhaustive match
- *(cli)* type the wizard's remote-mode choice instead of a usize
- *(cli)* share the write-and-setup tail between the two init flows
- *(cli)* route the blocking CLI write-lock acquisitions through one helper
- *(cli)* give the --json fork one helper and one flag conversion
- *(cli)* read the clock through rag_rat_base::time::now_ms and drop the dream pass unwraps
- *(dream)* move the verify zero-work probe beside the verdict runner
- *(core)* split consolidate::run_inner into named phases
- *(base)* derive per-repo flight paths from one FlightKind
- *(base)* back the config token enums with strum
- *(llm)* pass the throughput sweep entry points parameter structs
- *(core)* pass the active checkout through query_api as a CheckoutRef
- *(core)* key checkout scope resolution and set_context on CheckoutKey
- *(oracle)* pass the checkout scope as one CheckoutRef instead of a string pair
- *(core)* carry the traversal Direction through query_api instead of a bool
- *(query)* replace the reverse traversal flag with a Direction enum
- *(core)* re-attach the contribution_owner_account doc comment to its function
- *(core)* give memory_write's tests one home beside their modules
- *(core)* split memory_write authoring into ownership, reconcile and grants modules
- *(core)* split EventLoop::run into message handlers over a LoopState
- *(core)* share one IntervalClock and request-worker spawner in the watcher
- *(core)* lift distill extraction and record writing into named phases
- *(core)* share one clear_model_junctions between extraction and the drain
- *(core)* key distill extraction by the shared ThreadKey
- *(core)* parse closing-edge closer kind and source into their enums
- *(core)* partition eval expectations into hits and misses in one helper
- *(core)* collect rusqlite rows with the collect idiom instead of push loops
- *(core)* route every untrusted prompt field through bounded_untrusted
- *(core)* split reconcile_with_options_progress into named phases
- *(core)* share one fresh_conn fixture across the schema bootstrap tests
- *(core)* type the active-model provenance flag
- *(core)* make the options form the one public reconcile entry point
- *(core)* share compare_graph_to_scip's report envelope and contradiction mapping
- *(core)* build the early reconcile reports from the empty-report constructor
- *(core)* import rag_rat_query modules in the query_api graph, importance and memory files
- *(core)* split the poison-sibling harness into seed, assert and test files
- *(core)* split graph_index into logical-key, reference, drift-heal and freshness modules
- *(core)* keep the language edge extractor entry points thin
- *(core)* give tree-sitter child scans an owning named_children iterator
- *(core)* split resolve_symbol into named resolution stages
- *(core)* type the persisted edge resolution reason
- *(core)* build both resolve drivers' requests through one resolve_reference
- *(core)* share the resolver's unique-or-logical-variant rule across stages
- *(core)* read receiver-type retry rules off ReceiverTypeIdentity
- *(clones)* pass the refine inputs as one RefineRequest
- *(dream)* drop the vestigial queue budget parameter
- *(query)* curate the memory module index with explicit re-exports
- *(query)* collapse the three file-mention section helpers into one
- *(core)* share the overlay-refresh transaction envelope between both routes
- *(core)* give finalize_overlay_refresh a parameter struct and a manifest signal
- *(core)* move the consolidate, remove and adoption-hint test modules to sibling files
- *(core)* share the consolidation repo_meta reads and single-token merge
- *(core)* build the IndexDatabase handle in one constructor
- *(core)* name the overlay-basis meta key in one helper
- *(core)* route the git-availability test guard through test_git
- *(core)* curate the index module's re-export surface
- *(core)* lift the rebuild publish and incremental open/write phases into methods
- *(core)* route the index transaction dance through in_immediate_txn
- *(core)* name the file-write and write-origin modes
- *(core)* type the persisted chunk kind as ChunkKind
- *(core)* name the incremental pass's write effects
- *(core)* read the scope row once for both incremental skip gates
- *(core)* name the chunk-split line caps and anchor constants
- *(core)* ask GitChangedPaths whether it touched a Cargo.toml
- *(db)* settle the kv meta accessors on rusqlite::Result and move the watch-placement flush out
- *(db)* delete the duplicate meta reader
- *(papertrail)* type ref kind, ref source kind and doc kind as strum enums
- *(llm)* return a named ProvisionedEmbedding from the provisioning path
- *(llm)* re-attach detached doc comments and drop stale provider notes
- *(deps)* audit and bump the Rust and cookbook dependencies ([#1315](https://github.com/cq27-dev/rag-rat/pull/1315))
- *(mcp)* answer every symbol tool's select_symbol outcome through one helper
- *(mcp)* build graph_tool's traversal options once
- *(mcp)* split the hook listener's serve_one and type its request kind
- *(mcp)* serve the file and symbol-hop lenses through shared route bodies
- *(mcp)* build every symbol tool's selector through one spelling
- *(mcp)* pin that every catalog tool has its own description and arg schema
- *(base)* settle base tests on ScratchDir and split config tests by subject
- *(base)* give Config a Default for struct-update construction
- *(base)* share the remote-serving validation between chat and embedding blocks
- *(base)* split Config::load into named resolution phases
- *(base)* pass the log policy to sweep_retention by name
- *(base)* move ConfigError into config/error.rs
- *(base)* route worktree_hash through hash::hex_lower
- *(base)* hold the data-dir env lock in the config tests that read the global store path ([#1290](https://github.com/cq27-dev/rag-rat/pull/1290))
- *(db)* split register_repo_inner and name its root-recording mode
- *(db)* declare the additive migration roster in one ordered macro block
- *(db)* type the purge id sets as an enum
- *(db)* route every guarded ALTER TABLE ADD COLUMN through add_column_if_missing
- *(papertrail)* lift GitHub's Search continuation out of items_page
- *(papertrail)* thread the mirror walk through a MirrorWalk context
- *(papertrail)* type the attested walk phase and split its node parsers
- *(papertrail)* name the attested walk's SQL questions and bind their enum tokens
- *(papertrail)* route ref grammar passes through one GrammarScope
- *(papertrail)* share the mirror cursor's walk-progress reset
- *(papertrail)* generate the ProviderClient trait delegation from one method list
- *(llm)* split the cookbook provisioning core into named steps
- *(llm)* derive probe embedder params from one base instead of four literals
- *(llm)* route the eval dim probe through the shared embed request path
- *(llm)* share one OpenAI-compatible HTTP transport between the embed and chat clients
- *(llm)* share the endpoint authority parse between log sanitizing and loopback checks
- *(oracle)* give run_in_tx's drift gates a DriftGates home and lift its trailing passes
- *(oracle)* split live_oracle_pass into a version transition, a per-file resolver, and a shared status
- *(oracle)* collapse the live pass's repeated drift and readiness blocks
- *(oracle)* key the live prerequisite gate on readiness and put batch markers on BatchSpec
- *(oracle)* type the heuristic confidence instead of matching string literals
- *(oracle)* fix four doc comments that contradict their code
- *(query)* split impact_surface_report_for_symbol into gather and truncation phases
- *(query)* bind traversal parameters as typed values and drop the padding predicate
- *(query)* give the wide memory report structs a construction seam
- *(query)* type memory binding kinds and anchor statuses
- *(query)* hoist the confidence ORDER BY ladder into one const
- *(query)* splice the resolved-operator guard instead of re-typing it
- *(query)* deduplicate like_escape and untangle misleading helper name pairs
- *(dream)* share one ask-once-retry-once loop between the model passes
- *(dream)* type the finding status lifecycle
- *(dream)* make finding kinds a strum-backed FindingKind
- *(dream)* back Verdict and Direction with strum tokens
- *(dream)* build each failure stamp once and reuse it
- *(clones)* re-attach the fingerprint_symbols doc comment to its function
- *(clones)* split anti_unify_with_budget into named stages
- *(clones)* thread the anti-unify descent through a Descent context
- *(clones)* share one CellBudget between the fidelity and template lanes
- *(clones)* measure class fidelity through one function and a result struct
- *(clones)* keep one literal-leaf predicate and one string-body kind list
- *(clones)* give Confidence one derived ordering
- *(clones)* name the v2 scoring constants and share the non-value-majority test
- *(clones)* scope the dead_code allowance to the items that need it
- *(oplog)* rename the account-private entry-hash aliases to AccountEntryHash
- *(oplog)* move the largest table-sync test modules to sibling files
- *(oplog)* share the account test fixtures and one fixed-width reader
- *(oplog)* split fold_account_pass into its named depth stages
- *(oplog)* split the table-sync schema lint into named rules
- *(oplog)* give op-log entry hashes an EntryHash newtype
- *(oplog)* type the account entry status instead of passing strings
- *(oplog)* share one pin-aware branch selection between content and secrets
- *(oplog)* author owner and grantee content batches through one skeleton
- *(oplog)* share one pre-verify queue between account and content ingest
- *(oplog)* pass table-sync ingest context as parameter structs
- *(oplog)* name each projector-generation table snapshot once
- *(oplog)* read a roster fact through one helper
- *(oplog)* select the owner authority chain by enum, not a SQL string
- *(oplog)* share one LWW clock body between row clocks and tombstones
- *(oplog)* absorb the infallible CBOR write once in an encoder seam
- *(oplog)* share one CBOR helper set from cbor.rs
- *(oplog)* route sign_entry through sign_entry_from_op_bytes
- *(sync)* split enrollment.rs into a module directory by job
- *(sync)* screen invite redemptions through one function per invite kind
- *(sync)* share the authenticated dial and the reconcile round tally
- *(sync)* share the role-ordered completion acknowledgement between session lanes
- *(sync)* route acceptor streams through a SyncAlpn enum and one dispatch tail
- *(sync)* derive the sync error types with thiserror
- *(sync)* carry the session serving policy as SessionLimits
- *(sync)* share one token bucket between the global accept and egress limiters

## [0.23.1](https://github.com/cq27-dev/rag-rat/compare/v0.23.0...v0.23.1) - 2026-09-10

### Added

- *(sync)* subscribe from a checked-in stream locator, pinned on first use ([#1266](https://github.com/cq27-dev/rag-rat/pull/1266))
- *(cli)* expose tool catalog as native subcommands ([#1273](https://github.com/cq27-dev/rag-rat/pull/1273))
- *(memory)* demote a synced memory whose anchored text this checkout no longer holds ([#1264](https://github.com/cq27-dev/rag-rat/pull/1264))
- *(index)* read a same-file declared return under any callee name ([#1239](https://github.com/cq27-dev/rag-rat/pull/1239))
- *(sync)* mirror a published owner's memories read-only with sync subscribe ([#1156](https://github.com/cq27-dev/rag-rat/pull/1156)) ([#1241](https://github.com/cq27-dev/rag-rat/pull/1241))
- *(sync)* carry the source text a memory's author anchored to ([#1213](https://github.com/cq27-dev/rag-rat/pull/1213)) ([#1235](https://github.com/cq27-dev/rag-rat/pull/1235))
- *(sync)* sweep anchors for memories authored before the op existed ([#1212](https://github.com/cq27-dev/rag-rat/pull/1212)) ([#1228](https://github.com/cq27-dev/rag-rat/pull/1228))
- *(sync)* author a memory's anchor set so peers can seed it ([#1211](https://github.com/cq27-dev/rag-rat/pull/1211)) ([#1227](https://github.com/cq27-dev/rag-rat/pull/1227))
- *(sync)* seed a synced memory's bindings from its anchor snapshot ([#1210](https://github.com/cq27-dev/rag-rat/pull/1210)) ([#1222](https://github.com/cq27-dev/rag-rat/pull/1222))
- *(oplog)* project a node's portable anchor set ([#1209](https://github.com/cq27-dev/rag-rat/pull/1209)) ([#1221](https://github.com/cq27-dev/rag-rat/pull/1221))
- *(dream)* keep the reasoning when compacting a memory ([#1207](https://github.com/cq27-dev/rag-rat/pull/1207))
- *(mcp)* declare the per-request worktree parameter on every read tool ([#1206](https://github.com/cq27-dev/rag-rat/pull/1206))
- *(core)* gate the grep-augment memory lane on relevance ([#1205](https://github.com/cq27-dev/rag-rat/pull/1205))
- *(oplog)* add the node_anchors content op carrying portable binding facts ([#1208](https://github.com/cq27-dev/rag-rat/pull/1208)) ([#1216](https://github.com/cq27-dev/rag-rat/pull/1216))
- *(sync)* collapse the writer-grant paste flow into one invite ticket ([#1179](https://github.com/cq27-dev/rag-rat/pull/1179)) ([#1195](https://github.com/cq27-dev/rag-rat/pull/1195))
- *(oplog)* the fold rejects a StreamGrant on a private stream ([#1178](https://github.com/cq27-dev/rag-rat/pull/1178)) ([#1194](https://github.com/cq27-dev/rag-rat/pull/1194))
- *(sync)* add reason-driven sync revoke and the sync grants listing ([#1177](https://github.com/cq27-dev/rag-rat/pull/1177)) ([#1193](https://github.com/cq27-dev/rag-rat/pull/1193))
- *(sync)* pull contribution owners and grantees automatically on every reconcile pass ([#1188](https://github.com/cq27-dev/rag-rat/pull/1188))

### Fixed

- *(watch)* stop re-registering paths notify already watches ([#1270](https://github.com/cq27-dev/rag-rat/pull/1270))
- *(test)* release points instead of sleeps in the mcp blocking tests ([#1267](https://github.com/cq27-dev/rag-rat/pull/1267))
- *(watch)* stop the unconfigured warning diagnosing what it cannot know ([#1265](https://github.com/cq27-dev/rag-rat/pull/1265))
- *(oracle)* refuse a marker path that is not a regular file ([#1263](https://github.com/cq27-dev/rag-rat/pull/1263))
- *(oracle)* reject empty definition ranges ([#1260](https://github.com/cq27-dev/rag-rat/pull/1260))
- *(test)* gate the stub's wait on a response that lands after it parks ([#1258](https://github.com/cq27-dev/rag-rat/pull/1258))
- *(dispatch)* decide a method chain on its root through `?`, `.await` and parens ([#1253](https://github.com/cq27-dev/rag-rat/pull/1253))
- *(build)* decode fixed-size chunks as arrays, drop a redundant glob ([#1252](https://github.com/cq27-dev/rag-rat/pull/1252))
- *(oracle)* name the actual cause when a live pass configures nothing ([#1250](https://github.com/cq27-dev/rag-rat/pull/1250))
- *(dispatch)* read only the leading identifier when testing for PascalCase ([#1170](https://github.com/cq27-dev/rag-rat/pull/1170))
- *(oracle)* decide a verdict on the tightest occurrence, never a wider module ([#1232](https://github.com/cq27-dev/rag-rat/pull/1232))
- *(db)* give the refold steps the projection shape they fold into ([#1230](https://github.com/cq27-dev/rag-rat/pull/1230))
- *(query)* read the elision marker as state, not as a body prefix ([#1226](https://github.com/cq27-dev/rag-rat/pull/1226))
- *(query)* seed oracle-resolved callers into the flat impact lane ([#1224](https://github.com/cq27-dev/rag-rat/pull/1224))
- *(index)* heal orphaned overlay deletion tombstones ([#1218](https://github.com/cq27-dev/rag-rat/pull/1218))
- *(query)* return oracle-resolved callers and report unresolved honestly ([#1204](https://github.com/cq27-dev/rag-rat/pull/1204))
- *(sync)* keep an authored-onto account servable after contribution re-points ([#1185](https://github.com/cq27-dev/rag-rat/pull/1185)) ([#1196](https://github.com/cq27-dev/rag-rat/pull/1196))
- *(oplog)* clamp the attacker-controlled /3 content lamport ([#1176](https://github.com/cq27-dev/rag-rat/pull/1176)) ([#1190](https://github.com/cq27-dev/rag-rat/pull/1190))
- *(base)* make the test suite pass on windows-latest ([#1192](https://github.com/cq27-dev/rag-rat/pull/1192))

### Other

- avoid ephemeral port reuse races ([#1261](https://github.com/cq27-dev/rag-rat/pull/1261))
- *(oracle)* separate the end edge from the start edge in the namespace pin ([#1259](https://github.com/cq27-dev/rag-rat/pull/1259))
- *(sync)* load the discovery opener state once per fetch pass ([#1238](https://github.com/cq27-dev/rag-rat/pull/1238))
- *(oracle)* pin that a namespace answers only when it bounds the token ([#1245](https://github.com/cq27-dev/rag-rat/pull/1245))
- *(llm)* gauge requests in flight, not connections accepted, in the embed_batch concurrency test ([#1248](https://github.com/cq27-dev/rag-rat/pull/1248))
- *(sync)* single-home "does this sealed announcement fit one publish?" ([#1237](https://github.com/cq27-dev/rag-rat/pull/1237))
- *(query)* make three assertions pin the behavior they describe ([#1225](https://github.com/cq27-dev/rag-rat/pull/1225))

## [0.23.0](https://github.com/cq27-dev/rag-rat/compare/v0.22.0...v0.23.0) - 2026-08-17

### Added

- *(sync)* add sync pull so a store can fetch another account's memories ([#1174](https://github.com/cq27-dev/rag-rat/pull/1174)) ([#1181](https://github.com/cq27-dev/rag-rat/pull/1181))
- *(sync)* let a contributor read back the owner's memories ([#1164](https://github.com/cq27-dev/rag-rat/pull/1164)) ([#1169](https://github.com/cq27-dev/rag-rat/pull/1169))
- *(sync)* route a contributor's memory writes onto the owner's stream ([#1164](https://github.com/cq27-dev/rag-rat/pull/1164)) ([#1168](https://github.com/cq27-dev/rag-rat/pull/1168))
- *(sync)* author granted contributor content onto the owner's stream ([#1164](https://github.com/cq27-dev/rag-rat/pull/1164)) ([#1167](https://github.com/cq27-dev/rag-rat/pull/1167))
- *(sync)* make the /3 content lamport clock stream-global ([#1164](https://github.com/cq27-dev/rag-rat/pull/1164)) ([#1166](https://github.com/cq27-dev/rag-rat/pull/1166))
- *(sync)* author writer grants so a separate identity can contribute ([#1164](https://github.com/cq27-dev/rag-rat/pull/1164)) ([#1165](https://github.com/cq27-dev/rag-rat/pull/1165))
- *(sync)* seed a public node from an existing index ([#1157](https://github.com/cq27-dev/rag-rat/pull/1157)) ([#1163](https://github.com/cq27-dev/rag-rat/pull/1163))
- *(sync)* bound egress with a global byte rate limiter ([#1157](https://github.com/cq27-dev/rag-rat/pull/1157)) ([#1162](https://github.com/cq27-dev/rag-rat/pull/1162))
- *(sync)* serve a published account under public-read admission ([#1157](https://github.com/cq27-dev/rag-rat/pull/1157)) ([#1161](https://github.com/cq27-dev/rag-rat/pull/1161))
- *(sync)* author a repo's memories to a public stream when published ([#1157](https://github.com/cq27-dev/rag-rat/pull/1157)) ([#1158](https://github.com/cq27-dev/rag-rat/pull/1158))
- *(sync)* admit anonymous readers to public_read accounts ([#407](https://github.com/cq27-dev/rag-rat/pull/407)) ([#1153](https://github.com/cq27-dev/rag-rat/pull/1153))
- *(sync)* add a public-only serve scope for anonymous readers ([#407](https://github.com/cq27-dev/rag-rat/pull/407)) ([#1152](https://github.com/cq27-dev/rag-rat/pull/1152))
- *(sync)* accept foreign content on public_read streams ([#407](https://github.com/cq27-dev/rag-rat/pull/407)) ([#1151](https://github.com/cq27-dev/rag-rat/pull/1151))
- *(sync)* add a public_read stream access mode, folded into the /2 stream identity ([#407](https://github.com/cq27-dev/rag-rat/pull/407)) ([#1150](https://github.com/cq27-dev/rag-rat/pull/1150))
- *(sync)* cap concurrent sessions per peer so one node cannot monopolize the resident host ([#406](https://github.com/cq27-dev/rag-rat/pull/406)) ([#1149](https://github.com/cq27-dev/rag-rat/pull/1149))
- *(sync)* bound the inbound accept rate to shed connection floods before the handshake ([#406](https://github.com/cq27-dev/rag-rat/pull/406)) ([#1148](https://github.com/cq27-dev/rag-rat/pull/1148))
- *(sync)* route inbound sessions to the account the dialer names, so one endpoint can host several ([#406](https://github.com/cq27-dev/rag-rat/pull/406)) ([#1147](https://github.com/cq27-dev/rag-rat/pull/1147))
- *(distill)* resolve synced symbol anchors against the peer's local index ([#1143](https://github.com/cq27-dev/rag-rat/pull/1143)) ([#1144](https://github.com/cq27-dev/rag-rat/pull/1144))
- *(sync)* replicate distilled anchors with device-local resolution kept off the wire ([#1139](https://github.com/cq27-dev/rag-rat/pull/1139)) ([#1142](https://github.com/cq27-dev/rag-rat/pull/1142))
- *(sync)* replicate the distill evidence child ([#1141](https://github.com/cq27-dev/rag-rat/pull/1141))
- *(sync)* replicate the distill record_commits child ([#1140](https://github.com/cq27-dev/rag-rat/pull/1140))
- *(sync)* replicate the distill edges and alternatives children ([#1138](https://github.com/cq27-dev/rag-rat/pull/1138))
- *(sync)* register the distill/1 table-sync scope ([#1136](https://github.com/cq27-dev/rag-rat/pull/1136))
- *(sync)* register the overlay/1 table-sync scope ([#1134](https://github.com/cq27-dev/rag-rat/pull/1134))
- *(sync)* re-root chains whose accepted tip fell below the sender's floor ([#1131](https://github.com/cq27-dev/rag-rat/pull/1131))
- *(sync)* expire gapped table entries after a seven-day horizon ([#1130](https://github.com/cq27-dev/rag-rat/pull/1130))
- *(sync)* advertise retained floors so fresh peers converge through compaction ([#1129](https://github.com/cq27-dev/rag-rat/pull/1129))
- *(sync)* compact accepted table-sync prefixes below a retained floor ([#1128](https://github.com/cq27-dev/rag-rat/pull/1128))
- *(sync)* re-adopt rows orphaned by a removed writer ([#1120](https://github.com/cq27-dev/rag-rat/pull/1120))
- *(sync)* sync devices through active MCP ([#1114](https://github.com/cq27-dev/rag-rat/pull/1114))

### Fixed

- *(mcp)* emit SEP-2549 cache hints on tools/list for 2026-07-28 peers ([#1187](https://github.com/cq27-dev/rag-rat/pull/1187))
- *(dream)* verify live-bound absent evidence packs ([#1173](https://github.com/cq27-dev/rag-rat/pull/1173))
- *(oracle)* find an ancestor tsconfig when the index root is a package ([#1172](https://github.com/cq27-dev/rag-rat/pull/1172))
- *(sync)* persist discovery advertisements across restarts ([#1119](https://github.com/cq27-dev/rag-rat/pull/1119))
- *(index)* match target include/exclude patterns as globs ([#1072](https://github.com/cq27-dev/rag-rat/pull/1072)) ([#1118](https://github.com/cq27-dev/rag-rat/pull/1118))

### Other

- *(schema)* provision an empty database at the ladder's end state ([#1186](https://github.com/cq27-dev/rag-rat/pull/1186))
- *(sync)* guard that a rejected peer triggers no inventory snapshot before admission ([#406](https://github.com/cq27-dev/rag-rat/pull/406)) ([#1146](https://github.com/cq27-dev/rag-rat/pull/1146))
- *(sync)* drop the dead InviteToken admission mode; pin the two service modes ([#406](https://github.com/cq27-dev/rag-rat/pull/406)) ([#1145](https://github.com/cq27-dev/rag-rat/pull/1145))
- *(schema)* fail when a repo_id-scoped table declares no adoption or merge disposition ([#1123](https://github.com/cq27-dev/rag-rat/pull/1123))
- *(index)* unify the call-side path stripper onto the scanner ([#1125](https://github.com/cq27-dev/rag-rat/pull/1125))
- *(index)* hoist per-connection schema probes out of the regroup remap loop ([#1115](https://github.com/cq27-dev/rag-rat/pull/1115))

## [0.22.0](https://github.com/cq27-dev/rag-rat/compare/v0.21.1...v0.22.0) - 2026-08-02

### Added

- *(lens)* public read-only web appliance — code-server + Lens + rag-rat serve ([#1106](https://github.com/cq27-dev/rag-rat/pull/1106)) ([#1108](https://github.com/cq27-dev/rag-rat/pull/1108))
- *(sync)* paginate table reconciliation by chain frontier ([#1107](https://github.com/cq27-dev/rag-rat/pull/1107))
- *(sync)* add bounded table-stream transport ([#1097](https://github.com/cq27-dev/rag-rat/pull/1097))
- *(index)* resolve Rust method calls by receiver type ([#1024](https://github.com/cq27-dev/rag-rat/pull/1024))
- *(sync)* bind table streams to repository incarnations ([#1095](https://github.com/cq27-dev/rag-rat/pull/1095))
- *(sync)* seal discovery announcements to the roster-effective devices ([#1083](https://github.com/cq27-dev/rag-rat/pull/1083))
- *(sync)* account-keyed peer discovery ([#1078](https://github.com/cq27-dev/rag-rat/pull/1078))
- *(sync)* retain and promote gapped table-sync chain entries ([#1066](https://github.com/cq27-dev/rag-rat/pull/1066))
- *(oracle)* let a project marker have several names ([#1068](https://github.com/cq27-dev/rag-rat/pull/1068))
- *(oracle)* give each checkout its own live oracle sessions and worklist ([#1054](https://github.com/cq27-dev/rag-rat/pull/1054))
- *(languages)* add Go language support ([#994](https://github.com/cq27-dev/rag-rat/pull/994))
- *(watch)* report which linked checkout reindexed which paths ([#1051](https://github.com/cq27-dev/rag-rat/pull/1051))
- *(lens)* say what each answer was computed from, and withhold it otherwise ([#1035](https://github.com/cq27-dev/rag-rat/pull/1035))
- *(sync)* carry a per-table spec version in table-sync ops ([#1009](https://github.com/cq27-dev/rag-rat/pull/1009))
- *(lens)* add authenticated VS Code repository lens ([#985](https://github.com/cq27-dev/rag-rat/pull/985))
- *(oracle)* add a live clangd LSP backend for C and C++ ([#536](https://github.com/cq27-dev/rag-rat/pull/536))
- *(sync)* park table-sync entries this binary cannot fully project ([#1003](https://github.com/cq27-dev/rag-rat/pull/1003))
- *(sync)* enforce roster and role authority for table-sync ingest ([#998](https://github.com/cq27-dev/rag-rat/pull/998))
- *(oracle)* add a live TypeScript LSP backend ([#536](https://github.com/cq27-dev/rag-rat/pull/536)) ([#992](https://github.com/cq27-dev/rag-rat/pull/992))
- *(plugin)* add Cursor and VS Code hook adapters ([#990](https://github.com/cq27-dev/rag-rat/pull/990))
- *(sync)* add device pairing CLI (init/join) and restore-from-zero drill ([#991](https://github.com/cq27-dev/rag-rat/pull/991))
- *(sync)* enforce authenticated peer capabilities ([#978](https://github.com/cq27-dev/rag-rat/pull/978)) ([#983](https://github.com/cq27-dev/rag-rat/pull/983))
- *(oracle)* live LSP watcher wiring ([#534](https://github.com/cq27-dev/rag-rat/pull/534)) ([#972](https://github.com/cq27-dev/rag-rat/pull/972))
- *(sync)* add recoverable device enrollment protocol ([#945](https://github.com/cq27-dev/rag-rat/pull/945)) ([#949](https://github.com/cq27-dev/rag-rat/pull/949))
- *(oplog)* three-level DeviceRole with read-only content gate + DeviceAdd authoring seam ([#943](https://github.com/cq27-dev/rag-rat/pull/943))
- *(sync)* replicate /3 content over a second ALPN ([#927](https://github.com/cq27-dev/rag-rat/pull/927))
- *(sync)* device-side account-log sync to configured server peers ([#922](https://github.com/cq27-dev/rag-rat/pull/922))
- *(sync)* [sync] config + `rag-rat sync serve` headless peer ([#909](https://github.com/cq27-dev/rag-rat/pull/909))

### Fixed

- *(lens)* invalidate file lanes independently ([#1110](https://github.com/cq27-dev/rag-rat/pull/1110))
- *(index)* make graph upgrades resumable per file ([#1100](https://github.com/cq27-dev/rag-rat/pull/1100))
- *(index)* give an impl the identity rustc gives it ([#1023](https://github.com/cq27-dev/rag-rat/pull/1023))
- *(index)* treat a literal backslash in a Unix filename as part of the name ([#1052](https://github.com/cq27-dev/rag-rat/pull/1052))
- *(paths)* resolve filesystem paths off the Windows verbatim spelling ([#1055](https://github.com/cq27-dev/rag-rat/pull/1055))
- *(sync)* stop ingest destroying unsent local work ([#1056](https://github.com/cq27-dev/rag-rat/pull/1056)) ([#1057](https://github.com/cq27-dev/rag-rat/pull/1057))
- *(sync)* retry table-sync entries deferred behind local row state ([#1005](https://github.com/cq27-dev/rag-rat/pull/1005)) ([#1053](https://github.com/cq27-dev/rag-rat/pull/1053))
- *(sync)* tolerate an unreadable row at store open, and purge the table-sync entry log with its repo (#1017, #1004) ([#1046](https://github.com/cq27-dev/rag-rat/pull/1046))
- *(index)* canonicalize fixture config roots so the suite matches Config::load ([#1047](https://github.com/cq27-dev/rag-rat/pull/1047))
- *(lens)* resolve hop endpoints by symbol handle, not qualified name ([#1036](https://github.com/cq27-dev/rag-rat/pull/1036))
- *(oracle)* separate the checkout ceiling, the index root, and the indexed corpus (#1008, #1011) ([#1031](https://github.com/cq27-dev/rag-rat/pull/1031))
- *(sync)* reconcile each sync stream to a fixpoint across rounds ([#993](https://github.com/cq27-dev/rag-rat/pull/993))
- *(oracle)* finish live LSP lifecycle ([#989](https://github.com/cq27-dev/rag-rat/pull/989))
- *(oracle)* gate and bound live LSP requests ([#984](https://github.com/cq27-dev/rag-rat/pull/984))
- *(sync)* acknowledge transfers before closing ([#926](https://github.com/cq27-dev/rag-rat/pull/926)) ([#977](https://github.com/cq27-dev/rag-rat/pull/977))
- *(tests)* isolate fixture git invocations from the ambient environment ([#975](https://github.com/cq27-dev/rag-rat/pull/975))
- *(eval)* guard the replay scratch database against error paths ([#974](https://github.com/cq27-dev/rag-rat/pull/974))
- *(tests)* retain satellite-crate scratch cleanup guards ([#973](https://github.com/cq27-dev/rag-rat/pull/973))
- *(tests)* retain CLI unit-test scratch cleanup guards ([#971](https://github.com/cq27-dev/rag-rat/pull/971))
- *(tests)* retain core unit-test scratch cleanup guards ([#968](https://github.com/cq27-dev/rag-rat/pull/968))
- *(tests)* retain config scratch cleanup guards ([#967](https://github.com/cq27-dev/rag-rat/pull/967))
- *(tests)* retain CLI integration scratch cleanup guards ([#966](https://github.com/cq27-dev/rag-rat/pull/966))
- *(tests)* retain watch scratch cleanup guards ([#965](https://github.com/cq27-dev/rag-rat/pull/965))
- *(tests)* retain schema scratch cleanup guards ([#964](https://github.com/cq27-dev/rag-rat/pull/964))
- *(tests)* clean high-volume scratch fixtures ([#963](https://github.com/cq27-dev/rag-rat/pull/963))
- *(dream)* harden divergence verdict precision ([#954](https://github.com/cq27-dev/rag-rat/pull/954)) ([#959](https://github.com/cq27-dev/rag-rat/pull/959))
- *(edges)* retain recovered error descendants ([#951](https://github.com/cq27-dev/rag-rat/pull/951))

### Other

- replicate memory bindings through anchors/1 ([#1112](https://github.com/cq27-dev/rag-rat/pull/1112))
- *(query)* aggregate repo_brief fan-in/out on edges_data ([#1101](https://github.com/cq27-dev/rag-rat/pull/1101)) ([#1102](https://github.com/cq27-dev/rag-rat/pull/1102))
- derive schema recognizers from the ladder and reuse shared helpers ([#1098](https://github.com/cq27-dev/rag-rat/pull/1098))
- *(mcp)* upgrade rmcp to 3.0 for MCP 2026-07-28 ([#1071](https://github.com/cq27-dev/rag-rat/pull/1071))
- *(oracle)* pin that a batch clear cannot erase a sibling on another tool version ([#1073](https://github.com/cq27-dev/rag-rat/pull/1073))
- *(oracle)* declare a backend's project as one model, and its marker pin with it ([#1069](https://github.com/cq27-dev/rag-rat/pull/1069))
- document Go language support ([#1061](https://github.com/cq27-dev/rag-rat/pull/1061))
- *(db)* reach migration steps through the migrations module ([#1060](https://github.com/cq27-dev/rag-rat/pull/1060))
- *(oracle)* split the batch declaration off the shared tool registry ([#1045](https://github.com/cq27-dev/rag-rat/pull/1045))
- *(oracle)* declare coverage and authority, unify languages, drop dead declarations ([#1044](https://github.com/cq27-dev/rag-rat/pull/1044))
- *(sync)* close the two table-sync coverage gates — migration arming and two openers of one file (#1018, #1019) ([#1043](https://github.com/cq27-dev/rag-rat/pull/1043))
- *(index)* isolate adoption_hints git helper from ambient environment ([#581](https://github.com/cq27-dev/rag-rat/pull/581)) ([#969](https://github.com/cq27-dev/rag-rat/pull/969))
- *(dream)* reject intent context for verification ([#962](https://github.com/cq27-dev/rag-rat/pull/962))
- *(watch)* scope overlay refreshes to event paths ([#953](https://github.com/cq27-dev/rag-rat/pull/953))
- *(edges)* reject C-family query discovery ([#952](https://github.com/cq27-dev/rag-rat/pull/952))
- *(resolve)* use typed language policies ([#950](https://github.com/cq27-dev/rag-rat/pull/950))
- *(edges)* register extraction functions ([#948](https://github.com/cq27-dev/rag-rat/pull/948))
- *(edges)* prepare source owner lookup ([#947](https://github.com/cq27-dev/rag-rat/pull/947))
- *(edges)* add typed extraction context ([#946](https://github.com/cq27-dev/rag-rat/pull/946))
- *(edges)* capture identifier paths once ([#944](https://github.com/cq27-dev/rag-rat/pull/944))
- *(deps)* upgrade Rust dependencies ([#933](https://github.com/cq27-dev/rag-rat/pull/933))
- *(index)* split git history into cohesive siblings ([#931](https://github.com/cq27-dev/rag-rat/pull/931))
- *(index)* split worktree overlay into cohesive siblings ([#929](https://github.com/cq27-dev/rag-rat/pull/929))
- *(tests)* partition schema bootstrap catalogs ([#928](https://github.com/cq27-dev/rag-rat/pull/928))
- *(search)* split lexical search into cohesive siblings ([#925](https://github.com/cq27-dev/rag-rat/pull/925))
- *(clones)* split precompute into cohesive siblings ([#924](https://github.com/cq27-dev/rag-rat/pull/924))
- *(mcp)* split server into cohesive siblings ([#923](https://github.com/cq27-dev/rag-rat/pull/923))

## [0.21.1](https://github.com/cq27-dev/rag-rat/compare/v0.21.0...v0.21.1) - 2026-07-24

### Fixed

- green the cross-platform CI gate — macOS jemalloc startup crash, Windows clippy/tests ([#908](https://github.com/cq27-dev/rag-rat/pull/908))

### Other

- *(index)* return freed heap to the OS at heavy-pass terminals ([#910](https://github.com/cq27-dev/rag-rat/pull/910))

## [0.21.0](https://github.com/cq27-dev/rag-rat/compare/v0.20.0...v0.21.0) - 2026-07-24

### Added

- *(sync)* table→log sync engine — whole-row last-writer-wins ([#896](https://github.com/cq27-dev/rag-rat/pull/896))
- *(memory)* drain accepted /3 content into repo_memories ([#691](https://github.com/cq27-dev/rag-rat/pull/691)) ([#902](https://github.com/cq27-dev/rag-rat/pull/902))
- *(memory)* sync-origin provenance + edge tombstones, the projection foundation ([#691](https://github.com/cq27-dev/rag-rat/pull/691)) ([#891](https://github.com/cq27-dev/rag-rat/pull/891))
- *(init)* add Papertrail (issue tracker) and Distillation wizard steps ([#887](https://github.com/cq27-dev/rag-rat/pull/887))
- *(sync)* authorize connecting nodes against the account roster before syncing ([#881](https://github.com/cq27-dev/rag-rat/pull/881)) ([#886](https://github.com/cq27-dev/rag-rat/pull/886))
- *(sync)* extend the transport to /3 content — the memories themselves ([#406](https://github.com/cq27-dev/rag-rat/pull/406)) ([#885](https://github.com/cq27-dev/rag-rat/pull/885))
- *(sync)* iroh transport for account-log peer sync — hello + pull ([#406](https://github.com/cq27-dev/rag-rat/pull/406)) ([#877](https://github.com/cq27-dev/rag-rat/pull/877))
- *(distill)* default to the validated 30B ephemeral box, with distillation docs ([#876](https://github.com/cq27-dev/rag-rat/pull/876))
- *(oplog)* report an oversized snapshot as a typed outcome, not a raw encode failure ([#868](https://github.com/cq27-dev/rag-rat/pull/868)) ([#869](https://github.com/cq27-dev/rag-rat/pull/869))
- *(oplog)* chunk StreamKeyWrap authoring across ops for large rosters ([#764](https://github.com/cq27-dev/rag-rat/pull/764)) ([#866](https://github.com/cq27-dev/rag-rat/pull/866))
- *(oplog)* mint snapshots over the accepted control branch ([#609](https://github.com/cq27-dev/rag-rat/pull/609)) ([#851](https://github.com/cq27-dev/rag-rat/pull/851))
- *(oplog)* snapshot usability and coverage-dominance selection ([#609](https://github.com/cq27-dev/rag-rat/pull/609)) ([#846](https://github.com/cq27-dev/rag-rat/pull/846))
- *(oplog)* read-time verification of a snapshot's coverage claim ([#609](https://github.com/cq27-dev/rag-rat/pull/609)) ([#842](https://github.com/cq27-dev/rag-rat/pull/842))
- *(oplog)* canonical account projection and folded_state_hash ([#609](https://github.com/cq27-dev/rag-rat/pull/609)) ([#838](https://github.com/cq27-dev/rag-rat/pull/838))
- *(distill)* surface distilled records on impact_surface (drive-by attachment) ([#832](https://github.com/cq27-dev/rag-rat/pull/832)) ([#833](https://github.com/cq27-dev/rag-rat/pull/833))
- *(oplog)* annex log and the snapshot coverage-manifest wire ([#609](https://github.com/cq27-dev/rag-rat/pull/609)) ([#813](https://github.com/cq27-dev/rag-rat/pull/813))
- *(distill)* surface distilled records on symbol_lookup (drive-by attachment) ([#812](https://github.com/cq27-dev/rag-rat/pull/812)) ([#814](https://github.com/cq27-dev/rag-rat/pull/814))
- *(distill)* records_for_symbol — facet-gated symbol→record read for the drive-by lane ([#808](https://github.com/cq27-dev/rag-rat/pull/808)) ([#811](https://github.com/cq27-dev/rag-rat/pull/811))
- *(distill)* attach the distilled-record payload to rationale_search and papertrail_issue_search ([#806](https://github.com/cq27-dev/rag-rat/pull/806)) ([#807](https://github.com/cq27-dev/rag-rat/pull/807))
- *(distill)* coalesced record read-model + relocate the effective-status resolver ([#804](https://github.com/cq27-dev/rag-rat/pull/804)) ([#805](https://github.com/cq27-dev/rag-rat/pull/805))
- *(oplog)* batch and budget account-triggered content refolds ([#698](https://github.com/cq27-dev/rag-rat/pull/698)) ([#798](https://github.com/cq27-dev/rag-rat/pull/798))
- *(distill)* persist source-part identity on evidence rows ([#801](https://github.com/cq27-dev/rag-rat/pull/801)) ([#803](https://github.com/cq27-dev/rag-rat/pull/803))
- *(distill)* enriched prompt context — fix diffs, outbound-ref xrefs, all coalesced partners ([#800](https://github.com/cq27-dev/rag-rat/pull/800)) ([#802](https://github.com/cq27-dev/rag-rat/pull/802))
- *(distill)* drain prepared snapshots through the configured chat model ([#704](https://github.com/cq27-dev/rag-rat/pull/704)) ([#799](https://github.com/cq27-dev/rag-rat/pull/799))
- *(distill)* snapshot exact thread sources and unit spans for safe model input ([#797](https://github.com/cq27-dev/rag-rat/pull/797))
- *(sync)* re-wrap live content keys for enrolled-device catch-up ([#763](https://github.com/cq27-dev/rag-rat/pull/763)) ([#793](https://github.com/cq27-dev/rag-rat/pull/793))
- *(sync)* enable sealed /3 memory streams with decrypt-at-projection ([#608](https://github.com/cq27-dev/rag-rat/pull/608)) ([#790](https://github.com/cq27-dev/rag-rat/pull/790))

### Fixed

- *(query)* scope-filter surfaced logical variant_count/group_reason ([#897](https://github.com/cq27-dev/rag-rat/pull/897)) ([#900](https://github.com/cq27-dev/rag-rat/pull/900))
- *(distill)* count repaired replies per serde rung so the clean-guided rate is exact ([#873](https://github.com/cq27-dev/rag-rat/pull/873))
- *(distill)* normalize model output before validation to rescue records the gate would reject ([#872](https://github.com/cq27-dev/rag-rat/pull/872))
- *(index)* carry distill anchors and node-edge targets through a logical-id remap ([#810](https://github.com/cq27-dev/rag-rat/pull/810)) ([#864](https://github.com/cq27-dev/rag-rat/pull/864))
- *(index)* link each chunk to its defining symbol so distill records resolve precisely ([#858](https://github.com/cq27-dev/rag-rat/pull/858))
- *(index)* label logical-symbol groups by member evidence, not a blanket cfg_variant ([#855](https://github.com/cq27-dev/rag-rat/pull/855)) ([#863](https://github.com/cq27-dev/rag-rat/pull/863))
- *(query)* resolve a chunk's symbol by closest byte range, not an arbitrary same-named winner ([#855](https://github.com/cq27-dev/rag-rat/pull/855)) ([#862](https://github.com/cq27-dev/rag-rat/pull/862))
- *(cli)* harden rm sync and worktree cleanup ([#795](https://github.com/cq27-dev/rag-rat/pull/795))
- *(cli)* surface schema and writer lock waits ([#792](https://github.com/cq27-dev/rag-rat/pull/792))

### Other

- *(clones)* derive the clone-delta changed set from a hint + cache the postings count ([#904](https://github.com/cq27-dev/rag-rat/pull/904))
- *(index)* incrementally maintain content_revision as an O(1) read ([#903](https://github.com/cq27-dev/rag-rat/pull/903))
- *(index)* scope incremental logical-symbol re-derive to changed paths ([#826](https://github.com/cq27-dev/rag-rat/pull/826)) ([#895](https://github.com/cq27-dev/rag-rat/pull/895))
- *(index)* scope incremental edge re-resolution to changed files ([#827](https://github.com/cq27-dev/rag-rat/pull/827)) ([#894](https://github.com/cq27-dev/rag-rat/pull/894))
- *(index)* pin repo-scoped qualified-name resolution in consolidated DBs ([#852](https://github.com/cq27-dev/rag-rat/pull/852)) ([#870](https://github.com/cq27-dev/rag-rat/pull/870))
- *(oplog)* pin the total tombstone set a snapshot binds ([#609](https://github.com/cq27-dev/rag-rat/pull/609)) ([#861](https://github.com/cq27-dev/rag-rat/pull/861))
- *(oplog)* pin control-log quarantine as spec and correct the retention comment ([#809](https://github.com/cq27-dev/rag-rat/pull/809)) ([#859](https://github.com/cq27-dev/rag-rat/pull/859))
- *(watch)* quiet-window overlay skip; reuse repo handles and the recorded delta in the probe ([#857](https://github.com/cq27-dev/rag-rat/pull/857))
- *(index)* skip the logical-symbol rebuild when a batch's key multiset is unchanged ([#856](https://github.com/cq27-dev/rag-rat/pull/856))
- document distilled records in MCP tool docs + surface them in the CLI query --json ([#854](https://github.com/cq27-dev/rag-rat/pull/854))
- attach drive-by decision records on semantic_search hits ([#850](https://github.com/cq27-dev/rag-rat/pull/850))
- *(index)* rebuild logical symbols once per overlay batch; basis writes ride the refresh transaction ([#849](https://github.com/cq27-dev/rag-rat/pull/849))
- *(index)* content_revision once per sync_fts, probe-pinned reuse in the clone delta ([#821](https://github.com/cq27-dev/rag-rat/pull/821)) ([#848](https://github.com/cq27-dev/rag-rat/pull/848))
- *(watch)* add a minimum inter-pass cooldown to the watcher event loop ([#847](https://github.com/cq27-dev/rag-rat/pull/847))
- attach drive-by decision records on read_chunk ([#844](https://github.com/cq27-dev/rag-rat/pull/844))
- stop surfacing the keyword classification on retrieval and impact ([#841](https://github.com/cq27-dev/rag-rat/pull/841))
- *(db)* in-memory temp store, bounded page cache, and a deliberate WAL checkpoint cadence ([#836](https://github.com/cq27-dev/rag-rat/pull/836))
- *(reconcile)* unordered, lazy-text candidate stream for the count-only paths ([#816](https://github.com/cq27-dev/rag-rat/pull/816)) ([#835](https://github.com/cq27-dev/rag-rat/pull/835))
- *(watch)* stop overlay-only passes from forcing the base reconcile and clone delta ([#834](https://github.com/cq27-dev/rag-rat/pull/834))
- Add candidate-index anchor selection and the strict distill output ladder ([#794](https://github.com/cq27-dev/rag-rat/pull/794))

## [0.20.0](https://github.com/cq27-dev/rag-rat/compare/v0.19.0...v0.20.0) - 2026-07-20

### Added

- *(cli)* rag-rat rm <path> — remove a repo from the global index (purge + VACUUM), config, and hooks ([#778](https://github.com/cq27-dev/rag-rat/pull/778))
- *(oplog)* rebuild-all /3 content projections on projector-version staleness ([#688](https://github.com/cq27-dev/rag-rat/pull/688)) ([#787](https://github.com/cq27-dev/rag-rat/pull/787))
- *(oplog)* sealed /3 content authoring (unwired) — XChaCha20-Poly1305 with a random wire nonce ([#608](https://github.com/cq27-dev/rag-rat/pull/608)) ([#783](https://github.com/cq27-dev/rag-rat/pull/783))
- *(plugin)* opencode plugin bundle — @rag-rat/plugin-opencode (MCP + hooks) ([#785](https://github.com/cq27-dev/rag-rat/pull/785))
- *(cli)* rag-rat status — cross-repo inventory of the global store ([#766](https://github.com/cq27-dev/rag-rat/pull/766)) ([#777](https://github.com/cq27-dev/rag-rat/pull/777))
- *(config)* add [llm.distill] config with a model-size-aware provision timeout ([#779](https://github.com/cq27-dev/rag-rat/pull/779))
- *(llm)* generalize the chat client into rag-rat-llm with guided decoding ([#772](https://github.com/cq27-dev/rag-rat/pull/772))
- *(distill)* deterministic distilled-record store + extraction pass ([#703](https://github.com/cq27-dev/rag-rat/pull/703)) ([#755](https://github.com/cq27-dev/rag-rat/pull/755))
- *(agent-hook)* resurface-window dedup for grep/read augmentation, unified with #752 ([#759](https://github.com/cq27-dev/rag-rat/pull/759)) ([#765](https://github.com/cq27-dev/rag-rat/pull/765))
- *(agent-hook)* augment the Read tool with file/dir memories + load-bearing symbols ([#756](https://github.com/cq27-dev/rag-rat/pull/756)) ([#761](https://github.com/cq27-dev/rag-rat/pull/761))
- *(oplog)* lazy content-key rotation on device removal ([#607](https://github.com/cq27-dev/rag-rat/pull/607)) ([#762](https://github.com/cq27-dev/rag-rat/pull/762))
- *(oplog)* sealing-key selection + key_id adoption cross-check ([#607](https://github.com/cq27-dev/rag-rat/pull/607)) ([#760](https://github.com/cq27-dev/rag-rat/pull/760))
- *(oplog)* content-key minting + owner-gated key_wrap authoring ([#607](https://github.com/cq27-dev/rag-rat/pull/607)) ([#754](https://github.com/cq27-dev/rag-rat/pull/754))
- *(oplog)* owner-gated secrets-log acceptance evaluator + StreamKeyWrap op ([#607](https://github.com/cq27-dev/rag-rat/pull/607)) ([#751](https://github.com/cq27-dev/rag-rat/pull/751))
- *(index)* path-scoped overlay reconcile for linked-worktree edits ([#748](https://github.com/cq27-dev/rag-rat/pull/748))
- *(oplog)* wire secrets-chain cut registers into the account fold ([#737](https://github.com/cq27-dev/rag-rat/pull/737))
- *(agent-hook)* PostToolUse edit trigger — scoped reindex, watcher-aware, detached ([#738](https://github.com/cq27-dev/rag-rat/pull/738))
- *(papertrail)* provider-attested closing edges — the GraphQL lane (#702 stage 2) ([#727](https://github.com/cq27-dev/rag-rat/pull/727))
- *(papertrail)* closing-edge substrate — provider-neutral schema, gated text tier, item/comment ref mining ([#702](https://github.com/cq27-dev/rag-rat/pull/702)) ([#722](https://github.com/cq27-dev/rag-rat/pull/722))

### Fixed

- *(mcp)* preserve title tool arguments in schemas ([#788](https://github.com/cq27-dev/rag-rat/pull/788))
- *(oplog)* refresh the accepted-/3 memory projection on account refold ([#683](https://github.com/cq27-dev/rag-rat/pull/683)) ([#786](https://github.com/cq27-dev/rag-rat/pull/786))
- *(core)* oversized memory row or edge no longer wedges the write path; guard the assembled op + cap payload_json/edge anchors ([#680](https://github.com/cq27-dev/rag-rat/pull/680)) ([#776](https://github.com/cq27-dev/rag-rat/pull/776))
- *(oplog)* condemn beyond-cut content on a withheld watermark (I11 parity with the account fold) ([#773](https://github.com/cq27-dev/rag-rat/pull/773))
- *(tests)* route test scratch through a shared self-healing helper (fixes #726) ([#732](https://github.com/cq27-dev/rag-rat/pull/732))

### Other

- Add the distill LLM prompt contract — guided schema + enriched render ([#781](https://github.com/cq27-dev/rag-rat/pull/781))
- *(deps)* refresh runtime and tooling dependencies ([#789](https://github.com/cq27-dev/rag-rat/pull/789))
- *(oplog)* tripwires for AEAD nonce freshness, sealing-selection convergence, and pre-verify per-origin isolation ([#780](https://github.com/cq27-dev/rag-rat/pull/780))
- *(oplog)* bound-invariant coverage (lamport-i64, ed25519 strict, fold firewall, wrap cap) ([#775](https://github.com/cq27-dev/rag-rat/pull/775))
- *(oplog)* behavioral coverage for last-owner protection and no-cascade ([#774](https://github.com/cq27-dev/rag-rat/pull/774))
- Fix #697: bind reconcile embed-stub accept loops to request count, not wall clock ([#768](https://github.com/cq27-dev/rag-rat/pull/768))
- *(mcp)* trim repeated meta from tool results to cut per-call tokens ([#752](https://github.com/cq27-dev/rag-rat/pull/752)) ([#753](https://github.com/cq27-dev/rag-rat/pull/753))
- *(db)* materialize edge visibility as edges_data.hidden — one integer compare per view row ([#741](https://github.com/cq27-dev/rag-rat/pull/741))
- *(readme)* link the rag-rat.cq27.dev site ([#749](https://github.com/cq27-dev/rag-rat/pull/749))
- *(locks)* shared content-carrying single-flight coalescing primitive ([#736](https://github.com/cq27-dev/rag-rat/pull/736))
- *(workspace)* [**breaking**] extract the rag-rat-oplog crate ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#735](https://github.com/cq27-dev/rag-rat/pull/735))
- *(query)* suppressed-edge exclusion as a scalar compare — recovers the query_warm regression from the Swift baseline ([#731](https://github.com/cq27-dev/rag-rat/pull/731))
- *(reconcile)* ~75× faster kernel-scale reconcile — snapshot the scope view, read the certified policy column ([#725](https://github.com/cq27-dev/rag-rat/pull/725)) ([#730](https://github.com/cq27-dev/rag-rat/pull/730))
- *(mcp)* harden listener takeover test with explicit readiness signaling ([#537](https://github.com/cq27-dev/rag-rat/pull/537)) ([#723](https://github.com/cq27-dev/rag-rat/pull/723))
- *(llm)* force deterministic completion order in embed_batch out-of-order test ([#721](https://github.com/cq27-dev/rag-rat/pull/721))
- *(workspace)* [**breaking**] extract the rag-rat-dream crate ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#720](https://github.com/cq27-dev/rag-rat/pull/720))
- *(workspace)* [**breaking**] extract rag-rat-query — the read layer: graph/impact/symbol/tree queries, memory reads + evidence, pagerank ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#719](https://github.com/cq27-dev/rag-rat/pull/719))
- *(workspace)* [**breaking**] extract rag-rat-oracle — SCIP/LSP evidence, run manifests, verdict store ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#718](https://github.com/cq27-dev/rag-rat/pull/718))
- *(workspace)* [**breaking**] extract the rag-rat-clones crate ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#717](https://github.com/cq27-dev/rag-rat/pull/717))
- *(workspace)* [**breaking**] extract rag-rat-llm — embedder providers, cookbook provisioning, throughput tuning ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#716](https://github.com/cq27-dev/rag-rat/pull/716))
- *(workspace)* [**breaking**] extract rag-rat-papertrail — mirror, providers, transport, evidence ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#715](https://github.com/cq27-dev/rag-rat/pull/715))
- *(workspace)* [**breaking**] extract the rag-rat-db database layer with an explicit MigrationHooks seam ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#714](https://github.com/cq27-dev/rag-rat/pull/714))
- *(workspace)* [**breaking**] extract the rag-rat-base foundation crate ([#706](https://github.com/cq27-dev/rag-rat/pull/706)) ([#711](https://github.com/cq27-dev/rag-rat/pull/711))

## [0.19.0](https://github.com/cq27-dev/rag-rat/compare/v0.18.0...v0.19.0) - 2026-07-16

### Added

- *(index)* scoped reconcile over an explicit path set (index --paths) ([#687](https://github.com/cq27-dev/rag-rat/pull/687))
- *(oplog)* content-key crypto primitives — content keys, key_id, X25519 sealed-box wrap ([#709](https://github.com/cq27-dev/rag-rat/pull/709))
- *(evals)* regenerate memory-compaction verify-packs corpus + add a committed generator ([#695](https://github.com/cq27-dev/rag-rat/pull/695)) ([#700](https://github.com/cq27-dev/rag-rat/pull/700))
- *(oplog)* defer the /3 content-ingest refold + exclude local authoring from the remote budget ([#699](https://github.com/cq27-dev/rag-rat/pull/699))
- *(memory)* author the live memory path onto owner-bound /2//3 streams ([#681](https://github.com/cq27-dev/rag-rat/pull/681))
- *(watch)* surface silently-dropped filesystem watches in index_status ([#670](https://github.com/cq27-dev/rag-rat/pull/670))
- *(account)* add the in-tx owner-stream ownership ensure seam ([#677](https://github.com/cq27-dev/rag-rat/pull/677))
- *(account)* add the /3 local-content authoring seam + accepted→memory projection ([#668](https://github.com/cq27-dev/rag-rat/pull/668))
- *(papertrail)* native GitLab provider — namespaced ids, parallel list legs, events comment lane ([#654](https://github.com/cq27-dev/rag-rat/pull/654))
- *(account)* mint the store's local account ([#666](https://github.com/cq27-dev/rag-rat/pull/666))
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

- *(memory)* pin the read path unchanged under /3 + mark the foreign read projection ([#693](https://github.com/cq27-dev/rag-rat/pull/693))
- *(graph)* index-seed the remaining edges-view readers — grep_augment + impact_surface ([#692](https://github.com/cq27-dev/rag-rat/pull/692)) ([#694](https://github.com/cq27-dev/rag-rat/pull/694))
- *(graph)* seed find_callers/trace_callees on indexed edge id columns ([#682](https://github.com/cq27-dev/rag-rat/pull/682)) ([#684](https://github.com/cq27-dev/rag-rat/pull/684))
- neutral wording for the sync feature in repo_identity docs ([#674](https://github.com/cq27-dev/rag-rat/pull/674))
- split config.md into per-topic pages under docs/config/ ([#669](https://github.com/cq27-dev/rag-rat/pull/669))

## [0.18.0](https://github.com/cq27-dev/rag-rat/compare/v0.17.0...v0.18.0) - 2026-07-15

### Added

- *(account)* add /3 candidate ancestry and branch selection ([#647](https://github.com/cq27-dev/rag-rat/pull/647))
- *(lang)* add Swift baseline support ([#639](https://github.com/cq27-dev/rag-rat/pull/639))
- *(account)* add pure content acceptance evaluator; split auth_len freshness out of the authority seam ([#645](https://github.com/cq27-dev/rag-rat/pull/645))
- *(papertrail)* add automatic sync scheduling core ([#640](https://github.com/cq27-dev/rag-rat/pull/640))
- *(account)* add signed content entry envelope ([#643](https://github.com/cq27-dev/rag-rat/pull/643))
- *(account)* finish the account authority projection and query seams ([#604](https://github.com/cq27-dev/rag-rat/pull/604)) ([#641](https://github.com/cq27-dev/rag-rat/pull/641))
- *(papertrail)* add native GitHub project mirror ([#638](https://github.com/cq27-dev/rag-rat/pull/638))
- *(papertrail)* add shared provider transport ([#633](https://github.com/cq27-dev/rag-rat/pull/633))
- *(papertrail)* add tracker config and provider ref grammar ([#631](https://github.com/cq27-dev/rag-rat/pull/631))
- *(papertrail)* add provider-neutral schema ([#632](https://github.com/cq27-dev/rag-rat/pull/632))
- *(account)* candidate-DAG storage — ingest, refold + branch selection (V059) ([#627](https://github.com/cq27-dev/rag-rat/pull/627))

### Fixed

- *(account)* preserve bounded historical authority ([#642](https://github.com/cq27-dev/rag-rat/pull/642))
- *(account)* bound candidate ingest work ([#634](https://github.com/cq27-dev/rag-rat/pull/634))

### Other

- add content candidate DAG storage ([#644](https://github.com/cq27-dev/rag-rat/pull/644))
- tiered god-module sweep (watch, config, mcp, embed_loop) ([#630](https://github.com/cq27-dev/rag-rat/pull/630))
- explain Codex MCP approval for reviews ([#628](https://github.com/cq27-dev/rag-rat/pull/628))

## [0.17.0](https://github.com/cq27-dev/rag-rat/compare/v0.16.0...v0.17.0) - 2026-07-12

### Added

- *(plugin)* one-step plugin for Claude Code + Codex — MCP via npx, harness-neutral hooks ([#569](https://github.com/cq27-dev/rag-rat/pull/569))
- *(account)* the stratified control-log fold ([#622](https://github.com/cq27-dev/rag-rat/pull/622))
- *(dist)* ship an android/Termux binary + make npx @rag-rat/bin work there ([#616](https://github.com/cq27-dev/rag-rat/pull/616)) ([#621](https://github.com/cq27-dev/rag-rat/pull/621))
- *(account)* account sync wire/crypto layer + fold foundation ([#618](https://github.com/cq27-dev/rag-rat/pull/618))
- *(doctor)* reclaim freelist dead space with `doctor --vacuum` ([#574](https://github.com/cq27-dev/rag-rat/pull/574)) ([#613](https://github.com/cq27-dev/rag-rat/pull/613))

### Fixed

- *(version)* stamp release binaries clean — trust CI tag signal + scope dirty to tracked files ([#623](https://github.com/cq27-dev/rag-rat/pull/623)) ([#624](https://github.com/cq27-dev/rag-rat/pull/624))
- *(cli)* gate jemalloc off android — NDK r23+ dropped libgcc, breaking the link ([#615](https://github.com/cq27-dev/rag-rat/pull/615)) ([#620](https://github.com/cq27-dev/rag-rat/pull/620))
- *(mcp)* boot a dormant server outside a rag-rat repo instead of dying ([#603](https://github.com/cq27-dev/rag-rat/pull/603)) ([#611](https://github.com/cq27-dev/rag-rat/pull/611))

### Other

- *(wal)* checkpoint the shared -wal on the git-hook write path ([#573](https://github.com/cq27-dev/rag-rat/pull/573)) ([#619](https://github.com/cq27-dev/rag-rat/pull/619))

## [0.16.0](https://github.com/cq27-dev/rag-rat/compare/v0.15.0...v0.16.0) - 2026-07-11

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

## [0.15.0](https://github.com/cq27-dev/rag-rat/compare/v0.14.0...v0.15.0) - 2026-07-09

### Added

- *(oplog)* wire authoring into the live memory write path ([#538](https://github.com/cq27-dev/rag-rat/pull/538))
- *(oplog)* op-authoring helper + full backfill of existing memories (not yet wired) ([#526](https://github.com/cq27-dev/rag-rat/pull/526))
- *(oracle)* live LSP client substrate for the incremental resolution path ([#531](https://github.com/cq27-dev/rag-rat/pull/531))
- *(index)* realign logical-symbol references on key-derivation drift ([#493](https://github.com/cq27-dev/rag-rat/pull/493)) ([#525](https://github.com/cq27-dev/rag-rat/pull/525))
- *(memory)* two-observation downgrade hysteresis for anchor status ([#492](https://github.com/cq27-dev/rag-rat/pull/492)) ([#528](https://github.com/cq27-dev/rag-rat/pull/528))
- *(clones)* scip refine mode — moniker-collapse same-symbol callees to Type-2 ([#512](https://github.com/cq27-dev/rag-rat/pull/512))
- *(dream)* expose dream + dream_review MCP tools ([#263](https://github.com/cq27-dev/rag-rat/pull/263)) ([#514](https://github.com/cq27-dev/rag-rat/pull/514))
- *(oplog)* persisted local device identity — CSPRNG keygen + single-row store ([#523](https://github.com/cq27-dev/rag-rat/pull/523))
- *(oplog)* immutable stream identity — signed stream binding, stream-scoped store, fork quarantine ([#511](https://github.com/cq27-dev/rag-rat/pull/511))
- *(memory)* default the memory surface to summary; extend it to the memory-query tools ([#508](https://github.com/cq27-dev/rag-rat/pull/508))
- *(oplog)* durable storage — layer-1 signed log + full-replay shadow projection ([#504](https://github.com/cq27-dev/rag-rat/pull/504))
- *(oplog)* signed hash-chained entry envelope + ed25519 device keys ([#500](https://github.com/cq27-dev/rag-rat/pull/500))
- *(memory)* pending anchor status for in-flight worktree branches ([#496](https://github.com/cq27-dev/rag-rat/pull/496))
- *(oplog)* memory op model + deterministic projection fold ([#495](https://github.com/cq27-dev/rag-rat/pull/495))
- *(clones)* per-generation df snapshot (clone_df_epoch) frees the live df to move ([#490](https://github.com/cq27-dev/rag-rat/pull/490))
- *(memory)* canonical content_hash primitive ([#480](https://github.com/cq27-dev/rag-rat/pull/480))
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

## [0.14.0](https://github.com/cq27-dev/rag-rat/compare/v0.13.0...v0.14.0) - 2026-07-07

### Added

- *(index)* make explicit-config adoption loud and refuse empty indexes ([#458](https://github.com/cq27-dev/rag-rat/pull/458))
- *(memory)* apply [memory] surface="summary" to every drive-by renderer ([#426](https://github.com/cq27-dev/rag-rat/pull/426)) ([#453](https://github.com/cq27-dev/rag-rat/pull/453))

### Fixed

- reduce incremental index churn ([#460](https://github.com/cq27-dev/rag-rat/pull/460))
- *(watch)* prune linked worktree watches ([#454](https://github.com/cq27-dev/rag-rat/pull/454))
- *(mcp)* bound tool execution in stdio server ([#451](https://github.com/cq27-dev/rag-rat/pull/451))
- *(cross-platform)* full request drain in the ollama-probe and embed stubs ([#446](https://github.com/cq27-dev/rag-rat/pull/446)) ([#452](https://github.com/cq27-dev/rag-rat/pull/452))
- *(cross-platform)* TOML path escaping, cookbook slash, clone teardown, Windows clippy ([#446](https://github.com/cq27-dev/rag-rat/pull/446)) ([#449](https://github.com/cq27-dev/rag-rat/pull/449))
- *(dream)* persist failed model attempts ([#450](https://github.com/cq27-dev/rag-rat/pull/450))
- *(init)* canonicalize both sides before stripping the config root prefix ([#447](https://github.com/cq27-dev/rag-rat/pull/447))

### Other

- *(consolidate)* pin the embedding model to make the heal-owed-meta test deterministic ([#461](https://github.com/cq27-dev/rag-rat/pull/461))
- derive enum strings with strum ([#457](https://github.com/cq27-dev/rag-rat/pull/457))
- Split oracle tests and antiunify module ([#456](https://github.com/cq27-dev/rag-rat/pull/456))
- *(watch)* cover watcher state helpers ([#455](https://github.com/cq27-dev/rag-rat/pull/455))

## [0.13.0](https://github.com/cq27-dev/rag-rat/compare/v0.12.0...v0.13.0) - 2026-07-05

### Added

- *(mcp)* memory_show — expand a memory to its full body by id ([#445](https://github.com/cq27-dev/rag-rat/pull/445))
- *(dream)* human review surface — dream <id> --accept|--dismiss|--reset ([#262](https://github.com/cq27-dev/rag-rat/pull/262)) ([#440](https://github.com/cq27-dev/rag-rat/pull/440))
- *(dream)* run the verdict/compaction model on an ephemeral remote GPU (`[llm.dream.remote]`) ([#438](https://github.com/cq27-dev/rag-rat/pull/438))
- *(dream)* v2 memory passes — reality verdicts + compact summaries ([#428](https://github.com/cq27-dev/rag-rat/pull/428))
- *(index)* global database by default, rag-rat consolidate importer ([#402](https://github.com/cq27-dev/rag-rat/pull/402)) ([#419](https://github.com/cq27-dev/rag-rat/pull/419))
- *(index)* generation-staged full rebuild, per-repo write locks (V043) ([#416](https://github.com/cq27-dev/rag-rat/pull/416))
- *(index)* repo-scoped clones, oracle, reconcile, memories (V042) ([#415](https://github.com/cq27-dev/rag-rat/pull/415))
- *(search)* repo-scoped FTS and papertrail queries (V041) ([#414](https://github.com/cq27-dev/rag-rat/pull/414))
- *(index)* V040 — repo_id scoping on core tables, scope view, gc ([#398](https://github.com/cq27-dev/rag-rat/pull/398)) ([#413](https://github.com/cq27-dev/rag-rat/pull/413))
- *(schema)* V039 — move per-repo meta singletons to repo_meta ([#397](https://github.com/cq27-dev/rag-rat/pull/397)) ([#412](https://github.com/cq27-dev/rag-rat/pull/412))
- *(schema)* V038 repos registry, repo identity, and data_dir helper ([#396](https://github.com/cq27-dev/rag-rat/pull/396)) ([#408](https://github.com/cq27-dev/rag-rat/pull/408))

### Fixed

- *(locks)* release flock explicitly on drop — close alone races fork-inherited fds ([#409](https://github.com/cq27-dev/rag-rat/pull/409)) ([#410](https://github.com/cq27-dev/rag-rat/pull/410))

### Other

- multi-repo global-db integration matrix ([#433](https://github.com/cq27-dev/rag-rat/pull/433))
- give git fixtures deterministic distinct identities ([#435](https://github.com/cq27-dev/rag-rat/pull/435))

## [0.12.0](https://github.com/cq27-dev/rag-rat/compare/v0.11.0...v0.12.0) - 2026-07-02

### Added

- *(clones)* make the write-time clone-check size guard mode-aware ([#296](https://github.com/cq27-dev/rag-rat/pull/296)) ([#393](https://github.com/cq27-dev/rag-rat/pull/393))
- *(clones)* bounded postings fast path in the write-time clone check ([#296](https://github.com/cq27-dev/rag-rat/pull/296)) ([#392](https://github.com/cq27-dev/rag-rat/pull/392))
- *(clones)* populate clone_subblock_postings in the generation-staged precompute ([#296](https://github.com/cq27-dev/rag-rat/pull/296)) ([#391](https://github.com/cq27-dev/rag-rat/pull/391))
- *(schema)* V037 — clone_subblock_postings + postings_written gate ([#296](https://github.com/cq27-dev/rag-rat/pull/296)) ([#390](https://github.com/cq27-dev/rag-rat/pull/390))
- *(log)* config-gated tracing debug log + hook-embedding repro harness ([#377](https://github.com/cq27-dev/rag-rat/pull/377))

### Fixed

- *(embed)* a fresh index adopts the configured embedding model, not the hash fallback ([#394](https://github.com/cq27-dev/rag-rat/pull/394)) ([#395](https://github.com/cq27-dev/rag-rat/pull/395))

### Other

- *(clones)* BFS the subject's component in clones_for_symbol on the live path ([#270](https://github.com/cq27-dev/rag-rat/pull/270)) ([#384](https://github.com/cq27-dev/rag-rat/pull/384))
- *(store)* rewrap two doc comments to satisfy nightly rustfmt ([#385](https://github.com/cq27-dev/rag-rat/pull/385))
- *(reconcile)* stream estimated_reconcile_jobs — the last materialize-everything site ([#64](https://github.com/cq27-dev/rag-rat/pull/64)) ([#383](https://github.com/cq27-dev/rag-rat/pull/383))
- *(maintenance)* use cheap active-model llm_status backlog, not O(repo) reconcile_plan ([#380](https://github.com/cq27-dev/rag-rat/pull/380))
- *(reconcile)* stream embedding_reconcile_plan candidate materialization ([#379](https://github.com/cq27-dev/rag-rat/pull/379)) ([#382](https://github.com/cq27-dev/rag-rat/pull/382))
- *(embed)* pin base-scoping of embedding coverage counts ([#360](https://github.com/cq27-dev/rag-rat/pull/360)) ([#381](https://github.com/cq27-dev/rag-rat/pull/381))
- *(cli)* split commands/mod.rs god-module into per-domain siblings ([#374](https://github.com/cq27-dev/rag-rat/pull/374))
- *(embed)* split index/ai/reconcile.rs god-module into cohesive siblings ([#375](https://github.com/cq27-dev/rag-rat/pull/375))
- *(clones)* split god-module query_api/clones.rs into cohesive siblings ([#368](https://github.com/cq27-dev/rag-rat/pull/368))
- *(cli)* split init/wizard god-module steps.rs into cohesive siblings ([#367](https://github.com/cq27-dev/rag-rat/pull/367))
- extract oversized inline test modules into sibling files ([#366](https://github.com/cq27-dev/rag-rat/pull/366))
- dedup credential and row-count helpers ([#364](https://github.com/cq27-dev/rag-rat/pull/364))

## [0.11.0](https://github.com/cq27-dev/rag-rat/compare/v0.10.0...v0.11.0) - 2026-07-02

### Added

- *(embed)* content-address the vector cache so embeddings survive reindex ([#357](https://github.com/cq27-dev/rag-rat/pull/357)) ([#358](https://github.com/cq27-dev/rag-rat/pull/358))
- *(embed)* benchmark-embedding CLI behind the eval feature ([#354](https://github.com/cq27-dev/rag-rat/pull/354))
- *(embed)* embed light/incremental reconciles against the local query_endpoint ([#356](https://github.com/cq27-dev/rag-rat/pull/356))
- *(init)* backend picker (ollama/infinity/vLLM) in the embedding wizard, ordered by measured efficiency ([#352](https://github.com/cq27-dev/rag-rat/pull/352))
- *(embed)* model context-awareness — warn short-context models truncate long code, steer to a long-context model ([#351](https://github.com/cq27-dev/rag-rat/pull/351))
- *(cookbook)* infinity + vLLM ephemeral recipes over OpenAI /v1/embeddings ([#349](https://github.com/cq27-dev/rag-rat/pull/349))
- *(embed)* centralize remote embedding on OpenAI /v1/embeddings + backend selector ([#348](https://github.com/cq27-dev/rag-rat/pull/348))
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

## [0.10.0](https://github.com/cq27-dev/rag-rat/compare/v0.9.0...v0.10.0) - 2026-06-26

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
- *(maintenance)* skip the git-hook pass when a watcher is already live ([#307](https://github.com/cq27-dev/rag-rat/pull/307))
- *(mcp)* jemalloc so idle servers return heap to the OS ([#305](https://github.com/cq27-dev/rag-rat/pull/305))

## [0.9.0](https://github.com/cq27-dev/rag-rat/compare/v0.8.0...v0.9.0) - 2026-06-24

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
- *(deps)* upgrade rmcp to 1.8.0 (unbreaks `cargo install`) ([#269](https://github.com/cq27-dev/rag-rat/pull/269))
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

## [0.8.0](https://github.com/cq27-dev/rag-rat/compare/v0.7.0...v0.8.0) - 2026-06-19

### Added

- *(index)* intern symbol qualified_name into the shared name_strings pool ([#224](https://github.com/cq27-dev/rag-rat/pull/224)) ([#227](https://github.com/cq27-dev/rag-rat/pull/227))
- *(index)* dictionary-zstd chunk-text compression + drop chunks.text ([#77](https://github.com/cq27-dev/rag-rat/pull/77)) ([#225](https://github.com/cq27-dev/rag-rat/pull/225))
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

- *(impact)* stop scanning chunks.text in impact_surface ([#77](https://github.com/cq27-dev/rag-rat/pull/77)) ([#223](https://github.com/cq27-dev/rag-rat/pull/223))
- *(oracle)* pin indexer dep tree + lock corpus installs (#185 items 2-3) ([#197](https://github.com/cq27-dev/rag-rat/pull/197))

## [0.7.0](https://github.com/cq27-dev/rag-rat/compare/v0.6.0...v0.7.0) - 2026-06-16

### Added

- *(init)* bind C++ header dirs as cpp + scaffold the full commented config ([#189](https://github.com/cq27-dev/rag-rat/pull/189))
- *(oracle)* C++ corpus (yaml-cpp) + resolve .h headers as C++ under a cpp binding ([#186](https://github.com/cq27-dev/rag-rat/pull/186))
- *(oracle)* scip-typescript backend + ts-ky corpus ([#184](https://github.com/cq27-dev/rag-rat/pull/184))
- *(python)* from-import alias resolution ([#174](https://github.com/cq27-dev/rag-rat/pull/174)) ([#179](https://github.com/cq27-dev/rag-rat/pull/179))
- *(init)* Python root-entrypoint binding + content-aware dir selection ([#173](https://github.com/cq27-dev/rag-rat/pull/173)) ([#181](https://github.com/cq27-dev/rag-rat/pull/181))
- *(python)* prefer a base class when resolving `implements` edges ([#172](https://github.com/cq27-dev/rag-rat/pull/172)) ([#180](https://github.com/cq27-dev/rag-rat/pull/180))
- *(oracle)* unified tier-driven corpus runner + oracle.yml ([#177](https://github.com/cq27-dev/rag-rat/pull/177))
- *(oracle)* scip-python backend — Python compiler-grade resolution ([#176](https://github.com/cq27-dev/rag-rat/pull/176))
- *(oracle)* `oracle report --corpus <id>` — run a corpus + emit its resolution report ([#175](https://github.com/cq27-dev/rag-rat/pull/175))
- *(lang)* Python language support (symbols, graph edges, embeddings) + AST low-signal ([#167](https://github.com/cq27-dev/rag-rat/pull/167))
- *(oracle)* corpus profiles + health-gate loader ([#171](https://github.com/cq27-dev/rag-rat/pull/171))
- *(oracle)* live before/after resolution report computation ([#168](https://github.com/cq27-dev/rag-rat/pull/168))
- *(oracle)* resolution-report + corpus-profile schema contract ([#166](https://github.com/cq27-dev/rag-rat/pull/166))
- *(eval)* gate eval behind a non-default feature + add CI eval job ([#162](https://github.com/cq27-dev/rag-rat/pull/162))
- *(mcp)* nudge the agent to re-anchor stale memories via tool-result content ([#160](https://github.com/cq27-dev/rag-rat/pull/160))

### Fixed

- *(resolve)* stop asserting high confidence on guessed Rust type references ([#192](https://github.com/cq27-dev/rag-rat/pull/192))

### Other

- *(oracle)* normalize small-tier corpora to a comparable ~8k-12k edge scale ([#190](https://github.com/cq27-dev/rag-rat/pull/190))
- list Python in the README's code-graph languages ([#183](https://github.com/cq27-dev/rag-rat/pull/183))

## [0.6.0](https://github.com/cq27-dev/rag-rat/compare/v0.5.0...v0.6.0) - 2026-06-15

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
- *(mcp)* report the real crate version in serverInfo (was hardcoded 0.3.1) ([#134](https://github.com/cq27-dev/rag-rat/pull/134))
- *(mcp)* harden hook listener — bind-retry, coverage-flaky test, cfg-gated tokio (#53, #84, #54) ([#132](https://github.com/cq27-dev/rag-rat/pull/132))

### Other

- de-spine the index + mcp crates — module splits, param structs, naming ([#155](https://github.com/cq27-dev/rag-rat/pull/155))
- rewrite README, add oracle/grep-augmentation docs, fix MCP setup footgun
- *(index)* regression for foreign leaked rows self-healing on full rebuild ([#59](https://github.com/cq27-dev/rag-rat/pull/59)) ([#133](https://github.com/cq27-dev/rag-rat/pull/133))

## [0.5.0](https://github.com/cq27-dev/rag-rat/compare/v0.4.0...v0.5.0) - 2026-06-14

### Added

- default to TOON output for CLI + MCP, --json opt-out ([#104](https://github.com/cq27-dev/rag-rat/pull/104)) ([#123](https://github.com/cq27-dev/rag-rat/pull/123))
- *(oracle)* scip-clang C/C++ backend ([#71](https://github.com/cq27-dev/rag-rat/pull/71))
- *(oracle)* SCIP reader, occurrence→edge join, heuristic precision/recall eval ([#81](https://github.com/cq27-dev/rag-rat/pull/81))
- *(edges)* persist callee-identifier byte range on graph edges ([#67](https://github.com/cq27-dev/rag-rat/pull/67)) ([#76](https://github.com/cq27-dev/rag-rat/pull/76))
- *(index)* compile and apply real .gitignore in walker + watcher ([#66](https://github.com/cq27-dev/rag-rat/pull/66))

### Fixed

- *(mcp)* serialize logical_symbol_id as a string across serde boundaries ([#130](https://github.com/cq27-dev/rag-rat/pull/130)) ([#131](https://github.com/cq27-dev/rag-rat/pull/131))
- *(mcp)* wire memory_rebind into the tool router so it's callable ([#128](https://github.com/cq27-dev/rag-rat/pull/128)) ([#129](https://github.com/cq27-dev/rag-rat/pull/129))
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
- SCIP moniker anchors for repo memories + logical symbols ([#70](https://github.com/cq27-dev/rag-rat/pull/70)) ([#86](https://github.com/cq27-dev/rag-rat/pull/86))
- SCIP oracle run, Compiler tier, resolved-external, compare_graph_to_scip ([#69](https://github.com/cq27-dev/rag-rat/pull/69)) ([#82](https://github.com/cq27-dev/rag-rat/pull/82))
- *(readme)* lead with positioning + surface the grep-augmentation proof
- cut idle MCP load (no-op write skip + sweep gating + runtime cap) ([#65](https://github.com/cq27-dev/rag-rat/pull/65))
- *(git-history)* gate the per-pass reload on HEAD/root/shallow change
- *(index)* cut full-index peak RSS and add memory diagnostics
- drop module-doc references to the removed spec/plan files
- *(index)* nightly rustfmt the wave-rebuild code
- *(index)* process the full rebuild in waves to bound peak memory
- *(index)* nightly rustfmt the indexing-rework code
