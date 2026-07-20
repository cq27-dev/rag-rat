//! Strict model-output contract and the observable distill response ladder.

use std::fmt;

use rag_rat_llm::chat::{ChatModel, GuidedJson};
use rag_rat_papertrail::OutcomeStatus;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::prompts::{self, PromptBudget, PromptInput};

const RECORD_SCHEMA_NAME: &str = "papertrail_distill_record";
const MAX_ERROR_CHARS: usize = 240;

/// A rendered evidence-unit id. Models commonly emit `12`, `"12"`, or `"U12"`; no other
/// coercions are accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct CitationId(usize);

impl CitationId {
    pub(crate) fn get(self) -> usize {
        self.0
    }
}

impl<'de> Deserialize<'de> for CitationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(CitationIdVisitor)
    }
}

struct CitationIdVisitor;

impl Visitor<'_> for CitationIdVisitor {
    type Value = CitationId;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a non-negative integer, decimal string, or U-prefixed decimal string")
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        usize::try_from(value).map(CitationId).map_err(|_| E::custom("citation id overflows usize"))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let value =
            u64::try_from(value).map_err(|_| E::custom("citation id must be non-negative"))?;
        self.visit_u64(value)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let digits = value.strip_prefix('U').unwrap_or(value);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(E::custom("citation id string must be decimal or U-prefixed decimal"));
        }
        let value = digits.parse::<u64>().map_err(|_| E::custom("citation id overflows u64"))?;
        self.visit_u64(value)
    }
}

/// The typed counterpart of [`prompts::record_schema`]. Unknown fields are rejected at every
/// object level before the semantic visibility, bounds, and claim/citation checks run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordOutput {
    pub root_issue: Option<String>,
    pub root_cause_units: Vec<CitationId>,
    pub root_cause: Option<String>,
    pub root_cause_class: Option<String>,
    pub decision_units: Vec<CitationId>,
    pub decision: DecisionOutput,
    pub outcome_units: Vec<CitationId>,
    pub anchor_indices: Vec<usize>,
    pub outcome: OutcomeOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecisionOutput {
    pub chosen: Option<String>,
    pub rejected: Vec<RejectedAlternativeOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RejectedAlternativeOutput {
    pub alternative: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutcomeOutput {
    pub status: OutcomeStatus,
    pub summary: Option<String>,
}

/// Stable output-ladder tokens. The names map directly to the `rung_*` run-counter columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputRung {
    /// A guided model call was attempted.
    Guided,
    /// The guided call's raw reply passed strict typed parsing and validation.
    Serde,
    /// The single unguided retry passed strict typed parsing and validation.
    Unguided,
    /// The retry passed only after stripping one whole-response JSON markdown fence.
    Tolerant,
}

impl OutputRung {
    pub(crate) const VARIANTS: [Self; 4] =
        [Self::Guided, Self::Serde, Self::Unguided, Self::Tolerant];

    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Guided => "guided",
            Self::Serde => "serde",
            Self::Unguided => "unguided",
            Self::Tolerant => "tolerant",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "guided" => Ok(Self::Guided),
            "serde" => Ok(Self::Serde),
            "unguided" => Ok(Self::Unguided),
            "tolerant" => Ok(Self::Tolerant),
            _ => Err(anyhow::anyhow!("unknown output-rung token `{value}`")),
        }
    }
}

/// Additive counters suitable for accumulation into one `papertrail_distill_runs` row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct LadderStats {
    pub rung_guided: u64,
    pub rung_serde: u64,
    pub rung_unguided: u64,
    pub rung_tolerant: u64,
    pub failed: u64,
}

impl LadderStats {
    fn record(&mut self, rung: OutputRung) {
        match rung {
            OutputRung::Guided => self.rung_guided += 1,
            OutputRung::Serde => self.rung_serde += 1,
            OutputRung::Unguided => self.rung_unguided += 1,
            OutputRung::Tolerant => self.rung_tolerant += 1,
        }
    }

    pub(crate) fn accumulate(&mut self, other: Self) {
        self.rung_guided += other.rung_guided;
        self.rung_serde += other.rung_serde;
        self.rung_unguided += other.rung_unguided;
        self.rung_tolerant += other.rung_tolerant;
        self.failed += other.failed;
    }

    pub(crate) fn terminal_count(self) -> u64 {
        self.rung_serde + self.rung_unguided + self.rung_tolerant + self.failed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LadderResult {
    pub output: RecordOutput,
    /// Canonical JSON after tolerant citation ids have been normalized to integers.
    pub value: serde_json::Value,
    pub accepted_at: OutputRung,
    /// The exact reply that produced `output`.
    pub raw_reply: String,
    pub stats: LadderStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LadderFailure {
    /// Latest reply received from the model, suitable for the queue's `raw_reply` column.
    pub final_raw_reply: Option<String>,
    /// Ordered, whitespace-normalized diagnostics, each bounded to [`MAX_ERROR_CHARS`].
    pub errors: Vec<String>,
    pub stats: LadderStats,
}

/// Run the observable response ladder: one guided call, then exactly one unguided retry after any
/// guided call/parse/validation failure. Fence recovery applies only to the retry's whole response.
pub(crate) fn run_output_ladder(
    model: &dyn ChatModel,
    rendered_prompt: &str,
    schema: &serde_json::Value,
    input: &PromptInput,
    budget: &PromptBudget,
) -> Result<LadderResult, LadderFailure> {
    let mut stats = LadderStats::default();
    let mut errors = Vec::new();
    let mut latest_raw = None;

    stats.record(OutputRung::Guided);
    match model
        .complete_guided(rendered_prompt, Some(GuidedJson { name: RECORD_SCHEMA_NAME, schema }))
    {
        Ok(raw) => {
            latest_raw = Some(raw.clone());
            match parse_and_validate(&raw, input, budget) {
                Ok((output, value)) => {
                    stats.record(OutputRung::Serde);
                    return Ok(LadderResult {
                        output,
                        value,
                        accepted_at: OutputRung::Serde,
                        raw_reply: raw,
                        stats,
                    });
                },
                Err(error) => errors.push(bounded_error("guided reply", &error)),
            }
        },
        Err(error) => errors.push(bounded_error("guided call", &error.to_string())),
    }

    let retry_raw = match model.complete(rendered_prompt) {
        Ok(raw) => {
            latest_raw = Some(raw.clone());
            raw
        },
        Err(error) => {
            errors.push(bounded_error("unguided call", &error.to_string()));
            stats.failed = 1;
            return Err(LadderFailure { final_raw_reply: latest_raw, errors, stats });
        },
    };

    match parse_and_validate(&retry_raw, input, budget) {
        Ok((output, value)) => {
            stats.record(OutputRung::Unguided);
            return Ok(LadderResult {
                output,
                value,
                accepted_at: OutputRung::Unguided,
                raw_reply: retry_raw,
                stats,
            });
        },
        Err(error) => errors.push(bounded_error("unguided reply", &error)),
    }

    if let Some(stripped) = strip_whole_json_fence(&retry_raw) {
        match parse_and_validate(stripped, input, budget) {
            Ok((output, value)) => {
                stats.record(OutputRung::Tolerant);
                return Ok(LadderResult {
                    output,
                    value,
                    accepted_at: OutputRung::Tolerant,
                    raw_reply: retry_raw,
                    stats,
                });
            },
            Err(error) => errors.push(bounded_error("fence-stripped reply", &error)),
        }
    } else {
        errors
            .push("fence recovery: reply is not one whole `json` or unlabelled fence".to_string());
    }

    stats.failed = 1;
    Err(LadderFailure { final_raw_reply: latest_raw, errors, stats })
}

fn parse_and_validate(
    raw: &str,
    input: &PromptInput,
    budget: &PromptBudget,
) -> Result<(RecordOutput, serde_json::Value), String> {
    let output: RecordOutput =
        serde_json::from_str(raw).map_err(|error| format!("strict JSON parse failed: {error}"))?;
    let value = serde_json::to_value(&output)
        .map_err(|error| format!("typed output conversion failed: {error}"))?;
    prompts::validate_record_output(&value, input, budget)
        .map_err(|error| format!("record validation failed: {error}"))?;
    Ok((output, value))
}

fn strip_whole_json_fence(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```json\n")
        .or_else(|| trimmed.strip_prefix("```json\r\n"))
        .or_else(|| trimmed.strip_prefix("```\n"))
        .or_else(|| trimmed.strip_prefix("```\r\n"))?;
    let body = body.strip_suffix("\n```").or_else(|| body.strip_suffix("\r\n```"))?;
    (!body.contains("```")).then_some(body)
}

fn bounded_error(label: &str, detail: &str) -> String {
    let normalized = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let prefix = format!("{label}: ");
    let remaining = MAX_ERROR_CHARS.saturating_sub(prefix.chars().count());
    let mut bounded: String = normalized.chars().take(remaining).collect();
    if normalized.chars().count() > remaining && remaining >= 3 {
        for _ in 0..3 {
            bounded.pop();
        }
        bounded.push_str("...");
    }
    prefix + &bounded
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use rag_rat_llm::chat::{ChatModel, GuidedJson};

    use super::{LadderStats, OutputRung};
    use crate::distill::prompts::{
        AnchorContext, PromptBudget, PromptInput, PromptUnit, record_schema,
    };

    #[derive(Debug)]
    enum ScriptedReply {
        Ok(String),
        Err(String),
    }

    #[derive(Debug)]
    struct ScriptedChatModel {
        replies: Mutex<VecDeque<ScriptedReply>>,
        calls: Mutex<Vec<bool>>,
    }

    impl ScriptedChatModel {
        fn new(replies: impl IntoIterator<Item = ScriptedReply>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn guided_flags(&self) -> Vec<bool> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ChatModel for ScriptedChatModel {
        fn complete_guided(
            &self,
            _prompt: &str,
            guided: Option<GuidedJson<'_>>,
        ) -> anyhow::Result<String> {
            self.calls.lock().unwrap().push(guided.is_some());
            match self.replies.lock().unwrap().pop_front() {
                Some(ScriptedReply::Ok(reply)) => Ok(reply),
                Some(ScriptedReply::Err(error)) => Err(anyhow::anyhow!(error)),
                None => Err(anyhow::anyhow!("script exhausted")),
            }
        }

        fn model_id(&self) -> &str {
            "scripted"
        }
    }

    fn input() -> PromptInput {
        PromptInput {
            kind: "issue".to_string(),
            key: "1".to_string(),
            merged: false,
            title: "Title".to_string(),
            opened: "Body".to_string(),
            units: (0..=12)
                .map(|index| PromptUnit {
                    source: "issue".to_string(),
                    text: format!("Evidence {index}"),
                })
                .collect(),
            partner: None,
            xrefs: vec![],
            fix_commits: vec![],
            symbols: vec![],
            anchor_candidates: vec![AnchorContext {
                index: 3,
                kind: "file".to_string(),
                name: "src/lib.rs".to_string(),
                file: Some("src/lib.rs".to_string()),
                logical_symbol_id: None,
            }],
            diff: None,
        }
    }

    fn valid_json(citation: &str) -> String {
        format!(
            r#"{{"root_issue":null,"root_cause_units":[{citation}],"root_cause":"Cause","root_cause_class":"bug","decision_units":[{citation}],"decision":{{"chosen":"Fix","rejected":[]}},"outcome_units":[{citation}],"anchor_indices":[3],"outcome":{{"status":"landed","summary":"Landed"}}}}"#
        )
    }

    fn run(model: &ScriptedChatModel) -> Result<super::LadderResult, super::LadderFailure> {
        let input = input();
        let budget = PromptBudget::default();
        let schema = record_schema(&input, &budget);
        super::run_output_ladder(model, "prompt", &schema, &input, &budget)
    }

    #[test]
    fn citation_id_accepts_only_integer_decimal_and_u_decimal_forms() {
        for accepted in ["12", r#""12""#, r#""U12""#] {
            let parsed: super::CitationId = serde_json::from_str(accepted).unwrap();
            assert_eq!(parsed.get(), 12, "accepted form {accepted}");
            assert_eq!(serde_json::to_value(parsed).unwrap(), 12);
        }
        for rejected in [
            "-1",
            "1.0",
            r#""-1""#,
            r#"" 1""#,
            r#""1 ""#,
            r#""+1""#,
            r#""u1""#,
            r#""U""#,
            r#""1.0""#,
            r#""18446744073709551616""#,
        ] {
            assert!(
                serde_json::from_str::<super::CitationId>(rejected).is_err(),
                "rejected form {rejected}"
            );
        }
    }

    #[test]
    fn guided_reply_is_strictly_parsed_validated_and_normalized() {
        let model = ScriptedChatModel::new([ScriptedReply::Ok(valid_json(r#""U12""#))]);
        let result = run(&model).unwrap();
        assert_eq!(result.accepted_at, OutputRung::Serde);
        assert_eq!(result.output.root_cause_units[0].get(), 12);
        assert_eq!(result.value["root_cause_units"][0], 12);
        assert_eq!(result.stats, LadderStats {
            rung_guided: 1,
            rung_serde: 1,
            ..LadderStats::default()
        });
        assert_eq!(model.guided_flags(), [true]);
    }

    #[test]
    fn guided_call_failure_gets_exactly_one_unguided_retry() {
        let model = ScriptedChatModel::new([
            ScriptedReply::Err("guided unavailable".to_string()),
            ScriptedReply::Ok(valid_json("12")),
        ]);
        let result = run(&model).unwrap();
        assert_eq!(result.accepted_at, OutputRung::Unguided);
        assert_eq!(result.stats.rung_guided, 1);
        assert_eq!(result.stats.rung_unguided, 1);
        assert_eq!(model.guided_flags(), [true, false]);
    }

    #[test]
    fn guided_validation_failure_gets_exactly_one_unguided_retry() {
        let model = ScriptedChatModel::new([
            ScriptedReply::Ok(valid_json("99")),
            ScriptedReply::Ok(valid_json(r#""12""#)),
        ]);
        let result = run(&model).unwrap();
        assert_eq!(result.accepted_at, OutputRung::Unguided);
        assert_eq!(model.guided_flags(), [true, false]);
    }

    #[test]
    fn whole_response_json_or_unlabelled_fence_is_the_only_tolerant_form() {
        for opening in ["```json", "```"] {
            let fenced = format!("{opening}\n{}\n```", valid_json("12"));
            let model = ScriptedChatModel::new([
                ScriptedReply::Ok("bad".to_string()),
                ScriptedReply::Ok(fenced),
            ]);
            let result = run(&model).unwrap();
            assert_eq!(result.accepted_at, OutputRung::Tolerant);
            assert_eq!(result.stats.rung_tolerant, 1);
            assert_eq!(model.guided_flags(), [true, false]);
        }
    }

    #[test]
    fn prose_language_and_multiple_fences_are_rejected_without_a_third_call() {
        for reply in [
            format!("prose\n```json\n{}\n```", valid_json("12")),
            format!("```JSON\n{}\n```", valid_json("12")),
            format!("```json\n{}\n```\nmore", valid_json("12")),
            format!("```json\n{}\n```\n```", valid_json("12")),
        ] {
            let model = ScriptedChatModel::new([
                ScriptedReply::Ok("bad guided".to_string()),
                ScriptedReply::Ok(reply.clone()),
                ScriptedReply::Ok(valid_json("12")),
            ]);
            let failure = run(&model).unwrap_err();
            assert_eq!(failure.final_raw_reply.as_deref(), Some(reply.as_str()));
            assert_eq!(failure.stats.failed, 1);
            assert_eq!(model.guided_flags(), [true, false]);
        }
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_object_level() {
        let cases = [
            valid_json("12").replace(r#""root_issue":null"#, r#""extra":1,"root_issue":null"#),
            valid_json("12").replace(r#""chosen":"Fix""#, r#""extra":1,"chosen":"Fix""#),
            valid_json("12").replace(
                r#""rejected":[]"#,
                r#""rejected":[{"alternative":"A","reason":null,"extra":1}]"#,
            ),
            valid_json("12").replace(r#""status":"landed""#, r#""extra":1,"status":"landed""#),
        ];
        for json in cases {
            let input = input();
            let error =
                super::parse_and_validate(&json, &input, &PromptBudget::default()).unwrap_err();
            assert!(error.contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn final_failure_keeps_latest_raw_and_bounds_errors() {
        let huge_error = "x".repeat(1_000);
        let model = ScriptedChatModel::new([
            ScriptedReply::Err(huge_error),
            ScriptedReply::Ok("not json".to_string()),
        ]);
        let failure = run(&model).unwrap_err();
        assert_eq!(failure.final_raw_reply.as_deref(), Some("not json"));
        assert_eq!(failure.stats.failed, 1);
        assert_eq!(model.guided_flags(), [true, false]);
        assert!(failure.errors.iter().all(|error| error.chars().count() <= 240));
    }

    #[test]
    fn retry_call_failure_keeps_the_guided_raw_reply() {
        let guided_raw = "bad guided".to_string();
        let model = ScriptedChatModel::new([
            ScriptedReply::Ok(guided_raw.clone()),
            ScriptedReply::Err("retry transport failed".to_string()),
            ScriptedReply::Ok(valid_json("12")),
        ]);
        let failure = run(&model).unwrap_err();
        assert_eq!(failure.final_raw_reply.as_deref(), Some(guided_raw.as_str()));
        assert_eq!(failure.errors.len(), 2);
        assert_eq!(failure.stats.failed, 1);
        assert_eq!(model.guided_flags(), [true, false]);
    }

    #[test]
    fn two_call_failures_have_no_raw_reply_and_never_make_a_third_call() {
        let model = ScriptedChatModel::new([
            ScriptedReply::Err("guided transport failed".to_string()),
            ScriptedReply::Err("retry transport failed".to_string()),
            ScriptedReply::Ok(valid_json("12")),
        ]);
        let failure = run(&model).unwrap_err();
        assert_eq!(failure.final_raw_reply, None);
        assert_eq!(failure.errors.len(), 2);
        assert_eq!(failure.stats.failed, 1);
        assert_eq!(model.guided_flags(), [true, false]);
    }

    #[test]
    fn rung_tokens_are_closed_and_stable() {
        for rung in OutputRung::VARIANTS {
            assert_eq!(OutputRung::from_db_str(rung.as_db_str()).unwrap(), rung);
        }
        assert!(OutputRung::from_db_str("other").is_err());
    }
}
