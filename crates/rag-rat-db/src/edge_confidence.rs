//! The persisted `edges.confidence` vocabulary. The indexer writes it and the read layer orders,
//! ranks and weights edges by it, so the enum lives here, below both.

/// How sure the heuristic resolver was of an edge's target, strongest first — persisted as
/// `edges.confidence`, whose tokens are the variant names verbatim.
///
/// Every reading of the column hangs off this one enum: the stored token ([`Self::as_db_str`]),
/// the SQL ordering key ([`Self::order_sql`]), the tool-output form ([`Self::normalized`]), the
/// ladder position ([`Self::rank`]) and the PageRank multiplier ([`Self::weight`]). Each is an
/// exhaustive match, so a new tier cannot land without a place on every ladder. A stored token
/// outside the set is an `Err` from [`Self::from_db_str`], and each reader decides how it ranks.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
pub enum EdgeConfidence {
    Exact,
    Syntactic,
    NameOnly,
    Ambiguous,
}

impl EdgeConfidence {
    /// The exact persisted token.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a stored token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown edge confidence `{value}`"))
    }

    /// The snake_case form tool output carries, so graph traversal, read_chunk and search all
    /// serialize confidence identically.
    pub fn normalized(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Syntactic => "syntactic",
            Self::NameOnly => "name_only",
            Self::Ambiguous => "ambiguous",
        }
    }

    /// Position on the heuristic ladder, strongest first.
    pub fn rank(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Syntactic => 1,
            Self::NameOnly => 2,
            Self::Ambiguous => 3,
        }
    }

    /// PageRank multiplier: how much of a source's rank flows through an edge of this confidence.
    /// A name-only guess is a weak signal that a dependency exists, so it flows less than a
    /// structurally resolved call.
    pub fn weight(self) -> f64 {
        match self {
            Self::Exact => 1.0,
            Self::Syntactic => 0.85,
            Self::NameOnly => 0.4,
            Self::Ambiguous => 0.2,
        }
    }

    /// The ladder as an `ORDER BY` key over the stored tokens, strongest first: each tier's
    /// [`Self::rank`], with the weakest tier as the `ELSE` arm, so an unknown token orders with
    /// it. Valid wherever the edges table is named or aliased `edges`.
    pub fn order_sql() -> String {
        let tiers = <Self as strum::VariantArray>::VARIANTS;
        let weakest = tiers.iter().map(|tier| tier.rank()).max().unwrap_or_default();
        let arms: String = tiers
            .iter()
            .filter(|tier| tier.rank() != weakest)
            .map(|tier| format!("WHEN '{}' THEN {} ", tier.as_db_str(), tier.rank()))
            .collect();
        format!("CASE edges.confidence {arms}ELSE {weakest} END")
    }
}

#[cfg(test)]
mod tests {
    use super::EdgeConfidence;

    /// The tokens are persisted and every ladder feeds the read layer's SQL and scoring, so each
    /// tier's token, tool-output form, rank and weight is pinned exactly — the weight bit-for-bit.
    #[test]
    fn every_tier_is_pinned_on_every_ladder() {
        for (tier, token, normalized, rank, weight) in [
            (EdgeConfidence::Exact, "Exact", "exact", 0, 1.0_f64),
            (EdgeConfidence::Syntactic, "Syntactic", "syntactic", 1, 0.85),
            (EdgeConfidence::NameOnly, "NameOnly", "name_only", 2, 0.4),
            (EdgeConfidence::Ambiguous, "Ambiguous", "ambiguous", 3, 0.2),
        ] {
            assert_eq!(tier.as_db_str(), token);
            assert_eq!(EdgeConfidence::from_db_str(token).unwrap(), tier);
            assert_eq!(tier.normalized(), normalized);
            assert_eq!(tier.rank(), rank);
            assert_eq!(tier.weight().to_bits(), weight.to_bits(), "{token}");
        }
        assert!(EdgeConfidence::from_db_str("exact").is_err());
    }

    #[test]
    fn order_sql_is_the_rank_ladder_with_the_weakest_tier_as_else() {
        assert_eq!(
            EdgeConfidence::order_sql(),
            "CASE edges.confidence WHEN 'Exact' THEN 0 WHEN 'Syntactic' THEN 1 WHEN 'NameOnly' \
             THEN 2 ELSE 3 END"
        );
    }
}
