//! Native CLI projection of the repository intelligence tool catalog from the canonical schema.
//!
//! MCP remains one transport for these tools, not their implementation boundary. The canonical
//! names, descriptions, JSON schemas, defaults, worktree scoping, read/write classification, and
//! handlers stay owned by `rag-rat-mcp`. This module derives a conventional Clap surface from that
//! catalog, converts supplied flags back into the canonical JSON argument object, and dispatches it
//! through `call_tool_for_config` in the same process. No MCP server or client is started.

use std::collections::{BTreeMap, BTreeSet};

use clap::builder::PossibleValuesParser;
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Map, Value, json};

use crate::commands::{apply_embedding_runtime_env, set_output_format};
use crate::load_config_or_hint;
use crate::render::print_output;

const ARGUMENTS_JSON: &str = "arguments-json";
const CONFIG: &str = "config";
const JSON_OUTPUT: &str = "json";
const SCHEMA: &str = "schema";

/// Run the `rag-rat tools` namespace. Catalog, help, and schema paths do not need a config. A tool
/// invocation enters the same config/runtime/logging setup as the rest of the CLI before calling
/// the canonical dispatcher by name.
pub(crate) fn run_tools(
    outer_config: Option<&str>,
    outer_json: bool,
    argv: &[String],
) -> anyhow::Result<()> {
    let command = tools_command();
    let mut args = Vec::with_capacity(argv.len() + 1);
    args.push("rag-rat tools".to_string());
    args.extend(argv.iter().cloned());

    let matches = match command.try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(err) if matches!(err.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            err.print()?;
            return Ok(());
        },
        Err(err) => err.exit(),
    };

    let json_output = outer_json || matches.get_flag(JSON_OUTPUT);
    set_output_format(if json_output {
        rag_rat_core::OutputFormat::Json
    } else {
        rag_rat_core::OutputFormat::Toon
    });

    let Some((cli_tool_name, tool_matches)) = matches.subcommand() else {
        debug_assert!(matches.get_flag(SCHEMA));
        return print_output(&rag_rat_mcp::tools::list_tools());
    };
    let tool_name = canonical_tool_name(cli_tool_name)
        .ok_or_else(|| anyhow::anyhow!("unknown repository intelligence tool `{cli_tool_name}`"))?;

    if matches.get_flag(SCHEMA) || tool_matches.get_flag(SCHEMA) {
        return print_output(&tool_metadata(tool_name));
    }

    let arguments = if let Some(raw) = tool_matches.get_one::<String>(ARGUMENTS_JSON) {
        parse_json_object(ARGUMENTS_JSON, raw)?
    } else {
        arguments_from_matches(tool_name, tool_matches)?
    };

    let explicit_config = matches.get_one::<String>(CONFIG).map(String::as_str).or(outer_config);
    let config = load_config_or_hint(explicit_config)?;
    apply_embedding_runtime_env(&config.llm.embedding.runtime);
    let _log = rag_rat_base::logging::init_logging(
        &config,
        rag_rat_base::logging::Role::Cli(format!("tools_{}", cli_name(tool_name))),
    );

    let result =
        rag_rat_mcp::tools::call_tool_for_config(&config, tool_name, Value::Object(arguments))?;
    print_output(&result)
}

/// Build the complete native command tree from the same catalog MCP advertises through
/// `tools/list`. The visible CLI spelling uses kebab case. The canonical snake_case name is
/// retained only for dispatch so the wire/API contract never changes.
fn tools_command() -> Command {
    let mut command = Command::new("tools")
        .bin_name("rag-rat tools")
        .about("Invoke repository intelligence tools directly, without requiring MCP")
        .version(env!("RAG_RAT_VERSION"))
        .propagate_version(true)
        .long_about(
            "Invoke the same repository intelligence tool catalog exposed over MCP directly from \
             the CLI. Tool commands, descriptions, arguments, required fields, and enum values \
             are derived from the canonical MCP schemas; execution uses the same process \
             dispatcher and does not start an MCP server or client.",
        )
        .arg_required_else_help(true)
        .subcommand_required(false)
        .arg(
            Arg::new(CONFIG)
                .long(CONFIG)
                .value_name("PATH")
                .global(true)
                .help("Path to rag-rat.toml; otherwise use normal config discovery"),
        )
        .arg(
            Arg::new(JSON_OUTPUT)
                .long(JSON_OUTPUT)
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Emit JSON instead of the default TOON output"),
        )
        .arg(
            Arg::new(SCHEMA)
                .long(SCHEMA)
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Print the canonical tool catalog/schema instead of invoking a tool"),
        );

    for &tool_name in rag_rat_mcp::tools::TOOL_NAMES {
        command = command.subcommand(tool_command(tool_name));
    }
    command
}

fn tool_command(tool_name: &'static str) -> Command {
    let schema = rag_rat_mcp::tools::schema(tool_name);
    let required: BTreeSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    let cli_tool_name = cli_name(tool_name);
    let description = rag_rat_mcp::tools::description(tool_name);
    let mut command = Command::new(cli_tool_name.clone())
        .about(short_description(description))
        .long_about(description);
    if cli_tool_name != tool_name {
        // Keep the canonical MCP spelling as a hidden compatibility alias for automation while
        // showing idiomatic kebab case in CLI help.
        command = command.alias(tool_name);
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    let mut property_names = Vec::new();
    if let Some(properties) = properties {
        // serde_json's map order is not a public CLI contract. Sort flags so generated help is
        // deterministic across feature/build changes while required/default semantics remain in
        // the schema itself.
        let sorted: BTreeMap<&str, &Value> =
            properties.iter().map(|(name, value)| (name.as_str(), value)).collect();
        for (name, property) in sorted {
            property_names.push(name.to_string());
            command = command.arg(property_arg(&schema, name, property, required.contains(name)));
        }
    }

    command.arg(
        Arg::new(ARGUMENTS_JSON)
            .long(ARGUMENTS_JSON)
            .value_name("JSON")
            .conflicts_with_all(property_names)
            .help(
                "Pass the complete canonical JSON argument object directly; escape hatch for \
                 nested or otherwise awkward shell values",
            ),
    )
}

fn property_arg(root_schema: &Value, name: &str, property: &Value, required: bool) -> Arg {
    let spec = resolve_property(root_schema, property);
    let mut arg = Arg::new(name.to_string()).long(cli_name(name)).value_name(value_name(&spec));
    if let Some(help) = property_description(property, &spec) {
        arg = arg.help(help);
    }

    if required {
        arg = arg.required_unless_present_any([ARGUMENTS_JSON, SCHEMA]);
    }

    match spec.kind {
        PropertyKind::Boolean { default } =>
            if default {
                arg.action(ArgAction::Set)
                    .num_args(1)
                    .value_parser(clap::value_parser!(bool))
                    .value_name("BOOL")
                    .help_heading("Tool arguments")
            } else {
                arg.action(ArgAction::SetTrue)
            },
        PropertyKind::Array { values } => {
            let mut arg = arg
                .action(ArgAction::Append)
                .num_args(0..)
                .value_delimiter(',')
                .help_heading("Tool arguments");
            if !values.is_empty() {
                arg = arg.value_parser(PossibleValuesParser::new(values));
            }
            arg
        },
        PropertyKind::String { values } => {
            let mut arg = arg.action(ArgAction::Set).num_args(1).help_heading("Tool arguments");
            if !values.is_empty() {
                arg = arg.value_parser(PossibleValuesParser::new(values));
            }
            arg
        },
        PropertyKind::Integer { .. } | PropertyKind::Number | PropertyKind::Json =>
            arg.action(ArgAction::Set).num_args(1).help_heading("Tool arguments"),
    }
}

#[derive(Debug)]
struct PropertySpec {
    kind: PropertyKind,
    description: Option<String>,
}

#[derive(Debug)]
enum PropertyKind {
    String { values: Vec<String> },
    Integer { unsigned: bool },
    Number,
    Boolean { default: bool },
    Array { values: Vec<String> },
    Json,
}

fn resolve_property(root_schema: &Value, property: &Value) -> PropertySpec {
    let description = property.get("description").and_then(Value::as_str).map(str::to_string);

    if let Some(reference) = property.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve_ref(root_schema, reference)
    {
        return property_spec_from_fragment(
            root_schema,
            target,
            description
                .or_else(|| target.get("description").and_then(Value::as_str).map(str::to_string)),
        );
    }

    if let Some(any_of) = property.get("anyOf").and_then(Value::as_array)
        && let Some(non_null) = any_of.iter().find(|item| !is_null_schema(item))
    {
        if let Some(reference) = non_null.get("$ref").and_then(Value::as_str)
            && let Some(target) = resolve_ref(root_schema, reference)
        {
            return property_spec_from_fragment(
                root_schema,
                target,
                description.or_else(|| {
                    target.get("description").and_then(Value::as_str).map(str::to_string)
                }),
            );
        }
        return property_spec_from_fragment(root_schema, non_null, description);
    }

    property_spec_from_fragment(root_schema, property, description)
}

fn property_spec_from_fragment(
    root_schema: &Value,
    fragment: &Value,
    description: Option<String>,
) -> PropertySpec {
    let types: Vec<&str> = match fragment.get("type") {
        Some(Value::String(kind)) => vec![kind],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let enum_values = enum_values(fragment);
    let kind = if types.contains(&"boolean") {
        PropertyKind::Boolean {
            default: fragment.get("default").and_then(Value::as_bool).unwrap_or(false),
        }
    } else if types.contains(&"integer") {
        let unsigned = fragment
            .get("format")
            .and_then(Value::as_str)
            .is_some_and(|format| format.starts_with("uint"))
            || fragment.get("minimum").and_then(Value::as_i64).is_some_and(|minimum| minimum >= 0);
        PropertyKind::Integer { unsigned }
    } else if types.contains(&"number") {
        PropertyKind::Number
    } else if types.contains(&"array") {
        match string_array_values(root_schema, fragment) {
            Some(values) => PropertyKind::Array { values },
            None => PropertyKind::Json,
        }
    } else if types.contains(&"string") || !enum_values.is_empty() {
        PropertyKind::String { values: enum_values }
    } else {
        // Nested objects (`memory_* bind`) and intentionally polymorphic payloads remain lossless
        // JSON values rather than growing a second nested CLI schema that must be maintained
        // separately.
        PropertyKind::Json
    };
    PropertySpec { kind, description }
}

fn string_array_values(root_schema: &Value, fragment: &Value) -> Option<Vec<String>> {
    let items = fragment.get("items")?;
    let items = items
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| resolve_ref(root_schema, reference))
        .unwrap_or(items);
    let values = enum_values(items);
    let string_items =
        items.get("type").and_then(Value::as_str) == Some("string") || !values.is_empty();
    string_items.then_some(values)
}

fn enum_values(fragment: &Value) -> Vec<String> {
    if let Some(values) = fragment.get("enum").and_then(Value::as_array) {
        return values.iter().filter_map(Value::as_str).map(str::to_string).collect();
    }
    fragment
        .get("oneOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("const").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn resolve_ref<'a>(root_schema: &'a Value, reference: &str) -> Option<&'a Value> {
    let name = reference.strip_prefix("#/$defs/")?;
    root_schema.get("$defs")?.get(name)
}

fn is_null_schema(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("null")
}

fn property_description(property: &Value, spec: &PropertySpec) -> Option<String> {
    let mut description = spec.description.clone().unwrap_or_default();
    if let Some(default) = property.get("default").filter(|value| !value.is_object()) {
        if !description.is_empty() {
            description.push(' ');
        }
        description.push_str("[default: ");
        description.push_str(&compact_json(default));
        description.push(']');
    }
    (!description.is_empty()).then_some(description)
}

fn value_name(spec: &PropertySpec) -> &'static str {
    match spec.kind {
        PropertyKind::String { .. } => "VALUE",
        PropertyKind::Integer { .. } => "INTEGER",
        PropertyKind::Number => "NUMBER",
        PropertyKind::Boolean { .. } => "BOOL",
        PropertyKind::Array { .. } => "VALUE",
        PropertyKind::Json => "JSON",
    }
}

fn arguments_from_matches(
    tool_name: &str,
    matches: &ArgMatches,
) -> anyhow::Result<Map<String, Value>> {
    let schema = rag_rat_mcp::tools::schema(tool_name);
    let mut arguments = Map::new();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(arguments);
    };

    for (name, property) in properties {
        // Do not materialize schema defaults into the request: omission is meaningful for fields
        // such as `include`, and the canonical request structs own their defaults. Only explicit
        // command line input crosses the dispatcher boundary.
        if !matches.contains_id(name)
            || matches.value_source(name) != Some(clap::parser::ValueSource::CommandLine)
        {
            continue;
        }
        let spec = resolve_property(&schema, property);
        let value = match spec.kind {
            PropertyKind::Boolean { default } => {
                let value = if default {
                    *matches
                        .get_one::<bool>(name)
                        .ok_or_else(|| anyhow::anyhow!("missing value for --{}", cli_name(name)))?
                } else {
                    matches.get_flag(name)
                };
                Value::Bool(value)
            },
            PropertyKind::Array { .. } => {
                let values = matches
                    .get_many::<String>(name)
                    .map(|items| items.map(|item| Value::String(item.clone())).collect())
                    .unwrap_or_default();
                Value::Array(values)
            },
            PropertyKind::String { .. } => Value::String(required_string(matches, name)?),
            PropertyKind::Integer { unsigned } => {
                let raw = required_string(matches, name)?;
                if unsigned {
                    Value::Number(raw.parse::<u64>()?.into())
                } else {
                    Value::Number(raw.parse::<i64>()?.into())
                }
            },
            PropertyKind::Number => {
                let raw = required_string(matches, name)?;
                let parsed: f64 = raw.parse()?;
                let number = serde_json::Number::from_f64(parsed).ok_or_else(|| {
                    anyhow::anyhow!("--{} must be a finite JSON number", cli_name(name))
                })?;
                Value::Number(number)
            },
            PropertyKind::Json => {
                let raw = required_string(matches, name)?;
                serde_json::from_str(&raw).map_err(|err| {
                    anyhow::anyhow!("invalid JSON for --{}: {err}", cli_name(name))
                })?
            },
        };
        arguments.insert(name.clone(), value);
    }
    Ok(arguments)
}

fn required_string(matches: &ArgMatches, name: &str) -> anyhow::Result<String> {
    matches
        .get_one::<String>(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing value for --{}", cli_name(name)))
}

fn parse_json_object(flag: &str, raw: &str) -> anyhow::Result<Map<String, Value>> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|err| anyhow::anyhow!("invalid JSON for --{flag}: {err}"))?;
    value.as_object().cloned().ok_or_else(|| anyhow::anyhow!("--{flag} must be a JSON object"))
}

fn tool_metadata(tool_name: &str) -> Value {
    json!({
        "name": tool_name,
        "description": rag_rat_mcp::tools::description(tool_name),
        "inputSchema": rag_rat_mcp::tools::schema(tool_name),
    })
}

fn short_description(description: &str) -> String {
    const LIMIT: usize = 160;
    let compact = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= LIMIT {
        return compact;
    }

    let mut summary = String::new();
    for word in compact.split_whitespace() {
        let next_len =
            summary.chars().count() + usize::from(!summary.is_empty()) + word.chars().count();
        if next_len > LIMIT.saturating_sub(1) {
            break;
        }
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(word);
    }
    summary.push('…');
    summary
}

fn canonical_tool_name(cli_spelling: &str) -> Option<&'static str> {
    rag_rat_mcp::tools::TOOL_NAMES
        .iter()
        .copied()
        .find(|name| cli_name(name) == cli_spelling || *name == cli_spelling)
}

fn cli_name(name: &str) -> String {
    name.replace('_', "-")
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ArgMatches {
        let mut argv = vec!["rag-rat tools"];
        argv.extend_from_slice(args);
        tools_command().try_get_matches_from(argv).expect("parse generated tools command")
    }

    #[test]
    fn arguments_json_can_be_combined_with_global_output_and_config_flags() {
        let matches = parse(&[
            "--json",
            "--config",
            "/tmp/rag-rat.toml",
            "symbol-lookup",
            "--arguments-json",
            r#"{"symbol":"Foo"}"#,
        ]);
        assert!(matches.get_flag(JSON_OUTPUT));
        assert_eq!(
            matches.get_one::<String>(CONFIG).map(String::as_str),
            Some("/tmp/rag-rat.toml")
        );
        let (_, tool_matches) = matches.subcommand().expect("tool subcommand");
        assert_eq!(
            tool_matches.get_one::<String>(ARGUMENTS_JSON).map(String::as_str),
            Some(r#"{"symbol":"Foo"}"#)
        );
    }

    #[test]
    fn compact_help_does_not_treat_abbreviations_as_sentence_boundaries() {
        let description = "List edges, e.g. dependencies, and continue with more useful context \
                           after the abbreviation.";
        let summary = short_description(description);
        assert!(summary.contains("dependencies"));
        assert!(summary.contains("continue"));
    }

    #[test]
    fn generated_command_tree_satisfies_clap_invariants() {
        tools_command().debug_assert();
    }

    #[test]
    fn generated_catalog_has_every_canonical_tool_as_kebab_case() {
        let command = tools_command();
        let names: BTreeSet<_> = command
            .get_subcommands()
            .filter(|cmd| cmd.get_name() != "help")
            .map(|cmd| cmd.get_name().to_string())
            .collect();
        let expected: BTreeSet<_> =
            rag_rat_mcp::tools::TOOL_NAMES.iter().map(|name| cli_name(name)).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn primitive_enum_array_and_boolean_flags_round_trip_to_canonical_json() {
        let matches = parse(&[
            "semantic-search",
            "--query",
            "config reload",
            "--limit",
            "7",
            "--include",
            "git,papertrail",
            "--explain",
        ]);
        let (_, tool) = matches.subcommand().unwrap();
        let arguments = arguments_from_matches("semantic_search", tool).unwrap();
        assert_eq!(
            Value::Object(arguments),
            json!({
                "query": "config reload",
                "limit": 7,
                "include": ["git", "papertrail"],
                "explain": true,
            })
        );
    }

    #[test]
    fn bare_array_flag_represents_an_explicit_empty_array() {
        let matches = parse(&["symbol-lookup", "--symbol", "Thing", "--include"]);
        let (_, tool) = matches.subcommand().unwrap();
        let arguments = arguments_from_matches("symbol_lookup", tool).unwrap();
        assert_eq!(arguments.get("include"), Some(&json!([])));
    }

    #[test]
    fn non_string_arrays_fail_closed_to_json_valued_flags() {
        let root = json!({});
        let property = json!({"type":"array","items":{"type":"integer"}});
        let spec = resolve_property(&root, &property);
        assert!(matches!(spec.kind, PropertyKind::Json));
    }

    #[test]
    fn nested_json_arguments_remain_lossless_without_a_second_nested_schema() {
        let matches = parse(&[
            "memory-rebind",
            "--memory-id",
            "mem_123",
            "--bind",
            r#"{"path":"src/lib.rs","start_line":4,"end_line":8}"#,
        ]);
        let (_, tool) = matches.subcommand().unwrap();
        let arguments = arguments_from_matches("memory_rebind", tool).unwrap();
        assert_eq!(
            arguments.get("bind"),
            Some(&json!({"path":"src/lib.rs","start_line":4,"end_line":8}))
        );
    }

    #[test]
    fn arguments_json_is_a_lossless_escape_hatch() {
        let matches = parse(&[
            "find-callers",
            "--arguments-json",
            r#"{"symbol":"parse_config","include":[]}"#,
        ]);
        let (_, tool) = matches.subcommand().unwrap();
        let raw = tool.get_one::<String>(ARGUMENTS_JSON).unwrap();
        assert_eq!(
            Value::Object(parse_json_object(ARGUMENTS_JSON, raw).unwrap()),
            json!({"symbol":"parse_config","include":[]})
        );
    }
}
