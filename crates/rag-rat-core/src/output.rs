//! One source of truth for rendering rag-rat's structured results to text.
//!
//! Both surfaces — the `rag-rat` CLI and the MCP server — emit the SAME typed values; only the
//! wire encoding differs. TOON (Token-Oriented Object Notation) is the default because it is
//! materially denser than JSON on the uniform-row payloads that dominate rag-rat's output
//! (`find_callers`, `symbol_lookup`, `repo_clusters`, …), where it renders the tabular
//! `[N]{cols}:` form and drops ~34% of the tokens of compact JSON; on nested payloads it ties
//! compact JSON, so it never loses. JSON stays available as an opt-out (`--json` on the CLI) for
//! tooling that needs to parse the output with a JSON parser.

use serde::Serialize;

/// Wire encoding for a rendered result. Plain enum — the format is a per-invocation choice (a CLI
/// flag / the MCP default), never persisted, so it carries no `as_db_str` round-trip surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Token-Oriented Object Notation — the default; dense tabular form for uniform rows.
    #[default]
    Toon,
    /// Pretty-printed JSON — the opt-out for JSON-parsing consumers.
    Json,
}

/// Render a serializable value to text in the requested format.
///
/// TOON: encode via `toon_format`. A TOON encode error never aborts the command — it falls back to
/// compact JSON so the caller always gets parseable output (capability loss is surfaced as the
/// less-dense-but-honest format, not a panic or an empty string). JSON: pretty-printed, falling
/// back to compact only if pretty-printing somehow fails.
pub fn render<T: Serialize>(value: &T, format: OutputFormat) -> String {
    match format {
        OutputFormat::Toon => toon_format::encode(value, &toon_format::EncodeOptions::default())
            .unwrap_or_else(|_| render_compact_json(value)),
        OutputFormat::Json =>
            serde_json::to_string_pretty(value).unwrap_or_else(|_| render_compact_json(value)),
    }
}

/// Compact-JSON last resort. Used when the chosen encoder fails; itself falls back to an empty
/// JSON object string rather than panicking, so `render` is total.
fn render_compact_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use serde_json::Value;

    use super::{OutputFormat, render};

    /// A uniform-row payload mirrors the shape of `find_callers` / `symbol_lookup` results: a
    /// homogeneous array of flat objects carrying provenance fields. TOON must render this as the
    /// dense tabular `[N]{cols}:` form.
    #[derive(Serialize)]
    struct Caller {
        symbol: String,
        path: String,
        confidence: String,
        completeness_risk: bool,
    }

    #[derive(Serialize)]
    struct Callers {
        callers: Vec<Caller>,
    }

    fn sample() -> Callers {
        Callers {
            callers: vec![
                Caller {
                    symbol: "alpha".to_string(),
                    path: "src/a.rs".to_string(),
                    confidence: "high".to_string(),
                    completeness_risk: false,
                },
                Caller {
                    symbol: "beta".to_string(),
                    path: "src/b.rs".to_string(),
                    confidence: "low".to_string(),
                    completeness_risk: true,
                },
            ],
        }
    }

    #[test]
    fn toon_uniform_rows_render_tabular() {
        let out = render(&sample(), OutputFormat::Toon);
        // TOON's hallmark dense form for a uniform array: a length-tagged header listing the
        // shared columns, e.g. `callers[2]{symbol,path,confidence,completeness_risk}:`.
        assert!(out.contains("callers[2]{"), "expected tabular [N]{{cols}} header, got:\n{out}");
        // The column header carries every field name once (not repeated per row).
        assert!(out.contains("confidence"), "missing column header, got:\n{out}");
        assert!(out.contains("completeness_risk"), "missing column header, got:\n{out}");
        // Provenance values survive the encoding.
        assert!(out.contains("high"), "provenance value lost, got:\n{out}");
    }

    #[test]
    fn json_round_trips_via_serde() {
        let out = render(&sample(), OutputFormat::Json);
        let parsed: Value = serde_json::from_str(&out).expect("--json output must be valid JSON");
        let callers = parsed.get("callers").and_then(Value::as_array).expect("callers array");
        assert_eq!(callers.len(), 2);
        // Provenance fields survive into the JSON path too.
        assert_eq!(callers[0]["confidence"], "high");
        assert_eq!(callers[1]["completeness_risk"], true);
    }

    #[test]
    fn toon_preserves_all_provenance_fields() {
        // Both a confidence label and a completeness_risk flag must round-trip through TOON,
        // since downstream agents key on these provenance signals.
        let out = render(&sample(), OutputFormat::Toon);
        for needle in ["confidence", "completeness_risk", "high", "low"] {
            assert!(out.contains(needle), "TOON dropped `{needle}`, got:\n{out}");
        }
    }

    #[test]
    fn default_format_is_toon() {
        assert_eq!(OutputFormat::default(), OutputFormat::Toon);
    }
}
