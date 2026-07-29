//! What a live backend learns about a checkout's projects: the whole-checkout marker search, the
//! per-session layout it produces, and the compilation-database usability test both rely on.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
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
        // The scope the walk may not leave. A root that will not canonicalize is used as given:
        // the containment check then simply admits less, which is the safe direction.
        scope: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
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
        // A directory that is ITSELF a checkout belongs to that checkout, not this one. The common
        // case is a linked worktree kept inside the main checkout (`git worktree add
        // worktrees/feature`, or this repo's own `.claude/worktrees/<name>`), whose `.git` is a
        // FILE — so the name-based exclusion above never sees it, and the walk would count the
        // sibling's `compile_commands.json` as this checkout's. That flips a working
        // single-database checkout into multi-database mode: `sole_marker_dir` returns `None`,
        // `--compile-commands-dir` is dropped, and every source outside clangd's own
        // ancestor/`build/` search stops being resolvable. A nested clone or submodule is excluded
        // for the same reason, and by the same test — `.git` present, as either a file or a
        // directory. The search ROOT is exempt: it is entered directly, never through here.
        if path.join(".git").exists() {
            return None;
        }
        // `file_type` describes the LINK, and on Linux it is answered from the readdir result where
        // the filesystem reports it — so ordinary directories cost nothing here and only the rare
        // symlink is canonicalized. An ordinary subdirectory cannot leave the checkout, so only a
        // link is checked against `scope`; skipping a target already walked is what makes a
        // checkout with several self-referential links terminate quickly instead of exploring every
        // path through them. A link that cannot be canonicalized is not descended into at all.
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            let target = path.canonicalize().ok()?;
            if !target.starts_with(&self.scope) || !self.visited_links.insert(target) {
                return None;
            }
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
///
/// SYNTAX is where this is knowingly narrower than the server. clangd reads the file with clang's
/// YAML reader, and YAML is a superset of JSON, so it also loads databases carrying `#` comments,
/// trailing commas, or pure YAML block syntax — all of which `serde_json` refuses (see #1016).
/// Matching that needs a streaming YAML parser, which is a dependency decision rather than a patch.
/// The two cheap divergences are closed here instead: a UTF-8 BOM is skipped, and content after the
/// closing bracket is ignored, both of which clangd accepts and neither of which says anything
/// about whether the entries describe a loadable project.
fn marker_file_is_usable(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // `serde_json`'s reader deserializer pulls ONE BYTE at a time — measured unbuffered, a 2 MB
    // database costs two million `read` syscalls — so the buffer in front of it is what makes this
    // a sequential read.
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    if !skip_utf8_bom(&mut reader) {
        return false;
    }
    // `Deserializer::from_reader` + `deserialize` rather than `serde_json::from_reader`, which also
    // calls `end()` and so rejects anything after the top-level array. clangd stops at the closing
    // bracket and never looks further, so trailing content must not condemn the database.
    let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
    CompilationDatabase::deserialize(&mut deserializer).is_ok_and(|database| database.entries > 0)
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
    #[serde(other)]
    Other,
}

impl<'de> Deserialize<'de> for CompilationEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(CompilationEntryVisitor)
    }
}

struct CompilationEntryVisitor;

impl<'de> Visitor<'de> for CompilationEntryVisitor {
    type Value = CompilationEntry;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a compilation database entry")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let (mut file, mut directory, mut invocation) = (false, false, false);
        let mut configures_a_file = false;
        while let Some(field) = map.next_key::<EntryField>()? {
            match field {
                EntryField::File => {
                    map.next_value::<JsonScalar>()?;
                    file = true;
                },
                EntryField::Directory => {
                    map.next_value::<JsonScalar>()?;
                    directory = true;
                },
                EntryField::Command => {
                    // A non-string scalar is not blank: clangd reads the node as text, so `null`,
                    // `7`, and `true` each become a one-word command line it parses happily.
                    configures_a_file |= !map.next_value::<JsonScalar>()?.blank;
                    invocation = true;
                },
                EntryField::Arguments => {
                    configures_a_file |= map.next_value::<JsonSequence>()?.carries_an_argument;
                    invocation = true;
                },
                EntryField::Other => {
                    map.next_value::<IgnoredAny>()?;
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
        Ok(CompilationEntry { configures_a_file })
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
}

impl<'de> Deserialize<'de> for JsonScalar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonScalarVisitor)
    }
}

struct JsonScalarVisitor;

impl<'de> Visitor<'de> for JsonScalarVisitor {
    type Value = JsonScalar;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string, number, boolean, or null")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(JsonScalar { blank: false })
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(JsonScalar { blank: false })
    }

    fn visit_i128<E: de::Error>(self, _: i128) -> Result<Self::Value, E> {
        Ok(JsonScalar { blank: false })
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(JsonScalar { blank: false })
    }

    fn visit_u128<E: de::Error>(self, _: u128) -> Result<Self::Value, E> {
        Ok(JsonScalar { blank: false })
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(JsonScalar { blank: false })
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
        Ok(JsonScalar { blank: text.trim().is_empty() })
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsonScalar { blank: false })
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
        // The first entry `CompilationEntry` rejects ends the read: clangd would refuse the
        // database at that point too, so nothing later in the file could redeem it. An entry with
        // an EMPTY invocation is not such a rejection — it simply does not count, because it
        // configures no file while leaving the rest of the database working.
        while let Some(entry) = seq.next_element::<CompilationEntry>()? {
            if entry.configures_a_file {
                entries += 1;
            }
        }
        Ok(CompilationDatabase { entries })
    }
}
