//! Local memory-stream authoring configuration. No transport surface lives here.

use rag_rat_base::config::Config;

use crate::cli::{SyncArgs, SyncCommand};
use crate::{open_index, print_output};

pub(crate) fn sync(config: &Config, args: &SyncArgs) -> anyhow::Result<()> {
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(config);
    let _lock = rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    let db = open_index(config)?;
    match args.command {
        SyncCommand::Enable => {
            let enabled = db.sync_enable()?;
            print_output(&serde_json::json!({
                "status": if enabled { "enabled" } else { "already_enabled" },
                "repo_id": db.active_repo_id,
                "sealed_local_authoring": true,
                "transport_configured": false,
                "note": "subsequent local memory changes are sealed; transport is not configured",
            }))
        },
    }
}
