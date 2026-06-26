# Local reproduction of Glama's hosted MCP build for cq27-dev/rag-rat.
#
# Glama builds from its own Build Spec (base image + node + mcp-proxy, clone repo, then `buildSteps`,
# then `cmdArguments`), NOT from this file. This Dockerfile bundles the same steps into one runnable
# image so the Glama build can be reproduced and tested locally. The Build Spec's `buildSteps` must
# install a Rust toolchain and `cargo install` rag-rat (Glama's base has none), and `cmdArguments`
# must be `mcp-proxy -- rag-rat --config /workspace/rag-rat.toml mcp` — mcp-proxy spawns rag-rat as a
# stdio MCP server and exposes it at :8080/mcp (+/sse) with the /ping health check Glama polls.
#
# The server roots itself in a tiny sample repo with embeddings off, so introspection
# (initialize + tools/list, served from a static catalog) boots instantly — no model download,
# no outbound network.
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive \
    GLAMA_VERSION="1.0.0" \
    PYTHONUNBUFFERED=1 \
    RAG_RAT_NO_WATCH=1 \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH="/usr/local/cargo/bin:/app/node_modules/.bin:$PATH"

# node + mcp-proxy (Glama's harness); build-essential/pkg-config for rusqlite's bundled SQLite (cc);
# rustup toolchain pinned to satisfy rag-rat's 1.95+ MSRV.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config \
    && curl -fsSL https://deb.nodesource.com/setup_26.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g mcp-proxy@6.4.3 \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
         | sh -s -- -y --default-toolchain 1.96.0 --profile minimal \
    && apt-get clean && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

WORKDIR /app
# Track main HEAD, matching the Glama Build Spec's `pinnedCommit: null`.
RUN git clone --depth 1 https://github.com/cq27-dev/rag-rat .

# Hash-only build (no FastEmbed) -> small and offline; identical MCP tool surface for introspection.
RUN cargo install --locked --path crates/rag-rat-cli --bin rag-rat \
        --no-default-features --root /usr/local

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
