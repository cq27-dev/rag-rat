//! What a live backend learns about a checkout's projects: the whole-checkout marker search, the
//! per-session layout it produces, and the compilation-database usability test both rely on.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::scope::CheckoutScope;

/// What a live backend learned about a checkout's projects, resolved ONCE per session.
///
/// Every question the backend asks about layout — is this checkout usable, which database does the
/// session point at, which documents can it resolve — is answered from this one value. Re-deriving
/// them per call meant walking the whole checkout several times per spawn (proving there is no
/// SECOND database requires a full traversal), and the maintenance pass holds the repository write
/// lock while that happens.
#[derive(Debug, Clone)]
pub struct ProjectLayout {
    /// Marker sites found in the checkout, capped at two — the only distinction drawn is
    /// "exactly one" versus "several". UNUSABLE sites are recorded too: whether global pinning is
    /// safe depends on how many databases exist at all, not how many of them work.
    markers: Vec<MarkerSite>,
    /// Usability verdicts for marker FILES, memoized for as long as this layout is trusted.
    ///
    /// The per-file discovery path ([`Self::discoverable_marker_dir`]) asks the question once per
    /// worklist path per pass, and every source file in a directory shares the same nearest
    /// database — so without a memo one pass re-opens and re-parses the same file once per file,
    /// while the maintenance pass holds the repository write lock. Scoped to the layout value
    /// rather than to the process: a re-resolved layout starts with an empty memo, so the
    /// staleness window is the one [`LAYOUT_MAX_AGE`] already bounds and no other.
    usable_markers: RefCell<HashMap<PathBuf, MarkerReading>>,
    /// Whether the scan that produced this layout saw every database WHERE IT LOOKED. False when
    /// it stopped early — the depth bound, a directory it could not read, or a nested checkout
    /// whose own sources this checkout may still index.
    ///
    /// Where it looks is the index root's subtree, plus the ancestor chain up to the checkout
    /// ceiling (each ancestor and its `build/`, the set clangd itself searches). It is tempting to
    /// state this as "every database that could GOVERN an indexed file", and that would be wrong:
    /// governance is a property of a database's ENTRIES, and this crate's whole premise is that a
    /// database's LOCATION says nothing about what it describes — an out-of-tree build puts it
    /// somewhere unrelated to the sources it configures. Target containment bounds where indexed
    /// SOURCES live; it does not bound where a database describing them may sit.
    ///
    /// So the known miss is a database outside the index root's subtree and off the ancestor chain
    /// — `<checkout>/out/compile_commands.json` with `[index] root = <checkout>/sub`, say — which
    /// may well govern `sub/`'s sources. Such a database is invisible here, exactly as it was
    /// before the ancestor leg existed. The consequence is bounded but real: with no other
    /// database the checkout reports blocked, and with one the scan can pin it while a
    /// governing database exists elsewhere. Widening the descent to the ceiling would close it
    /// at the cost of walking the whole worktree under the repository write lock for precisely
    /// the configuration a subdirectory root was chosen to narrow — so the claim is kept
    /// honest instead.
    ///
    /// Pinning is the only question that needs this, and it needs it absolutely:
    /// `--compile-commands-dir` is global, so "there is exactly one database" has to be a proof,
    /// not an observation. A truncated scan can see one database and miss the one that actually
    /// governs half the sources, and pinning then hands those files another project's defines and
    /// include paths — a wrong definition, persisted. When the scan cannot prove it, the session
    /// simply is not pinned and clangd's own per-file lookup decides, which is always correct by
    /// construction.
    complete: bool,
}

/// One marker location, and what reading the file there established.
#[derive(Debug, Clone)]
pub(super) struct MarkerSite {
    dir: PathBuf,
    reading: MarkerReading,
}

/// What reading a marker file established about the project it describes.
///
/// Three states rather than a bool because the two questions asked of a database have OPPOSITE
/// costs of being wrong, and a single "usable" flag cannot serve both. Deciding to pin the session
/// to a database, or to resolve a file through one, is wrong at the cost of a WRONG VERDICT
/// persisted as trusted evidence. Deciding a checkout has no warmable project is wrong at the cost
/// of the backend never running at all. `Unknown` is what lets the first stay conservative while
/// the second stays permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkerVerdict {
    /// Parsed, and at least one entry yields a command line the server can use.
    Loadable,
    /// Parsed, and it describes no analysable translation unit — or carries a shape the server
    /// rejects outright.
    NotLoadable,
    /// Could not be read as JSON at all, which says nothing either way: clangd's YAML reader
    /// accepts comments, trailing commas, and block syntax that `serde_json` does not (#1016), and
    /// a genuinely corrupt file is indistinguishable from those here.
    Unknown,
}

/// Whether a compilation database describes anything this checkout indexes — the question that
/// decides whether forcing it on the WHOLE checkout is safe.
///
/// Three states for the same reason [`MarkerVerdict`] has three: a database this crate could not
/// read named nothing it could test, which is not the same finding as one whose entries were all
/// read and named nothing indexed. They are kept apart because a future reader accepting more
/// syntax (#1016) will produce far fewer unreadable files, and the two must not have silently
/// merged by then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Governs {
    /// An entry named a file in the indexed corpus.
    IndexedSource,
    /// The whole database was read, and no entry named one.
    NothingIndexed,
    /// It could not be read, so it named nothing this crate could test.
    Unknown,
}

/// What ONE read of a marker file established, from one streaming pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MarkerReading {
    verdict: MarkerVerdict,
    governs: Governs,
}

/// How much a layout question demands of a database before counting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trust {
    /// Only a database PROVEN loadable counts — for every question whose wrong answer gets
    /// persisted as evidence.
    Proven,
    /// A database that merely might load counts too — for warm-up, where being wrong costs a
    /// session that reports `Warming` rather than a verdict that is wrong. Reporting the whole
    /// checkout blocked because a database could not be parsed is the more expensive mistake:
    /// nothing runs, and the checkout gets no live evidence at all.
    Possible,
}

impl MarkerVerdict {
    fn counts_under(self, trust: Trust) -> bool {
        match trust {
            Trust::Proven => self == Self::Loadable,
            Trust::Possible => self != Self::NotLoadable,
        }
    }
}

impl Governs {
    /// The governance half of the same question [`MarkerVerdict::counts_under`] answers, biased the
    /// same way: `Unknown` counts for warm-up (refusing to warm costs the whole backend) and does
    /// not count where the answer gets persisted.
    fn counts_under(self, trust: Trust) -> bool {
        match trust {
            Trust::Proven => self == Self::IndexedSource,
            Trust::Possible => self != Self::NothingIndexed,
        }
    }
}

impl Default for ProjectLayout {
    /// An empty layout from no scan at all. `complete` is true because there is nothing to have
    /// missed: a backend with no project marker never asks a layout question, and the test seams
    /// that use this drive the branches explicitly.
    fn default() -> Self {
        Self { markers: Vec::new(), usable_markers: RefCell::default(), complete: true }
    }
}

impl ProjectLayout {
    /// The layout for a checkout-scoped `marker`, from the sites one scan found.
    ///
    /// The scan's verdicts seed the memo, so a file whose nearest database is one of the scanned
    /// sites reuses the verdict the scan already paid for, and the two routes cannot disagree
    /// about the same file within one layout.
    pub(super) fn from_marker_sites(marker: &str, scan: MarkerScan) -> Self {
        let usable_markers: HashMap<PathBuf, MarkerReading> =
            scan.sites.iter().map(|site| (site.dir.join(marker), site.reading)).collect();
        Self {
            markers: scan.sites,
            usable_markers: RefCell::new(usable_markers),
            complete: scan.complete,
        }
    }

    /// The single database this session can point the server at.
    ///
    /// `None` unless the checkout holds EXACTLY ONE database and it is usable. A second database
    /// disqualifies pinning even when it is empty or malformed: `--compile-commands-dir` is
    /// global, so pinning would hand the working database's flags to files that belong to the
    /// broken one, where clangd would otherwise stop at their own nearer database and fall back.
    /// Both are wrong for those files — but only pinning also makes them look configured.
    pub(super) fn sole_marker_dir(&self) -> Option<&Path> {
        if !self.complete {
            return None;
        }
        match self.markers.as_slice() {
            [only]
                if only.reading.verdict == MarkerVerdict::Loadable
                    && only.reading.governs == Governs::IndexedSource =>
                Some(&only.dir),
            _ => None,
        }
    }

    /// Whether this checkout holds a database that would have been pinned but for describing
    /// nothing the checkout indexes — the #1008 case, and the one whose remedy is specific enough
    /// to be worth saying out loud. Every other reason for declining to pin (several databases, an
    /// unreadable one, a truncated scan) already has its own operator-facing wording.
    pub fn has_database_governing_nothing_indexed(&self) -> bool {
        self.complete
            && matches!(
                self.markers.as_slice(),
                [only]
                    if only.reading.verdict == MarkerVerdict::Loadable
                        && only.reading.governs == Governs::NothingIndexed
            )
    }

    pub(super) fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }

    /// Whether this layout pins the session to one database, which is what a re-resolve has to be
    /// compared against: only a change to the PINNED database can invalidate an argv already
    /// passed to a running server.
    pub fn pins_same_database_as(&self, other: &ProjectLayout) -> bool {
        self.sole_marker_dir() == other.sole_marker_dir()
    }

    /// Whether the marker file at `file` describes a project the server can load, answered from
    /// [`Self::usable_markers`] when this layout has already read it.
    fn marker_reading(&self, file: &Path, checkout: &CheckoutScope<'_>) -> MarkerReading {
        // Borrow only for the lookup: the miss path reads the file and then takes a write borrow.
        let memoized = self.usable_markers.borrow().get(file).copied();
        if let Some(reading) = memoized {
            return reading;
        }
        let reading = read_marker_file(file, checkout);
        self.usable_markers.borrow_mut().insert(file.to_path_buf(), reading);
        reading
    }

    /// The marker directory the SERVER would find for `path` on its own — clangd searches an
    /// opened file's ancestor directories and a `build/` subdirectory of each (measured: a
    /// database in an ancestor's `build/` resolves with no flag passed). Used when the checkout
    /// holds several databases and the session therefore points at none.
    ///
    /// Applies the same usability test as the whole-checkout scan: a nearer but EMPTY database is
    /// what clangd would actually pick up, and it configures nothing — treating the file as
    /// configured because some *other* project's database exists is how a fallback-flags answer
    /// gets persisted.
    pub(super) fn discoverable_marker_dir(
        &self,
        checkout: &CheckoutScope<'_>,
        path: &Path,
        marker: &str,
        trust: Trust,
    ) -> Option<PathBuf> {
        // The CEILING, not the index root: this walk exists to mirror clangd's own ancestor
        // search, and clangd has no notion of an index root. Stopping at a subdirectory root
        // declared a file unconfigurable over a database clangd would have found for it.
        let ceiling = checkout.ceiling();
        let mut dir = path.parent()?;
        loop {
            for candidate in [dir.to_path_buf(), dir.join("build")] {
                let file = candidate.join(marker);
                if file.exists() {
                    // STOP at the nearest database, usable or not. clangd loads the first one it
                    // finds and falls back to generic flags if it configures nothing — it does not
                    // continue to a farther ancestor. Continuing here would declare the file
                    // configured by a database clangd never consults, and the live pass would then
                    // trust a fallback-flags answer.
                    // Governance applies HERE too, not only to the global pin. The walk now
                    // reaches the checkout ceiling rather than stopping at the index root, so
                    // "this database encloses the file" no longer implies "this database is about
                    // the file's project": with a subdirectory `[index] root`, an ancestor database
                    // describing only an unindexed sibling tree is reachable, and resolving a file
                    // through it persists definitions produced under another project's flags — the
                    // exact corruption the pin gate exists to prevent. The reading is memoized, so
                    // this costs no extra parse.
                    let reading = self.marker_reading(&file, checkout);
                    return (reading.verdict.counts_under(trust)
                        && reading.governs.counts_under(trust))
                    .then_some(candidate);
                }
            }
            if dir == ceiling {
                return None;
            }
            dir = dir.parent()?;
        }
    }
}

/// Directories never searched for a project MARKER. Deliberately more permissive than the
/// document search's `is_searchable_dir` (in `super::documents`): a build directory may
/// legitimately be hidden (`.build/`), and a compilation database there is as real as one in
/// `build/`. Only trees that cannot hold this checkout's own database are excluded — VCS
/// internals, rag-rat's own state, clangd's index, and vendored dependencies.
fn is_searchable_for_marker(path: &Path, name: &str) -> bool {
    if matches!(name, "node_modules" | ".git" | ".rag-rat" | ".hg" | ".svn") {
        return false;
    }
    // Only clangd's OWN index is off-limits under `.cache`, not every build artifact there: a
    // hidden build such as `.cache/cmake-build/compile_commands.json` is a real database, and
    // excluding the whole subtree would contradict supporting hidden build directories at all.
    name != "clangd"
        || path.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new(".cache"))
}

/// The directory holding `marker`, searched anywhere at or below `root` — the
/// [`super::registry::ProjectScope::Checkout`] lookup. Returns the DIRECTORY, not a bool, because
/// the server has to be told where it is: clangd searches only an opened file's ancestors and
/// their `build/` subdirectory, so a database in `out/` or `cmake-build-debug/` is invisible to it
/// without `--compile-commands-dir` (measured: no progress at all, and calls resolve to header
/// declarations). Accepting a checkout whose database the server cannot find would report it
/// usable while it silently never warms.
pub(super) fn marker_sites(checkout: &CheckoutScope<'_>, marker: &str) -> MarkerScan {
    let root = checkout.root();
    let mut search = MarkerSearch {
        marker,
        checkout,
        // The bound the walk may not leave — the enclosing CHECKOUT. A link into another part of
        // the same checkout points at a database that is genuinely this checkout's; only leaving
        // the checkout entirely is out of bounds. Already canonical (`CheckoutScope::resolve`).
        scope: checkout.ceiling().to_path_buf(),
        visited_links: HashSet::new(),
        recorded_markers: HashSet::new(),
        found: Vec::new(),
        truncated: false,
    };
    search.descend(root, MARKER_SEARCH_MAX_DEPTH);
    search.climb_to_ceiling(root, checkout.ceiling());
    // Stopping at the two-site cap is not truncation: two databases already disqualify pinning, so
    // nothing deeper could change the answer. Every OTHER early stop is, because it can hide the
    // second database that would have.
    let complete = !search.truncated || search.found.len() >= 2;
    MarkerScan { sites: search.found, complete }
}

/// What one whole-checkout marker search found, and whether it got to look everywhere it needed to.
pub(super) struct MarkerScan {
    pub(super) sites: Vec<MarkerSite>,
    pub(super) complete: bool,
}

/// What a marker search does with one directory entry — see [`MarkerSearch::step_for`].
enum Step {
    /// A directory of this checkout, to be searched.
    Descend(PathBuf),
    /// Deliberately not searched, and provably unable to hold a database this scan needed, so the
    /// scan stays complete without it.
    Excluded,
    /// Could not be classified, so the scan can no longer prove it saw every database.
    Indeterminate,
}

/// How deep the marker search descends — a backstop for a tree that is merely very deep, not the
/// cycle guard. [`MarkerSearch::visited_links`] is what keeps a symlink loop from re-entering a
/// directory the search has already descended into; a depth bound alone would still explore every
/// path through the loop, which is exponential in this bound once two links point at the same
/// ancestor. Deep enough for any real project layout.
const MARKER_SEARCH_MAX_DEPTH: u32 = 24;

/// One whole-checkout marker search: what it looks for, which symlinked directories and marker
/// files it has already visited, and the sites found so far.
struct MarkerSearch<'a> {
    marker: &'a str,
    /// What this checkout is: the corpus each database's entries are tested against, and the
    /// ceiling the walk may not leave.
    checkout: &'a CheckoutScope<'a>,
    /// The canonical checkout root the walk may not leave. Following directory symlinks is what
    /// makes a symlinked `build/` discoverable, but a link like `sdk -> /opt/sdk` or
    /// `external -> ..` points OUT of the checkout: without this the walk would recurse through an
    /// unrelated tree — to the depth limit, while the maintenance pass holds the repository write
    /// lock — and count any `compile_commands.json` it found there as this checkout's, changing
    /// both the pinning decision and the prerequisite verdict.
    scope: PathBuf,
    /// Canonical paths of the SYMLINKED directories already descended into. Following directory
    /// symlinks is what makes a symlinked build directory discoverable, and it is also the only
    /// way this walk can revisit a directory — so recording just those targets bounds the
    /// traversal without paying a canonicalize per ordinary entry.
    visited_links: HashSet<PathBuf>,
    /// Canonical paths of the marker FILES already recorded, so one database reachable through
    /// several directory paths counts once. See [`Self::record_marker_in`].
    recorded_markers: HashSet<PathBuf>,
    /// Marker sites, collected until there are two DISTINCT databases — the only distinction any
    /// caller draws is "exactly one" versus "several", and a monorepo can hold hundreds.
    found: Vec<MarkerSite>,
    /// Whether the walk stopped somewhere short of seeing everything, so "exactly one database"
    /// would be an observation rather than a proof. See [`ProjectLayout::complete`].
    truncated: bool,
}

impl MarkerSearch<'_> {
    /// Record the databases on the chain from `root` up to `ceiling` — each ancestor directory and
    /// its `build/` subdirectory, which is exactly the set clangd searches for an opened file.
    ///
    /// [`Self::descend`] covers everything at or below the index root; this covers what sits ABOVE
    /// it and still governs it, which a subdirectory `[index] root` otherwise hides. It is O(depth)
    /// `exists()` calls rather than a traversal, deliberately: widening the DESCENT to the whole
    /// checkout would walk every sibling of the index root under the repository write lock, and buy
    /// nothing — a database outside the index root that is not an ancestor governs no indexed
    /// source, because config load refuses a target directory that escapes the root.
    ///
    /// Re-recording the root's own database is harmless: [`Self::record_marker_in`] keys on the
    /// marker file's canonical path, so a site both legs reach counts once.
    fn climb_to_ceiling(&mut self, root: &Path, ceiling: &Path) {
        // Nothing to climb when the index root IS the checkout — and this is not merely an
        // optimisation. The loop below records a directory before testing whether it has reached
        // the ceiling, so without this guard an index root already at the checkout top would step
        // straight past it and walk to the filesystem root, counting databases from outside the
        // checkout entirely. `root == ceiling` is the COMMON case, and a linked worktree nested
        // inside another checkout is where it does damage: the walk climbs out of the worktree and
        // adopts the enclosing checkout's database as a second one.
        //
        // With `root` a strict descendant of `ceiling`, the walk is guaranteed to reach `ceiling`
        // before running off the top.
        if root == ceiling || !root.starts_with(ceiling) {
            return;
        }
        let mut dir = root;
        while let Some(parent) = dir.parent() {
            if self.found.len() >= 2 {
                return;
            }
            self.record_marker_in(parent);
            self.record_marker_in(&parent.join("build"));
            if parent == ceiling {
                return;
            }
            dir = parent;
        }
    }

    fn descend(&mut self, dir: &Path, depth_left: u32) {
        if self.found.len() >= 2 {
            return;
        }
        self.record_marker_in(dir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            // A directory that cannot be read may hold the database that would have disproved a
            // sole one, so the layout can no longer be pinned on.
            self.truncated = true;
            return;
        };
        let mut subdirectories = Vec::new();
        for entry in entries {
            // Every entry gets an explicit disposition. Enumeration itself can fail per item on a
            // tree that is changing underneath the walk or on a network filesystem, and dropping
            // those silently (`flatten`) would let a skipped directory holding the second database
            // pass for "there was no second database".
            match self.step_for_entry(entry) {
                Step::Descend(path) => subdirectories.push(path),
                Step::Excluded => {},
                Step::Indeterminate => self.truncated = true,
            }
        }
        subdirectories.sort();
        let Some(depth_left) = depth_left.checked_sub(1) else {
            // The bound exists for a tree that is merely very deep, but a project nested past it is
            // an ordinary monorepo layout — and its database is exactly the one whose absence would
            // make pinning look safe when it is not.
            self.truncated |= !subdirectories.is_empty();
            return;
        };
        for sub in subdirectories {
            self.descend(&sub, depth_left);
        }
    }

    /// Record the marker file in `dir`, unless the same PHYSICAL file was already recorded under
    /// another path.
    ///
    /// One database is routinely reachable twice: `out/` alongside a `current-build -> out`
    /// symlink, which this search follows on purpose (that is what makes a symlinked build
    /// directory discoverable at all). Recording both would make a single-database checkout look
    /// multi-database — `sole_marker_dir` returns `None`, no `--compile-commands-dir` is passed,
    /// and every source outside clangd's own ancestor/`build/` search stops being resolvable.
    ///
    /// Canonicalizing is affordable here because it happens once per marker file FOUND, and the
    /// walk stops at two distinct databases — unlike the traversal itself, which sees every
    /// directory in the checkout and therefore canonicalizes only the rare symlink.
    fn record_marker_in(&mut self, dir: &Path) {
        let candidate = dir.join(self.marker);
        if !candidate.exists() {
            return;
        }
        // A marker file that will not canonicalize is recorded under its own path rather than
        // dropped: it exists, and losing the checkout's only database is worse than the alias this
        // set exists to collapse.
        let identity = candidate.canonicalize().unwrap_or_else(|_| candidate.clone());
        if !self.recorded_markers.insert(identity) {
            return;
        }
        let reading = read_marker_file(&candidate, self.checkout);
        self.found.push(MarkerSite { dir: dir.to_path_buf(), reading });
    }

    /// What this search does with one ENUMERATED entry, error included.
    ///
    /// Enumeration itself can fail per item — a tree changing underneath the walk, a network
    /// filesystem — and `ReadDir` reports that as an `Err` element rather than by ending. Dropping
    /// those (`flatten`) would let a skipped directory holding the second database pass for "there
    /// was no second database", which is exactly the observation `sole_marker_dir` must never make.
    fn step_for_entry(&mut self, entry: std::io::Result<std::fs::DirEntry>) -> Step {
        match entry {
            Ok(entry) => self.step_for(&entry),
            Err(_) => Step::Indeterminate,
        }
    }

    /// What this search does with one directory entry.
    ///
    /// Three outcomes rather than `Option`, because "not descended" covers two situations that must
    /// not be confused. A DELIBERATE exclusion is a statement that nothing behind it could change
    /// the answer, so the scan stays provably complete. Anything the search could not classify —
    /// an entry the filesystem would not describe, a link it could not resolve, a nested checkout
    /// whose sources this checkout may still index — leaves a hole, and a hole means the layout can
    /// no longer be pinned on. Collapsing the two is how a scan that missed the second database
    /// reports "exactly one".
    fn step_for(&mut self, entry: &std::fs::DirEntry) -> Step {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // EXCLUDED, not indeterminate: every name refused here is also refused by indexing
        // (`FLOOR_DIRS` floors `.git`, `.rag-rat`, `node_modules`; `FLOOR_PATHS` floors
        // `.cache/clangd`), so none of them can contain a source whose database this scan needed to
        // find. That correspondence is what makes skipping them safe — not the names themselves.
        if !is_searchable_for_marker(&path, &name) {
            return Step::Excluded;
        }
        // The entry's OWN type first: on Linux this is answered from the readdir result, so
        // ordinary files and directories cost no extra syscall and only a symlink is resolved.
        let Ok(kind) = entry.file_type() else {
            return Step::Indeterminate;
        };
        if kind.is_symlink() {
            // `is_dir` FOLLOWS the link: `build -> cmake-build-debug` is an ordinary layout whose
            // database is reachable through the checkout path, and a broken link is simply nothing.
            if !path.is_dir() {
                return Step::Excluded;
            }
            let Ok(target) = path.canonicalize() else {
                return Step::Indeterminate;
            };
            // Outside the checkout is EXCLUDED rather than indeterminate: indexing skips symlinks
            // outright (`walker::walk_dir`), so nothing reachable only through a link is an indexed
            // source, and a database out there governs none of this checkout's files.
            if !target.starts_with(&self.scope) {
                return Step::Excluded;
            }
            // A target already walked is the same tree by another name — nothing new behind it.
            // This is what makes a checkout with several self-referential links terminate quickly
            // instead of exploring every path through them.
            if !self.visited_links.insert(target) {
                return Step::Excluded;
            }
        } else if !kind.is_dir() {
            return Step::Excluded;
        }
        // A directory that is ITSELF a checkout belongs to that checkout, not this one. The common
        // case is a linked worktree kept inside the main checkout (`git worktree add
        // worktrees/feature`, or this repo's own `.claude/worktrees/<name>`), whose `.git` is a
        // FILE — so the name-based exclusion above never sees it, and the walk would otherwise
        // count the sibling's `compile_commands.json` as this checkout's. A nested clone or
        // submodule is the same situation and answers the same test. The search ROOT is exempt: it
        // is entered directly, never through here.
        //
        // INDETERMINATE rather than excluded, and that is the whole point: its databases are not
        // this checkout's, but its SOURCES may be. Indexing descends an ordinary directory whatever
        // `.git` file it holds, so a submodule's files can be indexed here while its database stays
        // invisible to this scan — and pinning the parent's database over them would analyse them
        // under unrelated defines and include paths. Whether a nested checkout is inside the
        // indexed corpus is a question this crate cannot answer at all (#1008), so the honest
        // answer is that the layout is no longer provable.
        if path.join(".git").exists() {
            return Step::Indeterminate;
        }
        Step::Descend(path)
    }
}

/// How long a resident session may trust its cached project layout.
///
/// The layout is cached because resolving it walks the checkout (measured at roughly 80ms for a
/// 21k-directory tree, and the maintenance pass holds the repository write lock while it runs), so
/// re-resolving every pass is the wrong trade. But it cannot be trusted for a session's whole
/// lifetime either: a database added or removed meanwhile would leave a pinned session analysing a
/// new project's files with the old project's flags — wrong include and define flags select a
/// different preprocessor branch, so a wrong definition, persisted. This bounds that window to a
/// minute instead of the idle-shutdown timeout, at an amortised cost of one walk per minute.
pub const LAYOUT_MAX_AGE: Duration = Duration::from_secs(60);

/// Whether a marker file actually describes a project the server can load. This is the read:
/// every call opens and parses the file, so per-file callers go through
/// [`ProjectLayout::marker_verdict`] instead, which remembers the verdict.
///
/// EVERY entry has to be complete, not just the first, because clangd rejects the WHOLE database
/// when any single entry is malformed. Measured with clangd 19.1.2 on a database whose first entry
/// carries `-DPROBE_OK=1` and whose second lacks a compiler invocation:
///
/// ```text
/// E[..] Failed to load compilation database from …/compile_commands.json:
///       Missing key: "command" or "arguments".
/// I[..] Failed to find compilation database for …/src/main.c
/// I[..] Generic fallback command is: […]
/// ```
///
/// — the fallback command carries no `-DPROBE_OK=1`, so every file in that checkout is analysed
/// with heuristic flags, and a cross-translation-unit call resolves to the callee's header
/// declaration. Trusting the first entry alone would report such a database usable, pin the session
/// to it, and persist those answers as trusted evidence.
///
/// The other case this catches is a syntactically valid database that names no translation unit —
/// `[]`, `{}`, or entries without the required keys. Measured, clangd emits no progress cycle at
/// all for one, so a checkout holding it can never report ready and the backend would retry its
/// backlog forever while `oracle status` called it runnable.
///
/// The whole file is STREAMED rather than buffered: entries deserialize into presence flags and
/// discarded payloads, so the cost is one pass with no allocation proportional to the file. That
/// cost is real — a 39 MB / 120k-entry database, the size a large C++ project exports, takes about
/// 0.3s in a release build. The memo on [`ProjectLayout`] is what keeps it to once per marker file
/// per layout rather than once per worklist path, which matters because this runs while the
/// maintenance pass holds the repository write lock.
///
/// SYNTAX is where this is knowingly narrower than the server. clangd reads the file with clang's
/// YAML reader, and YAML is a superset of JSON, so it also loads databases carrying `#` comments,
/// trailing commas, or pure YAML block syntax — all of which `serde_json` refuses (see #1016).
/// Matching that needs a streaming YAML parser, which is a dependency decision rather than a patch.
/// The two cheap divergences are closed here instead: a UTF-8 BOM is skipped, and content after the
/// closing bracket is ignored, both of which clangd accepts and neither of which says anything
/// about whether the entries describe a loadable project.
fn read_marker_file(path: &Path, checkout: &CheckoutScope<'_>) -> MarkerReading {
    let unreadable = MarkerReading { verdict: MarkerVerdict::Unknown, governs: Governs::Unknown };
    let Ok(file) = std::fs::File::open(path) else {
        return unreadable;
    };
    // `serde_json`'s reader deserializer pulls ONE BYTE at a time — measured unbuffered, a 2 MB
    // database costs two million `read` syscalls — so the buffer in front of it is what makes this
    // a sequential read.
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    if !skip_utf8_bom(&mut reader) {
        return unreadable;
    }
    // `Deserializer::from_reader` + `deserialize` rather than `serde_json::from_reader`, which also
    // calls `end()` and so rejects anything after the top-level array. clangd stops at the closing
    // bracket and never looks further, so trailing content must not condemn the database.
    let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
    let read = de::DeserializeSeed::deserialize(DatabaseSeed(checkout.corpus()), &mut deserializer);
    // Governance is a property of the ENTRIES, so it is only knowable when they parsed. A database
    // whose shape was rejected says nothing either way.
    let governs = |database: &CompilationDatabase| match database {
        d if d.governs_indexed => Governs::IndexedSource,
        // "Could not be read" is not "names nothing indexed": clangd accepts a non-string `file`
        // and reads it as text, so such a database may well govern the corpus — this reader simply
        // cannot say. Scoring it as a negative would refuse warm-up for a checkout that works.
        d if d.governs_unknown => Governs::Unknown,
        _ => Governs::NothingIndexed,
    };
    match read {
        // Checked BEFORE the entry count: clangd refuses the file over an unrecognised key however
        // many good entries sit beside it, so counting them would be reading a database that will
        // not load as proof that it will.
        Ok(database) if database.unmodelled_key =>
            MarkerReading { verdict: MarkerVerdict::Unknown, governs: governs(&database) },
        Ok(database) if database.entries > 0 =>
            MarkerReading { verdict: MarkerVerdict::Loadable, governs: governs(&database) },
        Ok(database) =>
            MarkerReading { verdict: MarkerVerdict::NotLoadable, governs: governs(&database) },
        // The error's CATEGORY separates the two failures. `Data` means the document parsed and its
        // contents break the entry contract — the same thing the server refuses, so it is a
        // positive finding. `Syntax`/`Eof`/`Io` mean it could not be read as JSON at all, which is
        // not a finding about the project: clangd's YAML reader accepts syntax this does not, and a
        // corrupt file is indistinguishable from that here.
        Err(error) => match error.classify() {
            serde_json::error::Category::Data =>
                MarkerReading { verdict: MarkerVerdict::NotLoadable, governs: Governs::Unknown },
            _ => unreadable,
        },
    }
}

/// Consume a leading UTF-8 BOM if there is one. `false` only when the file could not be read at
/// all; a file with no BOM is left exactly where it was.
///
/// clangd loads a database that starts with one (measured), and a generator on Windows can easily
/// emit it, but `serde_json` reports it as a syntax error — so without this a perfectly good
/// database is reported unusable and the checkout silently loses its live evidence.
fn skip_utf8_bom(reader: &mut impl std::io::BufRead) -> bool {
    const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
    // The buffer is far larger than three bytes, so one fill sees the whole prefix.
    match reader.fill_buf() {
        Ok(buffer) => {
            if buffer.starts_with(BOM) {
                reader.consume(BOM.len());
            }
            true
        },
        Err(_) => false,
    }
}

/// A compilation database, read for its SHAPE alone: how many entries it holds, having checked
/// that each one carries what the format requires.
struct CompilationDatabase {
    entries: usize,
    /// Whether any entry named a file the checkout indexes. See [`Governs`].
    governs_indexed: bool,
    /// Whether any entry named a `file` this reader could not read as text, leaving governance
    /// unprovable rather than disproved.
    governs_unknown: bool,
    /// Whether any entry carried a key outside the modelled format. One is enough: clangd refuses
    /// the whole file over it, so no other entry can redeem the database.
    unmodelled_key: bool,
}

/// One compilation-database entry, with the fields the format REQUIRES. `clangd --check` rejects
/// an entry missing any of them (`Missing key: "directory"`, `Missing key: "command" or
/// "arguments"`) and falls back to generic flags for the whole checkout, so an entry naming only a
/// file is not a usable database however well-formed its JSON is.
///
/// The SHAPE of each value is checked, not just the key's presence, because clangd reads a
/// compilation database with clang's YAML/JSON reader and rejects the whole file on a shape it does
/// not expect. Measured against clangd 19.1.2, the rule it applies is:
///
/// - every field must be a SCALAR node, and which kind does not matter — `"directory": 7`,
///   `"command": null`, and `"command": true` all load, the value simply being read as text;
/// - except `arguments`, which must be a SEQUENCE — `null`, a number, and an object are each
///   rejected with `Expected sequence as value`, and its elements are not themselves type-checked;
/// - a composite where a scalar belongs (`"command": []`, `"directory": {}`) is rejected with
///   `Expected string as value`.
///
/// So the check accepts any scalar and refuses only the composite shapes. Being stricter would be
/// a false negative — rejecting a database the server loads silently costs a checkout its live
/// evidence, which is worse than the malformed input it would catch — and being looser lets a
/// database clangd discards look usable, which is how a fallback-flags answer gets persisted as
/// trusted evidence.
///
/// Presence is tracked separately from value, because serde's `Option` collapses "key absent" and
/// "key present and null" into `None` — and those differ here: `"arguments": null` is a PRESENT
/// arguments field of the wrong shape, which clangd rejects, while an absent one is satisfied by
/// `command`. Every payload is discarded, so nothing allocates per entry.
///
/// An entry whose invocation is present but EMPTY (`"arguments": []`, `"command": ""`) is a
/// different failure from every other one here, and the difference is load-bearing. Measured, it
/// does not reject the database: clangd loads it, and other entries keep working — only the file
/// that entry names becomes unanalysable (`Failed to parse command line`, and `--check` exits 3).
/// So such an entry is reported as configuring nothing rather than as an error, and the database
/// stays usable as long as SOME entry configures a file. Treating it as an error instead would
/// condemn a 120k-entry database over one degenerate line and cost the checkout all its live
/// evidence.
struct CompilationEntry {
    /// Whether this entry yields a command line clangd can parse.
    configures_a_file: bool,
    /// Whether the entry named a `file` whose text this reader could not capture — a non-string
    /// scalar, which clangd reads as its literal rendering and accepts. The path is then
    /// unknowable here, which is NOT the same finding as an entry naming a file outside the
    /// corpus: it makes the database's governance [`Governs::Unknown`] rather than
    /// `NothingIndexed`, so warm-up stays permissive while pinning still declines.
    file_text_unavailable: bool,
    /// Whether it carried a key outside the format this crate models, which makes the whole
    /// database's fate unknowable here — see [`EntryField::Unmodelled`].
    unmodelled_key: bool,
}

/// The entry fields whose shape is constrained. `Other` is every extra key a generator emits
/// (`output`, and whatever else) — read and discarded, since clangd ignores them too.
#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum EntryField {
    File,
    Directory,
    Command,
    Arguments,
    Output,
    #[serde(other)]
    Unmodelled,
}

impl<'de> Deserialize<'de> for CompilationEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(CompilationEntryVisitor(None))
    }
}

/// The `file` and `directory` of the entry being read, in buffers REUSED across entries.
#[derive(Default)]
struct EntryPaths {
    file: String,
    directory: String,
    /// Canonical spellings of entry parent directories whose literal form did not land in the
    /// corpus, memoized for the whole read.
    ///
    /// A checkout reached through a symlink has its database generated under the ALIAS spelling
    /// while the configured root is canonical, so no entry matches lexically and the checkout's
    /// only database would be judged to govern nothing — withdrawing the pin from a setup that
    /// works. Resolving is a syscall, so it happens only after the lexical test fails, and once
    /// per distinct parent: an aliased database aliases every entry the same way.
    canonical_parents: HashMap<PathBuf, Option<PathBuf>>,
}

impl EntryPaths {
    /// Whether this entry names a file the checkout indexes.
    ///
    /// The entry's `file` is resolved the way the format defines it — absolute as given, otherwise
    /// joined onto `directory` — then normalized LEXICALLY. Never `canonicalize`: that would be a
    /// syscall per entry, and it fails outright for a file that no longer exists, which is ordinary
    /// for a database written before the last edit.
    fn names_indexed_file(&mut self, corpus: &dyn super::IndexedCorpus) -> bool {
        if self.file.is_empty() {
            return false;
        }
        let lexical = {
            let file = Path::new(&self.file);
            let absolute = if file.is_absolute() {
                file.to_path_buf()
            } else {
                Path::new(&self.directory).join(file)
            };
            match rag_rat_base::paths::lexically_normalized(&absolute) {
                Some(path) => path,
                None => return false,
            }
        };
        if corpus.indexes_file(&lexical) {
            return true;
        }
        // The literal spelling is not in the corpus. Before concluding the database does not govern
        // it, resolve the parent once: the path may name the same file through a symlinked root.
        let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name()) else {
            return false;
        };
        let resolved = self
            .canonical_parents
            .entry(parent.to_path_buf())
            .or_insert_with(|| parent.canonicalize().ok());
        resolved.as_ref().is_some_and(|dir| corpus.indexes_file(&dir.join(name)))
    }
}

/// Reads one entry, capturing its paths when the caller still needs them.
struct EntrySeed<'a>(&'a mut EntryPaths);

impl<'de> de::DeserializeSeed<'de> for EntrySeed<'_> {
    type Value = CompilationEntry;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(CompilationEntryVisitor(Some(self.0)))
    }
}

struct CompilationEntryVisitor<'a>(Option<&'a mut EntryPaths>);

impl<'de> Visitor<'de> for CompilationEntryVisitor<'_> {
    type Value = CompilationEntry;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a compilation database entry")
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<Self::Value, A::Error> {
        // A fresh entry: whatever the previous one left in the buffers must not be read as this
        // one's, since either key may be absent here.
        if let Some(paths) = self.0.as_deref_mut() {
            paths.file.clear();
            paths.directory.clear();
        }
        let (mut file, mut directory, mut invocation) = (false, false, false);
        let mut file_is_text = false;
        // Tracked separately because clangd does not treat them as alternatives: measured, when an
        // entry carries BOTH, `arguments` is used and `command` is ignored entirely — whichever
        // order the keys appear in. So a good `command` beside an empty `arguments` configures
        // nothing, and accepting either independently would call that entry usable.
        let (mut command_configures, mut arguments_configure) = (false, None);
        let mut unmodelled_key = false;
        while let Some(field) = map.next_key::<EntryField>()? {
            match field {
                EntryField::File => {
                    file_is_text = match self.0.as_deref_mut() {
                        Some(paths) => map.next_value_seed(ScalarInto(&mut paths.file))?.is_string,
                        None => map.next_value::<JsonScalar>()?.is_string,
                    };
                    file = true;
                },
                EntryField::Directory => {
                    match self.0.as_deref_mut() {
                        Some(paths) => {
                            map.next_value_seed(ScalarInto(&mut paths.directory))?;
                        },
                        None => {
                            map.next_value::<JsonScalar>()?;
                        },
                    }
                    directory = true;
                },
                EntryField::Command => {
                    // A non-string scalar is not blank: clangd reads the node as text, so `null`,
                    // `7`, and `true` each become a one-word command line it parses happily.
                    command_configures = !map.next_value::<JsonScalar>()?.blank;
                    invocation = true;
                },
                EntryField::Arguments => {
                    arguments_configure =
                        Some(map.next_value::<JsonSequence>()?.carries_an_argument);
                    invocation = true;
                },
                // `output` is part of the format and must be a scalar like the paths are: an
                // object or array there is refused with `Expected string as value`, discarding the
                // database exactly as a malformed `command` would.
                EntryField::Output => {
                    map.next_value::<JsonScalar>()?;
                },
                // A key this crate does not model. clangd's entry schema is CLOSED — measured, an
                // unrecognised key is refused with `Unknown key` and the whole database falls back
                // to generic flags — so this is NOT something to swallow. It is reported as
                // uncertainty rather than as a finding, because the schema is clangd's and an
                // unrecognised key means this model may simply be behind it. Uncertain is enough
                // to decline pinning and decline resolving through it, without declaring the
                // checkout unwarmable on the strength of a list that could be out of date.
                EntryField::Unmodelled => {
                    map.next_value::<IgnoredAny>()?;
                    unmodelled_key = true;
                },
            }
        }
        // The three the server reports by REJECTING THE FILE: `Missing key: "file"`, `Missing key:
        // "directory"`, and `Missing key: "command" or "arguments"`. An empty invocation is not
        // among them — it is carried out on `configures_a_file` instead.
        if !file {
            return Err(de::Error::missing_field("file"));
        }
        if !directory {
            return Err(de::Error::missing_field("directory"));
        }
        if !invocation {
            return Err(de::Error::custom("entry has neither `command` nor `arguments`"));
        }
        // Unknowable ONLY when the node was not a string. `"file": ""` is a path this reader read
        // and found empty — a known non-governing entry — while `"file": 7` is one clangd renders
        // as its literal text and accepts but this reader cannot reconstruct. Collapsing the two
        // would let a database naming no translation unit count as `Governs::Unknown`, which
        // warm-up accepts, so the checkout would warm a session that then skips every source.
        let file_text_unavailable = self.0.is_some() && !file_is_text;
        Ok(CompilationEntry {
            configures_a_file: arguments_configure.unwrap_or(command_configures),
            file_text_unavailable,
            unmodelled_key,
        })
    }
}

/// Any JSON scalar — string, number, boolean, or null. The value itself is discarded; only whether
/// it carries TEXT is kept, because an empty or whitespace-only `command` is what clangd turns into
/// an unparseable command line. Composites are refused by leaving `visit_seq`/`visit_map` to the
/// erroring defaults.
struct JsonScalar {
    /// Whether the scalar renders as blank text. Only a string can: clangd reads any other scalar
    /// as its literal text (`null`, `7`, `true`), which is a perfectly good command word.
    blank: bool,
    /// Whether the node was a STRING, and so whether a capture buffer now holds its text. A
    /// non-string scalar leaves the buffer empty for a different reason than `""` does: the first
    /// is a path this reader cannot render, the second is a path it read and found empty.
    is_string: bool,
}

impl<'de> Deserialize<'de> for JsonScalar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonScalarVisitor(None))
    }
}

/// Captures a scalar's text into an existing buffer, REUSING its capacity.
///
/// A seed rather than a `String` value so the governance read allocates nothing per entry: a
/// 120k-entry database would otherwise mint and drop two strings per entry while the maintenance
/// pass holds the repository write lock.
struct ScalarInto<'a>(&'a mut String);

impl<'de> de::DeserializeSeed<'de> for ScalarInto<'_> {
    type Value = JsonScalar;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(JsonScalarVisitor(Some(self.0)))
    }
}

struct JsonScalarVisitor<'a>(Option<&'a mut String>);

impl JsonScalarVisitor<'_> {
    /// Record the scalar's text when a caller asked for it. A NON-string scalar clears the buffer
    /// rather than leaving the previous entry's value in it: clangd reads such a node as its
    /// literal rendering (`null`, `7`), which is never a path worth testing against the corpus.
    fn capture(self, text: Option<&str>) {
        if let Some(buffer) = self.0 {
            buffer.clear();
            if let Some(text) = text {
                buffer.push_str(text);
            }
        }
    }
}

impl<'de> Visitor<'de> for JsonScalarVisitor<'_> {
    type Value = JsonScalar;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string, number, boolean, or null")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        self.capture(None);
        Ok(JsonScalar { blank: false, is_string: false })
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        self.capture(None);
        Ok(JsonScalar { blank: false, is_string: false })
    }

    fn visit_i128<E: de::Error>(self, _: i128) -> Result<Self::Value, E> {
        self.capture(None);
        Ok(JsonScalar { blank: false, is_string: false })
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        self.capture(None);
        Ok(JsonScalar { blank: false, is_string: false })
    }

    fn visit_u128<E: de::Error>(self, _: u128) -> Result<Self::Value, E> {
        self.capture(None);
        Ok(JsonScalar { blank: false, is_string: false })
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        self.capture(None);
        Ok(JsonScalar { blank: false, is_string: false })
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
        let blank = text.trim().is_empty();
        self.capture(Some(text));
        Ok(JsonScalar { blank, is_string: true })
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.capture(None);
        Ok(JsonScalar { blank: false, is_string: false })
    }
}

/// A JSON array of command words. Each element must be a scalar — clangd refuses a database whose
/// `arguments` holds an object or a nested array — but their values are discarded beyond whether
/// any of them carries text, since `[]` and `[""]` both produce an empty command line.
struct JsonSequence {
    carries_an_argument: bool,
}

impl<'de> Deserialize<'de> for JsonSequence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_seq(JsonSequenceVisitor)
    }
}

struct JsonSequenceVisitor;

impl<'de> Visitor<'de> for JsonSequenceVisitor {
    type Value = JsonSequence;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("compile arguments as an array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        // An empty array is READ rather than rejected — clangd loads such a database, it just
        // cannot analyse the file that entry names. The whole sequence is still consumed so a
        // composite element anywhere in it is caught.
        let mut carries_an_argument = false;
        while let Some(word) = seq.next_element::<JsonScalar>()? {
            carries_an_argument |= !word.blank;
        }
        Ok(JsonSequence { carries_an_argument })
    }
}

/// Reads a database, testing each entry's file against the corpus until one qualifies.
struct DatabaseSeed<'a>(&'a dyn super::IndexedCorpus);

impl<'de> de::DeserializeSeed<'de> for DatabaseSeed<'_> {
    type Value = CompilationDatabase;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(CompilationDatabaseVisitor(self.0))
    }
}

/// Reads the top-level array one entry at a time. A hand-written visitor rather than a
/// `Vec<CompilationEntry>` so that neither the entries nor their payloads are ever collected:
/// a real database is tens of megabytes, and nothing here needs to outlive the check.
struct CompilationDatabaseVisitor<'a>(&'a dyn super::IndexedCorpus);

impl<'de> Visitor<'de> for CompilationDatabaseVisitor<'_> {
    type Value = CompilationDatabase;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of compilation database entries")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut entries = 0usize;
        let mut unmodelled_key = false;
        let mut governs_indexed = false;
        let mut governs_unknown = false;
        // Reused across every entry, so the read allocates nothing in proportion to the file.
        let mut paths = EntryPaths::default();
        // The first entry `CompilationEntry` rejects ends the read: clangd would refuse the
        // database at that point too, so nothing later in the file could redeem it. An entry with
        // an EMPTY invocation is not such a rejection — it simply does not count, because it
        // configures no file while leaving the rest of the database working.
        loop {
            // Once one entry has qualified, the question is answered and the rest of the file is
            // read with the plain payload-discarding visitor — no capture, no corpus calls. That
            // is the mainstream case, where the very first entry names an indexed source; the
            // whole-file walk falls only on a database that governs nothing, which is the answer
            // being caught and is a vendored project's small database.
            let entry = if governs_indexed {
                seq.next_element::<CompilationEntry>()?
            } else {
                seq.next_element_seed(EntrySeed(&mut paths))?
            };
            let Some(entry) = entry else {
                break;
            };
            if entry.configures_a_file {
                entries += 1;
            }
            unmodelled_key |= entry.unmodelled_key;
            // `configures_a_file` is required, not incidental: an entry whose selected invocation
            // is empty names a file clangd cannot analyse at all, so it is no evidence that this
            // database describes anything the checkout indexes. Without the gate, a database whose
            // only indexed entry is degenerate — while some vendored entry carries a real command —
            // would be `Loadable` AND count as governing, and get pinned for a file it cannot
            // configure.
            if !governs_indexed {
                if entry.configures_a_file && paths.names_indexed_file(self.0) {
                    governs_indexed = true;
                } else if entry.file_text_unavailable && entry.configures_a_file {
                    // Its path could not be read, so this entry is no evidence either way — but
                    // only an entry that configures SOMETHING could have been evidence in the first
                    // place. Without that second condition a degenerate entry with an unreadable
                    // path makes the whole database `Unknown`, which `Trust::Possible` accepts: the
                    // backend then warms on a database whose every indexed source `Trust::Proven`
                    // goes on to skip.
                    governs_unknown = true;
                }
            }
        }
        Ok(CompilationDatabase { entries, unmodelled_key, governs_indexed, governs_unknown })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `MarkerSearch` over `checkout`, in the state `marker_sites` builds one. The scope is the
    /// caller's because the search borrows it.
    fn search_over<'a>(checkout: &'a CheckoutScope<'a>) -> MarkerSearch<'a> {
        MarkerSearch {
            marker: "compile_commands.json",
            checkout,
            scope: checkout.ceiling().to_path_buf(),
            visited_links: HashSet::new(),
            recorded_markers: HashSet::new(),
            found: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn an_entry_that_cannot_be_enumerated_makes_the_scan_incomplete() {
        // `ReadDir` reports a per-item failure as an `Err` element, and the entry behind it could
        // have been a directory holding the second database. Silently dropping it would leave the
        // scan claiming completeness it does not have, and `sole_marker_dir` would then pin the one
        // database it did see over files the missed one governs.
        let dir = rag_rat_base::test_scratch::ScratchDir::new("marker-entry-error");
        let checkout = crate::test_support::every_path_scope(&dir);
        let mut search = search_over(&checkout);

        let step = search.step_for_entry(Err(std::io::Error::other("enumeration failed")));
        assert!(matches!(step, Step::Indeterminate), "an unreadable entry is not a decision");
        assert!(!search.truncated, "step_for_entry classifies; the caller records");

        // The loop's disposition is what turns that into incompleteness, and what a complete scan
        // must NOT report — so both halves are asserted against a real checkout.
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join("build/compile_commands.json"), "[]").unwrap();
        assert!(
            marker_sites(&crate::test_support::every_path_scope(&dir), "compile_commands.json")
                .complete,
            "a checkout the walk read end to end is complete",
        );
    }
}
