# C and C++ tree-sitter query spike

Issue #942 evaluated whether compiled tree-sitter queries should replace the shared named-node
walker for C-family imports, calls, and type references. The prototype is intentionally test-only:
queries discover candidate nodes, then pass every capture through the production extraction
context, `c_like_edges`, and `EdgeEmitter`. They do not own normalization or source-symbol
attribution.

## Result

Reject query-backed discovery for the production C and C++ extractors.

The query and manual paths produced byte-identical candidate identities on the pinned oracle
corpora, so queries provide no precision, recall, or candidate-coverage improvement. They cost more
time and allocations, add a second traversal mechanism, and require grammar-specific query
maintenance. A hybrid path is also not warranted: the existing backend recognizes only three node
families, and routing any subset through a query would still require the manual whole-tree walk for
the other languages.

Keep queries as a future option for genuinely non-local structural patterns, where predicates or
relationships between several nodes can replace substantial procedural matching.

## Differential output

The corpus harness uses the exact revisions and language bindings from `tools/oracle-corpora.toml`.
It parses each file once, compares every field of every ordered candidate, and reports totals by
edge kind.

| corpus scope | files | changed files | imports | calls | type references |
|---|---:|---:|---:|---:|---:|
| libuv `src` as C | 104 | 0 | 848 | 7,565 | 5,329 |
| yaml-cpp `src` as C++ | 53 | 0 | 239 | 1,747 | 4,243 |
| yaml-cpp `include` as C++ | 37 | 0 | 158 | 556 | 2,877 |

The current compiler-oracle baseline therefore remains unchanged:

| corpus | edges | heuristic to compiler resolution | precision | recall |
|---|---:|---:|---:|---:|
| `c-libuv` | 12,894 | 37.9% to 48.6% | 90.2% | 55.3% |
| `cpp-yaml` | 9,423 | 43.2% to 72.9% | 82.8% | 41.0% |

These oracle metrics are projected from exact candidate equality rather than a production query
switch: candidate content and order are identical before resolution, so the resolver and oracle
receive the same inputs.

## Runtime and allocations

Discovery-only timings exclude parsing and reuse the same parsed tree. On the real corpus fixtures
in an unoptimized test build, the query path was slower:

| corpus scope | manual | query | query overhead |
|---|---:|---:|---:|
| libuv `src` | 353.0 ms | 534.0 ms | +51% |
| yaml-cpp `src` + `include` | 186.6 ms | 352.4 ms | +89% |

An optimized 10,000-iteration microbenchmark over representative C and C++ files emitted 230,000
candidates per strategy. Across seven alternating runs, median discovery time was 287.8 ms for the
manual walker and 703.4 ms for queries (+144%).

Heaptrack over the same optimized harness reported:

| strategy | allocation calls | peak heap |
|---|---:|---:|
| manual | 1,690,554 | 326.81 KiB |
| query | 1,763,171 | 4.57 MiB |

Queries made 72,617 more allocation calls (+4.3%) and used about 14 times the peak heap. The totals
include the Rust test harness equally on both sides; the comparison isolates the selected discovery
strategy after startup.

## Grammar drift behavior

Each grammar has a dedicated query stored in a `OnceLock<Result<Query, String>>`. Compilation occurs
once per process. An invalid node name, field, or pattern is retained as an explicit error and makes
the differential test fail with the tree-sitter query diagnostic, rather than silently disabling
an edge kind.
