use super::*;

#[test]
fn embedding_runtime_defaults_match_local_profile() {
    let runtime = EmbeddingRuntimeConfig::default();

    assert_eq!(runtime.batch_size, 64);
    assert_eq!(runtime.ort_threads, Some(4));
    assert_eq!(runtime.omp_threads, Some(1));
    assert_eq!(runtime.max_embedding_chars, 4000);
}

#[test]
fn parses_embedding_runtime_overrides() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."
            database = ".rag-rat/index.sqlite"

            [llm.embedding.runtime]
            batch_size = 128
            ort_threads = 2
            omp_threads = 1
            max_embedding_chars = 5000
            "#,
    )
    .unwrap();

    let llm = LlmConfig::try_from(raw.llm).unwrap();

    assert_eq!(llm.embedding.runtime, EmbeddingRuntimeConfig {
        batch_size: 128,
        ort_threads: Some(2),
        omp_threads: Some(1),
        max_embedding_chars: 5000,
    });
}

#[test]
fn remote_embedding_absent_is_none() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"
            "#,
    )
    .unwrap();
    let llm = LlmConfig::try_from(raw.llm).unwrap();
    assert_eq!(llm.embedding.remote, None, "no [remote] block → remote: None");
}

#[test]
fn remote_embedding_connect_happy_path_applies_defaults() {
    // CONNECT is inferred from `endpoint` being set (#318) — no `mode` field. The selector
    // names a real MODEL; the [remote] block serves it via Ollama.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            "#,
    )
    .unwrap();
    let llm = LlmConfig::try_from(raw.llm).unwrap();
    assert_eq!(
        llm.embedding.remote,
        Some(RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            endpoint: Some("http://localhost:11434".to_string()),
            cookbook: None,
            query_endpoint: None, // connect mode: no local query box
            auth_env: None,
            gpu: None,
            num_ctx: None,
            // defaults applied when omitted
            batch_size: 256,
            concurrency: 1,
            max_batch_chars: 384_000,
            request_timeout_s: 60,
        })
    );
    let remote = llm.embedding.remote.as_ref().unwrap();
    assert!(remote.is_connect() && !remote.is_ephemeral());
    // The selector still resolves to the LOCAL fastembed model — the [remote] block overrides
    // the RUNTIME, not the model identity.
    assert_eq!(llm.embedding.backend.model_id(), Some(crate::embedding_models::FASTEMBED_MODEL_ID));
}

#[test]
fn remote_embedding_ephemeral_infers_mode_and_defaults_query_endpoint() {
    // EPHEMERAL is inferred from `cookbook` being set; `query_endpoint` defaults to the local
    // Ollama when omitted (queries embed the same model → same vector space as remote chunks).
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
    assert!(remote.is_ephemeral() && !remote.is_connect());
    assert_eq!(remote.cookbook.as_deref(), Some("@rag-rat/cookbook/modal"));
    assert_eq!(remote.endpoint, None);
    assert_eq!(remote.query_endpoint.as_deref(), Some(config::DEFAULT_QUERY_ENDPOINT));
    assert_eq!(remote.concurrency, 32);
}

#[test]
fn remote_embedding_ephemeral_honors_explicit_query_endpoint() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "./recipe.mjs"
            query_endpoint = "http://127.0.0.1:11999"
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
    assert_eq!(remote.query_endpoint.as_deref(), Some("http://127.0.0.1:11999"));
}

#[test]
fn ephemeral_non_ollama_backend_requires_an_explicit_query_endpoint() {
    // The config::DEFAULT_QUERY_ENDPOINT is a local OLLAMA URL; it only fits `backend = ollama`. A
    // non-ollama ephemeral backend that omits `query_endpoint` must be REJECTED (not silently
    // defaulted), or after teardown queries embed against local Ollama with the wrong route /
    // model → silent BM25 fallback. See `RemoteQueryEndpointRequiredForBackend`.
    let build = |backend: &str, query_line: &str| {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "sentence-transformers/all-MiniLM-L6-v2"
                backend = "{backend}"
                cookbook = "@rag-rat/cookbook modal"
                {query_line}
                "#,
        ))
        .unwrap();
        LlmConfig::try_from(raw.llm).map(|l| l.embedding.remote.unwrap())
    };

    for backend in ["infinity", "vllm"] {
        let err = build(backend, "").unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::RemoteQueryEndpointRequiredForBackend { backend: b } if b == backend
            ),
            "{backend} ephemeral without query_endpoint → RemoteQueryEndpointRequiredForBackend, \
             got {err:?}",
        );
        // An explicit query_endpoint is accepted, and the backend is preserved for the query
        // path.
        let remote = build(backend, r#"query_endpoint = "http://127.0.0.1:7997""#).unwrap();
        assert_eq!(remote.query_endpoint.as_deref(), Some("http://127.0.0.1:7997"));
        assert_eq!(remote.backend.as_db_str(), backend);
    }
    // ollama still defaults (its default IS a local Ollama).
    assert_eq!(
        build("ollama", "").unwrap().query_endpoint.as_deref(),
        Some(config::DEFAULT_QUERY_ENDPOINT),
    );
}

#[test]
fn remote_embedding_ephemeral_gpu_is_parsed_and_trimmed() {
    // EPHEMERAL: `gpu` picks the GPU the cookbook recipe provisions. The value is
    // provider-specific (Modal class / RunPod gpuTypeId) and NOT validated against an
    // allow-list here — only trimmed.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            gpu = "  A10G  "
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
    assert_eq!(remote.gpu.as_deref(), Some("A10G"));
}

#[test]
fn remote_embedding_gpu_with_connect_endpoint_is_rejected() {
    // `gpu` only applies to ephemeral `cookbook` provisioning. Set alongside a connect
    // `endpoint` it is meaningless → rejected (not silently ignored).
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            gpu = "A10G"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteGpuRequiresCookbook),
        "gpu + endpoint → RemoteGpuRequiresCookbook, got {err:?}",
    );
}

#[test]
fn remote_embedding_empty_gpu_is_rejected() {
    // A present-but-empty/whitespace `gpu` is a config error — clearer than silently dropping a
    // key the user meant to set. (Omitting `gpu` entirely is fine: the recipe uses its
    // default.)
    for value in ["\"\"", "\"   \""] {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "all-minilm"
                cookbook = "@rag-rat/cookbook/modal"
                gpu = {value}
                "#,
        ))
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteGpuEmpty { .. }),
            "gpu={value} → RemoteGpuEmpty, got {err:?}",
        );
    }
}

#[test]
fn remote_embedding_overrides_batch_and_timeout() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            auth_env = "OLLAMA_TOKEN"
            num_ctx = 4096
            batch_size = 512
            concurrency = 16
            max_batch_chars = 128000
            request_timeout_s = 120
            "#,
    )
    .unwrap();
    let llm = LlmConfig::try_from(raw.llm).unwrap();
    assert_eq!(
        llm.embedding.remote,
        Some(RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            endpoint: Some("http://localhost:11434".to_string()),
            cookbook: None,
            query_endpoint: None,
            auth_env: Some("OLLAMA_TOKEN".to_string()),
            gpu: None,
            num_ctx: Some(4096),
            batch_size: 512,
            concurrency: 16,
            max_batch_chars: 128_000,
            request_timeout_s: 120,
        })
    );
}

#[test]
fn remote_embedding_zero_concurrency_and_char_budget_are_clamped() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            concurrency = 0
            max_batch_chars = 0
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
    assert_eq!(remote.concurrency, 1);
    assert_eq!(remote.max_batch_chars, 1);
}

#[test]
fn remote_embedding_rejects_oversized_concurrency() {
    let raw: RawConfig = toml::from_str(&format!(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            concurrency = {}
            "#,
        config::MAX_REMOTE_EMBEDDING_CONCURRENCY + 1
    ))
    .unwrap();

    let err = LlmConfig::try_from(raw.llm).expect_err("oversized concurrency should reject");
    assert!(matches!(
        err,
        ConfigError::RemoteEmbeddingConcurrencyTooHigh {
            value,
            max: config::MAX_REMOTE_EMBEDDING_CONCURRENCY
        } if value == config::MAX_REMOTE_EMBEDDING_CONCURRENCY + 1
    ));
}

#[test]
fn older_remote_embedding_meta_json_deserializes_with_legacy_safe_defaults() {
    let json = r#"{
            "model": "all-minilm",
            "endpoint": "http://localhost:11434",
            "cookbook": null,
            "query_endpoint": null,
            "auth_env": null,
            "gpu": null,
            "num_ctx": null,
            "batch_size": 256,
            "request_timeout_s": 60
        }"#;
    let remote: RemoteEmbeddingConfig = serde_json::from_str(json).unwrap();
    assert_eq!(remote.concurrency, 1);
    assert_eq!(remote.max_batch_chars, 384_000);
}

#[test]
fn remote_embedding_requires_exactly_one_of_endpoint_or_cookbook() {
    // Neither → no server to reach; both → ambiguous mode. Both reject with the exactly-one
    // rule.
    let neither = r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            "#;
    let both = r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            cookbook = "@rag-rat/cookbook/modal"
            "#;
    for (label, toml_str) in [("neither", neither), ("both", both)] {
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteEmbeddingModeAmbiguous),
            "{label} endpoint/cookbook → RemoteEmbeddingModeAmbiguous, got {err:?}",
        );
    }
}

#[test]
fn remote_embedding_endpoint_with_credentials_is_rejected() {
    // The endpoint is persisted whole into the index meta, so a `user:token@host` URL would
    // copy the credential into the index. Reject it and direct the user to `auth_env`.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "https://user:token@host:11434"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteEmbeddingEndpointHasCredentials),
        "endpoint with userinfo → RemoteEmbeddingEndpointHasCredentials, got {err:?}",
    );
}

#[test]
fn remote_embedding_endpoint_without_credentials_is_accepted() {
    // A plain host and a loopback endpoint both pass the userinfo guard (and an `@` in a path
    // is not userinfo).
    for endpoint in [
        "https://host:11434",
        "http://127.0.0.1:11434",
        "http://localhost:11434/v1/embeddings?user=a@b",
    ] {
        let raw: RawConfig = toml::from_str(&format!(
            "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
             \"sentence-transformers/all-MiniLM-L6-v2\"\n\n[llm.embedding.remote]\nmodel = \
             \"all-minilm\"\nendpoint = \"{endpoint}\"\n"
        ))
        .unwrap();
        let remote = LlmConfig::try_from(raw.llm)
            .unwrap_or_else(|e| panic!("`{endpoint}` must be accepted: {e:?}"))
            .embedding
            .remote
            .expect("remote block present");
        assert_eq!(remote.endpoint.as_deref(), Some(endpoint));
    }
}

#[test]
fn endpoint_authority_has_userinfo_classifies_urls() {
    assert!(config::endpoint_authority_has_userinfo("https://user:token@host:11434"));
    assert!(config::endpoint_authority_has_userinfo("http://u@127.0.0.1"));
    assert!(!config::endpoint_authority_has_userinfo("https://host:11434"));
    assert!(!config::endpoint_authority_has_userinfo("http://127.0.0.1:11434"));
    // An `@` in the PATH/query is not userinfo.
    assert!(!config::endpoint_authority_has_userinfo("http://host:11434/path?x=a@b"));
}

#[test]
fn resolve_relative_cookbook_path_anchors_relative_recipe_paths_to_config_dir() {
    let dir = Path::new("/repo/sub");

    // A path-shaped spec resolves its FIRST token against `config_dir` and preserves any
    // trailing provider args verbatim. The resolved token carries the platform-NATIVE
    // separator (`\` on Windows), so assert it as a `Path`, not a `String`: `Path`
    // equality is separator-agnostic on Windows and normalizes a mid-path `.` on every
    // OS, so one assertion holds cross-platform without hardcoding a separator
    // rendering.
    let anchored = |spec: &str| -> (PathBuf, String) {
        let out =
            config::resolve_relative_cookbook_path(spec, dir).expect("path-shaped spec resolves");
        match out.split_once(' ') {
            Some((path, rest)) => (PathBuf::from(path), rest.to_string()),
            None => (PathBuf::from(out), String::new()),
        }
    };

    let (path, rest) = anchored("./recipes/x.mts");
    assert_eq!(path, dir.join("./recipes/x.mts"));
    assert_eq!(rest, "");

    let (path, rest) = anchored("../cookbook.mjs modal");
    assert_eq!(path, dir.join("../cookbook.mjs"));
    assert_eq!(rest, "modal");

    // A bare relative `.ts`/`.mts`/`.js` path (no `./`) is still path-shaped → resolved.
    let (path, rest) = anchored("recipe.mts");
    assert_eq!(path, dir.join("recipe.mts"));
    assert_eq!(rest, "");

    // npm package specs and a bare token are LEFT VERBATIM (None).
    assert_eq!(config::resolve_relative_cookbook_path("@rag-rat/cookbook modal", dir), None);
    assert_eq!(config::resolve_relative_cookbook_path("some-pkg", dir), None);
    // An ALREADY-ABSOLUTE recipe path is left verbatim (None). Use a platform-absolute path: a
    // bare `/abs/...` is NOT absolute on Windows (no drive), so it wouldn't reach the
    // absolute-bailout branch there.
    #[cfg(windows)]
    let abs_recipe = r"C:\abs\recipe.mjs runpod";
    #[cfg(not(windows))]
    let abs_recipe = "/abs/recipe.mjs runpod";
    assert_eq!(config::resolve_relative_cookbook_path(abs_recipe, dir), None);

    // Drive-agnostic on Windows: a NON-C drive anchors the same way (the `E:` prefix survives
    // untouched). An absolute `E:\…` recipe is still left verbatim.
    #[cfg(windows)]
    {
        let out = config::resolve_relative_cookbook_path("./r/x.mts", Path::new(r"E:\proj"))
            .expect("relative recipe on a non-C drive resolves");
        assert_eq!(PathBuf::from(out), Path::new(r"E:\proj").join("./r/x.mts"));
        assert_eq!(config::resolve_relative_cookbook_path(r"E:\abs\recipe.mjs", dir), None);
    }
}

#[test]
fn remote_embedding_query_endpoint_with_credentials_is_rejected() {
    // The query_endpoint is persisted too, so userinfo in it is rejected the same as
    // `endpoint`.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            query_endpoint = "http://user:tok@127.0.0.1:11434"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteEmbeddingEndpointHasCredentials),
        "query_endpoint with userinfo → RemoteEmbeddingEndpointHasCredentials, got {err:?}",
    );
}

#[test]
fn remote_embedding_missing_model_is_rejected() {
    // The two `model` keys are distinct: `[llm.embedding] model` is the registry SELECTOR;
    // `[remote] model` is the Ollama API model name — it's the latter that's required here.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            endpoint = "http://localhost:11434"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteEmbeddingMissingModel),
        "omitted [remote] model → RemoteEmbeddingMissingModel, got {err:?}",
    );

    // A whitespace-only `[remote] model` trims to empty and is rejected the same way.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "   "
            endpoint = "http://localhost:11434"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteEmbeddingMissingModel),
        "whitespace-only [remote] model → RemoteEmbeddingMissingModel, got {err:?}",
    );
}

#[test]
fn remote_backend_parses_defaults_to_ollama_and_rejects_unknown() {
    let parse = |backend_line: &str| -> Result<RemoteEmbeddingConfig, ConfigError> {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "all-minilm"
                endpoint = "http://localhost:11434"
                {backend_line}
                "#
        ))
        .unwrap();
        LlmConfig::try_from(raw.llm).map(|llm| llm.embedding.remote.unwrap())
    };
    // Omitted → ollama (back-compat with pre-selector configs).
    assert_eq!(parse("").unwrap().backend, RemoteBackend::Ollama);
    // Explicit, case-insensitive.
    assert_eq!(parse(r#"backend = "infinity""#).unwrap().backend, RemoteBackend::Infinity);
    assert_eq!(parse(r#"backend = "VLLM""#).unwrap().backend, RemoteBackend::Vllm);
    // Unknown → a clear config error naming the bad value.
    let err = parse(r#"backend = "tgi""#).unwrap_err();
    assert!(
        matches!(&err, ConfigError::RemoteBackendUnknown { got, .. } if got == "tgi"),
        "got {err:?}"
    );
}

#[test]
fn remote_backend_db_str_round_trips_and_matches_serde() {
    for b in [RemoteBackend::Ollama, RemoteBackend::Infinity, RemoteBackend::Vllm] {
        assert_eq!(RemoteBackend::from_db_str(b.as_db_str()), Some(b));
        // The serde repr (persisted into the index meta) MUST equal `as_db_str` (the runtime
        // marker + freshness/tune-key discriminator) so the two representations never drift.
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(json, format!("\"{}\"", b.as_db_str()));
    }
    assert_eq!(RemoteBackend::from_db_str("nope"), None);
}

#[test]
fn remote_backend_embed_path_is_per_backend() {
    // ollama + vLLM expose the OpenAI-standard route; infinity's v2 server serves `/embeddings`
    // (verified live). Same request/response shape — only the path differs.
    assert_eq!(RemoteBackend::Ollama.embed_path(), "/v1/embeddings");
    assert_eq!(RemoteBackend::Vllm.embed_path(), "/v1/embeddings");
    assert_eq!(RemoteBackend::Infinity.embed_path(), "/embeddings");
}

#[test]
fn remote_backend_provision_timeout_is_longer_for_vllm() {
    // vLLM's ~10-15 GB image needs a longer cold-start ceiling than ollama/infinity, or it
    // times out on Modal. ollama/infinity share the shorter default.
    assert_eq!(
        RemoteBackend::Ollama.provision_timeout(),
        RemoteBackend::Infinity.provision_timeout()
    );
    assert!(
        RemoteBackend::Vllm.provision_timeout() > RemoteBackend::Infinity.provision_timeout(),
        "vLLM must get a longer provisioning ceiling than infinity",
    );
}

#[test]
fn remote_block_on_a_non_transformer_model_is_rejected() {
    // #317 rework guardrail: Ollama can only serve transformer models. A [remote] block on the
    // static model2vec, the hash model, or `none` (embeddings disabled) is a misconfiguration —
    // reject at parse with a clear message rather than leaving a remote block that never
    // installs/provisions anything. Selectors are the HF-path model_ids now.
    for model in ["minishlab/potion-retrieval-32M", "embedding-hash", "none"] {
        let raw: RawConfig = toml::from_str(&format!(
                "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
                 \"{model}\"\n\n[llm.embedding.remote]\nmodel = \"all-minilm\"\nendpoint = \
                 \"http://localhost:11434\"\n"
            ))
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteEmbeddingNonTransformerModel(_)),
            "remote block + {model} → RemoteEmbeddingNonTransformerModel, got {err:?}",
        );
    }
}

#[test]
fn the_renamed_local_ai_table_is_rejected_with_a_migration_message() {
    // #317 renamed [local_ai] → [llm]. An old config's [local_ai] table must error LOUDLY:
    // serde would otherwise silently DROP it, reverting embedding settings to defaults on
    // upgrade. The error fires in Config::load before any directory resolution.
    let tmp = scratch("localai");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[local_ai.embedding]\nmodel = \"none\"\n",
    )
    .unwrap();
    let err = Config::load(tmp.join("rag-rat.toml")).unwrap_err();
    assert!(
        matches!(err, ConfigError::LocalAiTableRenamed),
        "[local_ai] table → LocalAiTableRenamed, got {err:?}",
    );
}

#[test]
fn the_legacy_dream_table_is_rejected_with_a_migration_message() {
    // The dream model config moved from [dream.model] → [llm.dream]. An old config's top-level
    // [dream] table must error LOUDLY: serde would otherwise silently DROP it, so an upgrade
    // from `[dream.model] enabled = true` would run the deterministic passes only (never the
    // model). Fires in Config::load before any directory resolution.
    let tmp = scratch("dream");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[dream.model]\nenabled = true\n",
    )
    .unwrap();
    let err = Config::load(tmp.join("rag-rat.toml")).unwrap_err();
    assert!(
        matches!(err, ConfigError::DreamTableMoved),
        "[dream] table → DreamTableMoved, got {err:?}",
    );
}

#[test]
fn remote_block_with_a_transformer_model_is_accepted() {
    // The inverse of the guardrail: the FastEmbed (transformer) HF-path models accept a
    // [remote] block.
    for model in [
        "sentence-transformers/all-MiniLM-L6-v2",
        "BAAI/bge-small-en-v1.5",
        "jinaai/jina-embeddings-v2-base-code",
    ] {
        let raw: RawConfig = toml::from_str(&format!(
                "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
                 \"{model}\"\n\n[llm.embedding.remote]\nmodel = \"all-minilm\"\nendpoint = \
                 \"http://localhost:11434\"\n"
            ))
        .unwrap();
        assert!(
            LlmConfig::try_from(raw.llm).is_ok(),
            "remote block + {model} (transformer) must be accepted",
        );
    }
}

#[test]
fn dream_absent_defaults_to_off_and_local_ollama_connect() {
    // No `[llm.dream]` at all → disabled, with a local-Ollama CONNECT serving default
    // (byte-for-byte the pre-migration `[dream.model]` default).
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"
            "#,
    )
    .unwrap();
    let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
    assert!(!dream.enabled, "the model pass is OFF by default");
    assert_eq!(dream.remote, RemoteDreamConfig::default());
    assert_eq!(dream.remote.backend, RemoteBackend::Ollama);
    assert_eq!(dream.remote.endpoint.as_deref(), Some("http://localhost:11434"));
    assert_eq!(dream.remote.model, "qwen3:4b-instruct");
    assert_eq!(dream.remote.request_timeout_s, 300);
    assert!(dream.remote.is_connect() && !dream.remote.is_ephemeral());
}

#[test]
fn dream_enabled_flag_without_remote_block_keeps_default_serving() {
    // `[llm.dream] enabled = true` with no `[llm.dream.remote]` still resolves to the default
    // (a local-Ollama connect) — dream has no in-process backend, so `remote` is never `None`.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true
            "#,
    )
    .unwrap();
    let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
    assert!(dream.enabled, "[llm.dream] enabled = true opts in");
    assert_eq!(dream.remote, RemoteDreamConfig::default());
}

#[test]
fn dream_remote_connect_happy_path_applies_defaults() {
    // CONNECT is inferred from `endpoint` being set.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true

            [llm.dream.remote]
            backend = "ollama"
            endpoint = "http://ollama.local:11434"
            model = "qwen3:8b"
            auth_env = "OLLAMA_TOKEN"
            request_timeout_s = 60
            "#,
    )
    .unwrap();
    let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
    assert!(dream.enabled);
    assert_eq!(dream.remote, RemoteDreamConfig {
        backend: RemoteBackend::Ollama,
        endpoint: Some("http://ollama.local:11434".to_string()),
        cookbook: None,
        model: "qwen3:8b".to_string(),
        gpu: None,
        auth_env: Some("OLLAMA_TOKEN".to_string()),
        request_timeout_s: 60,
        provision_timeout_s: None,
    });
    assert!(dream.remote.is_connect() && !dream.remote.is_ephemeral());
}

#[test]
fn distill_absent_defaults_to_off_and_the_validated_30b_ephemeral_box() {
    // No `[llm.distill]` → disabled, but its serving default is the *validated 30B ephemeral box*,
    // NOT dream's local-Ollama connect: distill's dense prompts need the big model, so the honest
    // default is the config validated on the full corpus. (Enabled stays false, so nothing is
    // provisioned until the operator opts in and the drain has work.)
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."
            "#,
    )
    .unwrap();
    let distill = LlmConfig::try_from(raw.llm).unwrap().distill;
    assert!(!distill.enabled, "the distill model pass is OFF by default");
    assert_eq!(distill.remote, RemoteDreamConfig::distill_default());
    assert_ne!(
        distill.remote,
        RemoteDreamConfig::default(),
        "distill must diverge from dream's local-Ollama default"
    );
    assert!(distill.remote.is_ephemeral() && !distill.remote.is_connect());
    assert_eq!(distill.remote.backend, RemoteBackend::Vllm);
    assert_eq!(distill.remote.model, "Qwen/Qwen3-30B-A3B-Instruct-2507-FP8");
    assert_eq!(distill.remote.gpu.as_deref(), Some("L40S"));
    // Cold provisioning PLUS at least one worst-case request must fit inside the cookbook box's
    // 30-min (1800 s) unconditional lifetime — otherwise a box that cold-starts near its budget
    // self-destructs mid-inference on its very first request.
    assert!(
        distill.remote.provision_timeout_s.unwrap() + distill.remote.request_timeout_s <= 1800,
        "provision budget + one request must fit under the 30-min box lifetime"
    );
    // The whole-`[llm]`-absent fallback (`DistillLlmConfig::default`) must agree with the
    // `.remote`-absent resolution above — same validated default from both paths.
    assert_eq!(DistillLlmConfig::default().remote, RemoteDreamConfig::distill_default());
}

#[test]
fn distill_and_dream_are_independent_gates() {
    // Enabling one pass must not enable the other — they are separate `[llm.*]` blocks.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.distill]
            enabled = true
            "#,
    )
    .unwrap();
    let llm = LlmConfig::try_from(raw.llm).unwrap();
    assert!(llm.distill.enabled, "[llm.distill] enabled = true opts in");
    assert!(!llm.dream.enabled, "distill's gate does not enable dream");
}

#[test]
fn distill_remote_ephemeral_parses_and_honors_the_provision_timeout_override() {
    // The distill default model is a 30B-class box whose weight pull can exceed the vLLM default
    // boot budget, so `[llm.distill.remote]` carries a `provision_timeout_s` override.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.distill]
            enabled = true

            [llm.distill.remote]
            backend = "vllm"
            cookbook = "@rag-rat/cookbook modal"
            model = "Qwen/Qwen3-30B-A3B-Instruct-2507-FP8"
            gpu = "L40S"
            provision_timeout_s = 1500
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().distill.remote;
    assert!(remote.is_ephemeral() && !remote.is_connect());
    assert_eq!(remote.backend, RemoteBackend::Vllm);
    assert_eq!(remote.model, "Qwen/Qwen3-30B-A3B-Instruct-2507-FP8");
    assert_eq!(remote.gpu.as_deref(), Some("L40S"));
    assert_eq!(remote.provision_timeout_s, Some(1500));
    // The override wins over the backend default (vLLM = 900s).
    assert_eq!(remote.resolved_provision_timeout(), std::time::Duration::from_secs(1500));
}

#[test]
fn distill_provision_timeout_below_the_backend_floor_is_rejected() {
    // An ephemeral override may only LENGTHEN the boot budget — a value under the vLLM floor (900s)
    // would starve the recipe, so it is rejected at parse time, naming the distill section.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.distill.remote]
            backend = "vllm"
            cookbook = "@rag-rat/cookbook modal"
            model = "Qwen/Qwen3-8B"
            provision_timeout_s = 60
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(
            &err,
            ConfigError::DreamRemoteProvisionTimeoutBelowFloor { section, got: 60, floor: 900, .. }
                if *section == "[llm.distill.remote]"
        ),
        "too-small override → below-floor error naming the distill section, got {err:?}",
    );
}

#[test]
fn distill_provision_timeout_override_is_ignored_in_connect_mode() {
    // Connect mode provisions nothing, so a small `provision_timeout_s` is simply unused, not
    // rejected — the floor check applies to ephemeral provisioning only.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.distill.remote]
            backend = "ollama"
            endpoint = "http://localhost:11434"
            model = "qwen3:8b"
            provision_timeout_s = 5
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().distill.remote;
    assert_eq!(remote.provision_timeout_s, Some(5), "connect mode keeps the value, unused");
}

#[test]
fn the_shared_backend_and_gpu_errors_name_the_distill_section() {
    // `RemoteBackendUnknown` / `RemoteGpuEmpty` are shared with the embedding parser; a distill
    // misconfig must still name `[llm.distill.remote]`, not the embedding block.
    let unknown_backend: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.distill.remote]
            backend = "tgi"
            endpoint = "http://localhost:8080"
            model = "some-model"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(unknown_backend.llm).unwrap_err().to_string();
    assert!(err.contains("[llm.distill.remote]"), "unknown-backend names the distill block: {err}");
    assert!(!err.contains("[llm.embedding.remote]"), "not the embedding block: {err}");

    let empty_gpu: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.distill.remote]
            backend = "vllm"
            cookbook = "@rag-rat/cookbook modal"
            model = "some-model"
            gpu = "   "
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(empty_gpu.llm).unwrap_err().to_string();
    assert!(err.contains("[llm.distill.remote]"), "empty-gpu names the distill block: {err}");
}

#[test]
fn resolved_provision_timeout_falls_back_to_the_backend_default() {
    // No override → the backend's own provision ceiling (vLLM = 900s here).
    let remote = RemoteDreamConfig {
        backend: RemoteBackend::Vllm,
        endpoint: None,
        cookbook: Some("@rag-rat/cookbook modal".to_string()),
        model: "Qwen/Qwen3-8B".to_string(),
        provision_timeout_s: None,
        ..RemoteDreamConfig::default()
    };
    assert_eq!(remote.resolved_provision_timeout(), RemoteBackend::Vllm.provision_timeout());
}

#[test]
fn a_bad_distill_remote_error_names_the_distill_section_not_dream() {
    // The shared `RemoteDreamConfig` validation is section-aware: a `[llm.distill.remote]`
    // misconfig must point at THAT block, never the `[llm.dream.remote]` the same parser serves
    // for dream.
    let distill_bad: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.distill.remote]
            endpoint = "http://localhost:11434"
            model = ""
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(distill_bad.llm).unwrap_err().to_string();
    assert!(err.contains("[llm.distill.remote]"), "distill error names its own section: {err}");
    assert!(!err.contains("[llm.dream.remote]"), "and not dream's: {err}");

    // Symmetry: the dream block still names `[llm.dream.remote]`.
    let dream_bad: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            endpoint = "http://localhost:11434"
            model = ""
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(dream_bad.llm).unwrap_err().to_string();
    assert!(err.contains("[llm.dream.remote]"), "dream error names its own section: {err}");
}

#[test]
fn dream_remote_ephemeral_infers_mode_and_parses_gpu() {
    // EPHEMERAL is inferred from `cookbook` being set; a vLLM backend serves chat, and `gpu` is
    // trimmed (not validated here). No `query_endpoint`/batching knobs exist for dream.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true

            [llm.dream.remote]
            backend = "vllm"
            cookbook = "@rag-rat/cookbook modal"
            gpu = "  A10G  "
            model = "Qwen/Qwen3-4B-Instruct-2507"
            request_timeout_s = 900
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().dream.remote;
    assert!(remote.is_ephemeral() && !remote.is_connect());
    assert_eq!(remote.backend, RemoteBackend::Vllm);
    assert_eq!(remote.cookbook.as_deref(), Some("@rag-rat/cookbook modal"));
    assert_eq!(remote.gpu.as_deref(), Some("A10G"), "gpu is trimmed");
    assert_eq!(remote.model, "Qwen/Qwen3-4B-Instruct-2507");
    assert_eq!(remote.request_timeout_s, 900);
}

#[test]
fn dream_remote_requires_a_non_empty_model() {
    for model_line in ["", "model = \"  \""] {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.dream.remote]
                endpoint = "http://localhost:11434"
                {model_line}
                "#,
        ))
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::DreamRemoteMissingModel { .. }),
            "model={model_line:?} → DreamRemoteMissingModel, got {err:?}",
        );
    }
}

#[test]
fn dream_remote_infinity_backend_cannot_serve_chat() {
    // `infinity` is embed-only; a dream remote on it is rejected at parse time.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            backend = "infinity"
            endpoint = "http://localhost:7997"
            model = "some-model"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(&err, ConfigError::DreamBackendCannotServeChat { backend, .. } if backend == "infinity"),
        "infinity → DreamBackendCannotServeChat, got {err:?}",
    );
}

#[test]
fn dream_remote_requires_exactly_one_of_endpoint_or_cookbook() {
    // Neither → no server to reach; both → ambiguous mode. Both reject with the exactly-one
    // rule.
    let neither = r#"
            [index]
            root = "."

            [llm.dream.remote]
            model = "qwen3:8b"
            "#;
    let both = r#"
            [index]
            root = "."

            [llm.dream.remote]
            model = "qwen3:8b"
            endpoint = "http://localhost:11434"
            cookbook = "@rag-rat/cookbook modal"
            "#;
    for (label, toml_str) in [("neither", neither), ("both", both)] {
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::DreamRemoteModeAmbiguous { .. }),
            "{label} endpoint/cookbook → DreamRemoteModeAmbiguous, got {err:?}",
        );
    }
}

#[test]
fn dream_remote_gpu_with_connect_endpoint_is_rejected() {
    // `gpu` only applies to ephemeral `cookbook` provisioning. Set alongside a connect
    // `endpoint` it is meaningless → rejected.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            endpoint = "http://localhost:11434"
            model = "qwen3:8b"
            gpu = "A10G"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::DreamRemoteGpuRequiresCookbook { .. }),
        "gpu + endpoint → DreamRemoteGpuRequiresCookbook, got {err:?}",
    );
}

#[test]
fn dream_remote_empty_gpu_is_rejected() {
    for value in ["\"\"", "\"   \""] {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.dream.remote]
                backend = "vllm"
                cookbook = "@rag-rat/cookbook modal"
                model = "Qwen/Qwen3-4B-Instruct-2507"
                gpu = {value}
                "#,
        ))
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteGpuEmpty { .. }),
            "gpu={value} → RemoteGpuEmpty, got {err:?}",
        );
    }
}

#[test]
fn dream_remote_endpoint_with_credentials_is_rejected() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            endpoint = "https://user:token@host:11434"
            model = "qwen3:8b"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::DreamRemoteEndpointHasCredentials { .. }),
        "endpoint with userinfo → DreamRemoteEndpointHasCredentials, got {err:?}",
    );
}

#[test]
fn dream_remote_unknown_backend_is_rejected() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            backend = "tgi"
            endpoint = "http://localhost:11434"
            model = "qwen3:8b"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(&err, ConfigError::RemoteBackendUnknown { got, .. } if got == "tgi"),
        "unknown backend → RemoteBackendUnknown, got {err:?}",
    );
}

#[test]
fn remote_backend_chat_capability_and_path() {
    assert!(RemoteBackend::Ollama.supports_chat());
    assert!(RemoteBackend::Vllm.supports_chat());
    assert!(!RemoteBackend::Infinity.supports_chat(), "infinity is embed-only");
    // The chat route is uniform across chat-capable backends; only serving differs.
    assert_eq!(RemoteBackend::Ollama.chat_path(), "/v1/chat/completions");
    assert_eq!(RemoteBackend::Vllm.chat_path(), "/v1/chat/completions");
}
