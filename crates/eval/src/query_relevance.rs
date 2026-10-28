//! Per-query relevance metrics for bookmark search.
//!
//! Each query has a few labeled candidates (relevant or not). A ranking blends
//! a lexical score with a semantic (embedding cosine) score after z-scoring
//! both, then is scored with precision@k and nDCG. Labels stay private; this
//! module only does the math.

/// Semantic share of the query blend, tuned on Nathan's 2026-10-28 labels
/// (31 labels, 5 queries): 0.9 gave P@3 0.80 / nDCG 0.94 vs 0.53 / 0.76 lexical-only.
pub const QUERY_SEMANTIC_WEIGHT: f64 = 0.9;

/// Z-score `v` (mean 0, unit variance); a constant vector maps to zeros.
pub fn zscore(v: &[f64]) -> Vec<f64> {
    if v.is_empty() {
        return Vec::new();
    }
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
    v.iter()
        .map(|x| if sd > 0.0 { (x - mean) / sd } else { 0.0 })
        .collect()
}

/// Blend `(1 - weight) * z(lexical) + weight * z(semantic)`; `weight` is clamped to 0..=1.
/// Returns `None` when the two slices differ in length (fail closed).
pub fn blend(lexical: &[f64], semantic: &[f64], weight: f64) -> Option<Vec<f64>> {
    if lexical.len() != semantic.len() {
        return None;
    }
    let w = weight.clamp(0.0, 1.0);
    Some(
        zscore(lexical)
            .iter()
            .zip(zscore(semantic))
            .map(|(a, b)| (1.0 - w) * a + w * b)
            .collect(),
    )
}

/// Relevance flags reordered by descending score (stable on ties).
pub fn ranked(scores: &[f64], relevant: &[bool]) -> Vec<bool> {
    let mut idx: Vec<usize> = (0..scores.len().min(relevant.len())).collect();
    idx.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx.into_iter().map(|i| relevant[i]).collect()
}

/// Precision@k of a ranked relevance list; 0 when `k` is 0.
pub fn precision_at_k(ranked: &[bool], k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    ranked.iter().take(k).filter(|r| **r).count() as f64 / k as f64
}

/// Binary nDCG over the whole ranked list; 0 when nothing is relevant.
pub fn ndcg(ranked: &[bool]) -> f64 {
    let dcg = |it: &mut dyn Iterator<Item = bool>| -> f64 {
        it.enumerate()
            .filter(|(_, r)| *r)
            .map(|(i, _)| 1.0 / ((i + 2) as f64).log2())
            .sum()
    };
    let mut ideal = ranked.to_vec();
    ideal.sort_by(|a, b| b.cmp(a));
    let i = dcg(&mut ideal.into_iter());
    if i == 0.0 {
        0.0
    } else {
        dcg(&mut ranked.iter().copied()) / i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_and_reversed_rankings() {
        assert_eq!(ndcg(&[true, true, false]), 1.0);
        assert!(ndcg(&[false, true, true]) < 1.0);
        assert_eq!(ndcg(&[false, false]), 0.0);
        assert!((precision_at_k(&[true, false, true], 3) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(precision_at_k(&[true], 0), 0.0);
    }

    #[test]
    fn blend_weights_and_fails_closed() {
        let r = ranked(
            &blend(&[1.0, 0.0], &[0.0, 1.0], 1.0).unwrap(),
            &[false, true],
        );
        assert_eq!(r, vec![true, false]);
        let r = ranked(
            &blend(&[1.0, 0.0], &[0.0, 1.0], 0.0).unwrap(),
            &[false, true],
        );
        assert_eq!(r, vec![false, true]);
        assert!(blend(&[1.0], &[1.0, 2.0], 0.5).is_none());
        assert_eq!(zscore(&[3.0, 3.0]), vec![0.0, 0.0]);
    }
}
