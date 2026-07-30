//! What a `sym_<hex>` logical-symbol handle resolves to in the ACTIVE checkout.
//!
//! Two lens surfaces speak about the same handle and must not disagree: `files` hands one out per
//! symbol row, and `hops` answers a request that sends one back. Both have to report the same
//! number of declarations behind it, so the counting SQL lives here once rather than twice.

use rusqlite::{Connection, OptionalExtension as _};

/// Members of a logical symbol the ACTIVE SCOPE can see, as a correlated scalar subquery.
///
/// `owner` is a SQL expression naming the logical-symbol id in the enclosing query — a column
/// reference or a bound parameter. A NULL there counts zero, which is the honest answer for a
/// symbol row that has no handle at all.
///
/// SCOPED through the `files` view, like every other member read (#897): `logical_symbol_members`
/// is corpus-level and holds one member per scope of a path, so an unscoped count would report a
/// sibling checkout's replica — or a linked worktree's overlay row — as another declaration this
/// checkout must answer for.
pub(super) fn scope_visible_members_sql(owner: &str) -> String {
    format!(
        "(SELECT COUNT(*)
            FROM logical_symbol_members member_scope
            JOIN symbols member_symbol ON member_symbol.id = member_scope.symbol_id
            JOIN files member_file ON member_file.id = member_symbol.file_id
           WHERE member_scope.logical_symbol_id = {owner})"
    )
}

/// The logical symbol's qualified name and how many members it has IN THE ACTIVE SCOPE. Scoping
/// through the `files` view is what makes a handle minted in a sibling checkout read as absent
/// instead of resolving against rows this checkout cannot see; `None` means no member survives.
pub(super) fn logical_symbol_in_scope(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<Option<(String, u64)>> {
    let members = scope_visible_members_sql("?1");
    let row = conn
        .query_row(
            // Short name as the second-choice seed: `qualified_name_id` is nullable, and a seed of
            // `''` leaves the traversal's query log naming nothing at all.
            &format!(
                "SELECT COALESCE(MIN(qn.value), MIN(symbols.name), ''), {members}
                 FROM logical_symbol_members member
                 JOIN symbols ON symbols.id = member.symbol_id
                 JOIN files ON files.id = symbols.file_id
                 LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
                 WHERE member.logical_symbol_id = ?1"
            ),
            [logical_symbol_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    Ok(row
        .filter(|(_, members)| *members > 0)
        .map(|(qualified_name, members)| (qualified_name, u64::try_from(members).unwrap_or(0))))
}
