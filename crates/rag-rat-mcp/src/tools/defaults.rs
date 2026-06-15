use super::*;

pub(crate) fn schema_for<T: schemars::JsonSchema>() -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| json!({"type": "object"}));
    strip_schema_metadata(&mut schema);
    schema
}

pub(crate) fn strip_schema_metadata(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            map.remove("title");
            for child in map.values_mut() {
                strip_schema_metadata(child);
            }
        },
        Value::Array(items) =>
            for item in items {
                strip_schema_metadata(item);
            },
        _ => {},
    }
}

pub(crate) fn optional_language(language: Option<String>) -> anyhow::Result<Option<Language>> {
    language.map(|value| Language::from_str(&value)).transpose().map_err(Into::into)
}

pub(crate) fn default_search_limit() -> u32 {
    10
}

pub(crate) fn default_search_graph_mode() -> McpGraphMode {
    McpGraphMode::Compact
}

pub(crate) fn default_search_graph_limit() -> u32 {
    3
}

pub(crate) fn default_repo_brief_mode() -> McpRepoBriefMode {
    McpRepoBriefMode::Spine
}

pub(crate) fn default_repo_brief_limit() -> u32 {
    10
}

pub(crate) fn default_min_cluster_size() -> u32 {
    2
}

pub(crate) fn default_read_chunk_graph_mode() -> McpGraphMode {
    // Compact by default: callers/callees summaries without the full imports + referenced-types
    // dump (pass `include_graph: "full"` when you need them). Keeps a chunk read focused on the
    // text plus high-signal graph context.
    McpGraphMode::Compact
}

pub(crate) fn default_read_chunk_graph_limit() -> u32 {
    20
}

pub(crate) fn default_symbol_limit() -> u32 {
    20
}

pub(crate) fn default_graph_limit() -> u32 {
    50
}

pub(crate) fn default_compare_limit() -> u32 {
    10_000
}
