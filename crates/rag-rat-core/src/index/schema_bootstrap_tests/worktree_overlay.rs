use super::*;

// Every fixture in `config_root` spells its root through a symlink, which needs developer mode or
// elevation on Windows. Gating the module — rather than each item inside it — keeps its helpers
// from becoming `dead_code` on a platform where none of its tests compile in.
#[cfg(unix)]
mod config_root;
mod delta_refresh;
mod lens_handles;
mod lifecycle;
mod logical_rebuild;
mod quiet_window;
mod relink;
mod visibility;

use logical_rebuild::{logical_rebuild_pending, logical_symbol_named};
use relink::logical_grouping_snapshot;
