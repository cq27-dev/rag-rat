---
name: rust-modern-style
description: "Apply rag-rat's Rust style when writing or reviewing Rust: current stable idioms, module-qualified free functions and constants, explicit persistence contracts, and narrow APIs."
metadata:
  type: code-style
  language: rust
  triggers: Rust, imports, modules, SQL, persisted enums, code style, code review
---

# Rust Modern Style

Use this skill when writing or reviewing Rust in the rag-rat repository. It complements async-specific
guidance; this skill owns the workspace's general Rust, module, persistence, and API conventions.

## Baseline

- Target the Rust 2024 edition and the current workspace `rust-version`; do not add compatibility
  shims for older compilers that the workspace does not support.
- Prefer current stable idioms when they make control flow clearer: `let-else`, inline format args,
  `matches!`, `is_some_and`, and `is_none_or`.
- Modern does not mean clever. Prefer explicit `match`, `if let`, and early returns over iterator or
  combinator chains that hide domain states or side effects.
- Prefer `std::sync::OnceLock` or `LazyLock` over new `lazy_static!` usage.
- Use `&str` and slices at borrowed boundaries instead of `&String` and `&Vec<T>`.

## Imports

Types, traits, enums, and structs may be imported directly. In a mixed or large import group, bring
the module in with `{self, ..}` and qualify free functions, constants, and macros at the call site.
This preserves origin information where the code is read.

```rust
// Avoid: callables and constants lose their module identity.
use crate::index::graph_index::{
    KeyVersionStamp, LOGICAL_KEY_VERSION, rebuild_logical_symbols,
};

// Prefer: types are direct; operations and constants retain their origin.
use crate::index::graph_index::{self, KeyVersionStamp};

graph_index::rebuild_logical_symbols(...);
let version = graph_index::LOGICAL_KEY_VERSION;
```

A small, single-purpose import of one function is fine. Apply this rule when an import list mixes
roles or makes bare call sites ambiguous, not mechanically to every `use`.

## Modules And Visibility

- Treat `mod.rs` as a curated index: module declarations and intentional `pub`/`pub(crate)`
  re-exports. Put implementation in cohesive sibling files.
- Split by domain job, not arbitrary line count. Keep helpers private and adjacent to the flow they
  support.
- Sibling modules should import through `super::`, not back through their parent's public re-export.
- Start private and widen only for a real production caller: private, then `pub(super)`, then
  `pub(crate)`, and `pub` only for a genuine public boundary.
- Never widen production visibility solely for a test; colocate the test or use a test-only seam.

## Errors And State

- Convert errors where subsystem ownership changes. Do not leak low-level DB or transport errors
  through layers whose callers cannot act on them.
- Capability loss must be explicit (`Blocked`, a typed reason, or an error), never an empty result or
  success-with-zero-work.
- Avoid catch-all classifications for errors that affect persistence, retry, or user-visible state.
  Use a testable classification function with deliberate cases.

## Persisted Enums

Persisted classifications are schema. Keep them closed and low-cardinality. Prefer deriving
`strum::EnumString` and `strum::IntoStaticStr` with an explicit `#[strum(serialize_all = "...")]`
token policy; keep `as_db_str()` and `from_db_str()` as thin, named DB-boundary wrappers and add an
exact-token round-trip test. Use a manual match only when parsing intentionally has custom semantics
such as a compatibility alias, fallback, or richer error classification.

Never persist `Display`, `Debug`, or user-facing prose as a machine classification. Changing a stored
token requires the same migration discipline as changing a column.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum RunState {
    Completed,
    Blocked,
}

impl RunState {
    fn as_db_str(self) -> &'static str {
        self.into()
    }

    fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown run state `{value}`"))
    }
}
```

When the same enum also crosses serde/config boundaries, set the corresponding explicit serde rename
policy and test that it agrees with the strum tokens. Do not assume one derive configures the other.

## Database Code

- Make read/write direction obvious in names. Reads use verbs such as `get_`, `list_`, `find_`,
  `load_`, and `exists_`; writes use `insert_`, `upsert_`, `record_`, `clear_`, `delete_`, `mark_`,
  and `apply_`.
- Keep non-trivial SQL in a helper named for the domain question, not the table or orchestration step.
- Document scope, visibility, generation, suppression, and ordering invariants beside the SQL.
- Tests assert the domain predicate and migration behavior, not the query text.
- One durable transition has one canonical writer that owns timestamping, versioning, cleanup, and
  related side effects.

## Time And Boundaries

- Use the subsystem's injected or centralized clock (`now_ms()` or a passed timestamp). Domain logic
  must not read wall-clock time independently.
- Read time once per logical operation and pass it into helpers so persisted rows agree.
- Use owned, simple DTOs across FFI, worker, thread, and persistence boundaries.
- Introduce newtypes for IDs or timestamps when same-typed values can be transposed.
- Never hold a database guard or borrowed row across `.await`; complete the DB phase, return owned
  data, then await, then enter the next DB phase.

## Function Shape

- Prefer parameter structs once a function has more than roughly four meaningful parameters,
  repeated same-typed IDs, or values that always travel together. Construct them with named fields,
  not another positional constructor.
- Separate stable context/dependencies from each call's command or query payload.
- Replace positional booleans with enums that name the behavior. Named boolean fields in a DTO are
  fine when the field name makes intent clear.
- Keep scopes small. Use inner blocks to end borrows, locks, and transactions at phase boundaries.
- A long function is acceptable when it reads as one linear protocol. Extract stable, named phases,
  not arbitrary chunks such as `process_part_2`.
- Keep match arms as dispatch. Extract multi-step side effects into a named operation.
- Peel off terminal states with early returns so the success path stays flat.

## Naming

- Types are nouns, operations are verb phrases, and boolean predicates read as questions.
- Name locals for their state-machine role (`pending_paths`, `active_generation`), not their container
  type (`items`, `data`, `result`).
- Side-effecting functions say so: `record_`, `persist_`, `emit_`, `apply_`, `mark_`, or `clear_`.
  Functions named `build_`, `resolve_`, `classify_`, or `derive_` should remain pure.
- Avoid vague `Manager`, `Handler`, `Processor`, and `Helper` types when a capability-specific name
  is available.

## Orchestration

Background reconciliation follows: scan for real work, decide capability/blocking state, act within
an explicit budget, then report what happened. Scan before checking optional capabilities so existing
work cannot be mislabeled as `NoWork`.

Orchestration owns sequencing, not mechanics. SQL, protocol encoding, retry bookkeeping, and durable
state transitions belong to their named subsystem helpers.

## Comments And Verification

- Comments explain invariants, rationale, ordering, or threat models, not the next obvious line.
- Durable comments describe the scenario, not a review round, agent workflow, or temporary task ID.
- Keep migration tests with migration SQL and add negative tests for scope/isolation boundaries.
- Format with `cargo +nightly fmt`; CI uses nightly rustfmt.
- Run the narrowest relevant tests first, then the affected crate or workspace checks required by the
  change.
