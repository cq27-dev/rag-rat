//! What a live backend learns about a checkout's projects: the whole-checkout marker search, the
//! per-session layout it produces, and the compilation-database usability test both rely on.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// What a live backend learned about a checkout's projects, resolved ONCE per session.
///
/// Every question the backend asks about layout — is this checkout usable, which database does the
/// session point at, which documents can it resolve — is answered from this one value. Re-deriving
/// them per call meant walking the whole checkout several times per spawn (proving there is no
/// SECOND database requires a full traversal), and the maintenance pass holds the repository write
/// lock while that happens.
#[derive(Debug, Clone, Default)]
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
    usable_markers: RefCell<HashMap<PathBuf, bool>>,
}

/// One marker location, and whether the file there describes a project the server can load.
#[derive(Debug, Clone)]
pub(super) struct MarkerSite {
    dir: PathBuf,
    usable: bool,
}

impl ProjectLayout {
    /// The layout for a checkout-scoped `marker`, from the sites one scan found.
    ///
    /// The scan's verdicts seed the memo, so a file whose nearest database is one of the scanned
    /// sites reuses the verdict the scan already paid for, and the two routes cannot disagree
    /// about the same file within one layout.
    pub(super) fn from_marker_sites(marker: &str, markers: Vec<MarkerSite>) -> Self {
        let usable_markers: HashMap<PathBuf, bool> =
            markers.iter().map(|site| (site.dir.join(marker), site.usable)).collect();
        Self { markers, usable_markers: RefCell::new(usable_markers) }
    }

    /// The single database this session can point the server at.
    ///
    /// `None` unless the checkout holds EXACTLY ONE database and it is usable. A second database
    /// disqualifies pinning even when it is empty or malformed: `--compile-commands-dir` is
    /// global, so pinning would hand the working database's flags to files that belong to the
    /// broken one, where clangd would otherwise stop at their own nearer database and fall back.
    /// Both are wrong for those files — but only pinning also makes them look configured.
    pub(super) fn sole_marker_dir(&self) -> Option<&Path> {
        match self.markers.as_slice() {
            [only] if only.usable => Some(&only.dir),
            _ => None,
        }
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
    fn marker_is_usable(&self, file: &Path) -> bool {
        // Borrow only for the lookup: the miss path reads the file and then takes a write borrow.
        let memoized = self.usable_markers.borrow().get(file).copied();
        if let Some(usable) = memoized {
            return usable;
        }
        let usable = marker_file_is_usable(file);
        self.usable_markers.borrow_mut().insert(file.to_path_buf(), usable);
        usable
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
        root: &Path,
        path: &Path,
        marker: &str,
    ) -> Option<PathBuf> {
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
                    return self.marker_is_usable(&file).then_some(candidate);
                }
            }
            if dir == root {
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
pub(super) fn marker_sites(root: &Path, marker: &str) -> Vec<MarkerSite> {
    let mut search = MarkerSearch {
        marker,
        visited_links: HashSet::new(),
        recorded_markers: HashSet::new(),
        found: Vec::new(),
    };
    search.descend(root, MARKER_SEARCH_MAX_DEPTH);
    search.found
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
}

impl MarkerSearch<'_> {
    fn descend(&mut self, dir: &Path, depth_left: u32) {
        if self.found.len() >= 2 {
            return;
        }
        self.record_marker_in(dir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut subdirectories: Vec<PathBuf> =
            entries.flatten().filter_map(|entry| self.searchable_subdirectory(&entry)).collect();
        subdirectories.sort();
        let Some(depth_left) = depth_left.checked_sub(1) else {
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
        let usable = marker_file_is_usable(&candidate);
        self.found.push(MarkerSite { dir: dir.to_path_buf(), usable });
    }

    /// The directory to descend into for `entry`, or `None` when it is not one this search may
    /// enter — excluded by name, not a directory, or a symlink whose target it has already walked.
    fn searchable_subdirectory(&mut self, entry: &std::fs::DirEntry) -> Option<PathBuf> {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !is_searchable_for_marker(&path, &name) {
            return None;
        }
        // `is_dir` FOLLOWS symlinks, unlike `DirEntry::file_type`: `build -> cmake-build-debug` is
        // an ordinary layout, and its database is reachable through the checkout path.
        if !path.is_dir() {
            return None;
        }
        // `file_type` describes the LINK, and on Linux it is answered from the readdir result where
        // the filesystem reports it — so ordinary directories cost nothing here and only the rare
        // symlink is canonicalized. Skipping a target already walked is what makes a checkout with
        // several self-referential links terminate quickly instead of exploring every path through
        // them; a link that cannot be canonicalized is not descended into at all.
        if entry.file_type().is_ok_and(|kind| kind.is_symlink())
            && !self.visited_links.insert(path.canonicalize().ok()?)
        {
            return None;
        }
        Some(path)
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
/// [`ProjectLayout::marker_is_usable`] instead, which remembers the verdict.
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
fn marker_file_is_usable(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // `serde_json`'s reader deserializer pulls ONE BYTE at a time — measured unbuffered, a 2 MB
    // database costs two million `read` syscalls — so the buffer in front of it is what makes this
    // a sequential read.
    let reader = std::io::BufReader::with_capacity(64 * 1024, file);
    serde_json::from_reader::<_, CompilationDatabase>(reader)
        .is_ok_and(|database| database.entries > 0)
}

/// A compilation database, read for its SHAPE alone: how many entries it holds, having checked
/// that each one carries what the format requires.
struct CompilationDatabase {
    entries: usize,
}

/// One compilation-database entry, with the fields the format REQUIRES. `clangd --check` rejects
/// an entry missing any of them (`Missing key: "directory"`, `Missing key: "command" or
/// "arguments"`) and falls back to generic flags for the whole checkout, so an entry naming only a
/// file is not a usable database however well-formed its JSON is.
///
/// Only the PRESENCE of each field is checked, so every payload is discarded while parsing —
/// `file` and `directory` are required by their types, and the invocation is checked in
/// [`CompilationDatabaseVisitor`] because either form satisfies it.
#[derive(Deserialize)]
struct CompilationEntry {
    #[allow(dead_code)]
    file: IgnoredAny,
    #[allow(dead_code)]
    directory: IgnoredAny,
    command: Option<IgnoredAny>,
    arguments: Option<IgnoredAny>,
}

impl<'de> Deserialize<'de> for CompilationDatabase {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_seq(CompilationDatabaseVisitor)
    }
}

/// Reads the top-level array one entry at a time. A hand-written visitor rather than a
/// `Vec<CompilationEntry>` so that neither the entries nor their payloads are ever collected:
/// a real database is tens of megabytes, and nothing here needs to outlive the check.
struct CompilationDatabaseVisitor;

impl<'de> Visitor<'de> for CompilationDatabaseVisitor {
    type Value = CompilationDatabase;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of compilation database entries")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut entries = 0usize;
        // The first incomplete entry ends the read: clangd would reject the database at that point
        // too, so there is nothing later in the file that could redeem it.
        while let Some(entry) = seq.next_element::<CompilationEntry>()? {
            if entry.command.is_none() && entry.arguments.is_none() {
                return Err(de::Error::custom("entry has neither `command` nor `arguments`"));
            }
            entries += 1;
        }
        Ok(CompilationDatabase { entries })
    }
}
