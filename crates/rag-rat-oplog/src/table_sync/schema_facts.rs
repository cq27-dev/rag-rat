//! What the LIVE SQLite schema says about a syncable table.
//!
//! [`super::registry`] declares what a table's replicated shape is *meant* to be; this module reads
//! what it actually is — column types and defaults, primary-key collation, triggers, foreign keys,
//! unique indexes, `STRICT`ness, and check constraints. Keeping the two apart means the registry
//! module stays spec + lint policy, and the reading of `PRAGMA` output and `sqlite_master.sql` —
//! including the small SQL lexer that makes constraint reading trustworthy — lives in one place.
//!
//! Nothing here interprets the registry's intent; every function answers a question about the
//! database as it stands.

use rusqlite::Connection;

use super::registry::DefaultValue;

/// Whether `table` is a STRICT table. SQLite exposes no pragma for this, so read the table options
/// that follow the column-list's closing `)` (all column-level parens nest inside it, so the LAST
/// `)` is always that closer) and look for the `STRICT` keyword.
pub(super) fn table_is_strict(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    let options = sql.rsplit(')').next().unwrap_or_default();
    Ok(options
        .to_ascii_uppercase()
        .split(|c: char| c == ',' || c.is_whitespace())
        .any(|token| token == "STRICT"))
}

/// One physical column of a table, from `PRAGMA table_info` (`conn.pragma` quotes the table name,
/// so a spec name never needs manual escaping). `pk_position` is the 1-based position within the
/// primary key, or `0` for a non-key column. `decl_type` is the declared column type (uppercased) —
/// on a STRICT table one of `INT`/`INTEGER`/`REAL`/`TEXT`/`BLOB`/`ANY`.
pub(super) struct PhysicalColumn {
    pub(super) name: String,
    pub(super) decl_type: String,
    pub(super) not_null: bool,
    /// SQLite's `dflt_value` verbatim (the literal AS WRITTEN, e.g. `0`, `'x'`, `NULL`), or `None`
    /// for a column with no DEFAULT clause. Compared against a synced column's declared default so
    /// the two can never disagree — see `default_matches`.
    pub(super) default_sql: Option<String>,
    pub(super) pk_position: i64,
}

/// The content of `sql` if it is EXACTLY ONE single-quoted SQL string literal, with `''` unescaped
/// to `'`. `None` for anything else.
///
/// Being a single literal is the whole point, not a formality. SQLite drops the outer parentheses
/// from a parenthesized default in `PRAGMA table_info`, so `DEFAULT ('x'||'y')` is reported as
/// `'x'||'y'` — which starts and ends with a quote. Accepting that on those two characters alone
/// would let a CONCATENATION pass as a literal: SQLite backfills the column with `xy` while the
/// applier synthesizes the raw text `x'||'y`, which is precisely the backfill-versus-projection
/// divergence this check exists to prevent. A lone interior quote is what distinguishes them.
pub(super) fn single_quoted_literal(sql: &str) -> Option<String> {
    let inner = sql.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut out = String::with_capacity(inner.len());
    let mut rest = inner.chars();
    while let Some(ch) = rest.next() {
        if ch != '\'' {
            out.push(ch);
            continue;
        }
        // A quote inside a literal is only legal doubled; a lone one closed the literal early, so
        // whatever follows is an expression, not part of the value.
        if rest.next() != Some('\'') {
            return None;
        }
        out.push('\'');
    }
    Some(out)
}

/// One lexical unit of SQL — enough structure to find constraints and identifiers without ever
/// mistaking the inside of a string or a comment for either.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum SqlToken {
    /// A bare word: a keyword or an unquoted identifier, ASCII-lowercased — SQLite folds
    /// identifier case for ASCII ONLY, so `to_lowercase` (full Unicode) would equate identifiers
    /// SQLite keeps distinct.
    Word(String),
    /// A quoted identifier — `"x"`, `` `x` ``, `[x]` — its content, ASCII-lowercased.
    Ident(String),
    /// A literal: a string, a blob (`X'ff'`), or a number. It is NEVER a reference — its text can
    /// spell anything, including `CHECK(` or a column name — so structure never reads it. A string
    /// literal carries its unescaped content for the one question that is legitimately about a
    /// VALUE: whether a date/time call was handed `'now'`.
    Literal(Option<String>),
    /// Anything else, one character at a time — parentheses, operators, commas.
    Punct(char),
}

/// Lex SQL into [`SqlToken`]s, skipping whitespace and both comment forms.
///
/// Scanning SQL as raw text does not work here, and the failure is silent in the dangerous
/// direction: a parenthesis inside a string literal (`CHECK(later <> ')' OR count = 0)`) truncates
/// a naive balanced-paren scan, so the rest of the constraint — including the other column it
/// references — is never examined and an unsafe CHECK passes. Literals and comments equally can
/// spell a column name or the word CHECK and produce the opposite error, refusing a schema that is
/// fine. Lexing once removes both classes.
///
/// Character-based throughout, so a multi-byte identifier can never split a slice.
pub(super) fn lex_sql(sql: &str) -> Vec<(SqlToken, usize, usize)> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let start = i;
        let ch = chars[i];
        match ch {
            c if c.is_whitespace() => i += 1,
            '-' if chars.get(i + 1) == Some(&'-') =>
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                },
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
            },
            '\'' => {
                i += 1;
                let mut content = String::new();
                while i < chars.len() {
                    if chars[i] == '\'' {
                        // A doubled quote is an escaped quote, not the end of the literal.
                        if chars.get(i + 1) == Some(&'\'') {
                            content.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    content.push(chars[i]);
                    i += 1;
                }
                tokens.push((SqlToken::Literal(Some(content)), start, i));
            },
            '"' | '`' | '[' => {
                let close = if ch == '[' { ']' } else { ch };
                i += 1;
                let mut ident = String::new();
                while i < chars.len() {
                    if chars[i] == close {
                        // `""` inside a quoted identifier is an escaped quote, as for strings.
                        if close != ']' && chars.get(i + 1) == Some(&close) {
                            ident.push(close);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    ident.push(chars[i]);
                    i += 1;
                }
                tokens.push((SqlToken::Ident(ident.to_ascii_lowercase()), start, i));
            },
            // A blob literal is `X'..'` with NO space before the quote. Lexed here so its `x` is
            // never mistaken for a reference to a column named `x` — which is a plausible name, and
            // `CHECK(later != X'ff')` is perfectly legal SQL.
            'x' | 'X' if chars.get(i + 1) == Some(&'\'') => {
                i += 2;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i = (i + 1).min(chars.len());
                tokens.push((SqlToken::Literal(None), start, i));
            },
            // A number: an identifier can never begin with a digit, so this is always a literal.
            c if c.is_ascii_digit() => {
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '.') {
                    i += 1;
                }
                tokens.push((SqlToken::Literal(None), start, i));
            },
            c if c.is_alphanumeric() || c == '_' => {
                let mut word = String::new();
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    word.push(chars[i]);
                    i += 1;
                }
                tokens.push((SqlToken::Word(word.to_ascii_lowercase()), start, i));
            },
            c => {
                i += 1;
                tokens.push((SqlToken::Punct(c), start, i));
            },
        }
    }
    tokens
}

/// A table's DDL, read and lexed ONCE: which columns it declares (verbatim), its table-level CHECK
/// constraints, and whether it is `STRICT`.
pub(super) struct TableDdl {
    pub(super) is_strict: bool,
    /// Lowercased column name → that column's verbatim declaration, in order.
    pub(super) columns: Vec<(String, String)>,
    /// EVERY `CHECK` the table declares, from any source.
    pub(super) checks: Vec<TableCheck>,
}

/// One `CHECK` constraint and where it was written.
///
/// An inline check is semantically table-level — `count INTEGER CHECK(later >= count)` constrains
/// the row, not the column it is attached to — so collecting only table-level ones misses a
/// constraint on a SIBLING that reads the column under test. Provenance is kept so the column's
/// OWN inline checks are not added twice: they already travel with its verbatim declaration.
pub(super) struct TableCheck {
    /// The column whose declaration carries this check, or `None` for a table-level constraint.
    pub(super) owner: Option<String>,
    pub(super) body: String,
}

/// Read `table`'s `CREATE TABLE` and take it apart.
///
/// `PRAGMA table_info` carries neither collation nor check constraints, and rebuilding a column
/// from its parts would be a re-implementation of SQLite's DDL that could only ever approximate it.
/// Lifting the original text is exact.
pub(super) fn read_table_ddl(conn: &Connection, table: &str) -> rusqlite::Result<TableDdl> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    let chars: Vec<char> = sql.chars().collect();
    let tokens = lex_sql(&sql);
    let mut ddl = TableDdl { is_strict: false, columns: Vec::new(), checks: Vec::new() };

    // Table options follow the column list's closing parenthesis.
    let Some(open) = tokens.iter().position(|(t, ..)| *t == SqlToken::Punct('(')) else {
        return Ok(ddl);
    };
    let mut depth = 0usize;
    let mut segment_start: Option<usize> = None;
    let mut segment_head: Option<SqlToken> = None;
    let mut tail = tokens.len();

    // Each top-level segment of the column list is either a column declaration (it starts with the
    // column's name) or a table-level constraint (it starts with a keyword).
    let finish = |ddl: &mut TableDdl, head: &Option<SqlToken>, from: usize, to: usize| {
        let text: String = chars[from..to].iter().collect();
        // EVERY segment is searched for checks, whatever heads it. SQLite does not require a comma
        // between table constraints, so `PRIMARY KEY(id) CHECK(later >= count)` is one segment —
        // and a head-keyed dispatch that only searched `CHECK`/`CONSTRAINT` segments dropped that
        // check entirely, which is the direction that accepts an unsafe default. The head decides
        // only whether the segment ALSO declares a column, and hence who owns the checks.
        //
        // A QUOTED head is always a column name: `"check"` is a legal column, not the keyword.
        let declares_column = match head {
            Some(SqlToken::Word(word)) => !is_constraint_keyword(word),
            Some(SqlToken::Ident(_)) => true,
            _ => false,
        };
        let owner = match (declares_column, head) {
            (true, Some(SqlToken::Word(word) | SqlToken::Ident(word))) => Some(word.clone()),
            _ => None,
        };
        for body in check_bodies(&text) {
            ddl.checks.push(TableCheck { owner: owner.clone(), body });
        }
        if let Some(name) = owner {
            ddl.columns.push((name, text.trim().to_string()));
        }
    };

    for (index, (token, from, to)) in tokens.iter().enumerate().skip(open) {
        match token {
            SqlToken::Punct('(') => {
                depth += 1;
                if depth == 1 {
                    segment_start = Some(*to);
                    segment_head = None;
                    continue;
                }
            },
            SqlToken::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    if let Some(begin) = segment_start {
                        finish(&mut ddl, &segment_head, begin, *from);
                    }
                    tail = index + 1;
                    break;
                }
            },
            SqlToken::Punct(',') if depth == 1 => {
                if let Some(begin) = segment_start {
                    finish(&mut ddl, &segment_head, begin, *from);
                }
                segment_start = Some(*to);
                segment_head = None;
                continue;
            },
            _ => {},
        }
        if depth == 1 && segment_head.is_none() {
            segment_head = Some(token.clone());
        }
    }
    ddl.is_strict =
        tokens[tail..].iter().any(|(t, ..)| matches!(t, SqlToken::Word(word) if word == "strict"));
    Ok(ddl)
}

/// Every `CHECK( ... )` body in one segment of the column list, located by TOKENS.
///
/// Applies to both kinds of segment. A table-level one holds at most one — but it may be named
/// (`CONSTRAINT c CHECK(...)`), so the keyword is found rather than assumed to lead, and a
/// differently-named constraint (`CONSTRAINT c UNIQUE(a)`) is not mistaken for one. A COLUMN
/// declaration may carry several, and each is semantically table-level.
///
/// Locating by token also means the keyword's case cannot hide it: the source is not normalised,
/// so `ChEcK(...)` defeats a literal prefix strip — and defeating it drops the constraint silently,
/// which is the FALSE-NEGATIVE direction.
fn check_bodies(segment: &str) -> Vec<String> {
    let chars: Vec<char> = segment.chars().collect();
    let tokens = lex_sql(segment);
    let mut bodies = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        let is_keyword = matches!(&tokens[index].0, SqlToken::Word(word) if word == "check");
        if !is_keyword || tokens.get(index + 1).map(|t| &t.0) != Some(&SqlToken::Punct('(')) {
            index += 1;
            continue;
        }
        index += 1;
        let (_, _, body_start) = tokens[index];
        let mut depth = 0usize;
        while index < tokens.len() {
            match &tokens[index].0 {
                SqlToken::Punct('(') => depth += 1,
                SqlToken::Punct(')') => {
                    depth -= 1;
                    if depth == 0 {
                        bodies.push(chars[body_start..tokens[index].1].iter().collect());
                        break;
                    }
                },
                _ => {},
            }
            index += 1;
        }
        index += 1;
    }
    bodies
}

/// Whether a top-level segment beginning with `word` is a table-level CONSTRAINT rather than a
/// column declaration. (A column may not be named any of these unquoted.)
fn is_constraint_keyword(word: &str) -> bool {
    matches!(word, "constraint" | "primary" | "unique" | "check" | "foreign")
}

/// The outcome of testing a declared default against the constraints on its column.
pub(super) enum CheckVerdict {
    /// The column accepts the default, and its constraints depend on nothing else.
    Satisfied,
    /// The column REJECTS the default — every older op would be quarantined on arrival.
    Violated(String),
    /// A constraint reads something other than this column, so no default can be proven safe.
    NotSelfContained(String),
}

/// Names of the implicit rowid, which resolve in ANY rowid table.
const ROWID_ALIASES: [&str; 3] = ["rowid", "_rowid_", "oid"];

/// Decide, BY CONSTRUCTION, whether `default` is a value this column actually accepts — and whether
/// its constraints depend only on this column.
///
/// Rebuilds the column ALONE in a scratch database, under the real table's NAME (so a qualified
/// self-reference still resolves) and `STRICT`ness, from its own verbatim DDL plus every
/// table-level CHECK that genuinely involves it, then attempts the insert the applier would
/// perform. SQLite answers both questions itself.
///
/// EVALUATING THE EXPRESSION INSTEAD IS NOT FAITHFUL, and both ways it lied were silent: a bare
/// expression carries BINARY collation and no affinity, so a `COLLATE NOCASE` or differently-typed
/// column disagreed with it in both directions; and SQLite treats a CHECK as violated only when it
/// evaluates to zero, so reading the result as an integer misread a REAL or a NULL. Re-declaring
/// the column keeps the type, the collation, and the truth semantics actually in force.
///
/// WHICH constraints involve the column is also SQLite's answer, not a token match: a name that
/// merely looks like the column (a type name inside `CAST`, a keyword) would otherwise drag an
/// unrelated constraint into the probe, where its foreign names produce the verdict.
///
/// The scratch database is SEPARATE, so this never mutates the caller's connection. A custom
/// collation registered only on that connection therefore fails to resolve and reads as
/// not-self-contained — the right answer regardless, since a collation defined by the application
/// is per-device state a synced column must not depend on.
pub(super) fn default_satisfies_check(
    ddl: &TableDdl,
    table: &str,
    column: &str,
    default: DefaultValue,
) -> CheckVerdict {
    let wanted = column.to_ascii_lowercase();
    let Some((_, definition)) = ddl.columns.iter().find(|(name, _)| *name == wanted) else {
        return CheckVerdict::NotSelfContained(format!("no declaration found for `{column}`"));
    };
    let others: Vec<&str> = ddl
        .columns
        .iter()
        .filter(|(name, _)| *name != wanted)
        .map(|(name, _)| name.as_str())
        .collect();

    // The implicit rowid resolves in any rowid table, INCLUDING the probe — so a constraint reading
    // it would look self-contained and pass. It is also per-device (assigned by insertion order),
    // so a synced column must never depend on it. Refuse it lexically, which is the only way to see
    // it at all.
    // A table may DECLARE a column named `rowid`; that shadows the implicit alias, so the name is
    // then an ordinary column reference and the involvement test handles it like any other.
    let declared: Vec<&str> = ddl.columns.iter().map(|(name, _)| name.as_str()).collect();
    let reads_rowid = |body: &str| {
        lex_sql(body).iter().any(|(t, ..)| {
            // Word OR Ident: `"rowid"` resolves to the implicit rowid exactly as `rowid` does.
            matches!(t, SqlToken::Word(w) | SqlToken::Ident(w)
                if ROWID_ALIASES.contains(&w.as_str()) && !declared.contains(&w.as_str()))
        })
    };
    if reads_rowid(definition) {
        return CheckVerdict::NotSelfContained(
            "the declaration reads the implicit rowid, which is assigned per device".to_string(),
        );
    }
    if let Some(function) = per_device_reference(definition) {
        return CheckVerdict::NotSelfContained(format!(
            "the declaration calls `{function}`, whose result can differ between devices"
        ));
    }

    let mut applicable = Vec::new();
    for check in &ddl.checks {
        // This column's own inline checks are already enforced by its verbatim declaration below;
        // adding them again would only duplicate them into the probe's DDL and its error messages.
        if check.owner.as_deref() == Some(wanted.as_str()) {
            continue;
        }
        let body = &check.body;
        match constraint_involvement(table, definition, &others, body) {
            Involvement::Irrelevant => continue,
            Involvement::SelfContained => {
                if reads_rowid(body) {
                    return CheckVerdict::NotSelfContained(format!(
                        "CHECK ({body}) reads the implicit rowid, which is assigned per device"
                    ));
                }
                if let Some(function) = per_device_reference(body) {
                    return CheckVerdict::NotSelfContained(format!(
                        "CHECK ({body}) calls `{function}`, whose result can differ between \
                         devices — the same op would be accepted on one peer and quarantined on \
                         another"
                    ));
                }
                applicable.push(body.as_str());
            },
            Involvement::CrossColumn(why) => return CheckVerdict::NotSelfContained(why),
        }
    }

    let Ok(scratch) = Connection::open_in_memory() else {
        return CheckVerdict::NotSelfContained("cannot open a scratch database".to_string());
    };
    if let Err(err) =
        scratch.execute_batch(&probe_ddl(table, definition, &applicable, ddl.is_strict))
    {
        // The column's OWN declaration naming something else lands here.
        return CheckVerdict::NotSelfContained(err.to_string());
    }
    let bound: rusqlite::types::Value = match default {
        DefaultValue::Null => rusqlite::types::Value::Null,
        DefaultValue::Bool(b) => rusqlite::types::Value::Integer(i64::from(b)),
        DefaultValue::I64(n) => rusqlite::types::Value::Integer(n),
        DefaultValue::Text(text) => rusqlite::types::Value::Text(text.to_string()),
        DefaultValue::Blob(bytes) => rusqlite::types::Value::Blob(bytes.to_vec()),
    };
    let quoted_table = table.replace('"', "\"\"");
    let quoted_column = column.replace('"', "\"\"");
    let insert = format!("INSERT INTO \"{quoted_table}\"(\"{quoted_column}\") VALUES (?1)");
    match scratch.execute(&insert, [bound]) {
        Ok(_) => CheckVerdict::Satisfied,
        Err(err) if is_constraint_failure(&err) => CheckVerdict::Violated(err.to_string()),
        Err(err) => CheckVerdict::NotSelfContained(err.to_string()),
    }
}

fn probe_ddl(table: &str, definition: &str, checks: &[&str], strict: bool) -> String {
    let quoted = table.replace('"', "\"\"");
    // Newlines for the same reason as in `constraint_involvement`: the declaration is verbatim
    // source and may end in a line comment.
    let mut ddl = format!("CREATE TABLE \"{quoted}\"(\n{definition}\n");
    for check in checks {
        ddl.push_str(&format!(", CHECK({check})\n"));
    }
    ddl.push(')');
    // Carried for structural fidelity rather than effect: the bound value's type is already pinned
    // to the declared type by the registry's own type lints, so no test can distinguish this. It
    // keeps the probe identical to the real table, which is the point of re-declaring at all.
    if strict {
        ddl.push_str(" STRICT");
    }
    ddl
}

/// How a table-level CHECK relates to the column under test.
enum Involvement {
    /// It does not read the column at all — it was already satisfied before the column existed.
    Irrelevant,
    /// It reads only the column.
    SelfContained,
    /// It reads the column AND something else, so no default can be proven safe.
    CrossColumn(String),
}

/// Ask SQLite which columns a CHECK body actually reads, by offering it two different worlds:
/// one holding every column EXCEPT this one, and one holding only this one.
fn constraint_involvement(
    table: &str,
    definition: &str,
    others: &[&str],
    body: &str,
) -> Involvement {
    let quoted = table.replace('"', "\"\"");
    // NEWLINES around a spliced declaration: it is verbatim source and may end in a `--` comment,
    // which on one line would comment out everything after it.
    let alone = format!("CREATE TABLE \"{quoted}\"(\n{definition}\n, CHECK({body})\n)");
    if let Ok(scratch) = Connection::open_in_memory()
        && scratch.execute_batch(&alone).is_ok()
    {
        return Involvement::SelfContained;
    }
    // The other columns go in as bare NAMES. This world exists only to answer "does the body
    // resolve WITHOUT this column", and a name is exactly that question — splicing their
    // declarations in would let a sibling's own constraints fail this CREATE for reasons that have
    // nothing to do with the column under test. The one dependency a bare name would erase is a
    // GENERATED column whose expression reads the column under test, and that cannot arise here:
    // `assert_spec_covers_schema` refuses a generated column outright.
    let without = {
        // QUOTED: a column may legally be named after a keyword (`check`, `order`), and splicing
        // such a name in bare would fail this CREATE on syntax — refusing a valid schema for a
        // reason that has nothing to do with the constraint being classified.
        let columns = if others.is_empty() {
            "\"__none__\"".to_string()
        } else {
            others
                .iter()
                .map(|name| format!("\"{}\"", name.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!("CREATE TABLE \"{quoted}\"({columns}, CHECK({body}))")
    };
    match Connection::open_in_memory() {
        Ok(scratch) => match scratch.execute_batch(&without) {
            // It resolves without this column, so it never reads it.
            Ok(()) => Involvement::Irrelevant,
            Err(err) => Involvement::CrossColumn(err.to_string()),
        },
        Err(err) => Involvement::CrossColumn(err.to_string()),
    }
}

/// SQLite's clock KEYWORDS. They read the time like `date('now')` but take no parentheses, so a
/// scan shaped around calls cannot see them at all. Only the bare forms matter: quoted, they are
/// ordinary identifiers that resolve to nothing and are refused as unresolvable.
const CLOCK_KEYWORDS: [&str; 3] = ["current_date", "current_time", "current_timestamp"];

/// Operators whose meaning is CONNECTION state, not their operands. `PRAGMA case_sensitive_like`
/// changes both `a LIKE b` and `like(a, b)`, so two peers can disagree about the same row. Refused
/// as a bare word, which catches the operator form as well as the call — a scan shaped around calls
/// would see only half of it.
const CONNECTION_CONFIGURABLE: [&str; 2] = ["like", "glob"];

/// Functions whose result depends ONLY on their arguments, and identically on every SQLite build.
///
/// An ALLOWLIST, not a denylist, and the direction matters. `pragma_function_list`'s
/// `SQLITE_DETERMINISTIC` flag answers a different question than this one: it means "same answer
/// for the same arguments WITHIN THIS BUILD", which `fts5_source_id()` satisfies while returning a
/// different string on a peer compiled against another SQLite. Denying the flagged-nondeterministic
/// set therefore cannot be complete — any present or future build-varying builtin passes it.
/// Listing what is safe cannot have that failure mode; the cost is that a constraint using
/// something unlisted is refused until someone adds it deliberately.
const ARGUMENT_ONLY_FUNCTIONS: &[&str] = &[
    // The date/time family is listed on purpose. SQLite ENFORCES the clock restriction itself, at
    // INSERT — which the probe performs — and more precisely than this lint could: it refuses
    // `date()`, `date('now')` and a `'localtime'` modifier inside a CHECK, while allowing
    // `date(column)` and `strftime(format, column)`. Reproducing that here was both redundant and
    // WRONG in the strict direction, since `date(column, 'now')` is not a clock read at all.
    "date",
    "datetime",
    "julianday",
    "strftime",
    "time",
    "unixepoch",
    // `like` and `glob` are deliberately ABSENT: `PRAGMA case_sensitive_like` is connection state,
    // so they are refused as bare words (operator form included) by `CONNECTION_CONFIGURABLE`.
    "abs",
    "char",
    "coalesce",
    "format",
    "hex",
    "ifnull",
    "iif",
    "instr",
    "json_array",
    "json_extract",
    "json_object",
    "json_quote",
    "json_type",
    "json_valid",
    "length",
    "likelihood",
    "likely",
    "lower",
    "ltrim",
    "max",
    "min",
    "nullif",
    "octet_length",
    "printf",
    "quote",
    "replace",
    "round",
    "rtrim",
    "sign",
    "substr",
    "substring",
    "trim",
    "typeof",
    "unhex",
    "unicode",
    "unlikely",
    "upper",
    "zeroblob",
];

/// The name of anything in `body` whose value can differ between two databases holding identical
/// rows — a clock keyword, or a call this cannot vouch for.
///
/// A CHECK is only usable as a shared guarantee if every peer agrees about it. `CHECK(random() >
/// 0)` is self-contained and satisfiable, yet the identical replicated op can be accepted on one
/// peer and quarantined on another — divergence with no local edit to signal it.
fn per_device_reference(body: &str) -> Option<String> {
    let tokens = lex_sql(body);
    let scratch = Connection::open_in_memory().ok()?;
    for (index, (token, ..)) in tokens.iter().enumerate() {
        // A clock keyword stands alone — check it before the call-shaped test below, which would
        // never reach it.
        if let SqlToken::Word(word) = token
            && (CLOCK_KEYWORDS.contains(&word.as_str())
                || CONNECTION_CONFIGURABLE.contains(&word.as_str()))
        {
            return Some(word.clone());
        }
        // A function may be named with a bare word or a QUOTED identifier — `"random"()` is a legal
        // call, and matching only bare words would let it through unexamined.
        let (SqlToken::Word(name) | SqlToken::Ident(name)) = token else {
            continue;
        };
        if tokens.get(index + 1).map(|t| &t.0) != Some(&SqlToken::Punct('(')) {
            continue;
        }
        // A name SQLite does not know as a function is a KEYWORD that happens to be followed by a
        // parenthesis — `CHECK(`, `IN (`, `CAST(` — not a call. It cannot be a user-defined
        // function either: those live on the caller's connection, so the self-contained probe
        // (which runs on a scratch one) would already have failed to create the table.
        let is_function: bool = scratch
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_function_list WHERE name = ?1)",
                [name],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !is_function {
            continue;
        }
        if !ARGUMENT_ONLY_FUNCTIONS.contains(&name.as_str()) {
            return Some(name.clone());
        }
    }
    None
}

/// Whether `err` is SQLite refusing the VALUE (a CHECK or NOT NULL), rather than refusing to
/// resolve the statement.
fn is_constraint_failure(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// The first GENERATED column of `table`, if any. `PRAGMA table_xinfo` reports `hidden` as 2 for a
/// VIRTUAL generated column and 3 for a STORED one; `table_info` omits them entirely, which is why
/// the ordinary column classification cannot see them at all.
pub(super) fn generated_column(conn: &Connection, table: &str) -> rusqlite::Result<Option<String>> {
    let mut found = None;
    conn.pragma(None, "table_xinfo", table, |row| {
        let hidden: i64 = row.get("hidden")?;
        if (hidden == 2 || hidden == 3) && found.is_none() {
            found = Some(row.get::<_, String>("name")?);
        }
        Ok(())
    })?;
    Ok(found)
}

/// Whether `table` declares any foreign key (`PRAGMA foreign_key_list` returns a row per FK).
pub(super) fn table_has_foreign_key(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let mut any = false;
    conn.pragma(None, "foreign_key_list", table, |_row| {
        any = true;
        Ok(())
    })?;
    Ok(any)
}

/// The name of `table`'s first UNIQUE index that is NOT the primary key, if any (a cross-row
/// constraint). `PRAGMA index_list` columns: 0 seq, 1 name, 2 unique, 3 origin, 4 partial; `origin`
/// is `pk` for the primary key's implicit index, `u` for a UNIQUE constraint, `c` for a
/// `CREATE UNIQUE INDEX`.
pub(super) fn non_pk_unique_index(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<Option<String>> {
    let mut found = None;
    conn.pragma(None, "index_list", table, |row| {
        let unique: i64 = row.get(2)?;
        let origin: String = row.get(3)?;
        if unique != 0 && origin != "pk" && found.is_none() {
            found = Some(row.get::<_, String>(1)?);
        }
        Ok(())
    })?;
    Ok(found)
}

/// The first pk column that uses a non-BINARY collation, if any. Reads the primary key's index
/// (`PRAGMA index_list` origin=`pk` → `index_xinfo`, whose col 2 is the column name — NULL for the
/// implicit rowid — and col 4 the collation). An INTEGER-rowid pk has no such index (integers have
/// no collation), so it returns `None`.
pub(super) fn pk_column_with_non_binary_collation(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<Option<String>> {
    let mut pk_index = None;
    conn.pragma(None, "index_list", table, |row| {
        if row.get::<_, String>(3)? == "pk" {
            pk_index = Some(row.get::<_, String>(1)?);
        }
        Ok(())
    })?;
    let Some(index) = pk_index else { return Ok(None) };
    let mut offending = None;
    conn.pragma(None, "index_xinfo", &index, |row| {
        // index_xinfo columns: 0 seqno, 1 cid, 2 name (NULL for the rowid), 3 desc, 4 coll, 5 key.
        let name: Option<String> = row.get(2)?;
        let collation: String = row.get(4)?;
        if let Some(name) = name
            && !collation.eq_ignore_ascii_case("BINARY")
            && offending.is_none()
        {
            offending = Some(name);
        }
        Ok(())
    })?;
    Ok(offending)
}

/// The name of the first trigger on `table`, if any (`sqlite_master` type='trigger'). A trigger can
/// mutate the row (or other rows) from local/derived state, so the SAME received op could fold to
/// different physical results on two devices — divergence the whole-row fold can't detect.
pub(super) fn table_trigger(conn: &Connection, table: &str) -> rusqlite::Result<Option<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1")?;
    let mut rows = stmt.query([table])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Whether any OTHER table declares a foreign key REFERENCING `table` (an inbound reference). Scans
/// every base table's `PRAGMA foreign_key_list` (col 2 is the referenced table).
pub(super) fn table_is_referenced_by_foreign_key(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<bool> {
    let mut tables = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            tables.push(row?);
        }
    }
    for other in tables {
        if other == table {
            continue;
        }
        let mut references = false;
        conn.pragma(None, "foreign_key_list", &other, |row| {
            // foreign_key_list columns: 0 id, 1 seq, 2 table (the referenced table), 3 from, 4 to.
            if row.get::<_, String>(2)? == table {
                references = true;
            }
            Ok(())
        })?;
        if references {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn physical_column_info(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<Vec<PhysicalColumn>> {
    let mut cols = Vec::new();
    conn.pragma(None, "table_info", table, |row| {
        // PRAGMA table_info columns: 0 cid, 1 name, 2 type, 3 notnull, 4 dflt_value, 5 pk.
        cols.push(PhysicalColumn {
            name: row.get::<_, String>(1)?,
            decl_type: row.get::<_, String>(2)?.to_ascii_uppercase(),
            not_null: row.get::<_, i64>(3)? != 0,
            default_sql: row.get::<_, Option<String>>(4)?,
            pk_position: row.get::<_, i64>(5)?,
        });
        Ok(())
    })?;
    Ok(cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lex_sql` without the char spans, which only `check_constraints` needs.
    fn toks(sql: &str) -> Vec<SqlToken> {
        lex_sql(sql).into_iter().map(|(token, ..)| token).collect()
    }

    #[test]
    fn constraint_scanning_reads_sql_lexically_not_textually() {
        // Every case here is one a raw-text scan gets WRONG, in both directions.
        // Asserted through `read_table_ddl`, the live consumer of the lexer.
        let bodies = |sql: &str| {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(sql).unwrap();
            read_table_ddl(&conn, "t")
                .unwrap()
                .checks
                .into_iter()
                .map(|c| c.body)
                .collect::<Vec<_>>()
        };
        let mentions = |sql: &str, column: &str| {
            bodies(sql).iter().any(|body| {
                lex_sql(body).iter().any(
                    |(t, ..)| matches!(t, SqlToken::Word(w) | SqlToken::Ident(w) if w == column),
                )
            })
        };

        // A parenthesis inside a literal must not end the constraint early. Textually the body
        // stops at the quote, hiding `count` — a FALSE NEGATIVE on the unsafe shape.
        assert!(
            mentions(
                "CREATE TABLE t(a TEXT, later TEXT, count INT, CHECK(later <> ')' OR count = 0)) \
                 STRICT;",
                "count"
            ),
            "a paren inside a string literal is not structure"
        );

        // A literal that spells a constraint is not one — a FALSE POSITIVE textually.
        assert!(
            bodies("CREATE TABLE t(a TEXT DEFAULT 'CHECK(later=count)', later TEXT) STRICT;")
                .is_empty(),
            "`CHECK(` inside a string is inert"
        );
        // Likewise a comment.
        assert!(
            bodies("CREATE TABLE t(\n -- CHECK(later=count)\n a TEXT, later TEXT) STRICT;")
                .is_empty(),
            "`CHECK(` inside a comment is inert"
        );
        assert!(
            bodies("CREATE TABLE t(/* CHECK(later=count) */ a TEXT, later TEXT) STRICT;")
                .is_empty(),
            "`CHECK(` inside a block comment is inert"
        );

        // A column name spelled inside a literal is not a reference to that column.
        assert!(
            !mentions(
                "CREATE TABLE t(note TEXT, later TEXT, CHECK(note <> 'later')) STRICT;",
                "later"
            ),
            "text that happens to equal a column name is not a column reference"
        );

        // `CHECK` as part of an identifier is not the keyword.
        assert!(
            bodies("CREATE TABLE t(checksum TEXT, later TEXT) STRICT;").is_empty(),
            "`CHECK` inside a longer identifier is not a constraint"
        );

        // Whole-identifier matching, and quoted identifiers still count as references.
        assert!(
            !mentions(
                "CREATE TABLE t(account_id TEXT, later TEXT, CHECK(account_id <> '')) STRICT;",
                "count"
            ),
            "`count` must not match inside `account_id`"
        );
        assert!(
            mentions(
                "CREATE TABLE t(\"odd name\" TEXT, later TEXT, CHECK(\"odd name\" <> '')) STRICT;",
                "odd name"
            ),
            "a quoted identifier is a real reference"
        );

        // Nested parentheses keep the whole body.
        assert!(
            mentions(
                "CREATE TABLE t(a INT, later INT, count INT, CHECK(later > (count + 1))) STRICT;",
                "count"
            ),
            "a nested group stays inside the body"
        );
    }
    #[test]
    fn a_literal_is_never_read_as_a_column_reference() {
        // `X'ff'` is a blob literal, not a reference to a column named `x` — and `x` is a plausible
        // column name, so lexing its leading character as a word would refuse a legitimate schema.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t(x BLOB, later BLOB DEFAULT X'00', CHECK(later != X'ff')) STRICT;",
        )
        .unwrap();
        let checks = read_table_ddl(&conn, "t")
            .unwrap()
            .checks
            .into_iter()
            .map(|c| c.body)
            .collect::<Vec<_>>();
        assert_eq!(checks.len(), 1);
        let tokens = toks(&checks[0]);
        assert!(
            tokens.contains(&SqlToken::Word("later".to_string())),
            "the constrained column lexes as a word"
        );
        assert!(
            !tokens.contains(&SqlToken::Word("x".to_string())),
            "the `x` of a blob literal is not an identifier: {tokens:?}"
        );

        // Numbers likewise: an identifier cannot begin with a digit.
        assert!(!toks("1e2 + 0x1f").iter().any(|t| matches!(t, SqlToken::Word(_))));

        // Case folding follows SQLite, which is ASCII-only. `to_lowercase()` is full Unicode and
        // would fold characters SQLite keeps distinct — U+212A KELVIN SIGN lowercases to `k`.
        assert_eq!(
            toks("\u{212A}"),
            vec![SqlToken::Word("\u{212A}".to_string())],
            "Unicode folding must not apply — SQLite folds ASCII only"
        );
        assert_eq!(
            toks("COUNT"),
            vec![SqlToken::Word("count".to_string())],
            "ASCII folding must apply"
        );
    }
    #[test]
    fn lexing_survives_multibyte_and_unterminated_input() {
        // The old textual matcher advanced by BYTES, so a rejected match adjacent to a multi-byte
        // identifier could slice mid-character and panic. Characters throughout now.
        let tokens = toks("CHECK(\"éx\" >= 0 AND é = 1)");
        assert!(
            tokens.contains(&SqlToken::Ident("éx".to_string()))
                && tokens.contains(&SqlToken::Word("é".to_string())),
            "multi-byte identifiers lex as whole tokens: {tokens:?}"
        );
        assert!(!tokens.contains(&SqlToken::Word("e".to_string())), "`é` is not `e`");

        // Truncated input must terminate rather than run off the end.
        for sql in ["CHECK('unterminated", "CHECK(\"unterminated", "/* unterminated", "CHECK((("] {
            let _ = toks(sql);
        }
    }
}
