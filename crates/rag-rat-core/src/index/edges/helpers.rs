use super::*;

/// Remove balanced `<...>` generic-argument regions from a call path — and the turbofish's leading
/// `::` (`f::<a::B>` → `f`, `Vec::<u8>::new` → `Vec::new`) — so a generic argument's `::` /
/// identifiers don't corrupt path-based call name / receiver / qualified-name extraction. `<`/`>`
/// inside a `{...}` const-generic block (`Foo::<{ 1 << 2 }>`) are shift/comparison operators, not
/// generic delimiters, so brace regions suppress angle counting.
///
/// A LEADING `<...>` is a UFCS qualifier (`<Resp as Default>::default`), NOT a generic argument —
/// stripping it would drop the type/receiver and mangle the qualified name (#208 review round 10),
/// so a path that starts with `<` is returned unchanged.
pub(crate) fn strip_generics(path: &str) -> String {
    if path.trim_start().starts_with('<') {
        return path.to_string();
    }
    let mut out = String::new();
    let mut angle: u32 = 0;
    let mut brace: u32 = 0;
    for ch in path.chars() {
        match ch {
            '{' => brace += 1,
            '}' => brace = brace.saturating_sub(1),
            '<' if brace == 0 => {
                if angle == 0 && out.ends_with("::") {
                    out.truncate(out.len() - 2);
                }
                angle += 1;
            },
            '>' if brace == 0 => angle = angle.saturating_sub(1),
            _ if angle == 0 => out.push(ch),
            _ => {},
        }
    }
    out
}

pub(crate) fn target_qualified_name(node: Node<'_>, text: &str) -> Option<String> {
    let function = node.child_by_field_name("function").unwrap_or(node);
    let value = strip_generics(&node_text(function, text));
    (value.contains("::") || value.contains('.')).then(|| value.replace('.', "::"))
}
pub(crate) fn dotted_qualified_name(identifiers: &[String]) -> Option<String> {
    (identifiers.len() > 1).then(|| identifiers.join("::"))
}
pub(crate) fn c_like_qualified_name(identifiers: &[String]) -> Option<String> {
    (identifiers.len() > 1).then(|| identifiers.join("::"))
}
pub(crate) fn containing_symbol(symbols: &[IndexedSymbol], byte: usize) -> Option<&IndexedSymbol> {
    let mut matches = symbols
        .iter()
        .filter(|symbol| symbol.start_byte <= byte && symbol.end_byte >= byte)
        .collect::<Vec<_>>();
    matches.sort_by_key(|symbol| symbol.end_byte.saturating_sub(symbol.start_byte));
    let first = matches.first().copied()?;
    if matches!(first.kind.as_str(), "const" | "property" | "static") {
        matches
            .iter()
            .copied()
            .find(|symbol| {
                symbol.id != first.id
                    && !matches!(symbol.kind.as_str(), "const" | "property" | "static")
            })
            .or(Some(first))
    } else {
        Some(first)
    }
}
/// A trailing turbofish (`f::<T>`) wraps the callee path in a `generic_function`; unwrap it so the
/// type arguments' identifiers / byte ranges aren't mistaken for the callee.
fn unwrap_generic_function(function: Node<'_>) -> Node<'_> {
    if function.kind() == "generic_function" {
        function.child_by_field_name("function").unwrap_or(function)
    } else {
        function
    }
}

pub(crate) fn call_target_name(node: Node<'_>, text: &str) -> Option<String> {
    node.child_by_field_name("function")
        .map(unwrap_generic_function)
        .and_then(|child| last_identifier_text(child, text))
        .map(|name| short_name(&name).to_string())
        .or_else(|| first_identifier_text(node, text))
}
/// The callee identifier node for a call expression — the same token [`call_target_name`] names,
/// returned as a node so its byte range can be recorded (SCIP occurrences key on the identifier's
/// position, #67). Points at the FINAL `::`/`.` segment (the callee name itself), matching how
/// `call_target_name`'s `short_name` collapses a path to its tail. `None` when no clean identifier
/// node is available — never guess a wrong range.
pub(crate) fn call_target_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("function")
        .map(unwrap_generic_function)
        .and_then(last_identifier_node)
        .or_else(|| first_identifier_node(node))
        .map(final_segment_node)
}
pub(crate) fn scoped_receiver_name(node: Node<'_>, text: &str) -> Option<String> {
    let function = node.child_by_field_name("function").unwrap_or(node);
    let value = strip_generics(&node_text(function, text));
    let separator = if value.contains("::") {
        "::"
    } else if value.contains('.') {
        "."
    } else {
        return None;
    };
    value
        .split(separator)
        .next()
        .map(|name| short_name(name.trim()).to_string())
        .filter(|name| !name.is_empty())
}
pub(crate) fn child_name_text(node: Node<'_>, text: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|child| child.utf8_text(text.as_bytes()).ok())
        .map(ToOwned::to_owned)
}
pub(crate) fn first_identifier_text(node: Node<'_>, text: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if is_identifier_kind(child.kind()) {
            return child.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned);
        }
        if let Some(value) = first_identifier_text(child, text) {
            return Some(value);
        }
    }
    None
}
pub(crate) fn last_identifier_text(node: Node<'_>, text: &str) -> Option<String> {
    identifiers_under(node, text).into_iter().last()
}
pub(crate) fn identifiers_under(node: Node<'_>, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect_identifiers(node, text, &mut out);
    out
}
pub(crate) fn collect_identifiers(node: Node<'_>, text: &str, out: &mut Vec<String>) {
    if is_identifier_kind(node.kind()) {
        if let Ok(value) = node.utf8_text(text.as_bytes())
            && !value.is_empty()
        {
            out.push(value.to_string());
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_identifiers(child, text, out);
    }
}
/// Node-returning twin of [`first_identifier_text`]: the first identifier-kind node in document
/// order, so its byte range can be recorded for the SCIP join (#67). Same traversal, so the node it
/// returns is exactly the token whose text [`first_identifier_text`] would have produced.
pub(crate) fn first_identifier_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if is_identifier_kind(child.kind()) {
            return Some(child);
        }
        if let Some(found) = first_identifier_node(child) {
            return Some(found);
        }
    }
    None
}
/// Node-returning twin of [`last_identifier_text`]: the last identifier-kind node under `node`.
pub(crate) fn last_identifier_node(node: Node<'_>) -> Option<Node<'_>> {
    identifier_nodes_under(node).into_iter().last()
}
/// Node-returning twin of [`identifiers_under`]: every identifier-kind node under `node`, in the
/// same document order, so the callee (`.last()`) and receiver (`.first()`) nodes line up 1:1 with
/// the strings the TS/Kotlin/C extractors already pick out of [`identifiers_under`]. The byte range
/// of the matching node is what the SCIP join keys on (#67).
pub(crate) fn identifier_nodes_under<'tree>(node: Node<'tree>) -> Vec<Node<'tree>> {
    let mut out = Vec::new();
    collect_identifier_nodes(node, &mut out);
    out
}
fn collect_identifier_nodes<'tree>(node: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    if is_identifier_kind(node.kind()) {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_identifier_nodes(child, out);
    }
}
/// Narrow a scoped/dotted identifier node (`scoped_identifier` / `scoped_type_identifier`, whose
/// text is the full `a::b::c`) down to its final segment — the callee/type name `c`. The
/// tree-sitter grammars expose the tail as the `name` field; without it (a plain `identifier`) the
/// node is already the final segment, so return it unchanged. This makes the recorded byte range
/// cover only the callee identifier, matching how `short_name` collapses the printed name and how
/// SCIP keys the occurrence.
pub(crate) fn final_segment_node(node: Node<'_>) -> Node<'_> {
    node.child_by_field_name("name").unwrap_or(node)
}
pub(crate) fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "simple_identifier"
            | "package_identifier"
            | "namespace_identifier"
    )
}
pub(crate) fn is_rust_path_keyword(value: &str) -> bool {
    matches!(value, "self" | "super" | "crate")
}
pub(crate) fn looks_like_type_name(value: &str) -> bool {
    value.chars().next().is_some_and(char::is_uppercase)
}
pub(crate) fn node_text(node: Node<'_>, text: &str) -> String {
    node.utf8_text(text.as_bytes()).unwrap_or_default().to_string()
}
pub(crate) fn edge_evidence(node: Node<'_>, text: &str) -> String {
    node_text(node, text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}
/// Whitespace-normalized but UNTRUNCATED node text, for a `use_declaration`'s evidence. The
/// crate-aware import scope re-parses this with `imports::parse_use` (#61); the 240-char cap in
/// [`edge_evidence`] would drop the late leaves of a long braced `use foo::{…, X}`, leaving those
/// names un-suppressed (#97 item 1). A `use` statement is bounded and small, so storing it in full
/// is cheap.
pub(crate) fn use_declaration_evidence(node: Node<'_>, text: &str) -> String {
    node_text(node, text).split_whitespace().collect::<Vec<_>>().join(" ")
}
pub(crate) fn short_name(name: &str) -> &str {
    name.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(name)
}
pub(crate) fn symbols_for_file(
    conn: &Connection,
    file_id: i64,
) -> anyhow::Result<Vec<IndexedSymbol>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, symbols.language, symbols.name, qn.value, symbols.kind,
               symbols.start_byte, symbols.end_byte, symbols.start_line, symbols.end_line, \
         COALESCE(symbols.scope_path, '')
        FROM symbols
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE file_id = ?1
        ORDER BY symbols.start_byte, symbols.end_byte
        ",
    )?;
    let rows = stmt.query_map([file_id], symbol_row)?;
    collect_rows(rows)
}
/// Every symbol in the ACTIVE CHECKOUT, ordered by qualified name. The `files` join goes through
/// the per-connection scoped TEMP VIEW (overlay wins, dead scopes excluded) — resolving against
/// raw `symbols` duplicated every symbol whenever multiple scopes coexist and collapsed edge
/// resolution (#89).
pub(crate) fn all_symbols(conn: &Connection) -> anyhow::Result<Vec<IndexedSymbol>> {
    // Scope the edge-resolution CANDIDATE POOL to the active repo (A3). This SELECT joins `files` —
    // the per-connection scope VIEW when one is installed (rebuild / incremental / overlay), which
    // already filters `repo_id`. But the bare-open graph refresh (`IndexDatabase::open` →
    // `ensure_graph_index_current`) runs with NO scope view, so `files` resolves to the unscoped
    // `main.files` and would pull EVERY repo's symbols into the pool in a consolidated DB — letting
    // repo A's edges resolve onto repo B's symbols. The `main.files` sub-select pins the pool to
    // the active repo in both cases: redundant under a scope view (view rows ⊆ active repo),
    // and the sole repo predicate on the view-less path. `active_repo_id` falls back to
    // `sole_repo_id` when no context is installed, matching the bare-open scope.
    let active_repo_id = crate::index::schema::active_repo_id(conn)?;
    // Also pin the pool to the active GENERATION (A6): a full rebuild leaves the superseded
    // generation's rows in place until gc, so a bare `repo_id` pin would pull a dead generation's
    // symbols into the pool. `active_generation` reads the connection's scope context (the WRITE
    // generation on the rebuild connection, so edge resolution during a rebuild resolves onto the
    // symbols it is building) and falls back to the repo's LIVE generation from `repo_meta` on the
    // view-less bare-open heal path — matching the `active_repo_id` fallback exactly.
    let active_generation = crate::index::schema::active_generation(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, symbols.language, symbols.name, qn.value, symbols.kind,
               symbols.start_byte, symbols.end_byte, symbols.start_line, symbols.end_line, \
         COALESCE(symbols.scope_path, '')
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE symbols.file_id IN (
                  SELECT id FROM main.files WHERE repo_id = ?1 AND generation = ?2
              )
        ORDER BY qn.value
        ",
    )?;
    let rows = stmt.query_map(rusqlite::params![active_repo_id, active_generation], symbol_row)?;
    collect_rows(rows)
}
pub(crate) fn symbol_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSymbol> {
    let start_byte = usize::try_from(row.get::<_, i64>(6)?).unwrap_or(0);
    let end_byte = usize::try_from(row.get::<_, i64>(7)?).unwrap_or(0);
    Ok(IndexedSymbol {
        id: row.get(0)?,
        file_id: row.get(1)?,
        language: row.get(2)?,
        name: row.get(3)?,
        qualified_name: row.get(4)?,
        kind: row.get(5)?,
        start_byte,
        end_byte,
        start_line: row.get(8)?,
        end_line: row.get(9)?,
        scope_path: row.get(10)?,
    })
}
/// Intern one string into `name_strings`, returning its id (#79). `INSERT OR IGNORE` + lookup —
/// two cached statements; callers on bulk paths wrap this in [`EdgeStringInterner`] so repeats
/// hit a process-side map instead of the b-tree.
pub(crate) fn intern_edge_string(conn: &Connection, value: &str) -> anyhow::Result<i64> {
    conn.prepare_cached("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)")?
        .execute([value])?;
    conn.prepare_cached("SELECT id FROM name_strings WHERE value = ?1")?
        .query_row([value], |row| row.get(0))
        .map_err(Into::into)
}

pub(crate) fn intern_edge_string_opt(
    conn: &Connection,
    value: Option<&str>,
) -> anyhow::Result<Option<i64>> {
    value.map(|value| intern_edge_string(conn, value)).transpose()
}

/// A process-side memo over [`intern_edge_string`] for bulk paths: the graph vocabulary is small
/// (tens of thousands of distinct strings against millions of edges), so the map stays tiny while
/// saving two b-tree probes per repeated string.
#[derive(Default)]
pub(crate) struct EdgeStringInterner {
    cache: std::collections::HashMap<String, i64>,
}

impl EdgeStringInterner {
    pub(crate) fn get(&mut self, conn: &Connection, value: &str) -> anyhow::Result<i64> {
        if let Some(id) = self.cache.get(value) {
            return Ok(*id);
        }
        let id = intern_edge_string(conn, value)?;
        self.cache.insert(value.to_string(), id);
        Ok(id)
    }

    pub(crate) fn get_opt(
        &mut self,
        conn: &Connection,
        value: Option<&str>,
    ) -> anyhow::Result<Option<i64>> {
        value.map(|value| self.get(conn, value)).transpose()
    }
}

pub(crate) fn insert_candidates(
    conn: &Connection,
    file_id: i64,
    candidates: Vec<EdgeCandidate>,
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        let to_name = candidate.to_name.trim();
        if to_name.is_empty() {
            continue;
        }
        if candidate.from_name.as_deref() == Some(to_name) {
            continue;
        }
        let key = (
            candidate.from_symbol_id,
            candidate.from_name.clone(),
            to_name.to_string(),
            candidate.edge_kind,
            candidate.source_span.start_byte,
            candidate.source_span.end_byte,
        );
        if !seen.insert(key) {
            continue;
        }
        // prepare_cached: this INSERT runs once per edge. conn.execute
        // recompiles the SQL every call; the cached statement compiles once per connection.
        // NULL when the candidate has no callee identifier range (non-call / file-level edges)
        // (#67).
        let (callee_start_byte, callee_end_byte) = match candidate.callee_span {
            Some(range) => (
                Some(i64::try_from(range.start_byte).unwrap_or(0)),
                Some(i64::try_from(range.end_byte).unwrap_or(0)),
            ),
            None => (None, None),
        };
        // Module-aware import scope (#61): the dedicated columns, NULL on non-import edges so the
        // oracle's `callee_start_byte IS NOT NULL` candidate filter never sees them.
        let (import_scope_start_byte, import_scope_end_byte, import_mod_id) =
            match candidate.import_scope {
                Some(scope) => (
                    Some(i64::try_from(scope.scope_start).unwrap_or(0)),
                    Some(i64::try_from(scope.scope_end).unwrap_or(0)),
                    Some(scope.mod_id),
                ),
                None => (None, None, None),
            };
        // Direct interned write to edges_data (#79): the view's INSTEAD OF insert would work but
        // costs 8 dictionary probes per row in SQL, and `last_insert_rowid` does not survive an
        // INSTEAD OF trigger.
        let from_name_id = intern_edge_string_opt(conn, candidate.from_name.as_deref())?;
        let to_name_id = intern_edge_string(conn, to_name)?;
        let target_qualified_name_id =
            intern_edge_string_opt(conn, candidate.target_qualified_name.as_deref())?;
        let receiver_hint_id = intern_edge_string_opt(conn, candidate.receiver_hint.as_deref())?;
        let edge_kind_id = intern_edge_string(conn, candidate.edge_kind.as_str())?;
        let confidence_id = intern_edge_string(conn, candidate.confidence.as_str())?;
        let resolution_id = intern_edge_string(conn, "unresolved")?;
        conn.prepare_cached(
            "
            INSERT INTO edges_data(
                source_file_id, from_symbol_id, from_name_id, to_name_id,
                target_qualified_name_id, evidence, receiver_hint_id,
                source_start_line, source_end_line, source_start_byte, source_end_byte,
                callee_start_byte, callee_end_byte,
                import_scope_start_byte, import_scope_end_byte, import_mod_id,
                edge_kind_id, confidence_id, resolution_id
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19)
            ",
        )?
        .execute(params![
            file_id,
            candidate.from_symbol_id,
            from_name_id,
            to_name_id,
            target_qualified_name_id,
            candidate.evidence,
            receiver_hint_id,
            candidate.source_span.start_line,
            candidate.source_span.end_line,
            candidate.source_span.start_byte,
            candidate.source_span.end_byte,
            callee_start_byte,
            callee_end_byte,
            import_scope_start_byte,
            import_scope_end_byte,
            import_mod_id,
            edge_kind_id,
            confidence_id,
            resolution_id,
        ])?;
    }
    Ok(())
}
pub(crate) fn span_for_node(node: Node<'_>) -> EdgeSpan {
    EdgeSpan {
        start_line: i64::try_from(node.start_position().row).unwrap_or(i64::MAX).saturating_add(1),
        end_line: i64::try_from(node.end_position().row).unwrap_or(i64::MAX).saturating_add(1),
        start_byte: i64::try_from(node.start_byte()).unwrap_or(i64::MAX),
        end_byte: i64::try_from(node.end_byte()).unwrap_or(i64::MAX),
    }
}
pub(crate) fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
