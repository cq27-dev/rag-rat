use std::collections::HashMap;

use rusqlite::Connection;

/// Options controlling the annotated directory tree builder.
#[derive(Debug, Clone)]
pub struct TreeOpts {
    /// Maximum depth from the repo root (0 = root only; default 6).
    pub max_depth: u8,
    /// A directory is included if its direct non-generated file count reaches this threshold
    /// (default 3).
    pub min_files: u32,
    /// Hard cap on the number of `TreeNode`s returned (default 30).
    pub max_nodes: usize,
}

impl Default for TreeOpts {
    fn default() -> Self {
        Self { max_depth: 6, min_files: 3, max_nodes: 30 }
    }
}

/// A single node in the annotated directory tree.
#[derive(Debug)]
pub struct TreeNode {
    /// Display nesting level: 0 for top-level displayed nodes; a node's children are always
    /// `parent.depth + 1`. This is the **displayed** ancestor count, not the filesystem depth.
    pub depth: u8,
    /// Display label — the node's `path` with the displayed parent's path prefix stripped (plus
    /// a leading `/`). For a non-collapsed node this is the single trailing segment (`"src"`).
    /// For a collapsed single-child chain anchor it spans multiple segments relative to the
    /// displayed parent (`"inner/deep"`). Use `depth` to indent and `label` to print; do not
    /// reconstruct the full path from these two fields — use `path` for that.
    pub label: String,
    /// Canonical repo-root-relative directory path (never collapsed, never modified).
    pub path: String,
    /// Number of non-generated indexed files whose *immediate* parent is this directory.
    pub file_count: u32,
    /// Title of the active `"dir"` repo memory anchored to this directory, if one exists.
    pub memory_title: Option<String>,
}

/// The annotated directory tree returned by [`dir_tree`].
#[derive(Debug)]
pub struct DirTree {
    /// Included nodes, ordered by path, capped at [`TreeOpts::max_nodes`].
    pub nodes: Vec<TreeNode>,
    /// Title of the active `"dir"` memory whose `binding_id` is `""` (the repo root), if any.
    pub root_memory_title: Option<String>,
    /// Count of nodes dropped because `max_nodes` was reached.
    pub truncated: u32,
}

/// Build an annotated directory tree from the scoped `files` view.
///
/// **Invariant**: the caller must have installed the per-connection scope view before calling
/// this function (via `crate::index::install_scope_view`). The query reads
/// `SELECT path FROM files WHERE generated = 0`, which resolves to the scoped temp view.
///
/// Algorithm:
/// 1. Count direct non-generated files per directory from `files` (scoped view).
/// 2. Include a dir if it has a "dir" memory OR direct `file_count >= opts.min_files`; also include
///    every ancestor up to `opts.max_depth` needed to connect included nodes.
/// 3. Collapse single-child chains: a dir with exactly one included child, no memory, and no direct
///    files folds into its child (rendered as one combined label).
/// 4. Compute `depth` as the number of displayed (non-absorbed) ancestors, and `label` as the
///    node's path relative to its displayed parent's path (strip prefix + leading `/`).
/// 5. Order by path; cap at `opts.max_nodes` (record `truncated` = dropped count).
/// 6. Annotate each node with `file_count` and `memory_title` from active "dir" memories.
pub fn dir_tree(conn: &Connection, opts: &TreeOpts) -> anyhow::Result<DirTree> {
    let dir_counts = file_counts_by_dir(conn)?;
    let mem_titles = dir_memory_titles(conn)?;
    let root_memory_title = mem_titles.get("").cloned();

    // Step 1: identify all directories that qualify on their own merits.
    // A dir qualifies if it has a dir memory OR direct_file_count >= min_files AND depth <=
    // max_depth.
    let mut included: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (dir, &count) in &dir_counts {
        let depth = dir_depth(dir);
        if depth > opts.max_depth as usize {
            continue;
        }
        if count >= opts.min_files || mem_titles.contains_key(dir.as_str()) {
            included.insert(dir.clone());
        }
    }
    // Dirs with a memory but zero direct files also qualify (no file-count entry).
    for dir in mem_titles.keys() {
        if dir.is_empty() {
            // root memory handled separately in root_memory_title
            continue;
        }
        let depth = dir_depth(dir);
        if depth <= opts.max_depth as usize {
            included.insert(dir.clone());
        }
    }

    // Step 2: add ancestors up to max_depth for every included dir.
    let qualified: Vec<String> = included.iter().cloned().collect();
    for dir in &qualified {
        for ancestor in ancestors(dir) {
            if dir_depth(&ancestor) <= opts.max_depth as usize {
                included.insert(ancestor);
            }
        }
    }

    // Step 3: collapse single-child chains.
    // Build the canonical label for each included dir: if it has exactly one included child
    // and has no memory and no direct files, fold it into its child.
    let included_vec: Vec<String> = {
        let mut v: Vec<String> = included.iter().cloned().collect();
        v.sort();
        v
    };
    // Compute included children for each dir.
    let child_counts = included_child_counts(&included_vec);
    // Collapsible: exactly one included child, no memory, no direct files.
    let is_collapsible = |dir: &str| -> bool {
        *child_counts.get(dir).unwrap_or(&0) == 1
            && !mem_titles.contains_key(dir)
            && *dir_counts.get(dir).unwrap_or(&0) == 0
    };

    // Walk the included set and decide which nodes to emit (post-collapse).
    // A dir is "absorbed" if it is consumed into a collapsing ancestor's label.
    let mut absorbed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // label_map: anchor dir path -> label string (path relative to displayed parent, computed
    // after collapse).
    let mut label_map: HashMap<String, String> = HashMap::new();
    // display_parent_map: anchor dir path -> the ANCHOR path of its displayed parent (or "" for
    // root). Used by compute_display_depths which keys depths by anchor.
    let mut display_parent_map: HashMap<String, String> = HashMap::new();
    // chain_end_map: anchor dir path -> chain-end path (deepest absorbed dir in the collapsed
    // chain). For non-collapsed nodes anchor == chain-end. Used to compute relative labels for
    // children: a child must strip the parent's chain-end prefix, not the anchor prefix.
    let mut chain_end_map: HashMap<String, String> = HashMap::new();

    for dir in &included_vec {
        if absorbed.contains(dir) {
            continue;
        }
        // Walk down the single-child chain, absorbing descendants.
        let mut chain_end = dir.clone();
        loop {
            if !is_collapsible(&chain_end) {
                break;
            }
            // Find the single included immediate child of `chain_end`.
            let child = included_vec
                .iter()
                .find(|d| immediate_parent(d).as_deref() == Some(chain_end.as_str()));
            let Some(child) = child else { break };
            absorbed.insert(child.clone());
            chain_end = child.clone();
        }
        // Compute the displayed parent: the nearest non-absorbed ancestor of `dir` in the
        // included set (after collapse, so also non-absorbed themselves). Returns the anchor
        // path of that ancestor (or "" for root-level nodes).
        let display_parent_anchor = displayed_parent(dir, &included_vec, &absorbed);
        display_parent_map.insert(dir.clone(), display_parent_anchor.clone());
        chain_end_map.insert(dir.clone(), chain_end.clone());

        // Compute label: `chain_end` path relative to the display parent's CHAIN-END path.
        // The display parent's chain-end is the deepest segment it absorbed; children must
        // strip that prefix (not the anchor prefix) to get a single leaf label.
        let display_parent_chain_end = chain_end_map
            .get(&display_parent_anchor)
            .cloned()
            .unwrap_or_else(|| display_parent_anchor.clone());
        let label = relative_label(&chain_end, &display_parent_chain_end);
        label_map.insert(dir.clone(), label);
    }

    // Step 4: build nodes for all non-absorbed included dirs, ordered by path.
    // Compute display depth from the display_parent_map by counting displayed ancestors.
    let display_depth_map = compute_display_depths(&included_vec, &absorbed, &display_parent_map);

    let mut nodes: Vec<TreeNode> = included_vec
        .iter()
        .filter(|dir| !absorbed.contains(*dir))
        .map(|dir| {
            let label = label_map.get(dir).cloned().unwrap_or_else(|| dir.clone());
            let depth = *display_depth_map.get(dir).unwrap_or(&0);
            let file_count = *dir_counts.get(dir.as_str()).unwrap_or(&0);
            let memory_title =
                if dir.is_empty() { None } else { mem_titles.get(dir.as_str()).cloned() };
            TreeNode { depth, label, path: dir.clone(), file_count, memory_title }
        })
        .collect();

    nodes.sort_by(|a, b| a.path.cmp(&b.path));

    // Step 5: cap at max_nodes.
    let total = nodes.len();
    let truncated = if total > opts.max_nodes {
        let dropped = (total - opts.max_nodes) as u32;
        nodes.truncate(opts.max_nodes);
        dropped
    } else {
        0
    };

    Ok(DirTree { nodes, root_memory_title, truncated })
}

/// READ: direct non-generated file count per immediate parent directory from the scoped
/// `files` view.
///
/// Invariant: only counts a file for its *immediate* parent (one `parent_dir()` call).
/// Files at the repo root (no `/` in their path) are counted under `""`.
/// Generated files (`generated = 1`) are excluded — they are noise for tree purposes.
fn file_counts_by_dir(conn: &Connection) -> anyhow::Result<HashMap<String, u32>> {
    let mut stmt = conn.prepare("SELECT path FROM files WHERE generated = 0")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut counts: HashMap<String, u32> = HashMap::new();
    for row in rows {
        let path = row?;
        let dir = immediate_parent(&path).unwrap_or_default();
        *counts.entry(dir).or_insert(0) += 1;
    }
    Ok(counts)
}

/// READ: active "dir" memory titles keyed by their directory path (`binding_id`).
///
/// Returns only `status = 'active'` memories with `binding_kind = 'dir'`.
fn dir_memory_titles(conn: &Connection) -> anyhow::Result<HashMap<String, String>> {
    // Scoped to the active repo (V042): a sibling repo's `dir` memory must not annotate this repo's
    // tree. `{repo_clause}` empty pre-A5 — see `schema::periphery_repo_scope`.
    let scope = crate::index::schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_clause = crate::index::schema::periphery_repo_scope_clause(&scope, "m");
    let mut stmt = conn.prepare(&format!(
        // Only active dir memories; binding_id is the normalized dir path (or "" for repo root).
        "SELECT b.binding_id, m.title
         FROM repo_memory_bindings b
         JOIN repo_memories m ON m.id = b.memory_id
         WHERE b.binding_kind = 'dir' AND m.status = 'active'{repo_clause}"
    ))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

// ─── path helpers ────────────────────────────────────────────────────────────

/// Return the immediate parent directory of a path, or `None` if it is at the repo root.
fn immediate_parent(path: &str) -> Option<String> {
    let idx = path.rfind('/')?;
    Some(path[..idx].to_string())
}

/// Depth of a directory relative to the repo root: `""` = 0, `"a"` = 1, `"a/b"` = 2, …
fn dir_depth(dir: &str) -> usize {
    if dir.is_empty() { 0 } else { dir.chars().filter(|&c| c == '/').count() + 1 }
}

/// All strict ancestors of `dir`, from the immediate parent up to (but not including) the repo
/// root `""`. The repo root is intentionally excluded; it is handled via `root_memory_title`.
fn ancestors(dir: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut cur = dir;
    while let Some(idx) = cur.rfind('/') {
        cur = &cur[..idx];
        result.push(cur.to_string());
    }
    result
}

/// For each included directory, count how many of the other included directories are its
/// *immediate* children.
fn included_child_counts(sorted: &[String]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for dir in sorted {
        if let Some(parent) = immediate_parent(dir)
            && sorted.binary_search(&parent).is_ok()
        {
            *counts.entry(parent).or_insert(0) += 1;
        }
    }
    counts
}

/// Return the path of the nearest included ancestor of `dir` that is itself not absorbed
/// (i.e., will be a displayed node). Returns `""` if there is no such ancestor (meaning `dir`
/// is a top-level displayed node).
fn displayed_parent(
    dir: &str,
    included: &[String],
    absorbed: &std::collections::HashSet<String>,
) -> String {
    let mut cur = dir;
    while let Some(idx) = cur.rfind('/') {
        cur = &cur[..idx];
        let cur_str = cur.to_string();
        if included.binary_search(&cur_str).is_ok() && !absorbed.contains(cur) {
            return cur_str;
        }
    }
    String::new()
}

/// Compute the label for a node at `chain_end_path` relative to a displayed parent at
/// `display_parent_path`. Strips the parent's path and a leading `/`.
fn relative_label(chain_end_path: &str, display_parent_path: &str) -> String {
    if display_parent_path.is_empty() {
        chain_end_path.to_string()
    } else {
        chain_end_path
            .strip_prefix(&format!("{display_parent_path}/"))
            .unwrap_or(chain_end_path)
            .to_string()
    }
}

/// Compute display depths for all non-absorbed nodes. A node's display depth is the number of
/// its displayed ancestors (non-absorbed included nodes that are ancestors of it).
fn compute_display_depths(
    included: &[String],
    absorbed: &std::collections::HashSet<String>,
    display_parent_map: &HashMap<String, String>,
) -> HashMap<String, u8> {
    // Iteratively resolve depths: for each anchor, depth = display_parent's depth + 1.
    // Since display_parent is always shallower in the filesystem, we process in sorted order
    // (shorter paths first) so parents are resolved before children.
    let mut depths: HashMap<String, u8> = HashMap::new();
    for dir in included {
        if absorbed.contains(dir) {
            continue;
        }
        let parent_path = display_parent_map.get(dir).map(|s| s.as_str()).unwrap_or("");
        let depth = if parent_path.is_empty() {
            0u8
        } else {
            depths.get(parent_path).copied().unwrap_or(0).saturating_add(1)
        };
        depths.insert(dir.clone(), depth);
    }
    depths
}
