#!/usr/bin/env bash
# Run after this job's builds, tests, and artifact uploads. Never sweep sibling runners here.
set -euo pipefail

target=${CARGO_TARGET_DIR:-}
case "$target" in
  "$HOME"/actions-runner*/cargo-target|"$HOME"/cargo-target-coverage) ;;
  *) echo "::error::Refusing unexpected runner target directory: $target"; exit 1 ;;
esac
[[ -d "$target" ]] || exit 0
[[ $(realpath -- "$target") == "$target" ]] || {
  echo "::error::Refusing a symlinked runner target directory: $target"
  exit 1
}

export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
command -v cargo-sweep >/dev/null || env -u CARGO_TARGET_DIR cargo install --locked cargo-sweep

# Coverage shares a target across runners, and its lock spans compilation AND tests.
# Do not wait behind another long coverage job: its own final step will sweep the shared cache.
locks=()
if [[ "$target" == "$HOME/cargo-target-coverage" ]]; then
  locks+=("$HOME/cargo-target-coverage.lock")
fi
# Cargo versions use different lock files. Nonblocking acquisition avoids a lock-order
# deadlock with Cargo or scheduled maintenance; a held lock must never be bypassed.
shopt -s nullglob
for profile in "$target"/*/ "$target"/*/*/; do
  for name in .cargo-lock .cargo-build-lock .cargo-artifact-lock; do
    [[ -e "$profile$name" ]] && locks+=("$profile$name")
  done
done
command=(env CARGO_TARGET_DIR="$target" cargo sweep --maxsize 20GB)
for ((i=${#locks[@]}-1; i>=0; i--)); do
  command=(flock --nonblock --conflict-exit-code 75 "${locks[i]}" "${command[@]}")
done

echo "Runner cache before: $(du -sh -- "$target")"
status=0
"${command[@]}" || status=$?
if [[ $status == 75 ]]; then
  echo "::warning::Runner cache cleanup skipped because a lock is busy: $target"
elif [[ $status != 0 ]]; then
  echo "::error::Runner cache cleanup failed: $target (exit $status)"
  exit "$status"
fi
echo "Runner cache after: $(du -sh -- "$target")"
