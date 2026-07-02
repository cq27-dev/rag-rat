//! Git-hook conflict choices + the chained-hook renderer used by the Integration step.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)] // The installer already supports these choices; the wizard conflict UI lands separately.
pub(crate) enum HookConflict {
    Skip,
    Overwrite,
    Chain,
    UninstallRagRatOnly,
    Abort,
}

pub(crate) fn render_chained_hook(original: &str, hook_name: &str) -> String {
    let body = original.trim_start();
    let inner = if body.starts_with("#!") {
        body.split_once('\n').map(|x| x.1).unwrap_or("").trim_start_matches('\n')
    } else {
        body
    };
    let rag = format!(
        "repo_root=\"$(git rev-parse --show-toplevel 2>/dev/null)\" || exit 0\ncd \"$repo_root\" \
         || exit 0\nunset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_PREFIX \
         GIT_NAMESPACE GIT_OBJECT_DIRECTORY \
         GIT_ALTERNATE_OBJECT_DIRECTORIES\nRAG_RAT_HOOK_DISABLE=1 rag-rat maintenance --trigger \
         {hook_name} --max-seconds 30 >\"${{TMPDIR:-/tmp}}/rag-rat-{hook_name}.log\" 2>&1 &"
    );
    format!("#!/bin/sh\n# Chained hook\n\n{inner}\n\n{rag}\n\nexit 0\n")
}
