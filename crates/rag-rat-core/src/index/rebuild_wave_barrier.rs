use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// `Arc`, not `Box`: [`run_after_wave_commit`] clones the hook out of the registry and RELEASES
/// the map lock before calling it — the hook blocks on its test barrier, and holding the map
/// lock across that block would serialize (or deadlock) every other test's registration.
/// `Sync` is required by the shared `Arc`; hook captures that are `!Sync` (channel endpoints)
/// ride in `Mutex`es.
pub(crate) type WaveHook = Arc<dyn Fn() + Send + Sync + 'static>;

static AFTER_WAVE_COMMIT: Mutex<BTreeMap<PathBuf, WaveHook>> = Mutex::new(BTreeMap::new());

/// Unregisters its database's hook on drop — panic-safe cleanup, so a failing barrier test
/// can never leak a hook into a stranger's rebuild.
pub(crate) struct WaveBarrierGuard {
    database: PathBuf,
}

impl Drop for WaveBarrierGuard {
    fn drop(&mut self) {
        if let Ok(mut hooks) = AFTER_WAVE_COMMIT.lock() {
            hooks.remove(&self.database);
        }
    }
}

/// Register the after-wave-commit hook for the rebuild whose `config.database` is `database`.
/// Hold the returned guard for the duration of the observed rebuild.
#[must_use = "dropping the guard unregisters the hook"]
pub(crate) fn set_after_wave_commit(database: &Path, hook: WaveHook) -> WaveBarrierGuard {
    AFTER_WAVE_COMMIT
        .lock()
        .expect("wave barrier registry poisoned")
        .insert(database.to_path_buf(), hook);
    WaveBarrierGuard { database: database.to_path_buf() }
}

/// Invoked by the full-rebuild wave loop after each wave commits, with the rebuilding
/// connection's database path; fires only a hook registered for THAT database.
pub(crate) fn run_after_wave_commit(database: &Path) {
    let hook =
        AFTER_WAVE_COMMIT.lock().expect("wave barrier registry poisoned").get(database).cloned();
    if let Some(hook) = hook {
        hook();
    }
}
