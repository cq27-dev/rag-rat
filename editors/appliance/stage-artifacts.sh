#!/bin/sh
# Stage CI-built artifacts into the build context (used by the deploy workflow and by
# hand for local image builds).
set -eu
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "$here/../.." && pwd)

mkdir -p "$here/artifacts"
cp "$root/target/dist/rag-rat" "$here/artifacts/rag-rat"
vsix=$(ls -t "$root"/editors/vscode/rag-rat-lens*.vsix | head -1)
cp "$vsix" "$here/artifacts/rag-rat-lens.vsix"
echo "staged: artifacts/rag-rat, artifacts/rag-rat-lens.vsix (from $vsix)"
