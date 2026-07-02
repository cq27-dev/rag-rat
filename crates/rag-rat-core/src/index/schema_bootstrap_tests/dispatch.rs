use super::*;

#[test]
fn find_callers_sees_message_dispatch_via_synthetic_edge() {
    // #200: a handler reached only through an enum-message dispatch (construct a variant in one fn,
    // handle it in a `match` arm in another) has no static caller edge to the leaf. The synthesized
    // `dispatches` edge connects the constructing fn to the handler the matching arm calls, so
    // find_callers on the leaf surfaces the sender.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum MlReq {
    UpsertJournalEmbedding { id: i64 },
    Other,
}

pub fn enqueue() {
    send(MlReq::UpsertJournalEmbedding { id: 1 });
}

fn send(_req: MlReq) {}

pub fn handle(req: MlReq) {
    match req {
        MlReq::UpsertJournalEmbedding { id } => {
            log_it();
            upsert_journal_embedding(id)
        },
        MlReq::Other => {},
    }
}

pub fn log_it() {}

pub fn upsert_journal_embedding(_id: i64) {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let callers = db.find_callers("upsert_journal_embedding", 50).unwrap();
    // The direct (calls_name) caller is the dispatcher `handle`.
    assert!(
        callers.iter().any(|hop| {
            hop.edge_kind == "calls_name"
                && hop.from_symbol.as_deref().is_some_and(|s| s.ends_with("handle"))
        }),
        "missing direct handler caller: {callers:?}"
    );
    // The synthesized dispatch caller is the constructing fn `enqueue`, via the MlReq variant.
    let dispatch = callers
        .iter()
        .find(|hop| hop.edge_kind == "dispatches")
        .expect("missing synthetic dispatch edge");
    assert!(
        dispatch.from_symbol.as_deref().is_some_and(|s| s.ends_with("enqueue")),
        "dispatch edge should come from the sender: {dispatch:?}"
    );
    assert_eq!(
        dispatch.evidence.as_deref(),
        Some("MlReq::UpsertJournalEmbedding"),
        "dispatch edge should record the routing variant as evidence"
    );

    // A symbol not reached by any dispatch arm gets no synthetic edge.
    let send_callers = db.find_callers("send", 50).unwrap();
    assert!(
        send_callers.iter().all(|hop| hop.edge_kind != "dispatches"),
        "no dispatch edge expected for a non-handler: {send_callers:?}"
    );

    // #200 review (P2 #4): a side-effect call earlier in the arm body (`log_it()`) is NOT the
    // routed handler — only the arm's tail delegate is. `log_it` must get no dispatch caller.
    let log_callers = db.find_callers("log_it", 50).unwrap();
    assert!(
        log_callers.iter().all(|hop| hop.edge_kind != "dispatches"),
        "an arm side-effect call must not become a dispatch target: {log_callers:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_fact_rows_are_hidden_from_the_edges_view() {
    // #200 adversarial review: the internal `dispatch_construct`/`dispatch_handle` FACT rows live
    // in `edges_data` (needed by `synthesize_dispatch_edges`) but are EXCLUDED from the `edges`
    // compatibility view, so every query-layer reader (repo_brief, clusters, grep-augment,
    // orientation, traversal, …) is structurally safe without each remembering an exclusion. The
    // synthesized `dispatches` edge IS a real edge and stays visible.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum MlReq { Upsert { id: i64 } }
pub fn enqueue() { send(MlReq::Upsert { id: 1 }); }
fn send(_r: MlReq) {}
pub fn handle(r: MlReq) {
    match r {
        MlReq::Upsert { id } => upsert(id),
    }
}
pub fn upsert(_id: i64) {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let view_count = |kind: &str| -> i64 {
        conn.query_row("SELECT COUNT(*) FROM edges WHERE edge_kind = ?1", [kind], |r| r.get(0))
            .unwrap()
    };
    let data_count = |kind: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM edges_data d JOIN name_strings ek ON ek.id = d.edge_kind_id
             WHERE ek.value = ?1",
            [kind],
            |r| r.get(0),
        )
        .unwrap()
    };

    // The FACT rows exist in the base table but are invisible through the view.
    assert!(data_count("dispatch_construct") > 0, "construct fact persisted in edges_data");
    assert!(data_count("dispatch_handle") > 0, "handle fact persisted in edges_data");
    assert_eq!(view_count("dispatch_construct"), 0, "construct fact must be hidden from the view");
    assert_eq!(view_count("dispatch_handle"), 0, "handle fact must be hidden from the view");
    // The synthesized real edge is visible through the view.
    assert!(view_count("dispatches") > 0, "the synthesized dispatches edge must stay visible");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handles_or_patterns_guards_and_let_constructs() {
    // #200 review: or-pattern arms emit a handle per variant; the delegate is the branch tail (a
    // guard/scrutinee call is never a handler); a unit variant in a `let` value position is a
    // construct.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Cmd { Start, Resume, Stop }

pub fn enqueue_start() { send(Cmd::Start); }
pub fn enqueue_resume() { send(Cmd::Resume); }
pub fn enqueue_stop() {
    let c = Cmd::Stop;
    send(c);
}
fn send(_c: Cmd) {}

pub fn handle(c: Cmd) {
    match c {
        Cmd::Start | Cmd::Resume => run_active(),
        Cmd::Stop => if should_stop() { run_stop() } else { run_active() },
    }
}

pub fn should_stop() -> bool { true }
pub fn run_active() {}
pub fn run_stop() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_senders = |symbol: &str| -> Vec<String> {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .collect()
    };
    let ends_with = |names: &[String], suffix: &str| names.iter().any(|n| n.ends_with(suffix));

    // Or-pattern: both `Cmd::Start` and `Cmd::Resume` senders dispatch to `run_active`. The
    // `Cmd::Stop` else-branch also lands on `run_active` (`enqueue_stop` constructs via a `let`).
    let active = dispatch_senders("run_active");
    assert!(ends_with(&active, "enqueue_start"), "or-pattern Start sender missing: {active:?}");
    assert!(ends_with(&active, "enqueue_resume"), "or-pattern Resume sender missing: {active:?}");
    assert!(ends_with(&active, "enqueue_stop"), "guard else-branch sender missing: {active:?}");

    // Guard if-branch: `Cmd::Stop` sender dispatches to `run_stop`.
    let stop = dispatch_senders("run_stop");
    assert!(ends_with(&stop, "enqueue_stop"), "guard if-branch sender missing: {stop:?}");

    // The guard/scrutinee call `should_stop()` is NOT a dispatch handler.
    assert!(
        dispatch_senders("should_stop").is_empty(),
        "a guard/predicate call must not become a dispatch handler"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_resolves_self_keyed_variants_per_impl() {
    // #200 adversarial review (P1): `Self::Variant` is rewritten to the enclosing impl type, so two
    // unrelated enums each writing `Self::Ripe` in construct + handle do NOT cross-link (the old
    // bare `Self::Ripe` key collapsed them). A single impl's `Self::`-keyed dispatch still
    // resolves.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Apple { Ripe }
pub enum Banana { Ripe }

fn sink<T>(_t: T) {}

impl Apple {
    pub fn enqueue_apple() { sink(Self::Ripe); }
    pub fn run_apple(self) { match self { Self::Ripe => apple_handler() } }
}

impl Banana {
    pub fn enqueue_banana() { sink(Self::Ripe); }
    pub fn run_banana(self) { match self { Self::Ripe => banana_handler() } }
}

pub fn apple_handler() {}
pub fn banana_handler() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_senders = |symbol: &str| -> Vec<String> {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .collect()
    };

    // `Self::Ripe` in `impl Apple` keys as `Apple::Ripe` (recall preserved) and does NOT reach the
    // Banana handler (no cross-enum collapse).
    let apple = dispatch_senders("apple_handler");
    assert!(
        apple.iter().any(|s| s.ends_with("enqueue_apple")),
        "self-keyed dispatch lost: {apple:?}"
    );
    let banana = dispatch_senders("banana_handler");
    assert!(
        banana.iter().any(|s| s.ends_with("enqueue_banana")),
        "self-keyed dispatch lost: {banana:?}"
    );
    assert!(
        apple.iter().all(|s| !s.ends_with("enqueue_banana"))
            && banana.iter().all(|s| !s.ends_with("enqueue_apple")),
        "Self::Variant must not cross-link distinct enums: apple={apple:?} banana={banana:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handles_await_tails_and_external_enum_heads() {
    // #200 review: an `.await` tail still resolves the handler; and an enum head with no LOCAL
    // definition (an imported/aliased enum) is admitted, not skipped, since both sender and handler
    // write the same head.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Job { Run }

pub async fn enqueue() { dispatch(Job::Run); }
async fn dispatch(_j: Job) {}
pub async fn handle(j: Job) {
    match j {
        Job::Run => run_job().await,
    }
}
pub async fn run_job() {}

pub fn emit() { ship(Status::Ready); }
fn ship(_s: Status) {}
pub fn route(s: Status) {
    match s {
        Status::Ready => deliver(),
    }
}
pub fn deliver() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from = |symbol: &str, sender: &str| {
        db.find_callers(symbol, 50).unwrap().iter().any(|hop| {
            hop.edge_kind == "dispatches"
                && hop.from_symbol.as_deref().is_some_and(|s| s.ends_with(sender))
        })
    };

    // `.await` tail: `Job::Run => run_job().await` still binds `run_job`.
    assert!(dispatch_from("run_job", "enqueue"), "await tail handler missing a dispatch caller");
    // External/aliased head (`Status` has no local enum definition) is admitted.
    assert!(dispatch_from("deliver", "emit"), "external enum-head dispatch missing");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_binds_let_bound_handler_under_a_wrapper_tail() {
    // #207: the real-world actor idiom delegates in a `let` binding and returns a wrapper —
    // `MlReq::EmbedText { .. } => { let v = embed_text(..)?; Ok(Resp::Embedded(v)) }`. The handler
    // is the let-bound `embed_text`, NOT the tail `Ok(..)` wrapper; the variant is constructed
    // elsewhere (a generic `call` helper sends it). The dispatch edge must reach `embed_text`,
    // not `Ok`.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum MlReq { EmbedText { text: String }, Other }
pub enum Resp { Embedded(i32), Empty }

pub fn enqueue(text: String) { call(MlReq::EmbedText { text }); }
fn call(_req: MlReq) {}

pub fn handle(req: MlReq) -> Result<Resp, ()> {
    match req {
        MlReq::EmbedText { text } => {
            let vector = embed_text(text)?;
            Ok(Resp::Embedded(vector))
        }
        MlReq::Other => Ok(Resp::Empty),
    }
}

fn embed_text(_text: String) -> Result<i32, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_senders = |symbol: &str| -> Vec<String> {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .collect()
    };

    // The let-bound handler `embed_text` is reached from the sender via the dispatch edge.
    let embed = dispatch_senders("embed_text");
    assert!(
        embed.iter().any(|s| s.ends_with("enqueue")),
        "let-bound handler must get a dispatch caller: {embed:?}"
    );
    // The `Ok(..)` / `Resp::Embedded(..)` wrapper constructors are NOT dispatch handlers.
    assert!(
        dispatch_senders("Embedded").is_empty(),
        "a wrapper constructor must not be a dispatch handler"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_selection_distinguishes_handlers_from_wrappers_and_setup() {
    // #208 review: the arm handler is the call producing the result. Keep real calls (incl. FFI/
    // codegen PascalCase fns); never bind a `Result`/`Option` wrapper, a response constructor under
    // a wrapper, or a setup `let` whose binding the tail never references.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Build { x: u8 }, Open { p: u8 }, Direct { y: u8 }, Empty, Setup { z: u8 } }
pub enum Resp { Wrapped(u8), Blank }
impl Resp { fn empty() -> Resp { Resp::Blank } }

pub fn enqueue() {
    send(Msg::Build { x: 1 });
    send(Msg::Open { p: 2 });
    send(Msg::Direct { y: 3 });
    send(Msg::Empty);
    send(Msg::Setup { z: 4 });
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) -> Result<Resp, ()> {
    match m {
        Msg::Build { x } => {
            let result = build_it(x)?;          // let-bound handler under a wrapper tail (#207)
            Ok(Resp::Wrapped(result))           // Resp::Wrapped is a constructor, not a handler
        }
        Msg::Open { p } => CreateFileW(p),      // PascalCase FFI fn — must stay a handler (#208)
        Msg::Direct { y } => handle_direct(y),  // plain tail handler
        Msg::Empty => Ok(Resp::empty()),        // response ctor under wrapper — NO handler (#208)
        Msg::Setup { z } => {
            let _guard = start_span(z);          // setup let not referenced by the tail (#208)
            run_setup()
        }
    }
}
fn build_it(_x: u8) -> Result<u8, ()> { Ok(0) }
fn CreateFileW(_p: u8) -> Result<Resp, ()> { Ok(Resp::Blank) }
fn handle_direct(_y: u8) -> Result<Resp, ()> { Ok(Resp::Blank) }
fn start_span(_z: u8) {}
fn run_setup() -> Result<Resp, ()> { Ok(Resp::Blank) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatches_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Handlers that MUST be reached: the let-bound one (through the `Resp::Wrapped` constructor),
    // the plain tail, and the setup arm's actual tail.
    for handler in ["build_it", "handle_direct", "run_setup"] {
        assert!(dispatches_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Non-handlers: a response ctor under a wrapper, a setup `let` the tail never reads, and the
    // bare PascalCase `CreateFileW` — indistinguishable from a tuple-struct ctor, so it reads as a
    // wrapper (traced through, not recorded) rather than risk crediting a ctor (accepted recall,
    // #208 review round 10).
    for non_handler in ["empty", "start_span", "CreateFileW"] {
        assert!(
            !dispatches_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_follows_result_dataflow_not_textual_names() {
    // #208 review round 2: the handler is the call whose result becomes the arm response, traced
    // through wrappers/constructors and `let` bindings. Covers per-branch let-feeds, shadowing,
    // condition-only lets, let-bound response constructors, and struct field-label collisions.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Branch, Shadow, Cond, LetCtor, Field }
pub enum Resp { A, B, Wrap(u8) }
impl Resp { fn empty() -> Resp { Resp::A } }
pub struct Out { status: u8 }

pub fn enqueue() {
    send(Msg::Branch);
    send(Msg::Shadow);
    send(Msg::Cond);
    send(Msg::LetCtor);
    send(Msg::Field);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // per-branch: slow_path feeds the else wrapper branch; fast_path is the if branch.
        Msg::Branch => { let r = slow_path()?; if cond() { fast_path() } else { Ok(Resp::Wrap(r)) } }
        // shadowing: only the last `r` (second_handler) feeds the tail.
        Msg::Shadow => { let r = first_handler()?; let r = second_handler(r)?; Ok(Resp::Wrap(r)) }
        // condition-only: check_ready feeds only the if condition, not the result.
        Msg::Cond => { let ready = check_ready(); if ready { Ok(Resp::A) } else { Ok(Resp::B) } }
        // let-bound response constructor: not a handler.
        Msg::LetCtor => { let resp = Resp::empty(); Ok(resp) }
        // struct field LABEL `status` collides with the setup binding name `status`.
        Msg::Field => { let status = start_span(); Ok(Out { status: 0 }) }
    }
}
fn slow_path() -> Result<u8, ()> { Ok(0) }
fn fast_path() -> Result<Resp, ()> { Ok(Resp::A) }
fn cond() -> bool { true }
fn first_handler() -> Result<u8, ()> { Ok(0) }
fn second_handler(_r: u8) -> Result<u8, ()> { Ok(0) }
fn check_ready() -> bool { true }
fn start_span() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatches_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: both branches of a mixed tail, and the in-scope (shadowing) binding.
    for handler in ["slow_path", "fast_path", "second_handler"] {
        assert!(dispatches_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Not reached: a shadowed binding, a condition-only let, a let-bound response constructor, and
    // a setup binding that only collides with a struct field label.
    for non_handler in ["first_handler", "cond", "check_ready", "empty", "start_span"] {
        assert!(
            !dispatches_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_handles_scope_and_turbofish_edge_cases() {
    // #208 review round 3: declaration-ordered binding resolution (shadowing, self-wrapping,
    // reassignment), match-arm pattern masking, let-pattern field labels, turbofish stripping, and
    // wrapper payload containers.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Nested, SelfWrap, FieldLet, Turbo, TurboCtor, Tuple, Reassign }
pub enum Resp { Wrap(u8), Empty }
impl Resp { fn empty() -> Resp { Resp::Empty } }
pub struct Out { status: u8 }

pub fn enqueue() {
    send(Msg::Nested);
    send(Msg::SelfWrap);
    send(Msg::FieldLet);
    send(Msg::Turbo);
    send(Msg::TurboCtor);
    send(Msg::Tuple);
    send(Msg::Reassign);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // nested match arm binding `value` (the Some payload) must not resolve to the outer let.
        Msg::Nested => {
            let value = start_span();
            match maybe() { Some(value) => Ok(Resp::Wrap(value)), _ => Ok(Resp::Empty) }
        }
        // declaration-time scope: the inner `Ok(r)` reads the FIRST `r` (load).
        Msg::SelfWrap => { let r = load()?; let r = Ok(r)?; Ok(Resp::Wrap(r)) }
        // a destructuring field label `status` must not shadow the outer `status` binding.
        Msg::FieldLet => {
            let status = build_status()?;
            let Out { status: other } = read_out()?;
            Ok(Resp::Wrap(status))
        }
        // turbofish on a bare fn stays a handler.
        Msg::Turbo => CreateThing::<u8>(),
        // turbofish on a constructor stays a constructor (no handler).
        Msg::TurboCtor => Ok(Resp::<u8>::empty()),
        // a wrapper payload tuple passes the handler result through.
        Msg::Tuple => { let v = produce()?; Ok((v, 0)) }
        // reassignment: the returned value is the LATEST assignment.
        Msg::Reassign => { let mut resp = start_response()?; resp = finish_response()?; Ok(resp) }
    }
}
fn start_span() {}
fn maybe() -> Option<u8> { None }
fn load() -> Result<u8, ()> { Ok(0) }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn read_out() -> Result<Out, ()> { Ok(Out { status: 0 }) }
fn CreateThing() -> Result<Resp, ()> { Ok(Resp::Empty) }
fn produce() -> Result<u8, ()> { Ok(0) }
fn start_response() -> Result<u8, ()> { Ok(0) }
fn finish_response() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatches_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the prior shadowed binding (load), and the outer field-let binding (build_status).
    for handler in ["load", "build_status"] {
        assert!(dispatches_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Not reached: a setup masked by a match-arm payload, a destructured-away initializer, a
    // turbofished constructor, a reassigning arm's BOTH producers (the arm bails), `produce` (its
    // `Ok((v, 0))` is a MULTI-element tuple), and the bare PascalCase `CreateThing` (reads as a
    // wrapper, not a handler — accepted recall, #208 review round 10).
    for non_handler in [
        "start_span",
        "read_out",
        "empty",
        "start_response",
        "finish_response",
        "produce",
        "CreateThing",
    ] {
        assert!(
            !dispatches_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_handles_destructuring_masking_and_control_flow() {
    // #208 review round 4: a destructuring `let` overwrites a stale binding; a struct-pattern field
    // LABEL doesn't mask an outer binding; a turbofish with a path type-arg stays a handler; and a
    // binding reassigned inside control flow is invalidated (no stale-setup edge).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Destructure, MatchLabel, TurboPath, CfgReassign }
pub enum Resp { Wrap(u8), Empty }
pub struct Out { status: u8 }

pub fn enqueue() {
    send(Msg::Destructure);
    send(Msg::MatchLabel);
    send(Msg::TurboPath);
    send(Msg::CfgReassign);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // the destructuring let overwrites the stale `status`; the value comes from read_out.
        Msg::Destructure => {
            let status = start_span()?;
            let Out { status } = read_out()?;
            Ok(Resp::Wrap(status))
        }
        // a struct-pattern field LABEL `status` must not mask the outer `status` binding.
        Msg::MatchLabel => {
            let status = build_status()?;
            match out() { Out { status: other } => Ok(Resp::Wrap(status)), _ => Ok(Resp::Empty) }
        }
        // a turbofish whose type argument is itself a `::` path stays a handler.
        Msg::TurboPath => OpenThing::<some::Handle>(),
        // a binding reassigned inside control flow is invalidated (no stale-setup edge).
        Msg::CfgReassign => {
            let mut resp = start_response()?;
            if cond() { resp = finish_a()?; } else { resp = finish_b()?; }
            Ok(resp)
        }
    }
}
fn start_span() -> Result<u8, ()> { Ok(0) }
fn read_out() -> Result<Out, ()> { Ok(Out { status: 0 }) }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn out() -> Out { Out { status: 0 } }
fn OpenThing() -> Result<Resp, ()> { Ok(Resp::Empty) }
fn cond() -> bool { true }
fn start_response() -> Result<u8, ()> { Ok(0) }
fn finish_a() -> Result<u8, ()> { Ok(0) }
fn finish_b() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatches_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the outer binding the match-arm label can't mask.
    assert!(dispatches_from_enqueue("build_status"), "build_status must be a dispatch handler");
    // Not reached: a setup overwritten by a destructuring let (and its producer `read_out`), a
    // control-flow reassignment (its arm bails), and the bare PascalCase `OpenThing` (reads as a
    // wrapper, not a handler — accepted recall, #208 review round 10).
    for non_handler in ["start_span", "read_out", "start_response", "OpenThing"] {
        assert!(
            !dispatches_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_handles_guards_typed_wrappers_scrutinee_and_shadow() {
    // #208 review round 5: guards excluded from masking; turbofished `Ok::<T,E>` recognized as a
    // wrapper; match-payload bindings inherit the scrutinee producer; assignments hidden in a `let`
    // initializer invalidate; inner-block shadowing doesn't invalidate the outer binding; a
    // multi-binding destructure doesn't credit every producer; const-generic call names resolve.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Guard, TypedOk, MatchPayload, LetInitAssign, InnerShadow, TupleDestr }
pub enum Resp { Wrap(u8), Empty }
pub struct Out { other: u8 }
pub struct Grid;
impl Grid { fn assemble<const N: usize>() -> u8 { 0 } }

pub fn enqueue() {
    send(Msg::Guard);
    send(Msg::TypedOk);
    send(Msg::MatchPayload);
    send(Msg::LetInitAssign);
    send(Msg::InnerShadow);
    send(Msg::TupleDestr);
}
fn send(_m: Msg) {}
pub fn const_caller() { Grid::<{ 2 << 1 }>::assemble::<3>(); }

pub fn handle(m: Msg) {
    match m {
        // a guard's `status` read must not be masked as a binding; build_status survives.
        Msg::Guard => {
            let status = build_status()?;
            match out() { Out { other } if status > 0 => Ok(Resp::Wrap(status)), _ => Ok(Resp::Empty) }
        }
        // a typed `Ok::<Resp, ()>` is a wrapper — traced through to the let-fed compute.
        Msg::TypedOk => { let v = compute()?; Ok::<Resp, ()>(Resp::Wrap(v)) }
        // a returned match payload inherits the scrutinee producer (load).
        Msg::MatchPayload => {
            let r = load()?;
            match r { Some(v) => Ok(Resp::Wrap(v)), _ => Ok(Resp::Empty) }
        }
        // an assignment hidden in a `let` initializer invalidates resp (no stale `start`).
        Msg::LetInitAssign => {
            let mut resp = start()?;
            let _ = { resp = finish()?; };
            Ok(resp)
        }
        // an inner-block shadow reassignment must not invalidate the outer `built`.
        Msg::InnerShadow => {
            let built = build()?;
            { let mut built = 0; built = 1; }
            Ok(Resp::Wrap(built))
        }
        // a multi-binding destructure must not credit `start_span` to `resp`.
        Msg::TupleDestr => {
            let (resp, _span) = (build_resp()?, start_span());
            Ok(Resp::Wrap(resp))
        }
    }
}
fn out() -> Out { Out { other: 0 } }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn compute() -> Result<u8, ()> { Ok(0) }
fn load() -> Result<Option<u8>, ()> { Ok(None) }
fn start() -> Result<u8, ()> { Ok(0) }
fn finish() -> Result<u8, ()> { Ok(0) }
fn build() -> Result<u8, ()> { Ok(0) }
fn build_resp() -> Result<u8, ()> { Ok(0) }
fn start_span() -> u8 { 0 }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };
    let called_by = |symbol: &str, caller: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with(caller))
    };

    for handler in ["build_status", "compute", "load"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // `build` (InnerShadow arm) now bails: the arm reassigns a local (`built = 1`), so the whole
    // arm is conservatively dropped rather than tracking shadow-aware scopes (accepted recall,
    // #208).
    for non_handler in ["start", "start_span", "build"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }
    // Const-generic turbofish: the call names the function, not the type — `assemble` has a caller.
    assert!(
        called_by("assemble", "const_caller"),
        "const-generic call must resolve to the callee name"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_handles_multibind_ufcs_field_and_path_qualifiers() {
    // #208 review round 6: multi-binding match scrutinee doesn't credit every producer; a
    // reassignment in an assignment RHS invalidates; UFCS is a constructor; pre-shadow assignments
    // target the outer binding; field projections trace their receiver; a module-qualified
    // pattern's qualifier doesn't mask an outer binding.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { MultiScrut, RhsReassign, Ufcs, ShadowOrder, FieldProj, PathQual }
pub enum Resp { Wrap(u8), Empty }
pub struct Thing { id: u8 }
pub enum E { Ready(u8) }

pub fn enqueue() {
    send(Msg::MultiScrut);
    send(Msg::RhsReassign);
    send(Msg::Ufcs);
    send(Msg::ShadowOrder);
    send(Msg::FieldProj);
    send(Msg::PathQual);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // a multi-binding match scrutinee must not credit start_span to resp.
        Msg::MultiScrut => match (build_resp()?, start_span()) { (resp, _span) => Ok(Resp::Wrap(resp)) },
        // a reassignment hidden in an assignment RHS invalidates resp (no stale start_a).
        Msg::RhsReassign => { let mut resp = start_a()?; other = { resp = finish_a()?; 1 }; Ok(resp) }
        // a UFCS associated call is a constructor, not a handler.
        Msg::Ufcs => Ok(<Resp as Default>::default()),
        // a pre-shadow assignment targets the outer resp → invalidate (no stale start_b).
        Msg::ShadowOrder => { let mut resp = start_b()?; { resp = finish_b()?; let resp = 0; } Ok(resp) }
        // a field projection of a result binding traces back to its producer.
        Msg::FieldProj => { let r = build()?; Ok(Resp::Wrap(r.id)) }
        // a module-qualified pattern's qualifier must not mask the outer binding.
        Msg::PathQual => {
            let status = build_status()?;
            match e() { status::Ready(v) => Ok(Resp::Wrap(status)), _ => Ok(Resp::Empty) }
        }
    }
}
fn build_resp() -> Result<u8, ()> { Ok(0) }
fn start_span() -> u8 { 0 }
fn start_a() -> Result<u8, ()> { Ok(0) }
fn finish_a() -> Result<u8, ()> { Ok(0) }
fn start_b() -> Result<u8, ()> { Ok(0) }
fn finish_b() -> Result<u8, ()> { Ok(0) }
fn build() -> Result<Thing, ()> { Ok(Thing { id: 0 }) }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn e() -> E { E::Ready(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    for handler in ["build", "build_status"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    for non_handler in ["start_span", "start_a", "default", "start_b"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_index_cast_orpattern_and_rebind_bail() {
    // #208 review round 7 (after the conservative restructure): index projections trace only the
    // receiver; casts trace the operand; or-pattern payloads inherit the scrutinee; and any arm
    // that rebinds a local (destructuring-assign, closure reassignment) or destructures a
    // discarded producer bails / invalidates rather than emitting a stale or false edge.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { IndexProj, OrPat, CastProj, DestrIgnore, DestrAssign, ClosureReassign }
pub enum Resp { Wrap(u8), Empty }
pub enum Sig { A(u8), B(u8) }

pub fn enqueue() {
    send(Msg::IndexProj);
    send(Msg::OrPat);
    send(Msg::CastProj);
    send(Msg::DestrIgnore);
    send(Msg::DestrAssign);
    send(Msg::ClosureReassign);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // an index projection traces only the receiver, not the index expression.
        Msg::IndexProj => { let r = build_idx()?; Ok(Resp::Wrap(r[choose_index()])) }
        // an or-pattern's repeated payload binding inherits the scrutinee producer.
        Msg::OrPat => match get() { Sig::A(v) | Sig::B(v) => Ok(Resp::Wrap(v)), },
        // a cast of a returned binding traces the operand.
        Msg::CastProj => { let v = build_cast()?; Ok(Resp::Wrap(v as u32)) }
        // a destructure that discards a producer doesn't credit it.
        Msg::DestrIgnore => { let (resp, _) = (build_d()?, start_span()); Ok(Resp::Wrap(resp)) }
        // a destructuring assignment rebinds a local → the arm bails.
        Msg::DestrAssign => { let mut resp = start_da()?; (resp, _) = (finish_da()?, 0); Ok(resp) }
        // a reassignment inside a closure rebinds a local → the arm bails.
        Msg::ClosureReassign => {
            let resp = build_c()?;
            let _f = || { resp = finish_c()?; };
            Ok(Resp::Wrap(resp))
        }
    }
}
fn build_idx() -> Result<u8, ()> { Ok(0) }
fn choose_index() -> usize { 0 }
fn get() -> Sig { Sig::A(0) }
fn build_cast() -> Result<u8, ()> { Ok(0) }
fn build_d() -> Result<u8, ()> { Ok(0) }
fn start_span() -> u8 { 0 }
fn start_da() -> Result<u8, ()> { Ok(0) }
fn finish_da() -> Result<u8, ()> { Ok(0) }
fn build_c() -> Result<u8, ()> { Ok(0) }
fn finish_c() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the indexed receiver, the or-pattern scrutinee, and the cast operand's producer.
    for handler in ["build_idx", "get", "build_cast"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Not reached: an index selector, a discarded destructure producer, and producers in arms that
    // rebind a local (destructuring-assign / closure reassignment — the arm bails).
    for non_handler in ["choose_index", "start_span", "start_da", "finish_c"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_suppresses_multivalue_error_and_rebind_false_edges() {
    // #208 review round 8: the contract is "a missed edge is OK, a FALSE edge is a bug."
    // Multi-value containers (multi-arg ctor / multi-element tuple / multi-field struct), `Err`
    // payloads, multi-producer scrutinees, and reassignments via wrapped/destructuring LHS must
    // NOT synthesize a handler edge to a discarded/stale/error call. Unit-variant path
    // qualifiers and unary projections are handled too.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { MultiCtor, MultiTuple, ErrArm, ParenRebind, DerefRebind, ArrayAssign, StructAssign, PartialScrut, UnitQual, DerefProj }
pub enum Resp { Wrap(u8), Two(u8, u8), Empty }
pub struct Out { resp: u8 }
pub enum Sig { Ready }

pub fn enqueue() {
    send(Msg::MultiCtor);
    send(Msg::MultiTuple);
    send(Msg::ErrArm);
    send(Msg::ParenRebind);
    send(Msg::DerefRebind);
    send(Msg::ArrayAssign);
    send(Msg::StructAssign);
    send(Msg::PartialScrut);
    send(Msg::UnitQual);
    send(Msg::DerefProj);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // multi-arg constructor: can't attribute the response → no edge (no false `record_metric`).
        Msg::MultiCtor => Ok(Resp::Two(embed_text(), record_metric())),
        // multi-element tuple → no edge (no false `side`).
        Msg::MultiTuple => Ok((handler_a(), side())),
        // an `Err` payload is an error value, not a response handler.
        Msg::ErrArm => Err(build_error()),
        // paren-wrapped reassignment rebinds a local → arm bails (no stale `first`).
        Msg::ParenRebind => { let mut resp = first(); (resp) = second(); Ok(Resp::Wrap(resp)) }
        // deref reassignment rebinds a local → arm bails (no stale `first_d`).
        Msg::DerefRebind => { let mut v = first_d(); let p = &mut v; *p = second_d(); Ok(Resp::Wrap(v)) }
        // array destructuring assignment → arm bails (no stale `first_arr`).
        Msg::ArrayAssign => { let mut resp = first_arr(); [resp, _] = [second_arr(), 0]; Ok(Resp::Wrap(resp)) }
        // struct destructuring assignment → arm bails (no stale `first_st`).
        Msg::StructAssign => { let mut resp = first_st(); Out { resp } = make_out(); Ok(Resp::Wrap(resp)) }
        // a single binding over a MULTI-producer scrutinee tuple → no scrutinee inheritance.
        Msg::PartialScrut => match (build_p()?, start_span()) { (resp, 0) => Ok(Resp::Wrap(resp)), _ => Ok(Resp::Empty) },
        // a unit-variant path qualifier must not mask the outer binding.
        Msg::UnitQual => { let status = build_status()?; match sig() { Sig::Ready => Ok(Resp::Wrap(status)), _ => Ok(Resp::Empty) } }
        // a unary deref projection of a returned binding traces the operand.
        Msg::DerefProj => { let v = build_deref()?; Ok(Resp::Wrap(*v)) }
    }
}
fn embed_text() -> u8 { 0 }
fn record_metric() -> u8 { 0 }
fn handler_a() -> u8 { 0 }
fn side() -> u8 { 0 }
fn build_error() -> () {}
fn first() -> u8 { 0 }
fn second() -> u8 { 0 }
fn first_d() -> u8 { 0 }
fn second_d() -> u8 { 0 }
fn first_arr() -> u8 { 0 }
fn second_arr() -> u8 { 0 }
fn first_st() -> u8 { 0 }
fn make_out() -> Out { Out { resp: 0 } }
fn build_p() -> Result<u8, ()> { Ok(0) }
fn start_span() -> u8 { 0 }
fn build_status() -> Result<u8, ()> { Ok(0) }
fn sig() -> Sig { Sig::Ready }
fn build_deref() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the unit-variant-qualifier arm's outer binding, and the unary-deref projection.
    for handler in ["build_status", "build_deref"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // FALSE edges that must NOT exist: discarded multi-value siblings, an `Err` payload builder,
    // stale producers behind a wrapped/destructuring reassignment, and a multi-producer scrutinee's
    // other producer.
    for non_handler in [
        "record_metric",
        "side",
        "build_error",
        "first",
        "first_d",
        "first_arr",
        "first_st",
        "start_span",
    ] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler (false edge)"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_field_store_scoped_err_config_ctor_and_comments() {
    // #208 review round 9: a field/index store before the tail must NOT bail the arm; a scoped
    // `Result::Err(..)` must be suppressed like bare `Err`; a snake-tail associated constructor
    // (`Vec::with_capacity(config)`) must NOT be traced as a payload wrapper; and a comment in a
    // wrapper's argument list must not hide the single payload.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { FieldStore, ScopedErr, ConfigCtor, Commented }
pub enum Resp { Wrap(u8), Empty }
pub struct S { count: u8 }

pub fn enqueue() {
    send(Msg::FieldStore);
    send(Msg::ScopedErr);
    send(Msg::ConfigCtor);
    send(Msg::Commented);
}
fn send(_m: Msg) {}

pub fn handle(state: &mut S, m: Msg) {
    match m {
        // a field store before the tail is a side effect, not a rebind — `run` is still the handler.
        Msg::FieldStore => { state.count = now(); run() }
        // a scoped `Result::Err(..)` is an error wrapper — its builder is not a handler.
        Msg::ScopedErr => Result::Err(build_error()),
        // `Vec::with_capacity(n)` is a snake-tail associated ctor; `n` configures, isn't the payload.
        Msg::ConfigCtor => Ok(Vec::with_capacity(record_metric())),
        // a comment in the wrapper arg list must not hide the single payload `v`.
        Msg::Commented => { let v = handler_c()?; Ok(v /* note */) }
    }
}
fn now() -> u8 { 0 }
fn run() -> Result<Resp, ()> { Ok(Resp::Empty) }
fn build_error() -> () {}
fn record_metric() -> usize { 0 }
fn handler_c() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the let-bound payload behind a comment.
    assert!(dispatch_from_enqueue("handler_c"), "handler_c must be a dispatch handler");
    // FALSE edges that must NOT exist: a scoped-`Err` builder, and a config arg of an associated
    // constructor. Plus `run`/`now` — a field store (`state.count = now()`) can stale a returned
    // projection, so an arm containing ANY local-mutating assignment bails entirely (accepted
    // recall, #208 review round 10).
    for non_handler in ["now", "build_error", "record_metric", "run"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_effect_only_wrappers_iflet_mut_and_adapters() {
    // #208 review round 10 + held feedback: the EFFECT-ONLY handler (`h()?; Ok(unit)`) is
    // recovered; module-qualified/bare PascalCase ctors are transparent wrappers; `if let`
    // payloads and `let mut` bindings resolve; a fire-and-forget side effect, a scoped-receiver
    // method adapter, and a field store that can stale a returned projection produce no edge.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { Effect, Fire, ModCtor, BareCtor, MutBind, IfLet, MethodAdapter, FieldProj }
pub enum MlResp { Diarized, Done }
pub enum Resp { Wrap(u8), Empty }
pub struct Bare(u8);
pub struct Out { id: u8 }
pub mod dto { pub struct Wrapped(pub u8); }

pub fn enqueue() {
    send(Msg::Effect);
    send(Msg::Fire);
    send(Msg::ModCtor);
    send(Msg::BareCtor);
    send(Msg::MutBind);
    send(Msg::IfLet);
    send(Msg::MethodAdapter);
    send(Msg::FieldProj);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // effect-only: a `?`-propagated work call + a fixed ack → the fallback records do_work.
        Msg::Effect => { do_work()?; Ok(MlResp::Diarized) }
        // a fire-and-forget side effect (no `?`) must NOT be recorded.
        Msg::Fire => { fire_and_forget(); Ok(MlResp::Done) }
        // a module-qualified tuple ctor is a transparent wrapper → traces v → dto_build.
        Msg::ModCtor => { let v = dto_build()?; Ok(dto::Wrapped(v)) }
        // a bare tuple-struct ctor is a transparent wrapper → traces v → bare_build.
        Msg::BareCtor => { let v = bare_build()?; Ok(Bare(v)) }
        // a `let mut` binding maps to its producer.
        Msg::MutBind => { let mut v = build_mut()?; Ok(Resp::Wrap(v)) }
        // an if-let payload inherits the condition value's producer.
        Msg::IfLet => if let Some(v) = load_il()? { Ok(Resp::Wrap(v)) } else { Ok(Resp::Empty) },
        // a method adapter on a scoped binding is suppressed (no false `into` edge; build_into is
        // conservatively dropped).
        Msg::MethodAdapter => { let v = build_into()?; Ok(Resp::Wrap(v.into())) }
        // a field store can stale a returned projection → the whole arm bails.
        Msg::FieldProj => { let mut r = mk()?; r.id = finish_fp()?; Ok(Resp::Wrap(r.id)) }
    }
}
fn do_work() -> Result<u8, ()> { Ok(0) }
fn fire_and_forget() {}
fn dto_build() -> Result<u8, ()> { Ok(0) }
fn bare_build() -> Result<u8, ()> { Ok(0) }
fn build_mut() -> Result<u8, ()> { Ok(0) }
fn load_il() -> Result<Option<u8>, ()> { Ok(None) }
fn build_into() -> Result<u8, ()> { Ok(0) }
fn mk() -> Result<Out, ()> { Ok(Out { id: 0 }) }
fn finish_fp() -> Result<u8, ()> { Ok(0) }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the effect-only work call, the module/bare wrapper payloads, the `let mut` binding,
    // and the if-let payload.
    for handler in ["do_work", "dto_build", "bare_build", "build_mut", "load_il"] {
        assert!(dispatch_from_enqueue(handler), "{handler} must be a dispatch handler");
    }
    // Not reached: a fire-and-forget side effect, a scoped-receiver method adapter's producer
    // (suppressed), and a field-store arm's producers (the arm bails).
    for non_handler in ["fire_and_forget", "build_into", "mk", "finish_fp"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_handler_tracing_effect_only_shadow_and_scoped_methods() {
    // #208 review round 11: the effect-only fallback records the DIRECT call, not a `let`-bound `?`
    // resolved against the final (shadowed) scope; and a method call on a scoped binding
    // (`worker.run()`) IS recorded as the handler again (a real method resolves; a pure adapter
    // `v.into()` is a std method that doesn't resolve, so it creates no edge).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Msg { ShadowFallback, Worker }
pub enum Resp { Done, Out(u8) }
pub struct W;
impl W { fn run(&self) -> Result<Resp, ()> { Ok(Resp::Done) } }

pub fn enqueue() {
    send(Msg::ShadowFallback);
    send(Msg::Worker);
}
fn send(_m: Msg) {}

pub fn handle(m: Msg) {
    match m {
        // the effect-only fallback must NOT resolve the bound `task?` against the SHADOWED final
        // scope (which maps `task` to record_metric) — `task?` isn't a direct call, so it's skipped.
        Msg::ShadowFallback => { let task = do_work(); task?; let task = record_metric(); Ok(Resp::Done) }
        // a method call on a scoped binding IS the handler (worker.run), recorded again.
        Msg::Worker => { let worker = make_worker(); worker.run() }
    }
}
fn do_work() -> Result<(), ()> { Ok(()) }
fn record_metric() -> Result<(), ()> { Ok(()) }
fn make_worker() -> W { W }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let dispatch_from_enqueue = |symbol: &str| -> bool {
        db.find_callers(symbol, 50)
            .unwrap()
            .into_iter()
            .filter(|hop| hop.edge_kind == "dispatches")
            .filter_map(|hop| hop.from_symbol)
            .any(|from| from.ends_with("enqueue"))
    };

    // Reached: the method handler on a scoped binding.
    assert!(dispatch_from_enqueue("run"), "run must be a dispatch handler");
    // Not reached: a shadowing producer the effect-only fallback must not misresolve to, and the
    // bound `task?`'s producer (a bound-`?` is not a direct call — accepted recall).
    for non_handler in ["record_metric", "do_work"] {
        assert!(
            !dispatch_from_enqueue(non_handler),
            "{non_handler} must NOT be a dispatch handler"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_ignores_nested_payload_variants() {
    // #200 review: an arm pattern `Outer::Wrapped(Inner::Start) => run()` handles `Outer::Wrapped`,
    // NOT the nested payload `Inner::Start`. A function that only constructs `Inner::Start` as data
    // must not be reported as a dispatch caller of `run`.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Outer { Wrapped(Inner) }
pub enum Inner { Start }

pub fn enqueue_outer() { send(Outer::Wrapped(Inner::Start)); }
pub fn enqueue_inner_only() { take(Inner::Start); }
fn send(_o: Outer) {}
fn take(_i: Inner) {}

pub fn handle(o: Outer) {
    match o {
        Outer::Wrapped(Inner::Start) => run(),
    }
}
pub fn run() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let senders: Vec<String> = db
        .find_callers("run", 50)
        .unwrap()
        .into_iter()
        .filter(|hop| hop.edge_kind == "dispatches")
        .filter_map(|hop| hop.from_symbol)
        .collect();
    assert!(
        senders.iter().any(|s| s.ends_with("enqueue_outer")),
        "the outer-variant sender should dispatch: {senders:?}"
    );
    assert!(
        senders.iter().all(|s| !s.ends_with("enqueue_inner_only")),
        "a nested-payload variant must not be treated as the handled variant: {senders:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dispatch_join_is_scoped_to_a_unique_enum_definition() {
    // #200 review (P2 #1): the variant key is module-stripped (`Msg::Start`), so two distinct enums
    // both named `Msg` must NOT merge — a sender of one enum's variant must not appear as a caller
    // of the other's handler. With the enum name ambiguous, the join is skipped entirely.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod a {
    pub enum Msg { Start }
    pub fn enqueue_a() { send_a(Msg::Start); }
    fn send_a(_m: Msg) {}
    pub fn handle_a(m: Msg) {
        match m {
            Msg::Start => run_a(),
        }
    }
    pub fn run_a() {}
}

pub mod b {
    pub enum Msg { Start }
    pub fn enqueue_b() { send_b(Msg::Start); }
    fn send_b(_m: Msg) {}
    pub fn handle_b(m: Msg) {
        match m {
            Msg::Start => run_b(),
        }
    }
    pub fn run_b() {}
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // `Msg` is ambiguous (two enums), so no dispatch edges are synthesized — crucially, NO
    // cross-enum edge from `enqueue_a` to `run_b` (or vice versa).
    for handler in ["run_a", "run_b"] {
        let callers = db.find_callers(handler, 50).unwrap();
        assert!(
            callers.iter().all(|hop| hop.edge_kind != "dispatches"),
            "ambiguous enum must not synthesize a (possibly cross-enum) dispatch into {handler}: \
             {callers:?}"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn symbol_search_excludes_generated_bindings_unless_opted_in() {
    // #202: a name/symbol_path search drowns in generated bindings (ubrn FFI output, codegen) that
    // shadow the hand-written source symbol. The real-world case is codegen living UNDER a source
    // target (e.g. `packages/.../src/generated/`): it keeps `kind = source` and gets full symbols,
    // but `is_generated_path` flags `files.generated = 1`. Symbol search defaults to
    // `files.generated = 0` (the same flag search/orientation use) and lets callers opt the
    // generated rows back in; an explicit id selection is never filtered.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/generated")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn shared_symbol() {}\n").unwrap();
    fs::write(root.join("src/generated/bindings.rs"), "pub fn shared_symbol() {}\n").unwrap();
    // A single SOURCE target covers both files; the nested `generated/` dir is flagged by the path
    // heuristic, not by target kind.
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let by_name = || crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("shared_symbol".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    };

    // Default (include_generated = false): the generated copy is filtered out, source remains.
    let default_hits = db.symbol_candidates(&by_name(), false).unwrap();
    assert!(!default_hits.candidates.is_empty(), "source symbol must still resolve");
    assert!(
        default_hits.candidates.iter().all(|c| !c.path.contains("/generated/")),
        "generated bindings must be excluded by default: {:?}",
        default_hits.candidates.iter().map(|c| &c.path).collect::<Vec<_>>()
    );

    // Opt-in (include_generated = true): both copies come back.
    let all_hits = db.symbol_candidates(&by_name(), true).unwrap();
    let generated = all_hits
        .candidates
        .iter()
        .find(|c| c.path.contains("/generated/"))
        .expect("opt-in must surface the generated copy");

    // An explicit symbol_id pick of the generated symbol is honored regardless of the filter —
    // the exclusion only governs name/path *search*, not a deliberate selection.
    let by_id = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: Some(generated.symbol_id),
        symbol_path: None,
        symbol: None,
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    let id_hits = db.symbol_candidates(&by_id, false).unwrap();
    assert_eq!(id_hits.candidates.len(), 1, "explicit id must resolve the generated symbol");
    assert!(id_hits.candidates[0].path.contains("/generated/"));

    let _ = fs::remove_dir_all(root);
}
