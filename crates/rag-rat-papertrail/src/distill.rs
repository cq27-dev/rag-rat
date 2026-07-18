//! Closed, persisted enums for the distilled-record store (V075, #703).
//!
//! These are the machine tokens written into the `papertrail_distill*` tables and read back by the
//! Phase-3 consumption layer. They live in `rag-rat-papertrail` (not the `rag-rat-core` extraction
//! module) because they are the lowest common type both the writer (`rag-rat-core`, above
//! papertrail in the crate DAG) and the readers (`papertrail::api`, `rag-rat-query`) share — an
//! enum in core would be invisible to papertrail-side consumption.
//!
//! Every enum follows the repo's persisted-enum contract: a closed set with a stable `as_db_str`
//! machine token and a `from_db_str` that rejects anything outside the set. The tokens ARE schema —
//! changing one needs a migration + a token test, the same as a column rename.

use serde::{Deserialize, Serialize};

/// Provenance of the record's fix edge (`papertrail_distill.fix_edge_source`). This is a
/// provenance FACET, not a fused confidence label: `provider` = the tracker attested the closing
/// edge, `text` = it was mined from commit/item/comment text, `none` = no fix edge exists (the
/// no-fix-edge status floor fires read-layer). The effective outcome status is computed downstream,
/// never here.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum FixEdgeSource {
    Provider,
    Text,
    None,
}

impl FixEdgeSource {
    /// The exact persisted token (`papertrail_distill.fix_edge_source`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown fix-edge source token `{value}`"))
    }
}

/// The conversational shape of the distilled thread (`papertrail_distill.thread_shape`), a
/// provenance FACET used to gate consumption: `investigation` (a root-cause/decision thread worth
/// surfacing), `review_stream` (PR review back-and-forth), `thin` (little discussion — the record
/// is mostly mechanical).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ThreadShape {
    Investigation,
    ReviewStream,
    Thin,
}

impl ThreadShape {
    /// The exact persisted token (`papertrail_distill.thread_shape`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown thread-shape token `{value}`"))
    }
}

/// The MODEL-emitted outcome status (`papertrail_distill.outcome_status_model`). This is stored
/// SEPARATELY from the mechanical floors (`revert_override`, `closing_keyword_floor`, no-fix-edge);
/// the EFFECTIVE status is computed in the read layer with precedence
/// revert > closing-keyword > no-fix-edge > this. Token set observed across the distillation spike.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    /// The decision was implemented and merged.
    Landed,
    /// The thread does not establish what happened.
    Unclear,
    /// The work was dropped / cut from scope without landing.
    Descoped,
    /// A later thread replaced this decision before it settled.
    Superseded,
    /// The change landed and was then reverted.
    Reverted,
}

impl OutcomeStatus {
    /// The exact persisted token (`papertrail_distill.outcome_status_model`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown outcome-status token `{value}`"))
    }
}

/// Event-factuality of a distilled claim (`papertrail_distill.epistemic_status_decision` and
/// `.epistemic_status_outcome`). Keeps a proposed-but-not-landed idea distinct from an
/// asserted-and-landed one, so consumption never surfaces a projection as a settled result:
/// `asserted_landed` (it happened), `projected` (a forward-looking claim), `proposed_not_landed`
/// (suggested, never implemented), `superseded` (overtaken by a later thread).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum EpistemicStatus {
    AssertedLanded,
    Projected,
    ProposedNotLanded,
    Superseded,
}

impl EpistemicStatus {
    /// The exact persisted token (`epistemic_status_decision` / `epistemic_status_outcome`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown epistemic-status token `{value}`"))
    }
}

/// Kind of thread-keyed edge (`papertrail_distill_edges.edge_kind`). Edges key to the THREAD, not
/// the record row, so they survive record regeneration: `coalesced` (issue↔PR, mechanical from the
/// closing edge), `supersedes` (a later thread replaces an earlier decision), `promoted` (the
/// deferred promotion lane, #113's later ask).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DistillEdgeKind {
    Coalesced,
    Supersedes,
    Promoted,
}

impl DistillEdgeKind {
    /// The exact persisted token (`papertrail_distill_edges.edge_kind`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown distill-edge kind token `{value}`"))
    }
}

/// Kind of code anchor bound to a record (`papertrail_distill_anchors.anchor_kind`). Anchors are
/// born as index-validated selections with EXACT paths (no basename fallback): `symbol` (carries a
/// `sym_<hex>` logical-symbol id when resolved), `file`, `schema_object` (a table/migration),
/// `crate`, `config_key`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    Symbol,
    File,
    SchemaObject,
    Crate,
    ConfigKey,
}

impl AnchorKind {
    /// The exact persisted token (`papertrail_distill_anchors.anchor_kind`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown anchor-kind token `{value}`"))
    }
}

#[cfg(test)]
mod distill_enum_token_tests {
    use super::{
        AnchorKind, DistillEdgeKind, EpistemicStatus, FixEdgeSource, OutcomeStatus, ThreadShape,
    };

    #[test]
    fn distill_enum_tokens_round_trip_and_pin_the_persisted_strings() {
        for (v, token) in [
            (FixEdgeSource::Provider, "provider"),
            (FixEdgeSource::Text, "text"),
            (FixEdgeSource::None, "none"),
        ] {
            assert_eq!(v.as_db_str(), token);
            assert_eq!(FixEdgeSource::from_db_str(token).unwrap(), v);
        }
        for (v, token) in [
            (ThreadShape::Investigation, "investigation"),
            (ThreadShape::ReviewStream, "review_stream"),
            (ThreadShape::Thin, "thin"),
        ] {
            assert_eq!(v.as_db_str(), token);
            assert_eq!(ThreadShape::from_db_str(token).unwrap(), v);
        }
        for (v, token) in [
            (OutcomeStatus::Landed, "landed"),
            (OutcomeStatus::Unclear, "unclear"),
            (OutcomeStatus::Descoped, "descoped"),
            (OutcomeStatus::Superseded, "superseded"),
            (OutcomeStatus::Reverted, "reverted"),
        ] {
            assert_eq!(v.as_db_str(), token);
            assert_eq!(OutcomeStatus::from_db_str(token).unwrap(), v);
        }
        for (v, token) in [
            (EpistemicStatus::AssertedLanded, "asserted_landed"),
            (EpistemicStatus::Projected, "projected"),
            (EpistemicStatus::ProposedNotLanded, "proposed_not_landed"),
            (EpistemicStatus::Superseded, "superseded"),
        ] {
            assert_eq!(v.as_db_str(), token);
            assert_eq!(EpistemicStatus::from_db_str(token).unwrap(), v);
        }
        for (v, token) in [
            (DistillEdgeKind::Coalesced, "coalesced"),
            (DistillEdgeKind::Supersedes, "supersedes"),
            (DistillEdgeKind::Promoted, "promoted"),
        ] {
            assert_eq!(v.as_db_str(), token);
            assert_eq!(DistillEdgeKind::from_db_str(token).unwrap(), v);
        }
        for (v, token) in [
            (AnchorKind::Symbol, "symbol"),
            (AnchorKind::File, "file"),
            (AnchorKind::SchemaObject, "schema_object"),
            (AnchorKind::Crate, "crate"),
            (AnchorKind::ConfigKey, "config_key"),
        ] {
            assert_eq!(v.as_db_str(), token);
            assert_eq!(AnchorKind::from_db_str(token).unwrap(), v);
        }
        assert!(FixEdgeSource::from_db_str("attested").is_err(), "outside the closed set");
        assert!(OutcomeStatus::from_db_str("fixed").is_err(), "outside the closed set");
        assert!(AnchorKind::from_db_str("module").is_err(), "outside the closed set");
    }
}
