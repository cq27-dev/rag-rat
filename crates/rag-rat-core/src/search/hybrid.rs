use std::collections::BTreeMap;

use crate::search::lexical::SearchHit;

pub fn reciprocal_rank_fusion(
    mut ranked_lists: Vec<Vec<SearchHit>>,
    limit: usize,
) -> Vec<SearchHit> {
    let mut scores = BTreeMap::<i64, (f64, SearchHit)>::new();
    for hits in &mut ranked_lists {
        for (rank, hit) in hits.drain(..).enumerate() {
            let score = 1.0 / (60.0 + rank as f64 + 1.0);
            scores.entry(hit.chunk_id).and_modify(|entry| entry.0 += score).or_insert((score, hit));
        }
    }
    let mut fused = scores.into_values().collect::<Vec<_>>();
    fused.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    fused
        .into_iter()
        .take(limit)
        .map(|(score, mut hit)| {
            hit.score = score;
            hit
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(chunk_id: i64) -> SearchHit {
        SearchHit {
            chunk_id,
            path: String::new(),
            language: String::new(),
            kind: String::new(),
            start_line: 0,
            end_line: 0,
            symbol_path: None,
            score: 0.0,
            retrieval_mode: String::new(),
            summary: String::new(),
            graph: None,
            score_components: None,
            importance: None,
            distilled_records: Vec::new(),
        }
    }

    #[test]
    fn rrf_accumulates_across_lists_then_ranks_by_fused_score() {
        // chunk 1 is in BOTH lists (rank 0 in A, rank 1 in B) so it accumulates two RRF
        // contributions; chunk 3 (rank 0 in B) and chunk 2 (rank 1 in A) each get one.
        let list_a = vec![hit(1), hit(2)];
        let list_b = vec![hit(3), hit(1)];
        let fused = reciprocal_rank_fusion(vec![list_a, list_b], 10);

        assert_eq!(fused.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![1, 3, 2]);
        // RRF weight is 1/(60 + rank + 1); chunk 1 = 1/61 + 1/62, set onto hit.score.
        assert!((fused[0].score - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-9);
        assert!((fused[1].score - 1.0 / 61.0).abs() < 1e-9);
    }

    #[test]
    fn rrf_respects_the_limit() {
        let capped = reciprocal_rank_fusion(vec![vec![hit(1), hit(2), hit(3)]], 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].chunk_id, 1);
    }
}
