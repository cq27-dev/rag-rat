pub(in crate::index) mod extract;
mod helpers;
mod imports;
pub(crate) use imports::use_binds_name;
mod intern;
mod resolve;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

pub(crate) use extract::*;
pub(crate) use helpers::*;
pub(crate) use imports::scan_packages;
use intern::{OptSym, StrArena, Sym};
use rag_rat_base::language::Language;
pub(crate) use resolve::*;
use rusqlite::{Connection, params};
use serde::Serialize;
use tree_sitter::Node;

use crate::index::parser;

pub const MAX_GRAPH_PARSE_BYTES: usize = 512_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum EdgeKind {
    Imports,
    Exports,
    CallsName,
    Constructs,
    /// A language-level operator token references its declaration independently of the callable
    /// implementation reached by the companion `calls_name` edge.
    UsesOperator,
    /// An operator declaration depends on the precedence group that defines its parse behavior.
    UsesPrecedenceGroup,
    UsesMacro,
    ReferencesType,
    Implements,
    Contains,
    /// Synthesized at resolve time (#200): a `dispatches` edge from a function that CONSTRUCTS a
    /// message/command enum variant to the handler that the matching arm calls — the actor-channel
    /// dispatch hop a static call graph otherwise misses. NOT extracted directly; produced by
    /// `synthesize_dispatch_edges` from the two fact kinds below.
    Dispatches,
    /// Internal dispatch FACT (#200), never user-visible: function `F` constructs `Enum::Variant`.
    /// `to_name` is the 2-segment `Enum::Variant` key. Consumed by dispatch synthesis; excluded
    /// from every user-facing edge-kind set, from PageRank, and from the oracle.
    DispatchConstruct,
    /// Internal dispatch FACT (#200), never user-visible: a `match`/`when` arm for `Enum::Variant`
    /// (carried in `evidence`) whose body calls handler `to_name`. Resolution binds `to_name` to
    /// the handler symbol; synthesis then joins it to the constructor on the variant key. Same
    /// exclusions as `DispatchConstruct`.
    DispatchHandle,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Imports => "imports",
            Self::Exports => "exports",
            Self::CallsName => "calls_name",
            Self::Constructs => "constructs",
            Self::UsesOperator => "uses_operator",
            Self::UsesPrecedenceGroup => "uses_precedence_group",
            Self::UsesMacro => "uses_macro",
            Self::ReferencesType => "references_type",
            Self::Implements => "implements",
            Self::Contains => "contains",
            Self::Dispatches => "dispatches",
            Self::DispatchConstruct => "dispatch_construct",
            Self::DispatchHandle => "dispatch_handle",
        }
    }

    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "imports" => Ok(Self::Imports),
            "exports" => Ok(Self::Exports),
            "calls_name" => Ok(Self::CallsName),
            "constructs" => Ok(Self::Constructs),
            "uses_operator" => Ok(Self::UsesOperator),
            "uses_precedence_group" => Ok(Self::UsesPrecedenceGroup),
            "uses_macro" => Ok(Self::UsesMacro),
            "references_type" => Ok(Self::ReferencesType),
            "implements" => Ok(Self::Implements),
            "contains" => Ok(Self::Contains),
            "dispatches" => Ok(Self::Dispatches),
            "dispatch_construct" => Ok(Self::DispatchConstruct),
            "dispatch_handle" => Ok(Self::DispatchHandle),
            _ => anyhow::bail!("unknown edge kind `{value}`"),
        }
    }
}

#[cfg(test)]
mod edge_kind_tests {
    use super::EdgeKind;

    #[test]
    fn persisted_tokens_round_trip() {
        let kinds = [
            EdgeKind::Imports,
            EdgeKind::Exports,
            EdgeKind::CallsName,
            EdgeKind::Constructs,
            EdgeKind::UsesOperator,
            EdgeKind::UsesPrecedenceGroup,
            EdgeKind::UsesMacro,
            EdgeKind::ReferencesType,
            EdgeKind::Implements,
            EdgeKind::Contains,
            EdgeKind::Dispatches,
            EdgeKind::DispatchConstruct,
            EdgeKind::DispatchHandle,
        ];

        for kind in kinds {
            assert_eq!(EdgeKind::from_db_str(kind.as_str()).unwrap(), kind);
        }
        assert!(EdgeKind::from_db_str("unknown").is_err());
    }
}

/// The `edges_data.hidden` value for a row (#734): 1 exactly when the row is not a public graph
/// edge — an internal dispatch FACT kind ([`EdgeKind::DispatchConstruct`] /
/// [`EdgeKind::DispatchHandle`], #200) or a suppressed unresolved candidate. Every direct
/// `edges_data` writer stamps the flag through this one helper so the `edges` view's
/// `WHERE hidden = 0` — a single integer compare per row, the point of materializing
/// visibility — can trust it. The view's INSTEAD OF triggers and the V075 backfill mirror the
/// same predicate in SQL.
pub(crate) fn edge_hidden_flag(edge_kind: &str, resolution: &str) -> i64 {
    let dispatch_fact = edge_kind == EdgeKind::DispatchConstruct.as_str()
        || edge_kind == EdgeKind::DispatchHandle.as_str();
    i64::from(dispatch_fact || resolution == "suppressed")
}

/// #734 test tripwire: assert `edges_data.hidden` agrees with the visibility predicate it
/// materializes ([`edge_hidden_flag`]) on EVERY row, whatever writer produced it. A disagreeing
/// row either leaks an internal/suppressed row into every query-layer read or silently drops a
/// real edge from the graph — call this after any test pass that inserts or re-resolves edges.
#[cfg(test)]
pub(crate) fn assert_hidden_agrees_with_visibility(conn: &rusqlite::Connection) {
    let disagreeing: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges_data d
             JOIN name_strings ek ON ek.id = d.edge_kind_id
             JOIN name_strings r ON r.id = d.resolution_id
             WHERE d.hidden <> CASE WHEN ek.value IN ('dispatch_construct', 'dispatch_handle')
                                         OR r.value = 'suppressed'
                                    THEN 1 ELSE 0 END",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        disagreeing, 0,
        "edges_data.hidden must match the visibility predicate on every row"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum EdgeConfidence {
    Exact,
    Syntactic,
    NameOnly,
    Ambiguous,
}

impl EdgeConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "Exact",
            Self::Syntactic => "Syntactic",
            Self::NameOnly => "NameOnly",
            Self::Ambiguous => "Ambiguous",
        }
    }
}

/// Byte range of the callee identifier token (the final `::`/`.` segment of the called name), as
/// `text[start_byte..end_byte]`. Distinct from `EdgeCandidate.source_span`, which covers the whole
/// `call_expression`. SCIP occurrences key on the identifier token's position, so the SCIP-oracle
/// join (#61) needs this range; `(line, col)` in the document's position encoding is derived at
/// join time from the checkout bytes, so only the byte range is stored. Set only for
/// symbol-referencing edges (built via `symbol_edge`/`symbol_edge_with_context`); `None` for
/// file-level / `contains` edges and for constructs where a clean identifier node isn't available.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CalleeRange {
    pub(in crate::index) start_byte: usize,
    pub(in crate::index) end_byte: usize,
}

impl CalleeRange {
    /// The byte range of a tree-sitter node — the callee identifier token's `start_byte..end_byte`.
    pub(crate) fn of_node(node: Node<'_>) -> Self {
        CalleeRange { start_byte: node.start_byte(), end_byte: node.end_byte() }
    }
}

/// Module-aware import scope of a Rust `use` (or the body range of an inline `mod`), stored on
/// Imports edges in the DEDICATED `import_scope_*` / `import_mod_id` columns (#61 per-package +
/// module-aware rework). For a `use`: `[scope_start, scope_end)` is the enclosing module body — or
/// the enclosing block, for a block-local `use` — and `mod_id` is the enclosing module body's start
/// byte (`MOD_FILE_ROOT` for a top-level `use`). For an inline `mod foo { … }`: the range is the
/// module body and `mod_id` is its own start byte, so resolution can rebuild the per-file module
/// interval set (the ref→mod-id lookup) from these edges WITHOUT the tree. Kept off the `callee_*`
/// columns so the oracle's `callee_start_byte IS NOT NULL` candidate filter is untouched.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ImportScopeRange {
    pub(crate) scope_start: usize,
    pub(crate) scope_end: usize,
    pub(crate) mod_id: i64,
}

/// Sentinel `import_mod_id` for a top-level `use` / a file-root reference: no enclosing module
/// body. `-1` because a real `mod_id` is a module body's `start_byte` (≥ 0).
pub(crate) const MOD_FILE_ROOT: i64 = -1;

#[derive(Debug, Clone)]
pub(crate) struct EdgeCandidate {
    pub(in crate::index) from_symbol_id: Option<i64>,
    pub(in crate::index) from_name: Option<String>,
    pub(in crate::index) to_name: String,
    pub(in crate::index) target_qualified_name: Option<String>,
    pub(in crate::index) evidence: Option<String>,
    pub(in crate::index) receiver_hint: Option<String>,
    pub(in crate::index) receiver_type_hint: Option<String>,
    pub(in crate::index) source_span: EdgeSpan,
    /// Byte range of the callee identifier token; see [`CalleeRange`]. `None` for non-symbol
    /// edges.
    pub(in crate::index) callee_span: Option<CalleeRange>,
    /// Module-aware import scope; see [`ImportScopeRange`]. `Some` only for Rust Imports edges (a
    /// `use`'s enclosing scope, or an inline `mod`'s body range); `None` for every other edge.
    pub(in crate::index) import_scope: Option<ImportScopeRange>,
    pub(in crate::index) edge_kind: EdgeKind,
    pub(in crate::index) confidence: EdgeConfidence,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexedSymbol {
    pub(in crate::index) id: i64,
    pub(in crate::index) file_id: i64,
    pub(in crate::index) language: String,
    pub(in crate::index) name: String,
    pub(in crate::index) qualified_name: String,
    pub(in crate::index) scope_path: String,
    pub(in crate::index) kind: String,
    pub(in crate::index) start_byte: usize,
    pub(in crate::index) end_byte: usize,
    pub(in crate::index) start_line: i64,
    pub(in crate::index) end_line: i64,
}

impl IndexedSymbol {
    /// Build symbols for the parallel prepare phase, where real DB ids don't exist yet. `id` is the
    /// symbol's index in the prepared `symbols` vec; edge candidates produced from these carry that
    /// local index as `from_symbol_id`, which `insert_prepared_file` remaps to the real DB id once
    /// symbols are inserted. Sorted by (start_byte, end_byte) to match `symbols_for_file`'s ORDER
    /// BY so containing-symbol tie-breaking is identical to the inline path.
    pub(crate) fn local_from_prepared(
        language: Language,
        symbols: &[crate::index::symbols::Symbol],
    ) -> Vec<Self> {
        let mut out = symbols
            .iter()
            .enumerate()
            .map(|(idx, symbol)| IndexedSymbol {
                id: idx as i64,
                file_id: 0,
                language: language.as_str().to_string(),
                name: symbol.name.clone(),
                qualified_name: symbol.qualified_name.clone(),
                scope_path: symbol.scope_path.clone(),
                kind: symbol.kind.clone(),
                start_byte: symbol.start_byte,
                end_byte: symbol.end_byte,
                start_line: i64::try_from(symbol.start_line).unwrap_or(0),
                end_line: i64::try_from(symbol.end_line).unwrap_or(0),
            })
            .collect::<Vec<_>>();
        out.sort_by_key(|symbol| (symbol.start_byte, symbol.end_byte));
        out
    }
}

impl EdgeCandidate {
    /// Replace a local `from_symbol_id` (an index into the prepared symbols, as produced by
    /// [`IndexedSymbol::local_from_prepared`]) with the real DB id from `db_ids`, indexed by the
    /// prepared symbol's original position. A `None` from_symbol_id (file-level edge) is left
    /// alone.
    pub(crate) fn remap_from_symbol_id(&mut self, db_ids: &[i64]) {
        if let Some(local) = self.from_symbol_id {
            self.from_symbol_id =
                usize::try_from(local).ok().and_then(|index| db_ids.get(index)).copied();
        }
    }
}

/// An accumulated symbol in interned form: the four string fields are [`Sym`] ids into the graph's
/// arena, so the per-symbol footprint is a handful of `u32`s instead of four heap `String`s.
/// Hydrated back to an owned [`IndexedSymbol`] only at resolution, when the arena is frozen.
struct CompactSymbol {
    id: i64,
    file_id: i64,
    language: Sym,
    name: Sym,
    qualified_name: Sym,
    scope_path: Sym,
    kind: Sym,
    start_byte: usize,
    end_byte: usize,
    start_line: i64,
    end_line: i64,
}

impl CompactSymbol {
    fn hydrate(&self, arena: &StrArena) -> IndexedSymbol {
        IndexedSymbol {
            id: self.id,
            file_id: self.file_id,
            language: arena.get(self.language).to_string(),
            name: arena.get(self.name).to_string(),
            qualified_name: arena.get(self.qualified_name).to_string(),
            scope_path: arena.get(self.scope_path).to_string(),
            kind: arena.get(self.kind).to_string(),
            start_byte: self.start_byte,
            end_byte: self.end_byte,
            start_line: self.start_line,
            end_line: self.end_line,
        }
    }
}

/// Source span as four `u32`s (16 bytes) instead of [`EdgeSpan`]'s four `i64`s (32). Byte offsets
/// and line numbers fit `u32` for any real source file; the accumulator holds millions of these.
#[derive(Clone, Copy)]
struct CompactSpan {
    start_line: u32,
    end_line: u32,
    start_byte: u32,
    end_byte: u32,
}

impl CompactSpan {
    fn from_span(span: EdgeSpan) -> Self {
        let clamp = |value: i64| u32::try_from(value).unwrap_or(0);
        CompactSpan {
            start_line: clamp(span.start_line),
            end_line: clamp(span.end_line),
            start_byte: clamp(span.start_byte),
            end_byte: clamp(span.end_byte),
        }
    }
}

/// An accumulated edge candidate in interned form. The five string fields become [`Sym`]/[`OptSym`]
/// ids and the span shrinks to [`CompactSpan`], taking the per-edge footprint from ~184 bytes (with
/// five owned `Option<String>`) to ~64 — the dominant lever on full-rebuild peak RSS, since the
/// edge accumulator is held in full until resolution.
///
/// The callee identifier byte range is two bare `u32`s, NOT `Option<CalleeRange>`: `u32::MAX` is
/// the none-sentinel (mirroring [`OptSym`]'s niche), so the optional range costs 8 bytes flat
/// rather than the 24 an `Option<(usize, usize)>` would. Safe because graph-parsed files are
/// bounded at `MAX_GRAPH_PARSE_BYTES` (512_000), so every real byte offset fits `u32` with
/// `u32::MAX` free as the sentinel. Footprint delta: +8 bytes/edge (~64 → ~72) — at the kernel's
/// ~11M candidates that is ~88 MB of additional accumulator, held only until the single
/// resolve-and-insert pass.
struct CompactEdge {
    from_symbol_id: Option<i64>,
    from_name: OptSym,
    to_name: Sym,
    target_qualified_name: OptSym,
    evidence: OptSym,
    receiver_hint: OptSym,
    receiver_type_hint: OptSym,
    source_span: CompactSpan,
    /// Callee identifier byte range; `u32::MAX` in `callee_start_byte` is the `None` sentinel.
    callee_start_byte: u32,
    callee_end_byte: u32,
    /// Module-aware import scope (#61); `u32::MAX` in `import_scope_start_byte` is the `None`
    /// sentinel (a non-import edge). `import_mod_id` is a real `i64` ([`MOD_FILE_ROOT`] = -1 for a
    /// top-level `use`), so it cannot carry the niche — the start-byte sentinel gates it.
    import_scope_start_byte: u32,
    import_scope_end_byte: u32,
    import_mod_id: i64,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
}

impl CompactEdge {
    /// `u32::MAX` sentinel marking an absent callee range (mirrors [`OptSym::NONE`]).
    const CALLEE_NONE: u32 = u32::MAX;

    /// Pack `Option<CalleeRange>` into the two `u32` fields, clamping like [`CompactSpan`] and
    /// using `u32::MAX` for `None`. A real offset can never collide with the sentinel: it is
    /// bounded by `MAX_GRAPH_PARSE_BYTES` ≪ `u32::MAX`.
    fn pack_callee(span: Option<CalleeRange>) -> (u32, u32) {
        match span {
            Some(range) => {
                let clamp = |value: usize| u32::try_from(value).unwrap_or(0);
                (clamp(range.start_byte), clamp(range.end_byte))
            },
            None => (Self::CALLEE_NONE, Self::CALLEE_NONE),
        }
    }

    /// The stored callee byte range as nullable `i64`s for the edge-row insert: `(NULL, NULL)` when
    /// the sentinel marks an absent range, otherwise the two offsets.
    fn callee_byte_columns(&self) -> (Option<i64>, Option<i64>) {
        if self.callee_start_byte == Self::CALLEE_NONE {
            (None, None)
        } else {
            (Some(i64::from(self.callee_start_byte)), Some(i64::from(self.callee_end_byte)))
        }
    }

    /// Pack `Option<ImportScopeRange>` into the two `u32` scope-byte fields + the `i64` mod-id,
    /// using `u32::MAX` in the start byte for `None` (mirrors `pack_callee`). The start sentinel —
    /// not `mod_id` — gates presence, because `MOD_FILE_ROOT` (-1) is itself a valid stored mod id.
    fn pack_import_scope(scope: Option<ImportScopeRange>) -> (u32, u32, i64) {
        match scope {
            Some(range) => {
                let clamp = |value: usize| u32::try_from(value).unwrap_or(0);
                (clamp(range.scope_start), clamp(range.scope_end), range.mod_id)
            },
            None => (Self::CALLEE_NONE, Self::CALLEE_NONE, MOD_FILE_ROOT),
        }
    }

    /// The stored import scope as an [`ImportScopeRange`], or `None` for a non-import edge. Used by
    /// the full-rebuild driver to feed `ImportScope::add_use` from the in-memory accumulator (the
    /// twin of the DB driver reading the `import_scope_*` columns).
    fn import_scope_range(&self) -> Option<ImportScopeRange> {
        if self.import_scope_start_byte == Self::CALLEE_NONE {
            None
        } else {
            Some(ImportScopeRange {
                scope_start: self.import_scope_start_byte as usize,
                scope_end: self.import_scope_end_byte as usize,
                mod_id: self.import_mod_id,
            })
        }
    }

    /// The stored import-scope range as nullable `i64`s for the edge-row insert: `(NULL, NULL,
    /// NULL)` for a non-import edge, otherwise `(scope_start, scope_end, mod_id)`.
    fn import_scope_columns(&self) -> (Option<i64>, Option<i64>, Option<i64>) {
        if self.import_scope_start_byte == Self::CALLEE_NONE {
            (None, None, None)
        } else {
            (
                Some(i64::from(self.import_scope_start_byte)),
                Some(i64::from(self.import_scope_end_byte)),
                Some(self.import_mod_id),
            )
        }
    }
}

/// Symbols (with real DB ids) and edge candidates (with their source file id) accumulated across
/// the full-rebuild insert loop, so edges can be resolved in memory and inserted once, fully
/// resolved — instead of inserting them unresolved per file and then resolving with a per-edge
/// UPDATE pass. All string fields are interned into `arena`; see [`intern`].
#[derive(Default)]
pub(crate) struct FullRebuildGraph {
    arena: StrArena,
    symbols: Vec<CompactSymbol>,
    edges: Vec<(i64, CompactEdge)>,
}

impl FullRebuildGraph {
    /// Intern and accumulate one just-inserted symbol (carrying its real DB `id`).
    pub(crate) fn push_symbol(
        &mut self,
        id: i64,
        file_id: i64,
        language: Language,
        symbol: &crate::index::symbols::Symbol,
    ) {
        let compact = CompactSymbol {
            id,
            file_id,
            language: self.arena.intern(language.as_str()),
            name: self.arena.intern(&symbol.name),
            qualified_name: self.arena.intern(&symbol.qualified_name),
            scope_path: self.arena.intern(&symbol.scope_path),
            kind: self.arena.intern(&symbol.kind),
            start_byte: symbol.start_byte,
            end_byte: symbol.end_byte,
            start_line: i64::try_from(symbol.start_line).unwrap_or(0),
            end_line: i64::try_from(symbol.end_line).unwrap_or(0),
        };
        self.symbols.push(compact);
    }

    /// Intern and accumulate one edge candidate produced by the prepare phase, after its local
    /// `from_symbol_id` has been remapped to the real DB id via `db_ids`.
    pub(crate) fn push_edge(&mut self, file_id: i64, candidate: &EdgeCandidate, db_ids: &[i64]) {
        let from_symbol_id = candidate.from_symbol_id.and_then(|local| {
            usize::try_from(local).ok().and_then(|index| db_ids.get(index)).copied()
        });
        let (callee_start_byte, callee_end_byte) = CompactEdge::pack_callee(candidate.callee_span);
        let (import_scope_start_byte, import_scope_end_byte, import_mod_id) =
            CompactEdge::pack_import_scope(candidate.import_scope);
        let compact = CompactEdge {
            from_symbol_id,
            from_name: self.arena.intern_opt(candidate.from_name.as_deref()),
            to_name: self.arena.intern(&candidate.to_name),
            target_qualified_name: self
                .arena
                .intern_opt(candidate.target_qualified_name.as_deref()),
            evidence: self.arena.intern_opt(candidate.evidence.as_deref()),
            receiver_hint: self.arena.intern_opt(candidate.receiver_hint.as_deref()),
            receiver_type_hint: self.arena.intern_opt(candidate.receiver_type_hint.as_deref()),
            source_span: CompactSpan::from_span(candidate.source_span),
            callee_start_byte,
            callee_end_byte,
            import_scope_start_byte,
            import_scope_end_byte,
            import_mod_id,
            edge_kind: candidate.edge_kind,
            confidence: candidate.confidence,
        };
        self.edges.push((file_id, compact));
    }

    fn into_parts(self) -> (StrArena, Vec<CompactSymbol>, Vec<(i64, CompactEdge)>) {
        (self.arena, self.symbols, self.edges)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EdgeSpan {
    pub(in crate::index) start_line: i64,
    pub(in crate::index) end_line: i64,
    pub(in crate::index) start_byte: i64,
    pub(in crate::index) end_byte: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EdgeContext {
    pub(in crate::index) target_qualified_name: Option<String>,
    pub(in crate::index) receiver_hint: Option<String>,
    pub(in crate::index) receiver_type_hint: Option<String>,
}

impl IndexedSymbol {
    fn span(&self) -> EdgeSpan {
        EdgeSpan {
            start_line: self.start_line,
            end_line: self.end_line,
            start_byte: i64::try_from(self.start_byte).unwrap_or(i64::MAX),
            end_byte: i64::try_from(self.end_byte).unwrap_or(i64::MAX),
        }
    }
}

pub(crate) fn degeneric_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut depth = 0;
    for ch in path.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => result.push(ch),
            _ => {},
        }
    }
    result
}

/// [`degeneric_path`] plus trait-impl owner folding: the `Type as Trait` scope segment emitted
/// for `impl Trait for Type` collapses to `Type`, so a source-form target (`Type::method`, a
/// receiver-type hint) finds trait-impl methods while their RAW scope keeps the trait — two
/// traits' same-named methods stay distinct logical symbols yet expose the same receiver
/// surface, and a call that could hit either declines as ambiguous (#567). The marker's trait
/// path is emitted with `::` rewritten to `.` (see the Rust `trait_marker`), so it is always ONE
/// `::`-segment and per-`::`-segment splitting is sound.
/// Borrowed ⇔ the path needs no normalization — the hot rebuild path allocates nothing for the
/// plain-scope majority.
/// Peel `&`/`&mut`/`*const`/`*mut` off an owner segment. The impl scope KEEPS the wrapper — it is
/// part of the impl's identity, since `impl Tr for W` and `impl Tr for &W` coexist — but the
/// RECEIVER surface must not: a source-form target is always written `W::method`, and an autoref
/// call site cannot tell which impl it lands in. Folding here is what lets `&W as Tr::method`
/// still answer to `W::method`, and lets a call that could hit either decline as ambiguous.
fn strip_receiver_wrappers(segment: &str) -> &str {
    let mut rest = segment.trim();
    loop {
        let peeled = rest
            .strip_prefix("*const ")
            .or_else(|| rest.strip_prefix("*mut "))
            .or_else(|| rest.strip_prefix('&'))
            .unwrap_or(rest)
            .trim_start();
        let peeled = peeled.strip_prefix("mut ").unwrap_or(peeled).trim_start();
        if peeled == rest {
            return rest.trim_end();
        }
        rest = peeled;
    }
}

pub(crate) fn normalized_scope_path<'a>(
    path: &'a str,
    language: Option<&str>,
) -> std::borrow::Cow<'a, str> {
    if language != Some(Language::Rust.as_str())
        || (!path.contains('<')
            && !path.contains(" as ")
            && !path.contains('&')
            && !path.contains('*'))
    {
        return std::borrow::Cow::Borrowed(path);
    }
    let degeneric = degeneric_path(path);
    std::borrow::Cow::Owned(
        degeneric
            .split("::")
            .map(|segment| strip_receiver_wrappers(segment.split(" as ").next().unwrap_or(segment)))
            .collect::<Vec<_>>()
            .join("::"),
    )
}

/// Name-keyed indexes over the symbol set, built once per resolve pass. Edge resolution used to
/// scan the entire `Vec<IndexedSymbol>` (several times) per edge — O(edges × symbols), the single
/// biggest cost in a full rebuild. These maps make each lookup ~O(1). Bucket order mirrors the
/// input `symbols` order, so first-match semantics are preserved exactly.
pub(crate) struct SymbolIndex<'a> {
    /// Exact `qualified_name` match.
    by_qualified: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
    /// Exact `scope_path` match (semantic scope, e.g. `Workspace::new`). Tried before bare-name
    /// fallback: it aligns with an edge's source-derived `target_qualified_name`, so the strong
    /// qualified path fires for methods/nested items instead of collapsing to bare-name
    /// collisions.
    by_scope_path: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
    /// Scope path with generics stripped and trait-impl owners folded (`SymbolIndex::build`
    /// matching `SymbolIndex<'a>::build`; `Worker::run` matching `Worker as Service::run`).
    by_normalized_scope_path: HashMap<String, Vec<&'a IndexedSymbol>>,
    /// Short-name fallback (`symbol.name`).
    by_name: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
    /// Candidates for the `qualified_name.ends_with("::{q}")` suffix match, keyed by the last
    /// `::`-segment of the qualified name (a name ending in `::{q}` necessarily shares `q`'s
    /// tail).
    by_qn_tail: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
}

impl<'a> SymbolIndex<'a> {
    fn build(symbols: &'a [IndexedSymbol]) -> Self {
        let mut by_qualified: HashMap<&str, Vec<&IndexedSymbol>> = HashMap::new();
        let mut by_scope_path: HashMap<&str, Vec<&IndexedSymbol>> = HashMap::new();
        let mut by_normalized_scope_path: HashMap<String, Vec<&IndexedSymbol>> = HashMap::new();
        let mut by_name: HashMap<&str, Vec<&IndexedSymbol>> = HashMap::new();
        let mut by_qn_tail: HashMap<&str, Vec<&IndexedSymbol>> = HashMap::new();
        for symbol in symbols {
            by_qualified.entry(symbol.qualified_name.as_str()).or_default().push(symbol);
            by_scope_path.entry(symbol.scope_path.as_str()).or_default().push(symbol);
            // Only paths the normalization actually changes go into the normalized map (`Owned`
            // ⇔ changed) — a plain scope is already reachable through `by_scope_path`, and
            // skipping it avoids one String per symbol on the hot rebuild path. Lookups consult
            // BOTH maps.
            if let std::borrow::Cow::Owned(normalized) =
                normalized_scope_path(&symbol.scope_path, Some(&symbol.language))
            {
                by_normalized_scope_path.entry(normalized).or_default().push(symbol);
            }
            by_name.entry(symbol.name.as_str()).or_default().push(symbol);
            by_qn_tail.entry(qn_tail(&symbol.qualified_name)).or_default().push(symbol);
        }
        Self { by_qualified, by_scope_path, by_normalized_scope_path, by_name, by_qn_tail }
    }

    /// Whether `file_id` itself defines a symbol named `name`. Import-alias rebinding defers when
    /// the imported target's bare name is also defined locally: a bare
    /// rewrite of `Account` → `User` could grab a same-file `User` instead of the import, so leave
    /// it to normal resolution. (The alias-shadow ordering — a same-module `class Account` /
    /// `Account = …` after the import reassigning the name — is handled at extraction by bounding
    /// the alias's `scope_end` at the next module-scope rebinding, not here.)
    pub(crate) fn file_defines(&self, file_id: i64, name: &str) -> bool {
        self.by_name.get(name).is_some_and(|symbols| symbols.iter().any(|s| s.file_id == file_id))
    }
}

/// The ONE explicit classification of a stored receiver-type hint (#567 review): computed once
/// at request build — the only place the import scope is visible — and consumed by
/// `resolve_symbol`, so every receiver-type rule hangs off this seam instead of independent
/// guards. The shared rule set:
/// - canonicalization (generics folded, `crate::`/`self::` resolved, `super::` declined) happened
///   at EXTRACTION; the stored hint is already canonical;
/// - `Local*` identities may resolve; tail matching is a conservative FALLBACK for a qualified
///   local hint only, and only when the tail names exactly one viable target;
/// - `ExternalQualified` and `Ambiguous` never resolve against local symbols — `use
///   external::Worker; fn f(w: Worker) { w.run() }` must not bind to an unrelated local
///   `Worker::run`, and the callee-name suppression (`imported_external` below) cannot see the TYPE
///   (it inspects `run` and the value receiver `w`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiverTypeIdentity<'a> {
    /// Module-qualified (`workers::Worker`) with a workspace-local root.
    LocalQualified(&'a str),
    /// A bare local type name (`Worker`) — nothing left to qualify.
    LocalUnqualified(&'a str),
    /// The root segment (or the bare name itself) names an external import.
    ExternalQualified(&'a str),
    /// Present but unusable evidence (empty after normalization, forms the classifier cannot
    /// place). Never resolves, and still suppresses the bare-name fallback — unusable evidence
    /// is not the same as no evidence.
    Ambiguous,
}

impl<'a> ReceiverTypeIdentity<'a> {
    /// Classify a stored hint against the reference's import scope; `None` when the edge carries
    /// no hint at all. `is_external_root` answers "does this segment name an external import at
    /// the call site?".
    pub(crate) fn classify(
        hint: Option<&'a str>,
        is_external_root: impl Fn(&str) -> bool,
    ) -> Option<Self> {
        let hint = hint?.trim();
        if hint.is_empty() {
            return Some(Self::Ambiguous);
        }
        Some(match hint.split_once("::") {
            Some((root, _)) if is_external_root(root) => Self::ExternalQualified(hint),
            Some(_) => Self::LocalQualified(hint),
            None if is_external_root(hint) => Self::ExternalQualified(hint),
            None => Self::LocalUnqualified(hint),
        })
    }
}

pub(crate) struct ResolveSymbolRequest<'a> {
    name: &'a str,
    target_qualified_name: Option<&'a str>,
    edge_kind: EdgeKind,
    evidence: Option<&'a str>,
    receiver_hint: Option<&'a str>,
    receiver_type: Option<ReceiverTypeIdentity<'a>>,
    source_file_id: i64,
    source_language: Option<&'a str>,
    /// `name` is brought into this file by a `use` from an EXTERNAL dependency crate (#61 Project
    /// B). When set, the name denotes that dependency's item, so resolution must NOT bind it to a
    /// local same-named symbol — it stays unresolved (the oracle bins it `resolved-external`).
    imported_external: bool,
}
