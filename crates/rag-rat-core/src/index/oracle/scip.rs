//! SCIP `.scip` reader: turns a serialized SCIP index into the two lookup maps the join needs.
//!
//! 1. An **occurrence map** `path -> [Occurrence]`, each carrying its `(line, start_char,
//!    end_char)` range in the **document's own position encoding** plus the SCIP symbol string.
//! 2. A **definition map** `scip_symbol -> (path, def-range)` built from the `Definition`-role
//!    occurrences, so a referenced symbol can be located back to its defining file + range.
//!
//! ## The position-encoding trap (read this before touching the conversion)
//!
//! SCIP occurrence ranges count columns in *code units of the document's `position_encoding`* —
//! UTF-8 bytes, UTF-16 units, or UTF-32 (Unicode scalar) units — declared **per `Document`**, not
//! globally. rag-rat's edge rows store the callee's **byte** range. To compare them we must convert
//! one space to the other in the document's declared encoding. This only diverges on non-ASCII
//! source (every encoding agrees on ASCII), so a UTF-8-assuming shortcut passes every ASCII test
//! and then silently mis-joins the moment an identifier sits after a multibyte character (an
//! accented name, a CJK comment earlier on the line). We therefore key occurrences by **byte**
//! offsets computed from the encoding at read time, and the join works purely in byte space.
//!
//! `local N` symbols are dropped entirely here — they are function-local and carry no cross-file
//! meaning, so they can never resolve a cross-file edge.

use std::collections::HashMap;

use ::protobuf::Message;
use ::scip::types::{Index, PositionEncoding};

/// A SCIP occurrence reduced to what the join needs: the **byte** range of the identifier token on
/// its line and the SCIP symbol it refers to. Byte offsets are absolute within the file (not
/// line-relative), so they compare directly against the edge's
/// `callee_start_byte`/`callee_end_byte`.
#[derive(Debug, Clone)]
pub(crate) struct ScipOccurrence {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) symbol: String,
    /// True when this occurrence carries the `Definition` role bit.
    pub(crate) is_definition: bool,
    /// True when this occurrence carries the `Import` role bit. An import (`use foo::bar`) is a
    /// reference, not a call site, so it must be excluded from the recall gap (see `run::run`).
    pub(crate) is_import: bool,
}

/// Where a SCIP symbol is defined: the document path and the **byte** range of its definition
/// occurrence. Used to decide in-corpus (maps to one of our symbols) vs. external.
#[derive(Debug, Clone)]
pub(crate) struct ScipDefinition {
    pub(crate) path: String,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
}

/// The parsed `.scip`, reduced to the join's two maps.
#[derive(Debug, Default)]
pub(crate) struct ScipIndex {
    /// `relative_path -> occurrences` (definitions and references both), byte-keyed.
    pub(crate) occurrences_by_path: HashMap<String, Vec<ScipOccurrence>>,
    /// `scip_symbol -> definition site`. First definition wins (SCIP emits one per symbol).
    pub(crate) definitions: HashMap<String, ScipDefinition>,
}

impl ScipIndex {
    /// Parse a serialized SCIP index (`.scip` protobuf bytes) into the join maps.
    ///
    /// `read_document_source(path)` yields the **current checkout bytes** of a document so column
    /// offsets can be converted in the document's encoding. A document whose source can't be read
    /// is skipped (its occurrences can't be byte-resolved); the join just finds no oracle data for
    /// those edges, which is the correct degradation.
    pub(crate) fn parse(
        bytes: &[u8],
        mut read_document_source: impl FnMut(&str) -> Option<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let index = Index::parse_from_bytes(bytes)
            .map_err(|err| anyhow::anyhow!("failed to parse SCIP index: {err}"))?;
        Self::from_index(&index, &mut read_document_source)
    }

    /// The relative paths of every document in a serialized `.scip`, WITHOUT reading any source or
    /// converting offsets. Used by `produce_scip_with_tool` to snapshot the disk content the tool
    /// just built against (the scip-vs-disk content gate, #82 TOCTOU) — that snapshot only needs
    /// the document list, so it skips the full [`ScipIndex::parse`]. Returns paths in document
    /// order; a duplicate document path (SCIP shouldn't emit one) is harmless to the caller's
    /// set/hash use.
    pub(crate) fn document_relative_paths(bytes: &[u8]) -> anyhow::Result<Vec<String>> {
        let index = Index::parse_from_bytes(bytes)
            .map_err(|err| anyhow::anyhow!("failed to parse SCIP index: {err}"))?;
        Ok(index.documents.iter().map(|document| document.relative_path.clone()).collect())
    }

    /// Build the maps from an already-deserialized [`Index`] — the entry point tests use after
    /// constructing an `Index` programmatically.
    pub(crate) fn from_index(
        index: &Index,
        read_document_source: &mut impl FnMut(&str) -> Option<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let mut out = ScipIndex::default();
        for document in &index.documents {
            let path = document.relative_path.clone();
            let Some(source) = read_document_source(&path) else {
                continue;
            };
            let encoding = document
                .position_encoding
                .enum_value()
                .unwrap_or(PositionEncoding::UnspecifiedPositionEncoding);
            let mapper = LineColumnToByte::new(&source, encoding);

            let mut occurrences = Vec::with_capacity(document.occurrences.len());
            for occ in &document.occurrences {
                // Skip function-local symbols entirely (no cross-file meaning).
                if is_local_symbol(&occ.symbol) {
                    continue;
                }
                let Some((start_byte, end_byte)) = mapper.byte_range(&occ.range) else {
                    continue;
                };
                let is_definition = has_definition_role(occ.symbol_roles);
                if is_definition {
                    out.definitions.entry(occ.symbol.clone()).or_insert(ScipDefinition {
                        path: path.clone(),
                        start_byte,
                        end_byte,
                    });
                }
                occurrences.push(ScipOccurrence {
                    start_byte,
                    end_byte,
                    symbol: occ.symbol.clone(),
                    is_definition,
                    is_import: has_import_role(occ.symbol_roles),
                });
            }
            out.occurrences_by_path.insert(path, occurrences);
        }
        Ok(out)
    }
}

/// `SymbolRole.Definition` is bit 1 (value `1`) of the `symbol_roles` bitset.
fn has_definition_role(symbol_roles: i32) -> bool {
    symbol_roles & (::scip::types::SymbolRole::Definition as i32) != 0
}

/// `SymbolRole.Import` is bit 2 (value `2`) of the `symbol_roles` bitset.
fn has_import_role(symbol_roles: i32) -> bool {
    symbol_roles & (::scip::types::SymbolRole::Import as i32) != 0
}

/// Whether a SCIP symbol names a *callable* (function / method), as opposed to a type, field,
/// const, static, enum variant, module, or parameter. This is the kind a call-graph edge would
/// target.
///
/// SCIP has no explicit "call" occurrence role, so call-likeness is necessarily a heuristic keyed
/// on the symbol's descriptor *kind* — the LAST descriptor's `Suffix`. rust-analyzer (and SCIP
/// generators generally) emit `Method` for both free functions and methods (the printed `().`
/// suffix). We accept ONLY `Method` and reject everything else.
///
/// CRITICAL — why `).` and NOT a bare trailing `.`: SCIP prints BOTH a `Method` descriptor (`…).`)
/// and a plain `Term` descriptor (`….`, no parens) with a trailing `.`. In rust-analyzer's SCIP
/// output `Term` covers struct **fields, consts, statics, and enum variants** — NOT just callables.
/// A bare-`.` test therefore admits an in-corpus field/const/static/variant *read* occurrence: it
/// is not an `Import`, ends in `.`, and its def maps via containment to the enclosing struct/enum
/// symbol — so every such read counted as an uncovered "call" and DEFLATED recall. SCIP's grammar
/// distinguishes the two: a `Method` ends `).` (the printed `()` disambiguator before the `.`),
/// whereas a plain `Term` ends `.` with no preceding `)`. Requiring the `).` suffix admits exactly
/// the callable kind and excludes field/const/variant terms. Other kinds end differently — a `Type`
/// `…#`, a `Macro` `…!`, a `Meta` `…:`, a `Namespace`/`Package` `…/` — so they were already out. A
/// symbol with no recognizable callable suffix is conservatively non-callable, so a malformed
/// string can't inflate the recall denominator.
pub(crate) fn symbol_is_callable(symbol: &str) -> bool {
    symbol.trim_end().ends_with(").")
}

/// A SCIP `local …` symbol is function-local. SCIP's own `is_local_symbol` checks the `local`
/// scheme prefix; we mirror it so the dependency's exact rule applies.
fn is_local_symbol(symbol: &str) -> bool {
    ::scip::symbol::is_local_symbol(symbol)
}

/// The constant version component pinned into in-corpus monikers so they stay stable across commits
/// (matches scip-python's `--project-version _`). See [`stabilize_moniker_version`].
pub(crate) const STABLE_MONIKER_VERSION: &str = "_";

/// Normalize the **version** component of an in-corpus moniker to a constant so a stored moniker is
/// invariant across releases — the prerequisite for moniker-anchored memory relocation.
///
/// scip-typescript bakes the project's `package.json` `version` into the package component of every
/// local moniker (`scip-typescript npm <name> <version> <descriptor>`) and exposes no
/// `--project-version` flag to override it (unlike scip-python, which we pin to `_` at the CLI). A
/// routine version bump would otherwise churn every TS moniker and orphan every memory bound to
/// one. Rewriting the version to [`STABLE_MONIKER_VERSION`] yields the same stable form scip-python
/// already produces. Tools whose monikers are already version-stable (rust-analyzer, scip-clang,
/// scip-python) pass through untouched, as do unparsable / package-less / local symbols.
pub(crate) fn stabilize_moniker_version(
    tool: super::OracleTool,
    symbol: &str,
) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if tool != super::OracleTool::ScipTypescript {
        return Cow::Borrowed(symbol);
    }
    let Ok(mut parsed) = ::scip::symbol::parse_symbol(symbol) else {
        return Cow::Borrowed(symbol);
    };
    let Some(package) = parsed.package.as_mut() else {
        return Cow::Borrowed(symbol);
    };
    if package.name.trim().is_empty() || package.version == STABLE_MONIKER_VERSION {
        return Cow::Borrowed(symbol);
    }
    package.version = STABLE_MONIKER_VERSION.to_string();
    Cow::Owned(::scip::symbol::format_symbol(parsed))
}

/// Converts a SCIP `Occurrence.range` (`[start_line, start_char, end_char]` for single-line, or
/// `[start_line, start_char, end_line, end_char]` for multi-line) — whose char columns are in a
/// document's `position_encoding` — into an absolute **byte** range within the file.
///
/// Precomputes, per line, the byte offset of the line start, so a `(line, char)` lookup is a walk
/// from the line start advancing one code unit at a time in the declared encoding. This is the
/// single piece of code where the encoding actually matters; everything downstream is byte space.
struct LineColumnToByte<'a> {
    source: &'a [u8],
    encoding: PositionEncoding,
    /// Byte offset of the start of each line (`line_starts[i]` = first byte of 0-based line `i`).
    line_starts: Vec<usize>,
}

impl<'a> LineColumnToByte<'a> {
    fn new(source: &'a [u8], encoding: PositionEncoding) -> Self {
        let mut line_starts = vec![0usize];
        for (idx, byte) in source.iter().enumerate() {
            if *byte == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self { source, encoding, line_starts }
    }

    /// Byte range for a SCIP occurrence range vector. Returns `None` for malformed ranges or
    /// positions past end-of-file.
    fn byte_range(&self, range: &[i32]) -> Option<(usize, usize)> {
        let (start_line, start_char, end_line, end_char) = match range {
            [sl, sc, ec] => (*sl, *sc, *sl, *ec),
            [sl, sc, el, ec] => (*sl, *sc, *el, *ec),
            _ => return None,
        };
        let start = self.byte_at(start_line, start_char)?;
        let end = self.byte_at(end_line, end_char)?;
        if end < start {
            return None;
        }
        Some((start, end))
    }

    /// Byte offset of a `(line, char)` position, where `char` counts code units in `self.encoding`.
    fn byte_at(&self, line: i32, character: i32) -> Option<usize> {
        let line = usize::try_from(line).ok()?;
        let target_units = usize::try_from(character).ok()?;
        let line_start = *self.line_starts.get(line)?;
        // Bound the walk at the next line start (or EOF) so a column overrun can't spill onto the
        // following line.
        let line_end = self.line_starts.get(line + 1).copied().unwrap_or(self.source.len());
        let mut byte = line_start;
        let mut units = 0usize;
        while units < target_units {
            if byte >= line_end {
                // Column points exactly at (or past) line end; clamp to line end. SCIP end columns
                // commonly sit at the line's trailing position, which is valid.
                return Some(line_end.min(self.source.len()));
            }
            let ch_len = utf8_char_len(self.source[byte]);
            let ch_bytes = self.source.get(byte..byte + ch_len)?;
            units += code_units_for(ch_bytes, self.encoding);
            byte += ch_len;
        }
        Some(byte)
    }
}

/// Length in bytes of the UTF-8 sequence whose lead byte is `lead`.
fn utf8_char_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        // Continuation or invalid lead byte: treat as a single byte so the walk always advances.
        1
    }
}

/// How many code units a single Unicode scalar (given as its UTF-8 bytes) occupies in `encoding`.
///
/// - UTF-8: the UTF-8 byte length.
/// - UTF-16: 1 for BMP scalars, 2 for astral (surrogate pair) scalars (UTF-8 length 4).
/// - UTF-32 / unspecified: 1 (one scalar = one unit). SCIP treats unspecified as UTF-32-ish; one
///   unit per scalar is the safe default and matches the spec's fallback intent.
fn code_units_for(utf8_bytes: &[u8], encoding: PositionEncoding) -> usize {
    match encoding {
        PositionEncoding::UTF8CodeUnitOffsetFromLineStart => utf8_bytes.len(),
        PositionEncoding::UTF16CodeUnitOffsetFromLineStart =>
            if utf8_bytes.len() == 4 {
                2
            } else {
                1
            },
        PositionEncoding::UTF32CodeUnitOffsetFromLineStart
        | PositionEncoding::UnspecifiedPositionEncoding => 1,
    }
}
