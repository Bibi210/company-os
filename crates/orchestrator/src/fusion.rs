//! Reciprocal Rank Fusion (RRF) for hybrid search (RFC bdee1af4
//! proposition 3, design-doc f433fefc composant fusion-reciprocal-rank).
//!
//! RRF combines two (or more) ranked lists into a single ranking by
//! summing `1 / (k + rank_r(d))` across rankers `r`. It needs no score
//! calibration — only ranks — which makes it robust to changes in either
//! ranker (BM25 weight tuning, embedding model swap).
//!
//! Pure module: no I/O, no SQL, no allocation beyond the result vector.

use std::collections::HashMap;

/// A document ranked by one of the two search axes.
#[derive(Debug, Clone)]
pub struct RankedResult {
    pub id: String,
    /// 1-indexed rank within the source ranking. Rank 1 = best hit.
    pub rank: usize,
}

/// A fused result after RRF.
#[derive(Debug, Clone)]
pub struct FusedResult {
    pub id: String,
    pub score: f64,
    pub lexical_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
}

/// Canonical RRF smoothing constant from the TREC literature.
pub const DEFAULT_RRF_K: f64 = 60.0;

/// Fuse a lexical and a semantic ranking via RRF and return the top
/// `top_n` documents.
///
/// - `k` is the smoothing constant. 60 is canonical; smaller values
///   bias the top of the list more aggressively.
/// - Ties in fused score are broken by id (lexicographic ascending)
///   for deterministic ordering across runs.
/// - Documents present in only one ranker still appear, with a smaller
///   score than those present in both — by design.
pub fn rrf_fuse(
    lexical: &[RankedResult],
    semantic: &[RankedResult],
    k: f64,
    top_n: usize,
) -> Vec<FusedResult> {
    let mut accum: HashMap<String, FusedResult> = HashMap::new();

    for r in lexical {
        let entry = accum.entry(r.id.clone()).or_insert(FusedResult {
            id: r.id.clone(),
            score: 0.0,
            lexical_rank: None,
            semantic_rank: None,
        });
        entry.lexical_rank = Some(r.rank);
        entry.score += 1.0 / (k + r.rank as f64);
    }
    for r in semantic {
        let entry = accum.entry(r.id.clone()).or_insert(FusedResult {
            id: r.id.clone(),
            score: 0.0,
            lexical_rank: None,
            semantic_rank: None,
        });
        entry.semantic_rank = Some(r.rank);
        entry.score += 1.0 / (k + r.rank as f64);
    }

    let mut out: Vec<FusedResult> = accum.into_values().collect();
    // Sort by score desc, then by id asc as tie-breaker.
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    out.truncate(top_n);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: &str, rank: usize) -> RankedResult {
        RankedResult {
            id: id.into(),
            rank,
        }
    }

    #[test]
    fn rrf_only_lexical_score_one_over_k_plus_1() {
        let out = rrf_fuse(&[r("a", 1)], &[], 60.0, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a");
        assert!((out[0].score - 1.0 / 61.0).abs() < 1e-9);
        assert_eq!(out[0].lexical_rank, Some(1));
        assert_eq!(out[0].semantic_rank, None);
    }

    #[test]
    fn rrf_only_semantic_score_one_over_k_plus_1() {
        let out = rrf_fuse(&[], &[r("a", 1)], 60.0, 10);
        assert_eq!(out[0].id, "a");
        assert!((out[0].score - 1.0 / 61.0).abs() < 1e-9);
        assert_eq!(out[0].lexical_rank, None);
        assert_eq!(out[0].semantic_rank, Some(1));
    }

    #[test]
    fn rrf_present_in_both_outranks_singles() {
        let lex = vec![r("a", 1), r("b", 2)];
        let sem = vec![r("a", 2), r("c", 1)];
        let out = rrf_fuse(&lex, &sem, 60.0, 10);
        // 'a' is in both, 'b' and 'c' are in one each. 'a' must rank first.
        assert_eq!(out[0].id, "a");
        assert!(out[0].lexical_rank.is_some());
        assert!(out[0].semantic_rank.is_some());
    }

    #[test]
    fn rrf_top_n_truncation() {
        let lex: Vec<_> = (1..=20).map(|i| r(&format!("d{i}"), i)).collect();
        let out = rrf_fuse(&lex, &[], 60.0, 5);
        assert_eq!(out.len(), 5);
        // top is rank 1 = d1
        assert_eq!(out[0].id, "d1");
    }

    #[test]
    fn rrf_deterministic_tie_breaker_on_id() {
        // Two docs with identical rank in identical rankers => identical
        // score. The tie-breaker must order them by id ascending.
        let lex = vec![r("zeta", 1), r("alpha", 1)];
        let out = rrf_fuse(&lex, &[], 60.0, 10);
        assert_eq!(out[0].id, "alpha");
        assert_eq!(out[1].id, "zeta");
    }

    #[test]
    fn rrf_higher_rank_means_lower_score() {
        // Rank 1 (best) must yield higher score than rank 10.
        let lex = vec![r("a", 1), r("b", 10)];
        let out = rrf_fuse(&lex, &[], 60.0, 10);
        assert!(out[0].score > out[1].score);
        assert_eq!(out[0].id, "a");
    }

    #[test]
    fn rrf_empty_both_returns_empty() {
        let out = rrf_fuse(&[], &[], 60.0, 10);
        assert!(out.is_empty());
    }
}
