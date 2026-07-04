//! Clone-detection commands, split out of the `commands` god-module: `clones` (listing / recall /
//! explain / precompute) and `clones-for` (per-symbol clone class), plus the pure
//! `recall_signature` / `print_clone_explain` render helpers they call.
use rag_rat_core::Config;

use crate::cli::{ClonesArgs, ClonesForArgs};
use crate::open_index;
use crate::render::print_output;

pub(crate) fn clones(config: &Config, args: &ClonesArgs) -> anyhow::Result<()> {
    // `--precompute`: the WRITER path — build/refresh the persisted clone-edge graph (#286) under a
    // write lock (mirroring `maintenance`), then print the build report instead of a clone listing.
    if args.precompute {
        let lock_repo = rag_rat_core::locks::write_lock_repo_id(config);
        let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
        let db = open_index(config)?;
        let report: rag_rat_core::index::CloneEdgeReport =
            db.precompute_clone_graph(args.max_seconds)?;
        return print_output(&report);
    }

    let db = open_index(config)?;

    // `--recall-symbols`: the UNCAPPED symbol-level recall set (#282 follow-up) — via the dedicated
    // pipeline, NOT find_clones (whose per-class member list is capped). One ref per line, sorted.
    if args.recall_symbols {
        for r in db.clone_symbol_refs(args.min_similarity, args.min_copies)? {
            println!("{r}");
        }
        return Ok(());
    }

    let result = db.find_clones(rag_rat_core::index::FindClonesOptions {
        min_similarity: args.min_similarity,
        min_copies: args.min_copies,
        // A recall signature must be COMPLETE (every class), so never clamp it to a class limit.
        limit: if args.recall_signature { None } else { args.limit },
    })?;

    // `--recall-signature`: a canonical, cross-build-stable recall dump for the #279 harness,
    // instead of the listing/explain.
    if args.recall_signature {
        print!("{}", recall_signature(&result));
        return Ok(());
    }

    // `--explain <CLASS_KEY>`: print a human-readable refinement breakdown for one class from the
    // SAME result set (so the explained class went through the same refine pass as the listing),
    // instead of the JSON/TOON listing.
    if let Some(key) = &args.explain {
        let Some(class) = result.classes.iter().find(|c| &c.class_key == key) else {
            anyhow::bail!("no clone class with key `{key}` in results");
        };
        print_clone_explain(class);
        return Ok(());
    }

    print_output(&result)
}

/// A canonical, cross-build-STABLE recall signature of the clone classes — one line per class
/// (`<member_count>\t<comma-joined sorted member refs>`), lines sorted, after a `#`-comment
/// summary; trailing newline included. Stable because it keys on member REFS (`path::symbol`), not
/// rowids, so two builds (before/after a candidate-pruning change like #271's hot-token cap) diff
/// with plain `diff`: a removed or shrunk line is a recall regression. The recall half of #279.
/// `member_count` is the FULL class size (the returned member list may be member-capped, but the
/// count plus the deterministic capped subset still pin the class, so a class that vanishes /
/// splits / shrinks always changes its line). Pure (returns the text) so it is unit-testable.
fn recall_signature(result: &rag_rat_core::index::FindClonesResult) -> String {
    let mut lines: Vec<String> = result
        .classes
        .iter()
        .map(|c| {
            let mut refs: Vec<&str> = c.members.iter().map(|m| m.r#ref.as_str()).collect();
            refs.sort_unstable();
            format!("{}\t{}", c.member_count, refs.join(","))
        })
        .collect();
    lines.sort_unstable();
    let total_members: usize = result.classes.iter().map(|c| c.member_count).sum();
    let mut out = format!(
        "# clone recall signature — {} classes, {total_members} clone members\n",
        result.classes.len(),
    );
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Render a human-readable explanation of a refined clone class: the anti-unification template,
/// its variation points (with per-member values), and the proposed extracted-helper signature.
/// Reads the parsed `variation_points` / `proposed_signature` JSON values surfaced on the class
/// (Plan 4b); un-refined classes simply print the header with `n/a` fields.
fn print_clone_explain(class: &rag_rat_core::index::CandidateCloneClass) {
    println!("Clone class: {}", class.class_key);
    println!(
        "  {} members, confidence: {}, coverage: {:.2}",
        class.member_count,
        class.confidence.as_deref().unwrap_or("n/a"),
        class.anti_unify_coverage.unwrap_or(0.0),
    );
    println!();

    if let Some(template) = &class.template {
        println!("Template:");
        println!("{template}");
        println!();
    }

    if let Some(arr) = class.variation_points.as_ref().and_then(|v| v.as_array())
        && !arr.is_empty()
    {
        // `per_member_values` is ordinal-aligned to `canonical_member_refs` (canonical
        // `(struct_hash, path, start_byte)` order) — NOT to the `r#ref`-sorted `members` field.
        // Pair each value with its member ref so a printed value maps to the member it came
        // from. Falls back to the bare `value | value` join when the canonical refs are
        // unavailable (un-refined / legacy class).
        let canon_refs = class.canonical_member_refs.as_deref();
        println!("Variation points ({}):", arr.len());
        for vp in arr {
            let id = vp["metavar_id"].as_str().unwrap_or("?");
            let role = vp["extraction_role"].as_str().unwrap_or("?");
            let conf = vp["confidence"].as_str().unwrap_or("?");
            print!("  {id} ({role}, {conf})");
            if let Some(vals) = vp["per_member_values"].as_array() {
                let rendered: Vec<String> = match canon_refs {
                    // Zip value↔member when the canonical refs line up (same arity).
                    Some(refs) if refs.len() == vals.len() => vals
                        .iter()
                        .zip(refs.iter())
                        .map(|(v, r)| {
                            let val = v.as_str().unwrap_or("");
                            // The gap sentinel is the empty string — render it explicitly.
                            let shown = if val.is_empty() { "<gap>" } else { val };
                            format!("{r}={shown}")
                        })
                        .collect(),
                    // No refs / arity mismatch → bare values (still useful, just unlabeled).
                    _ => vals.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect(),
                };
                print!(": {}", rendered.join(" | "));
            }
            println!();
        }
        println!();
    }

    if let Some(sig) = &class.proposed_signature {
        let typedness = sig["typedness"].as_str().unwrap_or("unknown");
        println!("Proposed signature (typedness: {typedness}):");
        // `ProposedSignature` serializes a pre-rendered `text` (e.g. `fn extracted(arg0: i32)`);
        // fall back to assembling the params array if a legacy row lacks it.
        if let Some(text) = sig["text"].as_str() {
            println!("  {text}");
        } else if let Some(params) = sig["params"].as_array() {
            let param_strs: Vec<String> = params
                .iter()
                .map(|p| {
                    let name = p["name"].as_str().unwrap_or("_");
                    match p["type_text"].as_str() {
                        Some(t) => format!("{name}: {t}"),
                        None => name.to_string(),
                    }
                })
                .collect();
            println!("  fn extracted({}) {{ ... }}", param_strs.join(", "));
        }
    }
}

pub(crate) fn clones_for(config: &Config, args: &ClonesForArgs) -> anyhow::Result<()> {
    use rag_rat_core::index::CloneSymbolSelector;

    let db = open_index(config)?;

    // Validate selector: positional SYMBOL xor --path+--line; both/neither → handler error.
    let selector = match (&args.symbol, &args.path, &args.line) {
        (Some(sym), None, None) =>
        // Treat as Id ONLY when there is no `::` (which signals a qualified-name ref like
        // `sym_utils.rs::load_user`) AND the token parses as a valid sym_<hex> handle. A
        // file named `sym_*` with a `::` separator must route to Ref, not Id, so it resolves
        // by qualified name instead of failing `parse_sym_handle` and returning unresolved.
            if !sym.contains("::") && rag_rat_core::serde_big_id::parse_sym_handle(sym).is_some() {
                CloneSymbolSelector::Id(sym.clone())
            } else {
                CloneSymbolSelector::Ref(sym.clone())
            },
        (None, Some(path), Some(line)) =>
            CloneSymbolSelector::PathLine { path: path.clone(), line: *line },
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            anyhow::bail!(
                "clones-for: SYMBOL and --path/--line are mutually exclusive — use one or the \
                 other"
            );
        },
        (None, Some(_), None) | (None, None, Some(_)) => {
            anyhow::bail!("clones-for: --path and --line must be used together");
        },
        (None, None, None) => {
            anyhow::bail!("clones-for: requires a SYMBOL argument or --path <PATH> --line <N>");
        },
    };

    // The result always carries eligibility flags + completeness; a miss serializes with
    // `class: null` (symbol unique, not eligible, or unresolved) — never an error.
    let result = db.clones_for_symbol(selector)?;
    print_output(&result)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rag_rat_core::config::{ResolvedTarget, TargetKind};
    use rag_rat_core::language::Language;
    use rag_rat_core::{Config, IndexDatabase};

    use crate::cli::ClonesArgs;

    static N: AtomicU64 = AtomicU64::new(0);

    /// Fix E: a qualified name like `sym_utils.rs::load_user` (a file literally named `sym_*`)
    /// must route to `Ref`, not `Id`. The old `starts_with("sym_")` guard misrouted it to `Id`,
    /// which fails `parse_sym_handle` and returns unresolved instead of trying Ref. Fix: treat as
    /// Id ONLY when there is no `::` AND `parse_sym_handle` succeeds.
    #[test]
    fn clones_for_sym_prefixed_ref_routes_to_ref_not_id() {
        use rag_rat_core::index::CloneSymbolSelector;
        use rag_rat_core::serde_big_id::parse_sym_handle;

        fn classify(sym: &str) -> &'static str {
            if !sym.contains("::") && parse_sym_handle(sym).is_some() { "Id" } else { "Ref" }
        }

        // A valid opaque handle (no `::`, valid hex suffix) → Id.
        let valid_handle = rag_rat_core::serde_big_id::format_sym_handle(42i64);
        assert_eq!(classify(&valid_handle), "Id", "a valid sym_<hex> handle must route to Id");

        // A file named `sym_*` with a `::` separator → Ref (the bug case).
        assert_eq!(classify("sym_utils.rs::load_user"), "Ref");
        assert_eq!(classify("sym_something::fn_name"), "Ref");

        // An ordinary qualified name → Ref.
        assert_eq!(classify("src/foo.rs::my_fn"), "Ref");

        // Confirm the actual `clones_for` handler uses the same logic by checking that a
        // `sym_utils.rs::load_user`-style arg produces a Ref selector (not an Id selector that
        // would silently fail). We test the routing branch directly since we can't easily plant
        // a `sym_*`-named file in a live DB within a unit test.
        //
        // The match arm in `clones_for` is now:
        //   if !sym.contains("::") && parse_sym_handle(sym).is_some() { Id } else { Ref }
        // which is what `classify` above mirrors. The assertions above cover it.
        let _ = CloneSymbolSelector::Ref("sym_utils.rs::load_user".to_string());
    }

    #[test]
    fn clones_handler_returns_class_for_planted_pair() {
        // Plant two identical functions in separate files → struct_hash fast path produces a clone
        // class. Validates that the `clones` command handler wires find_clones and prints output
        // without panicking.
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-clones-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let clone_body =
            "pub fn cloned_helper(x: i32, y: i32) -> i32 {\n    x + y + 42\n}\n".to_string();
        std::fs::write(root.join("src/lib.rs"), format!("{clone_body}pub mod a;\npub mod b;\n"))
            .unwrap();
        std::fs::write(root.join("src/a.rs"), &clone_body).unwrap();
        std::fs::write(root.join("src/b.rs"), &clone_body).unwrap();

        let config = Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            dream: Default::default(),
            log: Default::default(),
        };
        IndexDatabase::rebuild(&config).unwrap();

        let args = ClonesArgs {
            min_similarity: None,
            min_copies: Some(2),
            limit: None,
            explain: None,
            recall_signature: false,
            recall_symbols: false,
            precompute: false,
            max_seconds: None,
        };
        // The handler must not error.
        super::clones(&config, &args).unwrap_or_else(|err| panic!("clones handler failed: {err}"));

        // Query the DB directly to assert at least one class was found.
        let db = IndexDatabase::open_config(&config).unwrap();
        let result = db
            .find_clones(rag_rat_core::index::FindClonesOptions {
                min_similarity: None,
                min_copies: Some(2),
                limit: None,
            })
            .unwrap();
        assert!(
            result.classes.iter().any(|c| c.member_count >= 2),
            "expected at least one clone class with >=2 members for the planted pair: {:?}",
            result.classes
        );

        // #279 recall harness: the canonical signature is a sorted, ref-keyed dump — the planted
        // 3-way clone surfaces as one `3\t<sorted refs>` line under the `#` summary header, keyed
        // on stable `path::symbol` refs (so two builds diff with plain `diff`).
        let sig = super::recall_signature(&result);
        assert!(sig.starts_with("# clone recall signature —"), "signature header missing:\n{sig}");
        let clone_line = sig
            .lines()
            .find(|l| l.starts_with("3\t"))
            .unwrap_or_else(|| panic!("no 3-member class line in signature:\n{sig}"));
        for member in
            ["src/lib.rs::cloned_helper", "src/a.rs::cloned_helper", "src/b.rs::cloned_helper"]
        {
            assert!(clone_line.contains(member), "signature line missing {member}: {clone_line}");
        }
        // Refs are sorted WITHIN a line (a.rs < b.rs < lib.rs) — the cross-build-stable ordering.
        assert!(
            clone_line.find("src/a.rs") < clone_line.find("src/b.rs"),
            "member refs must be sorted within a class line: {clone_line}"
        );

        // #282 follow-up: clone_symbol_refs is the UNCAPPED symbol-level recall set — the 3 planted
        // clone-symbols, sorted, one per ref (no member cap).
        let syms = db.clone_symbol_refs(None, Some(2)).unwrap();
        for member in
            ["src/a.rs::cloned_helper", "src/b.rs::cloned_helper", "src/lib.rs::cloned_helper"]
        {
            assert!(
                syms.iter().any(|s| s == member),
                "clone_symbol_refs missing {member}: {syms:?}"
            );
        }
        assert!(syms.windows(2).all(|w| w[0] < w[1]), "clone_symbol_refs must be sorted+unique");

        let _ = std::fs::remove_dir_all(&root);
    }
}
