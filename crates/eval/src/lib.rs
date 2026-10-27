//! Offline evaluation harness for the bookmarks agent.
//!
//! Joins human labels (JSONL) with bookmarks by `post_id` and scores two
//! tasks with no network and no paid calls:
//! - try-it classification (`bookmarks_agent::try_it::classify`)
//! - brief ranking (`bookmarks_agent::brief::select_candidates` with an empty
//!   live set, then `bookmarks_agent::brief::local_picks`)
//!
//! Reports carry aggregate numbers only, never post text, ids or urls.

use bookmarks_agent::fixture::Bookmark;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

/// One human label, as one JSONL line:
/// `{"post_id": "...", "try_it": true|false, "brief_worthy": true|false}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Label {
    pub post_id: String,
    pub try_it: bool,
    pub brief_worthy: bool,
}

/// Load labels from a JSONL file. Blank lines are skipped; a malformed line
/// fails the whole load naming its line number.
pub fn load_labels(path: impl AsRef<Path>) -> Result<Vec<Label>, String> {
    let path = path.as_ref();
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse_labels(&text)
}

fn parse_labels(text: &str) -> Result<Vec<Label>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let label: Label =
            serde_json::from_str(line).map_err(|e| format!("labels line {}: {e}", i + 1))?;
        out.push(label);
    }
    Ok(out)
}

/// Binary confusion counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Confusion {
    pub tp: usize,
    pub fp: usize,
    pub r#fn: usize,
    pub tn: usize,
}

fn safe_div(num: usize, den: usize) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

impl Confusion {
    /// Total examples counted.
    pub fn n(&self) -> usize {
        self.tp + self.fp + self.r#fn + self.tn
    }

    /// TP / (TP + FP), or 0.0 when nothing was predicted positive.
    pub fn precision(&self) -> f64 {
        safe_div(self.tp, self.tp + self.fp)
    }

    /// TP / (TP + FN), or 0.0 when nothing is actually positive.
    pub fn recall(&self) -> f64 {
        safe_div(self.tp, self.tp + self.r#fn)
    }

    /// (TP + TN) / n, or 0.0 when empty.
    pub fn accuracy(&self) -> f64 {
        safe_div(self.tp + self.tn, self.n())
    }

    /// Harmonic mean of precision and recall, or 0.0 when both are 0.0.
    pub fn f1(&self) -> f64 {
        let p = self.precision();
        let r = self.recall();
        if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        }
    }
}

/// Brief ranking scores over one labeled set.
#[derive(Debug, Clone, PartialEq)]
pub struct RankingEval {
    /// Brief-worthy fraction of the local picks (up to 5).
    pub precision_at_5: f64,
    /// Brief-worthy fraction of the candidate set.
    pub candidate_precision: f64,
    /// Fraction of all brief-worthy labels present in the candidate set.
    pub candidate_recall: f64,
    /// Brief-worthy fraction of all matched labels.
    pub base_rate: f64,
    /// Distinct topics in the candidate set.
    pub topic_coverage: usize,
    pub n_candidates: usize,
    pub n_picks: usize,
    pub n_brief_worthy: usize,
}

/// Aggregate-only report: counts and ratios, never post text, ids or urls.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalReport {
    pub name: String,
    pub n_labels: usize,
    pub n_matched: usize,
    pub n_unmatched_labels: usize,
    pub try_it: Confusion,
    pub ranking: RankingEval,
}

/// Join `labels` to `bookmarks` by `post_id` and score both tasks. Labels
/// without a matching bookmark are counted in `n_unmatched_labels`: never an
/// error, never a panic.
pub fn evaluate(bookmarks: &[Bookmark], labels: &[Label], name: &str) -> EvalReport {
    let by_id: HashMap<&str, &Bookmark> =
        bookmarks.iter().map(|b| (b.post_id.as_str(), b)).collect();
    let mut matched: Vec<(&Bookmark, &Label)> = Vec::new();
    let mut unmatched = 0usize;
    for label in labels {
        match by_id.get(label.post_id.as_str()) {
            Some(bookmark) => matched.push((bookmark, label)),
            None => unmatched += 1,
        }
    }

    let mut try_it = Confusion::default();
    for (bookmark, label) in &matched {
        let predicted = bookmarks_agent::try_it::classify(bookmark).is_some();
        match (predicted, label.try_it) {
            (true, true) => try_it.tp += 1,
            (true, false) => try_it.fp += 1,
            (false, true) => try_it.r#fn += 1,
            (false, false) => try_it.tn += 1,
        }
    }

    let rows: Vec<Bookmark> = matched.iter().map(|(b, _)| (*b).clone()).collect();
    let worthy: HashSet<&str> = matched
        .iter()
        .filter(|(_, l)| l.brief_worthy)
        .map(|(_, l)| l.post_id.as_str())
        .collect();
    let candidates = bookmarks_agent::brief::select_candidates(&rows, &BTreeSet::new());
    let picks = bookmarks_agent::brief::local_picks(&candidates);
    let mut picks_hit = 0usize;
    for (n, _) in &picks {
        if *n == 0 {
            continue;
        }
        if let Some(candidate) = candidates.get(n - 1) {
            if worthy.contains(candidate.post_id.as_str()) {
                picks_hit += 1;
            }
        }
    }
    let candidate_hits = candidates
        .iter()
        .filter(|c| worthy.contains(c.post_id.as_str()))
        .count();
    let topics: BTreeSet<&str> = candidates.iter().map(|c| c.topic.as_str()).collect();

    EvalReport {
        name: name.to_string(),
        n_labels: labels.len(),
        n_matched: matched.len(),
        n_unmatched_labels: unmatched,
        try_it,
        ranking: RankingEval {
            precision_at_5: safe_div(picks_hit, picks.len()),
            candidate_precision: safe_div(candidate_hits, candidates.len()),
            candidate_recall: safe_div(candidate_hits, worthy.len()),
            base_rate: safe_div(worthy.len(), matched.len()),
            topic_coverage: topics.len(),
            n_candidates: candidates.len(),
            n_picks: picks.len(),
            n_brief_worthy: worthy.len(),
        },
    }
}

/// Load bookmarks and labels from JSONL files and score both tasks.
pub fn run(
    bookmarks_path: impl AsRef<Path>,
    labels_path: impl AsRef<Path>,
    name: &str,
) -> Result<EvalReport, String> {
    let bookmarks_path = bookmarks_path.as_ref();
    let bookmarks = bookmarks_agent::fixture::load(bookmarks_path)
        .map_err(|e| format!("bookmarks {}: {e}", bookmarks_path.display()))?;
    let labels = load_labels(labels_path.as_ref())?;
    Ok(evaluate(&bookmarks, &labels, name))
}

/// Render the report as markdown. Aggregate numbers only: no post text, ids
/// or urls ever appear.
pub fn render_markdown(report: &EvalReport) -> String {
    let t = &report.try_it;
    let r = &report.ranking;
    format!(
        "# Eval report — {name}\n\
         \n\
         Labels: {n_labels} total, {n_matched} matched, {n_unmatched} unmatched.\n\
         \n\
         | Task | n | TP | FP | FN | TN | Precision | Recall | F1 |\n\
         | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n\
         | try-it | {n} | {tp} | {fp} | {f} | {tn} | {p:.3} | {rc:.3} | {f1:.3} |\n\
         \n\
         Try-it accuracy: {acc:.3}\n\
         \n\
         | Ranking metric | Value |\n\
         | --- | --- |\n\
         | Local picks (n) | {n_picks} |\n\
         | Precision@5 | {p5:.3} |\n\
         | Candidates (n) | {n_cand} |\n\
         | Candidate precision | {cp:.3} |\n\
         | Candidate recall | {cr:.3} |\n\
         | Brief-worthy base rate | {br:.3} |\n\
         | Brief-worthy (n) | {n_worthy} |\n\
         | Topic coverage | {topics} |\n",
        name = report.name,
        n_labels = report.n_labels,
        n_matched = report.n_matched,
        n_unmatched = report.n_unmatched_labels,
        n = t.n(),
        tp = t.tp,
        fp = t.fp,
        f = t.r#fn,
        tn = t.tn,
        p = t.precision(),
        rc = t.recall(),
        f1 = t.f1(),
        acc = t.accuracy(),
        n_picks = r.n_picks,
        p5 = r.precision_at_5,
        n_cand = r.n_candidates,
        cp = r.candidate_precision,
        cr = r.candidate_recall,
        br = r.base_rate,
        n_worthy = r.n_brief_worthy,
        topics = r.topic_coverage,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn metric_math_known_counts() {
        let c = Confusion {
            tp: 5,
            fp: 3,
            r#fn: 2,
            tn: 10,
        };
        assert_eq!(c.n(), 20);
        assert!(approx(c.precision(), 5.0 / 8.0));
        assert!(approx(c.recall(), 5.0 / 7.0));
        assert!(approx(c.accuracy(), 15.0 / 20.0));
        let p = 5.0 / 8.0;
        let r = 5.0 / 7.0;
        assert!(approx(c.f1(), 2.0 * p * r / (p + r)));
    }

    #[test]
    fn metric_math_zero_denominators() {
        let c = Confusion::default();
        assert_eq!(c.precision(), 0.0);
        assert_eq!(c.recall(), 0.0);
        assert_eq!(c.accuracy(), 0.0);
        assert_eq!(c.f1(), 0.0);
        let negatives = Confusion {
            tn: 4,
            ..Confusion::default()
        };
        assert_eq!(negatives.precision(), 0.0);
        assert_eq!(negatives.recall(), 0.0);
        assert_eq!(negatives.f1(), 0.0);
        assert_eq!(negatives.accuracy(), 1.0);
    }

    fn fixture_path(name: &str) -> String {
        format!("{}/../../fixtures/eval/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn load_synthetic() -> (Vec<Bookmark>, Vec<Label>) {
        let bookmarks =
            bookmarks_agent::fixture::load(fixture_path("synthetic_bookmarks.jsonl")).unwrap();
        let labels = load_labels(fixture_path("synthetic_labels.jsonl")).unwrap();
        (bookmarks, labels)
    }

    #[test]
    fn synthetic_end_to_end() {
        let (bookmarks, labels) = load_synthetic();
        assert_eq!(bookmarks.len(), 24);
        assert_eq!(labels.len(), 24);
        let report = evaluate(&bookmarks, &labels, "synthetic");
        assert_eq!(report.n_matched, 24);
        assert_eq!(report.n_unmatched_labels, 0);
        assert_eq!(
            report.try_it,
            Confusion {
                tp: 9,
                fp: 0,
                r#fn: 1,
                tn: 14,
            }
        );
        assert!(approx(report.ranking.precision_at_5, 0.6));
        assert!(approx(report.ranking.candidate_precision, 9.0 / 20.0));
        assert!(approx(report.ranking.candidate_recall, 1.0));
        assert!(approx(report.ranking.base_rate, 9.0 / 24.0));
        assert_eq!(report.ranking.topic_coverage, 20);
        let md = render_markdown(&report);
        assert!(
            md.contains("| Task | n | TP | FP | FN | TN | Precision | Recall | F1 |"),
            "{md}"
        );
    }

    #[test]
    fn markdown_leaks_no_ids_urls_or_text() {
        let (bookmarks, labels) = load_synthetic();
        let md = render_markdown(&evaluate(&bookmarks, &labels, "synthetic"));
        assert!(!md.contains("eval-"), "{md}");
        assert!(!md.contains("https://"), "{md}");
        assert!(!md.contains("x.com"), "{md}");
        assert!(!md.contains("status/"), "{md}");
        for b in &bookmarks {
            assert!(!md.contains(&b.text), "leaked post text");
            assert!(!md.contains(&b.url), "leaked url");
        }
    }

    #[test]
    fn unmatched_labels_are_counted() {
        let (bookmarks, mut labels) = load_synthetic();
        labels.push(Label {
            post_id: "missing-post".to_string(),
            try_it: true,
            brief_worthy: true,
        });
        let report = evaluate(&bookmarks, &labels, "synthetic");
        assert_eq!(report.n_labels, 25);
        assert_eq!(report.n_matched, 24);
        assert_eq!(report.n_unmatched_labels, 1);
    }

    #[test]
    fn load_labels_rejects_bad_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("labels.jsonl");
        std::fs::write(
            &path,
            "{\"post_id\":\"a\",\"try_it\":true,\"brief_worthy\":false}\nnot json\n",
        )
        .unwrap();
        let err = load_labels(&path).unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        assert!(load_labels(dir.path().join("nope.jsonl")).is_err());
    }
}
