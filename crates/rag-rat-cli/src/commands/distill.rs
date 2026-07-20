//! Explicit distillation commands: deterministic extraction and the opt-in model queue drain.

use rag_rat_base::config::Config;

use crate::cli::{DistillArgs, DistillCommand};
use crate::open_index;
use crate::render::print_output;

pub(crate) fn distill(config: &Config, args: &DistillArgs) -> anyhow::Result<()> {
    match &args.command {
        DistillCommand::Extract => {
            // `extract` is a WRITER (skeleton records + junctions + queue) that also reads the
            // symbol index for anchors, so it must serialize with indexing under the per-repo write
            // lock like every other CLI writer — otherwise a concurrent generation switch can pin
            // the scope views to a stale generation and mine anchors from stale symbols. Held for
            // the whole pass.
            let lock_repo = rag_rat_base::locks::write_lock_repo_id(config);
            let _lock =
                rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
            let db = open_index(config)?;
            // Route through the shared renderer so the global `--json` flag is honored (the report
            // is `Serialize`); TOON otherwise.
            print_output(&db.distill_extract()?)
        },
        DistillCommand::Drain { limit } => drain(config, *limit),
    }
}

fn drain(config: &Config, limit: u32) -> anyhow::Result<()> {
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(config);
    let _entry_flight_lock = rag_rat_base::locks::FileLock::acquire_blocking(
        &rag_rat_base::locks::distill_lock_path(&config.database, &lock_repo),
    )?;

    // Keep the ordinary writer lock short: extraction and the prepared-work decision need it, but
    // provisioning and inference can take minutes and are serialized by the flight lock instead.
    let (pending, active_repo_id) = {
        let _write_lock =
            rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
        let db = open_index(config)?;
        db.distill_extract()?;
        let pending = db.distill_pending_count()?;
        (pending, db.active_repo_id.clone())
    };
    // An identity upgrade during open can change the final repo discriminator. Retain both flight
    // locks in that rare case so neither old-identity nor new-identity callers can overlap us.
    let _resolved_flight_lock = (active_repo_id != lock_repo)
        .then(|| {
            rag_rat_base::locks::FileLock::acquire_blocking(
                &rag_rat_base::locks::distill_lock_path(&config.database, &active_repo_id),
            )
        })
        .transpose()?;

    if pending == 0 {
        return print_output(&rag_rat_core::distill::DistillDrainReport::default());
    }
    if !config.llm.distill.enabled {
        anyhow::bail!(
            "distill work is pending but `[llm.distill] enabled = false`; set `[llm.distill] \
             enabled = true` to run the model drain"
        );
    }

    // Reopen after dropping the short writer scope. In particular, do not retain an identity-
    // upgrade WriteLock stored by the first config-bearing open across provisioning or inference.
    let db = open_index(config)?;
    anyhow::ensure!(
        db.active_repo_id == active_repo_id,
        "the repo identity changed while preparing the distill drain; re-run the command"
    );
    let remote = &config.llm.distill.remote;
    let mut _provisioned = None;
    let model = if remote.is_ephemeral() {
        let (model, provisioned) = rag_rat_llm::chat::provision_chat_model(remote)?;
        _provisioned = Some(provisioned);
        model
    } else {
        rag_rat_llm::chat::HttpChatModel::from_config(remote)?
    };
    print_output(&db.distill_drain(&model, limit)?)
}
