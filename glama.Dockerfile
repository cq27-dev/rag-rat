# Local reproduction of Glama's hosted MCP build for cq27-dev/rag-rat.
#
# Glama builds from its own Build Spec (base image + node + mcp-proxy, then buildSteps, then
# cmdArguments), NOT from this file. This Dockerfile bundles the same steps into one runnable image so
# the Glama build can be reproduced + tested locally. buildSteps install the PREBUILT rag-rat CLI from
# npm (@rag-rat/bin — no Rust toolchain, no compile), and cmdArguments must be
# `mcp-proxy -- rag-rat --config /workspace/rag-rat.toml mcp` — mcp-proxy spawns rag-rat as a stdio
# MCP server at :8080/mcp (+/sse) with the /ping health check Glama polls.
#
# @rag-rat/bin is installed at BUILD time (its postinstall fetches the platform prebuilt), so the
# server roots itself in a tiny sample repo with embeddings off and boots instantly — no model
# download, no cargo compile, no outbound network at runtime.
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive \
    GLAMA_VERSION="1.0.0" \
    PYTHONUNBUFFERED=1 \
    RAG_RAT_NO_WATCH=1

# node + mcp-proxy (Glama's harness) + git (rag-rat roots itself in a git worktree) + xz-utils (the
# postinstall extracts cargo-dist's `.tar.xz` Linux archive — `tar` alone can't). No Rust toolchain /
# build-essential: rag-rat is the prebuilt @rag-rat/bin npm package. `npm i -g @rag-rat/bin` runs its
# postinstall, which downloads the platform binary (glibc >=2.38 — trixie has it; FastEmbed's ONNX
# Runtime is statically linked, so no libonnxruntime.so at runtime) and puts `rag-rat` on PATH.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils \
    && curl -fsSL https://deb.nodesource.com/setup_26.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g mcp-proxy@6.4.3 @rag-rat/bin@latest \
    && rag-rat --version \
    && apt-get clean && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Minimal sample repo the server roots itself in. Absolute paths so it's CWD-independent;
# model="none" (BM25-only) downloads nothing; watcher + crates.io version check disabled.
RUN mkdir -p /workspace/src \
    && printf '%s\n' \
        '[index]' 'root = "/workspace"' 'database = "/workspace/.rag-rat/index.sqlite"' '' \
        '[llm.embedding]' 'model = "none"' '' \
        '[watch]' 'enabled = false' '' \
        '[version_check]' 'enabled = false' '' \
        '[target_bindings]' 'rust = ["src"]' > /workspace/rag-rat.toml \
    && printf '%s\n' 'pub fn hello() {}' > /workspace/src/lib.rs \
    && git init -q /workspace
WORKDIR /workspace

# The fix vs. Glama's stock template: a non-empty start command after `--` (was `mcp-proxy -- ""`).
CMD ["mcp-proxy","--","rag-rat","--config","/workspace/rag-rat.toml","mcp"]
