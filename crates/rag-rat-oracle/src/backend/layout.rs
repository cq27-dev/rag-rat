//! What a live backend learns about a checkout's projects: the whole-checkout marker search, the
//! per-session layout it produces, and the compilation-database usability test both rely on.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    let mut search = MarkerSearch { marker, visited_links: HashSet::new(), found: Vec::new() };
    search.descend(root, MARKER_SEARCH_MAX_DEPTH);
    search.found
}

/// How deep the marker search descends — a backstop for a tree that is merely very deep, not the
/// cycle guard. [`MarkerSearch::visited_links`] is what keeps a symlink loop from re-entering a
/// directory the search has already descended into; a depth bound alone would still explore every
/// path through the loop, which is exponential in this bound once two links point at the same
/// ancestor. Deep enough for any real project layout.
const MARKER_SEARCH_MAX_DEPTH: u32 = 24;

/// One whole-checkout marker search: what it looks for, which symlinked directories it has already
/// descended into, and the sites found so far.
struct MarkerSearch<'a> {
    marker: &'a str,
    /// Canonical paths of the SYMLINKED directories already descended into. Following directory
    /// symlinks is what makes a symlinked build directory discoverable, and it is also the only
    /// way this walk can revisit a directory — so recording just those targets bounds the
    /// traversal without paying a canonicalize per ordinary entry.
    visited_links: HashSet<PathBuf>,
    /// Marker sites, collected until there are two — the only distinction any caller draws is
    /// "exactly one" versus "several", and a monorepo can hold hundreds.
    found: Vec<MarkerSite>,
}

impl MarkerSearch<'_> {
    fn descend(&mut self, dir: &Path, depth_left: u32) {
        if self.found.len() >= 2 {
            return;
        }
        let candidate = dir.join(self.marker);
        if candidate.exists() {
            let usable = marker_file_is_usable(&candidate);
            self.found.push(MarkerSite { dir: dir.to_path_buf(), usable });
        }
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
/// The case that matters is a syntactically valid database that names no translation unit — `[]`,
/// `{}`, or an array of objects without a `file` key. Measured, clangd emits no progress cycle at
/// all for one, so a checkout holding it can never report ready and the backend would retry its
/// backlog forever while `oracle status` called it runnable.
///
/// The FIRST entry is parsed, not the whole file and not a byte pattern. A real database can be
/// tens of megabytes and this runs while the maintenance pass holds the repository write lock, so
/// parsing all of it is too expensive; scanning for a token is simply wrong in both directions —
/// it rejects a valid database whose first entry is larger than the window, and accepts a hollow
/// one that merely contains the token inside some unrelated string.
fn marker_file_is_usable(path: &Path) -> bool {
    /// One compilation-database entry, with the fields the format REQUIRES. `clangd --check`
    /// rejects an entry missing any of them (`Missing key: "directory"`, `Missing key: "command"
    /// or "arguments"`) and falls back to generic flags, so an entry naming only a file is not a
    /// usable database however well-formed its JSON is.
    ///
    /// Only the PRESENCE of each field is checked, so every payload is discarded while parsing —
    /// `file` and `directory` are required by their types, and the invocation is checked below
    /// because either form satisfies it.
    #[derive(serde::Deserialize)]
    struct CompilationEntry {
        #[allow(dead_code)]
        file: serde::de::IgnoredAny,
        #[allow(dead_code)]
        directory: serde::de::IgnoredAny,
        command: Option<serde::de::IgnoredAny>,
        arguments: Option<serde::de::IgnoredAny>,
    }

    // One entry is small even when the database is not, so a bounded prefix always contains it.
    const PREFIX_LIMIT: u64 = 1024 * 1024;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut prefix = Vec::new();
    if std::io::Read::read_to_end(&mut std::io::Read::take(file, PREFIX_LIMIT), &mut prefix)
        .is_err()
    {
        return false;
    }
    let text = String::from_utf8_lossy(&prefix);
    first_json_object(&text)
        .and_then(|object| serde_json::from_str::<CompilationEntry>(object).ok())
        .is_some_and(|entry| entry.command.is_some() || entry.arguments.is_some())
}

/// The first top-level JSON object in `text`, as a slice — brace-matched with string and escape
/// awareness so a `{` or `}` inside a compile command cannot end it early. `None` when the prefix
/// holds no complete object.
pub(super) fn first_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + ch.len_utf8()]);
                }
            },
            _ => {},
        }
    }
    None
}
