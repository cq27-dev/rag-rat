use super::*;

/// The main checkout: two `run` overloads sharing `src/lib.rs::run`, each reaching its own leaf,
/// plus `alpha_leaf`, which the branch below leaves byte-identical.
const MAIN_SOURCE: &str = r#"
pub struct Alpha;
pub struct Beta;

pub fn alpha_leaf() {}
pub fn beta_leaf() {}

impl Alpha {
    pub fn run(&self) { alpha_leaf(); }
}

impl Beta {
    pub fn run(&self, extra: i64) { beta_leaf(); let _ = extra; }
}
"#;

/// The linked checkout: `Beta` is gone, `Alpha::run` reaches a different leaf, and `alpha_leaf` is
/// carried over unchanged — so one symbol is shared between the checkouts, one is re-declared into
/// its own logical symbol, and one exists in each checkout alone.
const BRANCH_SOURCE: &str = r#"
pub struct Alpha;

pub fn alpha_leaf() {}
pub fn gamma_leaf() {}

impl Alpha {
    pub fn run(&self) { gamma_leaf(); }
}
"#;

/// A `sym_<hex>` handle is scoped to the checkout that minted it, and the file lane that hands it
/// out reports its reach in the same scope.
///
/// A shared database is what makes this a question at all: `logical_symbol_members` is
/// corpus-level, so a symbol carried over unchanged has ONE logical symbol with a member row per
/// checkout, and a re-declared one has a second logical symbol whose members the other checkout
/// must not see. Reading either unscoped gives an answer that looks authoritative and is wrong in
/// both directions — a handle from the sibling checkout resolving here, and a handle that covers
/// one declaration reporting two.
///
/// Only a real linked worktree exercises that: planting an out-of-scope row proves the `files` view
/// filters, but not that the overlay pass populates the rows the filter then has to separate, nor
/// that switching the active checkout back leaves the first checkout's answers intact.
#[test]
fn lens_symbol_handles_are_scoped_to_the_active_checkout() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), MAIN_SOURCE).unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.to_path_buf(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/lib.rs"), BRANCH_SOURCE).unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // The base checkout's own answers, taken before any scope switch.
    db.use_worktree_scope(&main, None).unwrap();
    let base = handles(&db);
    assert_eq!(
        base.iter().map(|(name, ..)| name.as_str()).collect::<Vec<_>>(),
        // Each type name is indexed twice — the struct and its impl block.
        ["Alpha", "Alpha", "Beta", "Beta", "alpha_leaf", "beta_leaf", "run", "run"],
        "the base scope sees the main checkout's file, not the branch's"
    );
    let base_alpha_run = handle_reaching(&db, &base, "run", "alpha_leaf");
    let base_beta_run = handle_reaching(&db, &base, "run", "beta_leaf");
    let base_shared_leaf = named(&base, "alpha_leaf");
    assert_ne!(base_alpha_run.0, base_beta_run.0, "the overloads are distinct logical symbols");

    // The shared symbol is the one that could inflate: `alpha_leaf` is byte-identical in both
    // checkouts, so its logical symbol has a member row per checkout — and exactly one of them is
    // a declaration THIS checkout has.
    assert_eq!(
        base_shared_leaf.1, 1,
        "a symbol carried over unchanged is one declaration per checkout, not two"
    );
    assert_eq!(base_alpha_run.1, 1);
    assert_eq!(
        callee_names(&db, base_alpha_run.0),
        ["alpha_leaf"],
        "the base overload reaches the leaf its own body calls"
    );
    assert_eq!(callee_names(&db, base_beta_run.0), ["beta_leaf"]);

    // The linked checkout: its own re-declared `run`, and no trace of `Beta`.
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    let branch = handles(&db);
    assert_eq!(
        branch.iter().map(|(name, ..)| name.as_str()).collect::<Vec<_>>(),
        ["Alpha", "Alpha", "alpha_leaf", "gamma_leaf", "run"],
        "the overlay scope serves the branch's file"
    );
    let branch_run = named(&branch, "run");
    assert_ne!(
        branch_run.0, base_alpha_run.0,
        "a re-declared symbol is a different logical symbol, so a different handle"
    );
    assert_eq!(branch_run.1, 1);
    assert_eq!(callee_names(&db, branch_run.0), ["gamma_leaf"]);
    assert_eq!(
        named(&branch, "alpha_leaf"),
        base_shared_leaf,
        "the shared symbol keeps one handle across checkouts, and still covers one declaration"
    );

    // ISOLATION: neither base-only handle answers here. Both name a member whose file the overlay
    // shadows or replaces, so they are absent — never an empty hop list, which reads as "this
    // symbol has no callees".
    for (label, handle) in [("Alpha::run", base_alpha_run.0), ("Beta::run", base_beta_run.0)] {
        assert!(
            db.lens_symbol_callees(&LensHopSelector::Handle(handle), 50).unwrap().is_none(),
            "the base checkout's {label} must not resolve under the linked checkout's scope"
        );
    }

    // PRESERVATION: switching back restores the base checkout's answers unchanged, and the
    // branch-only handle is now the absent one.
    db.use_worktree_scope(&main, None).unwrap();
    assert_eq!(handles(&db), base, "the overlay pass must not have moved the base checkout's rows");
    assert_eq!(callee_names(&db, base_alpha_run.0), ["alpha_leaf"]);
    assert_eq!(callee_names(&db, base_beta_run.0), ["beta_leaf"]);
    assert!(
        db.lens_symbol_callees(&LensHopSelector::Handle(branch_run.0), 50).unwrap().is_none(),
        "the linked checkout's handle must not resolve back in the base scope"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// `(name, handle, declarations)` for every `src/lib.rs` symbol in the active scope, with the graph
/// lane asserted to agree — both lanes hand out the selector a CodeLens sends back.
fn handles(db: &IndexDatabase) -> Vec<(String, i64, u64)> {
    let mut rows = db
        .lens_file_symbols("src/lib.rs")
        .unwrap()
        .symbols
        .into_iter()
        .map(|symbol| {
            (
                symbol.name,
                symbol.logical_symbol_id.expect("an indexed row carries a handle"),
                symbol.logical_symbol_declarations,
            )
        })
        .collect::<Vec<_>>();
    let mut graph = db
        .lens_file_graph("src/lib.rs")
        .unwrap()
        .symbols
        .into_iter()
        .map(|symbol| {
            (
                symbol.name,
                symbol.logical_symbol_id.expect("an indexed row carries a handle"),
                symbol.logical_symbol_declarations,
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    graph.sort();
    assert_eq!(rows, graph, "both file lanes must hand out the same handle and the same reach");
    rows
}

fn named(rows: &[(String, i64, u64)], name: &str) -> (i64, u64) {
    let matched = rows
        .iter()
        .filter(|(row_name, ..)| row_name == name)
        .map(|(_, handle, declarations)| (*handle, *declarations))
        .collect::<Vec<_>>();
    assert_eq!(matched.len(), 1, "{name} must be one row in this scope: {rows:?}");
    matched[0]
}

/// The `name` row whose own body reaches `callee` — how the two same-named overloads are told
/// apart without depending on which order the index returned them in.
fn handle_reaching(
    db: &IndexDatabase,
    rows: &[(String, i64, u64)],
    name: &str,
    callee: &str,
) -> (i64, u64) {
    let matched = rows
        .iter()
        .filter(|(row_name, ..)| row_name == name)
        .filter(|(_, handle, _)| callee_names(db, *handle).iter().any(|hop| hop == callee))
        .map(|(_, handle, declarations)| (*handle, *declarations))
        .collect::<Vec<_>>();
    assert_eq!(matched.len(), 1, "exactly one {name} may reach {callee}: {rows:?}");
    matched[0]
}

fn callee_names(db: &IndexDatabase, handle: i64) -> Vec<String> {
    let answer = db
        .lens_symbol_callees(&LensHopSelector::Handle(handle), 50)
        .unwrap()
        .expect("a handle from the active checkout resolves");
    assert_eq!(
        answer.matched_symbols, 1,
        "no fixture symbol here groups, so the answer covers one declaration"
    );
    let mut names = answer.callees.into_iter().map(|hop| hop.name).collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}
