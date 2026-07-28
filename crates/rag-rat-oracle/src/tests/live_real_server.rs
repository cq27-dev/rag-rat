//! Real-server verification for the live TypeScript backend (#536).
//!
//! Every other live test drives the in-process fake server, which proves the write path but
//! cannot prove the two things that are properties of the REAL server: that the spawn argv
//! actually yields a session, and that the readiness policy actually brackets the window where
//! `typescript-language-server` answers WRONG.
//!
//! That window is the whole reason this backend gates on readiness. Asked before its project has
//! loaded, the server resolves an imported callee to the `import` statement in the CALLING file
//! rather than to the definition — a plausible non-null that the write path would persist as a
//! real verdict and (under the covered-skip budget) never revisit until the file's bytes change.
//! This test asserts the resolved target is the definition, never the import.
//!
//! Ignored by default: it needs `typescript-language-server` on PATH plus a TypeScript install the
//! server can resolve. Run it with a project's `node_modules` (one containing `typescript`):
//!
//! ```text
//! RAG_RAT_TS_NODE_MODULES=/path/to/node_modules \
//!   cargo nextest run -p rag-rat-oracle --run-ignored all -E 'test(real_typescript_server)'
//! ```

use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::live::{LiveOracleSession, LivePassInput, live_oracle_pass};

/// `greet` is defined here; its identifier spans bytes 16..21 on line 0.
const LIB_TS: &str =
    "export function greet(name: string): string {\n  return `hello ${name}`;\n}\n";
/// `greet` appears TWICE: the import binding (which a warming server wrongly resolves to) and the
/// call. The edge is seeded on the call.
const MAIN_TS: &str = "import { greet } from \"./lib.js\";\n\nexport function run(): void {\n  \
                       console.log(greet(\"world\"));\n}\n";

/// The live pass reports `Warming` until the server's project load completes. Poll passes rather
/// than sleeping a fixed interval, so a slow machine waits longer and a fast one doesn't.
const WARMUP_BUDGET: Duration = Duration::from_secs(60);

fn write_ts_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.ts"), LIB_TS).unwrap();
    std::fs::write(root.join("src/main.ts"), MAIN_TS).unwrap();
    // A tsconfig is what makes the server emit the project-load progress cycle at all — the
    // prerequisite the manifest enforces. Without it there is no readiness signal to wait for.
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"bundler",
            "strict":true},"include":["src"]}"#,
    )
    .unwrap();
    std::fs::write(root.join("package.json"), r#"{"name":"live-ts-fixture","version":"1.0.0"}"#)
        .unwrap();
}

/// Point the fixture at a TypeScript install. The server falls back to a bundled compiler when it
/// finds none, and that fallback does not resolve cross-file imports — which would make this test
/// pass for the wrong reason (an unresolved callee writes no verdict either).
fn link_typescript(root: &Path) {
    let node_modules = std::env::var("RAG_RAT_TS_NODE_MODULES").expect(
        "set RAG_RAT_TS_NODE_MODULES to a node_modules directory containing `typescript` — \
         without a resolvable compiler the server cannot resolve the import, and the test would \
         pass vacuously",
    );
    let source = Path::new(&node_modules);
    assert!(source.join("typescript").is_dir(), "{node_modules} has no `typescript` package");
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, root.join("node_modules")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(source, root.join("node_modules")).unwrap();
}

#[test]
#[ignore = "needs typescript-language-server on PATH + RAG_RAT_TS_NODE_MODULES"]
fn real_typescript_server_warms_before_it_resolves_an_imported_callee() {
    let h = Harness::new();
    write_ts_fixture(h.root());
    link_typescript(h.root());

    // Seed the corpus against the fixture's real bytes: `greet`'s definition in lib.ts, and the
    // unresolved call edge in main.ts (the CALL occurrence, not the import binding).
    let defs = h.add_file("src/lib.ts", LIB_TS);
    let target = h.add_symbol(defs, "greet", 0, LIB_TS.len());
    let src = h.add_file("src/main.ts", MAIN_TS);
    let call = MAIN_TS.rfind("greet").expect("the call site");
    let edge = h.add_edge(src, "greet", call, call + "greet".len(), "NameOnly", None);

    let Some(mut session) = LiveOracleSession::spawn(crate::OracleTool::TsLsp, h.root()) else {
        panic!("typescript-language-server must be on PATH for this test");
    };
    let worklist = vec!["src/main.ts".to_string()];
    let input = LivePassInput {
        commit_sha: COMMIT,
        worktree_id: WORKTREE,
        checkout_root: h.root(),
        worklist: &worklist,
        max_requests: 100,
        started_at_ms: 1_000,
    };

    // The FIRST pass must not resolve anything: the server has not loaded its project, so every
    // answer it would give is a warm-up artifact.
    let first = live_oracle_pass(&h.conn, &mut session, &input).unwrap();
    assert_eq!(first.status, "Warming", "a cold server must not be asked for definitions");
    assert_eq!(first.rows_written, 0);
    assert_eq!(first.requests_used, 0);
    assert!(h.verdict(edge).is_none(), "no verdict may be written before the project loads");

    // Later passes retry until the project-load cycle latches the session ready.
    let deadline = Instant::now() + WARMUP_BUDGET;
    let report = loop {
        let report = live_oracle_pass(&h.conn, &mut session, &input).unwrap();
        if report.status != "Warming" {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "the server never reported a completed project load within {WARMUP_BUDGET:?}",
        );
        std::thread::sleep(Duration::from_millis(250));
    };

    assert_eq!(report.status, "Completed", "{report:?}");
    assert_eq!(report.rows_written, 1, "{report:?}");
    let (kind, resolved, symbol) = h.verdict(edge).expect("a verdict once the server is ready");
    assert_eq!(kind, "upgrade");
    // THE assertion this test exists for: the callee resolves to `greet`'s definition in lib.ts.
    // A warming server would have pointed at the import statement in main.ts instead, which maps
    // to no indexed symbol here — or, in a corpus that indexed one, to the wrong symbol entirely.
    assert_eq!(
        resolved,
        Some(target),
        "the callee must resolve to the definition in lib.ts, not the import in main.ts",
    );
    assert!(symbol.starts_with("local ts-lsp-"), "{symbol}");

    session.shutdown();
}
