use super::*;

pub(crate) fn category_rank(category: &str) -> u8 {
    match category {
        "Direct structural impact" => 0,
        "Probable textual impact" => 1,
        "Historical/papertrail evidence" => 2,
        _ => 3,
    }
}

pub(crate) fn reason_rank(reason: &str) -> u8 {
    match reason {
        "exact_symbol_definition" => 0,
        "direct_caller" => 1,
        "direct_callee" => 2,
        "import_export_dependent" => 3,
        "same_file_sibling" => 4,
        "textual_fallback" => 5,
        "git_commit_touched_file" => 6,
        "papertrail" => 7,
        _ => 8,
    }
}

pub(crate) fn exact_symbols(conn: &Connection, query: &str) -> anyhow::Result<Vec<SymbolTarget>> {
    let candidates = symbol_query_candidates(query);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, files.path, files.language, files.kind,
               symbols.name, qn.value
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE symbols.name = ?1
           OR symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
        ORDER BY files.kind, files.path, symbols.start_byte
        ",
    )?;
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    let multi_candidate_query = candidates.len() > 1;
    for candidate in candidates {
        let qualified_candidate = is_qualified_symbol(candidate);
        if multi_candidate_query && !qualified_candidate && !is_high_signal_query_token(candidate) {
            continue;
        }
        let rows = stmt.query_map([candidate], |row| {
            Ok(SymbolTarget {
                id: row.get(0)?,
                file_id: row.get(1)?,
                path: row.get(2)?,
                language: row.get(3)?,
                file_kind: row.get(4)?,
                name: row.get(5)?,
                qualified_name: row.get(6)?,
            })
        })?;
        let rows = collect_rows(rows)?;
        if !qualified_candidate && !is_high_signal_symbol_candidate(&rows) {
            continue;
        }
        for row in rows {
            if seen.insert(row.id) {
                targets.push(row);
            }
        }
    }
    Ok(targets)
}

pub(crate) fn is_high_signal_query_token(value: &str) -> bool {
    value.contains('_') || value.chars().next().is_some_and(char::is_uppercase)
}

pub(crate) fn is_high_signal_symbol_candidate(rows: &[SymbolTarget]) -> bool {
    match rows {
        [] => false,
        [_] => true,
        [first, ..] if rows.len() <= 4 =>
            rows.iter().all(|row| row.path == first.path && row.name == first.name),
        _ => false,
    }
}

pub(crate) fn target_names(query: &str, targets: &[SymbolTarget]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for candidate in symbol_query_candidates(query) {
        names.insert(candidate.to_string());
        names.insert(short_symbol_name(candidate).to_string());
    }
    for target in targets {
        names.insert(target.name.clone());
        names.insert(target.qualified_name.clone());
    }
    names.into_iter().collect()
}

pub(crate) fn symbol_query_candidates(query: &str) -> Vec<&str> {
    query
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|ch: char| {
                !(ch.is_alphanumeric() || matches!(ch, '_' | ':' | '/' | '.' | '-'))
            })
        })
        .filter(|token| !token.is_empty())
        .filter(|token| token.contains("::") || is_non_stopword_identifier(token))
        .collect()
}

pub(crate) fn is_non_stopword_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let is_identifier = (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
    is_identifier
        && !matches!(
            value,
            "of" | "in"
                | "to"
                | "from"
                | "for"
                | "and"
                | "or"
                | "the"
                | "callers"
                | "callee"
                | "callees"
                | "caller"
                | "impact"
                | "symbol"
        )
}

pub(crate) fn short_symbol_name(value: &str) -> &str {
    value.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(value)
}

pub(crate) fn is_qualified_symbol(value: &str) -> bool {
    value.contains("::") || value.contains('/')
}
