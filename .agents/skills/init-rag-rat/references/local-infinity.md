# Local infinity via Docker (Connect mode)

Run [michaelfeil/infinity](https://github.com/michaelfeil/infinity) locally in Docker and point
rag-rat's `[llm.embedding.remote]` at it. Good when the user has Docker and wants a stronger /
code-specific embedder without paying for cloud. Default model: `jinaai/jina-embeddings-v2-base-code`
(768-dim, long context). See `remote-embeddings.md` for the pairing table and dim rules.

## 1. Write the compose file

Somewhere stable, e.g. `~/.rag-rat/infinity/docker-compose.yml`. **CPU variant** (works anywhere;
good for queries + incremental reconcile):

```yaml
services:
  infinity:
    image: michaelf34/infinity:latest-cpu
    container_name: rag-rat-infinity
    restart: unless-stopped
    ports:
      - "127.0.0.1:7997:7997"     # localhost only — never expose the embedder off-box
    volumes:
      - ./hf-cache:/app/.cache/huggingface   # persist ~640 MB weights across restarts
    environment:
      - HF_HOME=/app/.cache/huggingface
    command: ["v2", "--model-id", "jinaai/jina-embeddings-v2-base-code",
              "--port", "7997", "--engine", "torch", "--device", "cpu"]
```

**FOOTGUN — `--engine torch --device cpu` is required, not optional.** The `-cpu` image defaults to
the optimum/ONNX (OpenVINO) path, which **cannot compile jina-v2's gated-GLU MLP**
(`VariadicSplit … Default output not supported`). PyTorch-CPU (the sentence-transformers reference
impl) loads jina-code cleanly. Do not drop those args on the CPU image.

**GPU machine instead** — fast enough to serve the big reindex too:

```yaml
services:
  infinity:
    image: michaelf34/infinity:latest        # CUDA image (no -cpu)
    container_name: rag-rat-infinity
    restart: unless-stopped
    ports:
      - "127.0.0.1:7997:7997"
    volumes:
      - ./hf-cache:/app/.cache/huggingface
    environment:
      - HF_HOME=/app/.cache/huggingface
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: all
              capabilities: ["gpu"]
    # drop --engine torch --device cpu: the CUDA path handles jina-v2
    command: ["v2", "--model-id", "jinaai/jina-embeddings-v2-base-code", "--port", "7997"]
```

## 2. Bring it up and wait for readiness

```bash
docker compose -f ~/.rag-rat/infinity/docker-compose.yml up -d
# first boot downloads the model; retry until it answers 200:
until curl -sf http://localhost:7997/health >/dev/null; do sleep 2; done
```

## 3. The config block

```toml
[llm.embedding]
model = "jinaai/jina-embeddings-v2-base-code"

[llm.embedding.remote]
backend  = "infinity"
endpoint = "http://localhost:7997"
model    = "jinaai/jina-embeddings-v2-base-code"
```

## Keep it running (optional)

To survive logins, offer a user-systemd unit that runs the same compose on login, e.g.
`~/.config/systemd/user/rag-rat-infinity.service` with
`ExecStart=/usr/bin/docker compose -f %h/.rag-rat/infinity/docker-compose.yml up` (and
`WantedBy=default.target`, enabled with `systemctl --user enable --now rag-rat-infinity`).
