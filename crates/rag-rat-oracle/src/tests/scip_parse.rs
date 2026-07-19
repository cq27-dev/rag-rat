use super::scip::ScipIndex;
use super::*;

// ---------------------------------------------------------------------------
// scip.rs — reader branches: empty/malformed bytes, multi-line, encoding fallback.
// ---------------------------------------------------------------------------

/// Malformed protobuf bytes return a clean parse error (not a panic), naming the SCIP index.
#[test]
fn parse_malformed_bytes_returns_clean_error() {
    // A long run of 0xFF is not a valid protobuf wire stream.
    let err = ScipIndex::parse(&[0xFF; 64], |_| Some(Vec::new())).unwrap_err();
    assert!(err.to_string().contains("SCIP"), "error mentions SCIP: {err}");
}

/// Empty bytes parse to an empty index (zero documents) — not an error.
#[test]
fn parse_empty_bytes_yields_empty_maps() {
    let idx = ScipIndex::parse(&[], |_| Some(Vec::new())).unwrap();
    assert!(idx.occurrences_by_path.is_empty());
    assert!(idx.definitions.is_empty());
}

/// A document whose source can't be read is skipped entirely — its occurrences never enter the
/// maps (the correct degradation; the join just finds no oracle data for those edges).
#[test]
fn parse_skips_documents_with_unreadable_source() {
    let bytes =
        scip_bytes("gone.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occurrence(
            0,
            0,
            3,
            "scip-rust crate v1 `f`().",
            SymbolRole::UnspecifiedSymbolRole as i32,
        )]);
    // read_document_source returns None → document dropped.
    let idx = ScipIndex::parse(&bytes, |_| None).unwrap();
    assert!(idx.occurrences_by_path.is_empty());
}

/// A document with no occurrences still registers an (empty) path entry; definitions stay empty.
#[test]
fn parse_document_with_no_occurrences_registers_empty_entry() {
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![]);
    let idx = ScipIndex::parse(&bytes, |_| Some(b"fn a() {}\n".to_vec())).unwrap();
    assert_eq!(idx.occurrences_by_path.get("a.rs").map(Vec::len), Some(0));
    assert!(idx.definitions.is_empty());
}

/// scip-typescript leaves `position_encoding` UNSET (Unspecified) but emits UTF-16 column offsets.
/// `parse_with_default(UTF16, …)` must read those columns as UTF-16 so an identifier after an
/// astral character lands on the right bytes; the historical Unspecified→UTF-32 fallback misaligns
/// it.
#[test]
fn unspecified_encoding_uses_the_supplied_default() {
    // "😀foo\n": the emoji is 4 UTF-8 bytes = 2 UTF-16 units = 1 UTF-32 unit; "foo" is bytes 4..7.
    let source = "😀foo\n".as_bytes().to_vec();
    // UTF-16 columns [2,5] (past the 2-unit emoji).
    let occ =
        occurrence(0, 2, 5, "scip-typescript npm p 1 `a.ts`/foo().", SymbolRole::Definition as i32);
    // Document with NO position_encoding set (serialized as the protobuf default = Unspecified).
    let bytes = scip_bytes("a.ts", PositionEncoding::UnspecifiedPositionEncoding, vec![occ]);

    let s = source.clone();
    let idx = ScipIndex::parse_with_default(
        &bytes,
        PositionEncoding::UTF16CodeUnitOffsetFromLineStart,
        move |_| Some(s.clone()),
    )
    .unwrap();
    let occ16 = &idx.occurrences_by_path.get("a.ts").unwrap()[0];
    assert_eq!((occ16.start_byte, occ16.end_byte), (4, 7));
    assert_eq!(&source[occ16.start_byte..occ16.end_byte], b"foo");

    // The plain 2-arg parse (Unspecified → UTF-32 fallback) misaligns onto the wrong bytes.
    let s = source.clone();
    let idx32 = ScipIndex::parse(&bytes, move |_| Some(s.clone())).unwrap();
    let occ32 = &idx32.occurrences_by_path.get("a.ts").unwrap()[0];
    assert_ne!(&source[occ32.start_byte..occ32.end_byte], b"foo", "UTF-32 fallback misaligns");
}

/// `symbol_is_module` recognizes the SCIP namespace/module/package suffix (`/`) and nothing else.
#[test]
fn symbol_is_module_matches_only_namespace_suffix() {
    use super::scip::symbol_is_module;
    assert!(symbol_is_module("scip-typescript npm p 1 `a.ts`/"));
    assert!(!symbol_is_module("scip-typescript npm p 1 `a.ts`/foo().")); // method
    assert!(!symbol_is_module("scip-typescript npm p 1 `a.ts`/Bar#")); // type
    assert!(!symbol_is_module("local 1"));
}

/// A multi-line occurrence range (`[start_line, start_char, end_line, end_char]`) parses to the
/// byte span crossing the newline.
#[test]
fn parse_multi_line_occurrence_range() {
    // Source: line 0 = "fn a(\n", line 1 = "   b) {}\n". A range from (0,3) to (1,4) spans
    // bytes 3 .. (line1_start 6 + 4) = 3..10.
    let source = b"fn a(\n   b) {}\n".to_vec();
    let occ = Occurrence {
        range: vec![0, 3, 1, 4],
        symbol: "scip-rust crate v1 `span`().".to_string(),
        symbol_roles: SymbolRole::UnspecifiedSymbolRole as i32,
        ..Default::default()
    };
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!(occs.len(), 1);
    assert_eq!((occs[0].start_byte, occs[0].end_byte), (3, 10));
}

#[test]
fn parse_typed_single_line_occurrence_range() {
    let source = b"fn typed() {}\n".to_vec();
    let mut occ =
        Occurrence { symbol: "scip-rust crate v1 `typed`().".to_string(), ..Default::default() };
    occ.set_single_line_range(::scip::types::SingleLineRange {
        line: 0,
        start_character: 3,
        end_character: 8,
        ..Default::default()
    });
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let parsed = &idx.occurrences_by_path.get("a.rs").unwrap()[0];
    assert_eq!((parsed.start_byte, parsed.end_byte), (3, 8));
}

#[test]
fn parse_typed_multi_line_occurrence_range() {
    let source = b"fn a(\n   b) {}\n".to_vec();
    let mut occ =
        Occurrence { symbol: "scip-rust crate v1 `span`().".to_string(), ..Default::default() };
    occ.set_multi_line_range(::scip::types::MultiLineRange {
        start_line: 0,
        start_character: 3,
        end_line: 1,
        end_character: 4,
        ..Default::default()
    });
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let parsed = &idx.occurrences_by_path.get("a.rs").unwrap()[0];
    assert_eq!((parsed.start_byte, parsed.end_byte), (3, 10));
}

#[test]
fn typed_occurrence_range_takes_precedence_over_legacy_range() {
    let source = b"fn typed() {}\n".to_vec();
    let mut occ = Occurrence {
        range: vec![0, 0, 2],
        symbol: "scip-rust crate v1 `typed`().".to_string(),
        ..Default::default()
    };
    occ.set_single_line_range(::scip::types::SingleLineRange {
        line: 0,
        start_character: 3,
        end_character: 8,
        ..Default::default()
    });
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let parsed = &idx.occurrences_by_path.get("a.rs").unwrap()[0];
    assert_eq!((parsed.start_byte, parsed.end_byte), (3, 8));
}

/// An unspecified `position_encoding` falls back to one-byte-per-code-unit on ASCII (it behaves
/// like UTF-32: one unit per scalar), so an ASCII identifier resolves to the same span a UTF-8
/// document would.
#[test]
fn parse_unspecified_encoding_falls_back_for_ascii() {
    let source = b"fn a() { foo(); }\n".to_vec();
    // `foo` sits at chars/bytes 9..12 on line 0 (ASCII → all encodings agree).
    let occ = occurrence(
        0,
        9,
        12,
        "scip-rust crate v1 `foo`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes = scip_bytes("a.rs", PositionEncoding::UnspecifiedPositionEncoding, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!((occs[0].start_byte, occs[0].end_byte), (9, 12));
}

/// A malformed occurrence range (wrong arity) is dropped, not fatal — the rest of the document
/// still parses.
#[test]
fn parse_drops_malformed_occurrence_range() {
    let source = b"fn a() { foo(); }\n".to_vec();
    let bad = Occurrence {
        range: vec![0, 9], // arity 2 — neither single- nor multi-line shape.
        symbol: "scip-rust crate v1 `foo`().".to_string(),
        symbol_roles: SymbolRole::UnspecifiedSymbolRole as i32,
        ..Default::default()
    };
    let good = occurrence(
        0,
        9,
        12,
        "scip-rust crate v1 `bar`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes =
        scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![bad, good]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    // Only the well-formed range survives.
    assert_eq!(occs.len(), 1);
    assert_eq!(occs[0].symbol, "scip-rust crate v1 `bar`().");
}

/// A 3-byte UTF-8 scalar (a BMP CJK character) is one UTF-16 unit; an ASCII identifier after it
/// lands at the right byte offset under UTF-16 — exercises the 3-byte `utf8_char_len` arm and the
/// BMP (non-astral) UTF-16 branch.
#[test]
fn parse_utf16_three_byte_bmp_character_offsets_correctly() {
    // Line 0: "中x foo()\n". '中' (U+4E2D) is 3 UTF-8 bytes / 1 UTF-16 unit.
    //   bytes: 中(3 → 0..3) x(byte 3) ' '(byte 4) f o o → `foo` at bytes 5..8.
    //   UTF-16 units before `foo`: 中=1, x=1, space=1 = 3 → `foo` is units 3..6.
    let source = "中x foo()\n".as_bytes().to_vec();
    assert_eq!(&source[5..8], b"foo", "byte offset sanity check");
    let occ = occurrence(
        0,
        3,
        6,
        "scip-rust crate v1 `foo`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF16CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!((occs[0].start_byte, occs[0].end_byte), (5, 8));
}

/// A range whose end column lands BEFORE its start column is malformed and dropped (the
/// `end < start` guard in `byte_range`), not surfaced as an inverted span.
#[test]
fn parse_drops_range_with_end_before_start() {
    let source = b"fn a() { foo(); }\n".to_vec();
    // Single-line range [line 0, start_char 12, end_char 9] → end byte < start byte.
    let occ = occurrence(
        0,
        12,
        9,
        "scip-rust crate v1 `foo`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!(occs.len(), 0, "inverted range dropped");
}

/// A column that overruns the line is clamped to the line end (the `byte >= line_end` branch in
/// `byte_at`), so an over-long end column resolves to the newline boundary instead of spilling.
#[test]
fn parse_clamps_column_overrun_to_line_end() {
    // Line 0 = "ab\n": line_starts = [0, 3]. An end column of 9 overruns the line → the walk hits
    // `byte >= line_end` and clamps to the line-end boundary (byte 3, the start of line 1).
    let source = b"ab\ncd\n".to_vec();
    let occ =
        occurrence(0, 0, 9, "scip-rust crate v1 `ab`().", SymbolRole::UnspecifiedSymbolRole as i32);
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF8CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    assert_eq!(occs[0].start_byte, 0);
    assert_eq!(occs[0].end_byte, 3, "overrun clamped to the line-end boundary");
}

// ---------------------------------------------------------------------------
// join.rs — package extraction, def-range → symbol overlap mapping.
// ---------------------------------------------------------------------------

/// `package_of` / `names_external_package` extract the crate/package name from a package-bearing
/// SCIP symbol and return `None`/false for a local symbol.
/// A 4-byte UTF-8 scalar (an astral-plane character) counts as **2** UTF-16 code units; an ASCII
/// identifier after it lands at the right byte offset only when the reader applies that surrogate
/// width — the UTF-16 astral branch in `code_units_for` plus the 4-byte `utf8_char_len` arm.
#[test]
fn parse_utf16_astral_character_shifts_offset_by_surrogate_width() {
    // Source line 0: "𝛂x foo()\n". '𝛂' (U+1D6C2) is 4 UTF-8 bytes / 2 UTF-16 units.
    //   bytes:  𝛂(4 → 0..4) x(byte 4) ' '(byte 5) f o o → `foo` starts at byte 6, ends at 9.
    //   UTF-16 units before `foo`: 𝛂=2, x=1, space=1 = 4 → `foo` is units 4..7.
    let source = "𝛂x foo()\n".as_bytes().to_vec();
    assert_eq!(&source[6..9], b"foo", "byte offset sanity check");
    let occ = occurrence(
        0,
        4,
        7,
        "scip-rust crate v1 `foo`().",
        SymbolRole::UnspecifiedSymbolRole as i32,
    );
    let bytes = scip_bytes("a.rs", PositionEncoding::UTF16CodeUnitOffsetFromLineStart, vec![occ]);
    let idx = ScipIndex::parse(&bytes, move |_| Some(source.clone())).unwrap();
    let occs = idx.occurrences_by_path.get("a.rs").unwrap();
    // Correct surrogate accounting lands the byte range on `foo` (bytes 6..9).
    assert_eq!((occs[0].start_byte, occs[0].end_byte), (6, 9));
}

// ---------------------------------------------------------------------------
// scip.rs — external `SymbolInformation` side map (#114): kind / signature / docs / deprecation.
// ---------------------------------------------------------------------------

/// Build a `SymbolInformation` for an external dependency symbol. `signature` is `(language,
/// text)`.
fn external_symbol(
    moniker: &str,
    kind: ::scip::types::symbol_information::Kind,
    display_name: &str,
    signature: Option<(&str, &str)>,
    docs: &[&str],
) -> ::scip::types::SymbolInformation {
    ::scip::types::SymbolInformation {
        symbol: moniker.to_string(),
        display_name: display_name.to_string(),
        kind: ::protobuf::EnumOrUnknown::new(kind),
        documentation: docs.iter().map(|doc| doc.to_string()).collect(),
        signature_documentation: signature
            .map(|(language, text)| {
                ::protobuf::MessageField::some(::scip::types::Signature {
                    language: language.to_string(),
                    text: text.to_string(),
                    ..Default::default()
                })
            })
            .unwrap_or_default(),
        ..Default::default()
    }
}

/// Parse an index carrying only `external_symbols` (no documents to read).
fn parse_external(symbols: Vec<::scip::types::SymbolInformation>) -> ScipIndex {
    let index = Index { external_symbols: symbols, ..Default::default() };
    ScipIndex::from_index(&index, PositionEncoding::UTF8CodeUnitOffsetFromLineStart, &mut |_| None)
        .unwrap()
}

/// `index.external_symbols` populate the moniker-keyed side map with kind / display_name /
/// signature text+language / documentation, keyed by the RAW moniker (so it exact-joins
/// `edge_oracle`).
#[test]
fn parse_external_symbols_populates_the_side_map() {
    use ::scip::types::symbol_information::Kind;
    let moniker = "scip-typescript npm ky 1.7.2 index.ts/Ky#get().";
    let idx = parse_external(vec![external_symbol(
        moniker,
        Kind::Method,
        "get",
        Some(("typescript", "get(url: string, options?: Options): ResponsePromise")),
        &["Fetches the URL with the given options."],
    )]);

    let info = idx.external_symbol_info.get(moniker).expect("moniker keyed raw, exact");
    assert_eq!(info.kind, "Method");
    assert_eq!(info.display_name, "get");
    assert_eq!(info.signature_language, "typescript");
    assert!(info.signature_text.contains("ResponsePromise"), "signature text preserved");
    assert_eq!(info.documentation, "Fetches the URL with the given options.");
    assert!(!info.deprecated, "no deprecation marker present");
}

/// The deprecation verdict fires on a case-insensitive "deprecated" in EITHER the documentation or
/// the signature text, and stays false when absent.
#[test]
fn parse_external_symbols_detect_deprecation_in_docs_or_signature() {
    use ::scip::types::symbol_information::Kind;
    let deprecated_in_docs = "pkg a 1 mod/old().";
    let deprecated_in_sig = "pkg a 1 mod/old2().";
    let clean = "pkg a 1 mod/current().";
    let idx = parse_external(vec![
        external_symbol(deprecated_in_docs, Kind::Function, "old", None, &[
            "@deprecated Use `new` instead."
        ]),
        external_symbol(
            deprecated_in_sig,
            Kind::Function,
            "old2",
            // Capitalized marker in the signature, none in docs — still caught (case-insensitive).
            Some(("rust", "fn old2() // DEPRECATED")),
            &[],
        ),
        external_symbol(clean, Kind::Function, "current", Some(("rust", "fn current()")), &[
            "The supported entry point.",
        ]),
    ]);

    assert!(idx.external_symbol_info.get(deprecated_in_docs).unwrap().deprecated);
    assert!(idx.external_symbol_info.get(deprecated_in_sig).unwrap().deprecated);
    assert!(!idx.external_symbol_info.get(clean).unwrap().deprecated);
}

/// `local N` external entries carry no cross-file meaning and are dropped; a duplicate moniker
/// keeps the FIRST `SymbolInformation` (SCIP emits one per symbol, but the map must be
/// deterministic).
#[test]
fn parse_external_symbols_skip_locals_and_keep_first_on_duplicate() {
    use ::scip::types::symbol_information::Kind;
    let dup = "pkg a 1 mod/dup().";
    let idx = parse_external(vec![
        external_symbol("local 0", Kind::Function, "scratch", None, &[]),
        external_symbol(dup, Kind::Function, "first", Some(("rust", "fn dup()")), &[]),
        external_symbol(dup, Kind::Method, "second", Some(("rust", "fn dup(x)")), &[]),
    ]);

    assert!(!idx.external_symbol_info.contains_key("local 0"), "local symbols dropped");
    assert_eq!(
        idx.external_symbol_info.get(dup).unwrap().display_name,
        "first",
        "first entry wins"
    );
    assert_eq!(idx.external_symbol_info.len(), 1);
}

/// A symbol with no `signature_documentation` and an unset `kind` yields empty signature fields and
/// the "unspecified" kind label — no panic, graceful empties.
#[test]
fn parse_external_symbols_tolerate_missing_signature_and_kind() {
    use ::scip::types::symbol_information::Kind;
    let moniker = "pkg a 1 mod/bare().";
    let idx = parse_external(vec![external_symbol(moniker, Kind::UnspecifiedKind, "", None, &[])]);

    let info = idx.external_symbol_info.get(moniker).unwrap();
    assert_eq!(info.kind, "unspecified");
    assert_eq!(info.signature_text, "");
    assert_eq!(info.signature_language, "");
    assert_eq!(info.documentation, "");
    assert!(!info.deprecated);
}
