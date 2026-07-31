//! The ONE parser for scope-path and type-path strings.
//!
//! A scope path is structured text — `&W as std.fmt.Display::fmt`, `[u8; { 1 < 2 }]::run`,
//! `Foo<Bar<T>>::run` — and the index compares, splits and folds it in several places: logical
//! identity, the resolution surface, receiver-hint matching, the trait-impl marker. Every one of
//! those used to walk the string with its own ad-hoc loop, and each loop drew the boundaries
//! slightly differently: one counted `<` inside a const-generic block, another counted it inside a
//! string literal, a third split on the first ` as ` even when it sat inside `[u8; N as usize]`,
//! a fourth kept the `::` of a turbofish that the call side dropped. Two parsers of one string
//! that disagree produce a definition and a call that can never meet, and the disagreement moves
//! rather than disappears when patched at one site.
//!
//! So the rules live here once:
//! - a `"…"`/`'…'` literal is opaque — nothing inside it is punctuation, and a raw string's
//!   delimiter is its `"` plus the `#` run its prefix declared, not the next `"` it happens to
//!   hold;
//! - a `//` or `/* */` comment is opaque for the same reason, and Rust block comments NEST, so the
//!   `}` in `/* } */` closes nothing;
//! - `<`…`>` nest, but only outside literals and outside `(`/`[`/`{`, because a const-generic block
//!   holds an EXPRESSION whose `<` and `>` are operators;
//! - `->` is a return arrow, never a closing angle;
//! - structural splits (`::`, ` as `) happen at depth zero only.

/// Where one byte of a path sits, structurally.
#[derive(Clone, Copy, Default)]
pub(crate) struct Depth {
    angle: u32,
    group: u32,
}

impl Depth {
    /// At the top level of the path — not inside `<…>`, `(…)`, `[…]` or `{…}`.
    pub(crate) fn is_top(self) -> bool {
        self.angle == 0 && self.group == 0
    }

    /// Inside a generic argument list, so this text is an ARGUMENT rather than the path itself.
    fn in_angle(self) -> bool {
        self.angle > 0
    }
}

/// What one char of a path IS, for consumers that treat the three differently: structure is only
/// ever read from code, a literal's exact bytes are part of the type, and a comment is neither.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Span {
    Code,
    Literal,
    Comment,
}

impl Span {
    /// Punctuation counts as structure only here.
    pub(crate) fn is_code(self) -> bool {
        self == Self::Code
    }
}

/// What kind of literal the scan is inside — the closing rule differs.
#[derive(Clone, Copy)]
enum Literal {
    /// `"…"`, `b"…"`, `'…'` — a backslash escapes the next char.
    Escaped(char),
    /// `r"…"`, `br#"…"#` — no escapes at all; ends only at a `"` followed by `hashes` `#`.
    Raw { hashes: usize },
}

/// What kind of comment the scan is inside — a line comment ends at the newline, a block comment
/// at its own matching `*/`, and Rust block comments nest.
#[derive(Clone, Copy)]
enum Comment {
    Line,
    Block { depth: u32 },
}

/// The literal a quote at `at` opens, if any.
///
/// The closing rule is decided by a PREFIX that sits before the quote, so it is read backwards:
/// any `#` run immediately before the quote, then the character under it. That covers `r`, `br`,
/// `cr` and every hash count without enumerating the spellings. A lifetime (`'a`) is not a
/// character literal — it has no closing quote — so `'` opens one only when another follows within
/// a few chars.
fn open_literal(path: &str, at: usize, quote: char) -> Option<Literal> {
    if quote == '\'' {
        return path[at + quote.len_utf8()..]
            .chars()
            .take(4)
            .any(|next| next == '\'')
            .then_some(Literal::Escaped(quote));
    }
    let before = &path[..at];
    // Every counted char is `#`, one byte each, so the split point is a char boundary.
    let hashes = before.chars().rev().take_while(|&ch| ch == '#').count();
    match before[..before.len() - hashes].chars().next_back() {
        Some('r') => Some(Literal::Raw { hashes }),
        _ => Some(Literal::Escaped(quote)),
    }
}

/// The `#` run starting at `from` — the closing-delimiter width available to a raw string.
fn hash_run(path: &str, from: usize) -> usize {
    path.get(from..).map_or(0, |rest| rest.chars().take_while(|&ch| ch == '#').count())
}

/// Walk `path`, reporting each char with the depth INCLUDING itself and what the char IS — an
/// opener and its matching closer both report the depth of the pair they delimit, so dropping a
/// bracketed run is a single `depth.in_angle()` test that covers the brackets as well as their
/// contents.
pub(crate) fn scan(path: &str, mut visit: impl FnMut(usize, char, Depth, Span)) {
    let mut depth = Depth::default();
    let mut literal: Option<Literal> = None;
    let mut comment: Option<Comment> = None;
    let mut escaped = false;
    let mut previous = '\0';
    // The second char of a two-char delimiter. It is reported, but it must not also be read as the
    // FIRST char of the next one, or `/*/` would open and close on the same `*`.
    let mut consumed_through = 0usize;
    for (at, ch) in path.char_indices() {
        if at < consumed_through {
            visit(at, ch, depth, Span::Comment);
            previous = '\0';
            continue;
        }
        if let Some(open) = literal {
            visit(at, ch, depth, Span::Literal);
            match open {
                Literal::Escaped(quote) =>
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == quote {
                        literal = None;
                    },
                // A raw string holds unescaped quotes — `r#""}"#` closes at its LAST one. Only a
                // quote whose `#` run matches the opening prefix ends it; anything shorter is
                // content, and closing there would let the following `}` count as structural.
                Literal::Raw { hashes } =>
                    if ch == '"' && hash_run(path, at + ch.len_utf8()) >= hashes {
                        literal = None;
                    },
            }
            previous = ch;
            continue;
        }
        if let Some(open) = comment {
            visit(at, ch, depth, Span::Comment);
            match open {
                Comment::Line =>
                    if ch == '\n' {
                        comment = None;
                    },
                Comment::Block { depth: nesting } =>
                    if previous == '/' && ch == '*' {
                        comment = Some(Comment::Block { depth: nesting + 1 });
                        previous = '\0';
                        continue;
                    } else if previous == '*' && ch == '/' {
                        comment = (nesting > 1).then_some(Comment::Block { depth: nesting - 1 });
                        previous = '\0';
                        continue;
                    },
            }
            previous = ch;
            continue;
        }
        match ch {
            '"' | '\'' => {
                visit(at, ch, depth, Span::Code);
                literal = open_literal(path, at, ch);
                escaped = false;
            },
            // A lone `/` is division; only `//` and `/*` open a comment. Both chars are claimed
            // here so the opener's `*` cannot double as a closer's.
            '/' if matches!(path[at + 1..].chars().next(), Some('/') | Some('*')) => {
                let block = path[at + 1..].starts_with('*');
                comment = Some(if block { Comment::Block { depth: 1 } } else { Comment::Line });
                consumed_through = at + 2;
                visit(at, ch, depth, Span::Comment);
            },
            '(' | '[' | '{' => {
                depth.group += 1;
                visit(at, ch, depth, Span::Code);
            },
            ')' | ']' | '}' => {
                visit(at, ch, depth, Span::Code);
                depth.group = depth.group.saturating_sub(1);
                if ch == '}' {}
            },
            '<' if depth.group == 0 => {
                depth.angle += 1;
                visit(at, ch, depth, Span::Code);
            },
            // `->` closes nothing; the `>` belongs to the arrow.
            '>' if depth.group == 0 && depth.angle > 0 && previous != '-' => {
                visit(at, ch, depth, Span::Code);
                depth.angle -= 1;
            },
            _ => visit(at, ch, depth, Span::Code),
        }
        previous = ch;
    }
}

/// Byte offsets of every top-level `::` separator.
fn separator_offsets(path: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut previous_colon: Option<usize> = None;
    scan(path, |at, ch, depth, span| {
        if !span.is_code() || !depth.is_top() || ch != ':' {
            previous_colon = None;
            return;
        }
        match previous_colon.take() {
            Some(start) if start + 1 == at => offsets.push(start),
            _ => previous_colon = Some(at),
        }
    });
    offsets
}

/// Split a path on its TOP-LEVEL `::` separators. `Foo<A::B>::run` is two segments, not three;
/// `[u8; { a::b() }]::run` likewise.
pub(crate) fn segments(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for at in separator_offsets(path) {
        out.push(&path[start..at]);
        start = at + 2;
    }
    out.push(&path[start..]);
    out
}

/// The owner half of a `Type as Trait` scope segment, or the whole segment when it carries no
/// marker. The split is on a TOP-LEVEL ` as `, so a cast inside the owner — `[u8; N as usize]` —
/// is not mistaken for the marker.
pub(crate) fn strip_trait_marker(segment: &str) -> &str {
    let mut split = None;
    let bytes = segment.as_bytes();
    scan(segment, |at, ch, depth, span| {
        if split.is_some() || !span.is_code() || !depth.is_top() || ch != ' ' {
            return;
        }
        if bytes[at..].starts_with(b" as ") {
            split = Some(at);
        }
    });
    match split {
        Some(at) => segment[..at].trim_end(),
        None => segment,
    }
}

/// Drop generic ARGUMENTS from a path: `Foo<T>::run` → `Foo::run`, `Foo::<T>::run` → `Foo::run`.
///
/// The turbofish's `::` goes with the arguments it introduces — a definition scope and a call
/// target must normalize to the same string, and the call side has always dropped it.
pub(crate) fn degeneric(path: &str) -> String {
    // A LEADING `<…>` is a UFCS qualifier (`<W as a::Runs>::run`), not an argument list — dropping
    // it would erase the type and the trait and leave a bare `::run`. The call side has always
    // returned such a path unchanged, and the whole point of this module is that the two agree.
    if path.trim_start().starts_with('<') {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len());
    scan(path, |_, ch, depth, span| {
        if !span.is_code() {
            if !depth.in_angle() {
                out.push(ch);
            }
            return;
        }
        match ch {
            // The outermost `<` of an argument list; the `::` of a turbofish belongs to it.
            '<' if depth.angle == 1 =>
                if out.ends_with("::") {
                    out.truncate(out.len() - 2);
                },
            _ if depth.in_angle() => {},
            _ => out.push(ch),
        }
    });
    out
}

/// `text` with every comment replaced by one space, so the tokens either side stay apart.
///
/// A comment is trivia the Rust lexer drops before anything sees the tokens, so any parser reading
/// this text has to drop it too — `use crate::dep::{/* kept */ Worker};` binds `Worker`, and a leaf
/// compared as raw text does not. Borrowed when there is nothing to strip.
pub(crate) fn strip_comments(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("//") && !text.contains("/*") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut in_comment = false;
    scan(text, |_, ch, _, span| match span {
        Span::Comment =>
            if !in_comment {
                out.push(' ');
                in_comment = true;
            },
        _ => {
            in_comment = false;
            out.push(ch);
        },
    });
    std::borrow::Cow::Owned(out)
}

/// Peel `&`/`&mut`/`*const`/`*mut` off an owner segment.
///
/// The impl scope KEEPS the wrapper — `impl Tr for W` and `impl Tr for &W` coexist, so they are
/// different entities — but the RECEIVER surface must not: a source-form target is written
/// `W::method`, and an autoref call site cannot tell which impl it lands in.
pub(crate) fn strip_receiver_wrappers(segment: &str) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_split_only_at_the_top_level() {
        assert_eq!(segments("Foo::run"), vec!["Foo", "run"]);
        assert_eq!(segments("Foo<A::B>::run"), vec!["Foo<A::B>", "run"]);
        assert_eq!(segments("[u8; { a::b() }]::run"), vec!["[u8; { a::b() }]", "run"]);
        assert_eq!(segments("W as a.Tr::run"), vec!["W as a.Tr", "run"]);
    }

    #[test]
    fn the_marker_split_ignores_a_cast_inside_the_owner() {
        assert_eq!(strip_trait_marker("W as a.Tr"), "W");
        assert_eq!(strip_trait_marker("[u8; N as usize] as Tr"), "[u8; N as usize]");
        assert_eq!(strip_trait_marker("[u8; N as usize]"), "[u8; N as usize]");
        assert_eq!(strip_trait_marker("W"), "W");
    }

    #[test]
    fn degeneric_respects_literals_blocks_and_arrows() {
        assert_eq!(degeneric("Foo<T>::run"), "Foo::run");
        assert_eq!(degeneric("Foo::<T>::run"), "Foo::run");
        assert_eq!(degeneric("Foo<{ 1 < 2 }>::run"), "Foo::run");
        assert_eq!(degeneric(r#"Foo<{ "{".len() }>::run"#), "Foo::run");
        assert_eq!(degeneric(r#"Foo<{ "<>".len() }>::run"#), "Foo::run");
        assert_eq!(degeneric("fn(&T) -> U"), "fn(&T) -> U");
        assert_eq!(degeneric("[u8; { 1 < 2 }]::run"), "[u8; { 1 < 2 }]::run");
        // A leading UFCS qualifier survives, exactly as the call side leaves it.
        assert_eq!(degeneric("<W as a::Runs>::run"), "<W as a::Runs>::run");
    }

    /// A raw string's closing delimiter is its `"` plus the `#` run its prefix declared. Ending it
    /// at the first interior quote would hand the rest of the literal back as structure — the `}`
    /// in `r#""}"#` would close the const-generic block and swallow the path's tail.
    #[test]
    fn a_raw_string_closes_only_on_its_own_delimiter() {
        assert_eq!(degeneric(r##"Foo<{ r#""}"#.len() }>::run"##), "Foo::run");
        assert_eq!(degeneric(r####"Foo<{ r###""##}"###.len() }>::run"####), "Foo::run");
        // No hashes: `r"…"` still ends at its first quote, and `br`/`cr` read the same way.
        assert_eq!(degeneric(r#"Foo<{ r"}".len() }>::run"#), "Foo::run");
        assert_eq!(degeneric(r##"Foo<{ br#""}"#.len() }>::run"##), "Foo::run");
        // A `#` with no `r` under it is not a raw prefix, so escapes still apply.
        assert_eq!(degeneric(r#"Foo<{ "\"}".len() }>::run"#), "Foo::run");
    }

    /// A comment holds no punctuation. Block comments nest, and `/*/` opens without closing, so
    /// neither the opener's `*` nor a nested one may double as a terminator.
    #[test]
    fn a_comment_is_opaque_to_structure() {
        assert_eq!(segments("[u8; { /* :: */ N }]::run"), vec!["[u8; { /* :: */ N }]", "run"]);
        assert_eq!(segments("W /* as Tr */ ::run"), vec!["W /* as Tr */ ", "run"]);
        assert_eq!(strip_trait_marker("[u8; N /* as usize */]"), "[u8; N /* as usize */]");
        assert_eq!(degeneric("Foo<{ /* < > */ N }>::run"), "Foo::run");
        assert_eq!(degeneric("Foo<{ /* /* < */ */ N }>::run"), "Foo::run");
        assert_eq!(degeneric("Foo<{ /*/ < */ N }>::run"), "Foo::run");
        // A line comment runs to the newline and no further.
        assert_eq!(segments("[u8; { // }\n N }]::run"), vec!["[u8; { // }\n N }]", "run"]);
        // A lone `/` is division, not a comment.
        assert_eq!(degeneric("[u8; { N / 2 }]::run"), "[u8; { N / 2 }]::run");
        // A `//` inside a literal is content.
        assert_eq!(degeneric(r#"Foo<{ "// }".len() }>::run"#), "Foo::run");
    }

    #[test]
    fn wrappers_peel_off_the_receiver_surface() {
        assert_eq!(strip_receiver_wrappers("&W"), "W");
        assert_eq!(strip_receiver_wrappers("&mut W"), "W");
        assert_eq!(strip_receiver_wrappers("*const W"), "W");
        assert_eq!(strip_receiver_wrappers("W"), "W");
    }
}
