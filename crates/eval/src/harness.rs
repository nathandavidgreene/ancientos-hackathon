//! Offline evaluation harness for the bookmarks agent.
//!
//! Joins reference labels (JSONL) with bookmarks by `post_id` and scores two
//! tasks with no network and no paid calls:
//! - try-it classification (`bookmarks_agent::try_it::classify`)
//! - brief ranking (`bookmarks_agent::brief::select_candidates` with an empty
//!   live set, then `bookmarks_agent::brief::local_picks`)
//!
//! Non-English bookmarks are always sampled (never hidden for language) and
//! scored on the cached English translation when available, else on the
//! original. Reports carry aggregate numbers only — translated/untranslated
//! counts, never post text, ids or urls.

use bookmarks_agent::fixture::Bookmark;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

/// One reference label, as one JSONL line:
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
    /// Matched bookmarks that are non-English (always sampled; equals
    /// `n_translated + n_untranslated`).
    pub n_non_english: usize,
    /// Non-English matches scored on a cached English translation.
    pub n_translated: usize,
    /// Non-English matches with no cached translation (scored on the
    /// original).
    pub n_untranslated: usize,
    pub try_it: Confusion,
    pub ranking: RankingEval,
}

/// Join `labels` to `bookmarks` by `post_id` and score both tasks with no
/// cached translations (every non-English match counts as untranslated and
/// scores on the original). See [`evaluate_with_cache`].
pub fn evaluate(bookmarks: &[Bookmark], labels: &[Label], name: &str) -> EvalReport {
    evaluate_with_cache(bookmarks, labels, name, &HashMap::new())
}

/// Join `labels` to `bookmarks` by `post_id` and score both tasks. Labels
/// without a matching bookmark are counted in `n_unmatched_labels`: never an
/// error, never a panic. Non-English matches are always sampled (never
/// hidden for language): those with a cached English translation score on
/// the translation (counted in `n_translated`), the rest score on the
/// original (counted in `n_untranslated`). Reads the cache only; makes no
/// model calls.
pub fn evaluate_with_cache(
    bookmarks: &[Bookmark],
    labels: &[Label],
    name: &str,
    cache: &HashMap<String, bookmarks_agent::translate::CacheEntry>,
) -> EvalReport {
    let by_id: HashMap<&str, &Bookmark> =
        bookmarks.iter().map(|b| (b.post_id.as_str(), b)).collect();
    let mut matched: Vec<(Bookmark, &Label)> = Vec::new();
    let mut unmatched = 0usize;
    let mut non_english = 0usize;
    let mut translated = 0usize;
    let mut untranslated = 0usize;
    for label in labels {
        match by_id.get(label.post_id.as_str()) {
            Some(bookmark) => {
                if bookmarks_agent::language::is_english(bookmark) {
                    matched.push(((*bookmark).clone(), label));
                } else {
                    non_english += 1;
                    let t = bookmarks_agent::translate::cached_only(bookmark, cache);
                    if t.status == bookmarks_agent::translate::TranslationStatus::Translated {
                        translated += 1;
                        let mut effective = (*bookmark).clone();
                        if let Some(english) = t.english {
                            effective.text = english;
                        }
                        matched.push((effective, label));
                    } else {
                        untranslated += 1;
                        matched.push(((*bookmark).clone(), label));
                    }
                }
            }
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

    let rows: Vec<Bookmark> = matched.iter().map(|(b, _)| b.clone()).collect();
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
        n_non_english: non_english,
        n_translated: translated,
        n_untranslated: untranslated,
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

/// One synthetic verdict for ranker training, as one JSONL line:
/// `{"post_id": "...", "verdict": "approve"|"skip"|..., "voted_at": <unix secs>}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct VerdictEntry {
    pub post_id: String,
    pub verdict: String,
    pub voted_at: u64,
}

/// Load verdicts from a JSONL file. Blank lines are skipped; a malformed
/// line fails the whole load naming its line number.
pub fn load_verdicts(path: impl AsRef<Path>) -> Result<Vec<VerdictEntry>, String> {
    let path = path.as_ref();
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: VerdictEntry =
            serde_json::from_str(line).map_err(|e| format!("verdicts line {}: {e}", i + 1))?;
        out.push(entry);
    }
    Ok(out)
}

fn signal_of(
    verdict: &str,
    line_hint: &str,
) -> Result<Option<bookmarks_agent::ranker::Signal>, String> {
    use bookmarks_agent::ranker::Signal;
    match verdict {
        "approve" => Ok(Some(Signal::Approve)),
        "delegate" => Ok(Some(Signal::Delegate)),
        "skip" => Ok(Some(Signal::Skip)),
        "never_post" => Ok(Some(Signal::NeverPost)),
        "never_category" | "never" => Ok(Some(Signal::NeverCategory)),
        "never_author" => Ok(Some(Signal::NeverAuthor)),
        // A hide-undo carries no preference: skip, like the log reader.
        "unhide_author" => Ok(None),
        other => Err(format!("{line_hint}: unknown verdict: {other}")),
    }
}

/// Local-picks P@5 with and without the personal ranker, on a
/// leave-later-out split by vote time. Aggregate numbers only: never post
/// text, ids or urls.
#[derive(Debug, Clone, PartialEq)]
pub struct RankingComparison {
    pub name: String,
    /// Verdicts resolved to a bookmark and used in the split.
    pub n_verdicts: usize,
    /// Verdicts for unknown post ids (counted, never an error).
    pub n_unmatched: usize,
    /// Earlier verdicts the ranker trains on.
    pub n_train: usize,
    /// Later posts the local picks are scored on.
    pub n_test: usize,
    /// Brief-worthy labels among the later posts.
    pub n_test_worthy: usize,
    /// Local picks P@5 without the ranker (today's order).
    pub baseline_precision_at_5: f64,
    /// Local picks P@5 after the ranker re-ranks candidates.
    pub ranked_precision_at_5: f64,
    /// Whether the ranker trained on enough labels to activate.
    pub ranker_active: bool,
}

fn precision_at_5(
    candidates: &[bookmarks_agent::brief::Candidate],
    picks: &[(usize, String)],
    worthy: &HashSet<&str>,
) -> f64 {
    let mut hits = 0usize;
    for (n, _) in picks {
        if *n == 0 {
            continue;
        }
        if let Some(candidate) = candidates.get(n - 1) {
            if worthy.contains(candidate.post_id.as_str()) {
                hits += 1;
            }
        }
    }
    safe_div(hits, picks.len())
}

/// Compare local-picks P@5 with and without the ranker. Verdicts resolving
/// to a bookmark split by `voted_at` (ties break by post id): the earlier
/// two thirds train the ranker (minimum `min_records` labels to activate),
/// the later third is the scored pool. `now` is the latest vote time, so
/// the run is deterministic with no wall clock. Offline: no network, no
/// paid calls.
pub fn compare_ranking(
    bookmarks: &[Bookmark],
    labels: &[Label],
    verdicts: &[VerdictEntry],
    name: &str,
    min_records: usize,
) -> Result<RankingComparison, String> {
    let by_id: HashMap<&str, &Bookmark> =
        bookmarks.iter().map(|b| (b.post_id.as_str(), b)).collect();
    let worthy: HashSet<&str> = labels
        .iter()
        .filter(|l| l.brief_worthy)
        .map(|l| l.post_id.as_str())
        .collect();
    let mut resolved: Vec<(&Bookmark, bookmarks_agent::ranker::Signal, u64)> = Vec::new();
    let mut unmatched = 0usize;
    for v in verdicts {
        let Some(bookmark) = by_id.get(v.post_id.as_str()) else {
            unmatched += 1;
            continue;
        };
        let hint = format!("verdict for {}", v.post_id);
        if let Some(signal) = signal_of(&v.verdict, &hint)? {
            resolved.push((bookmark, signal, v.voted_at));
        }
    }
    resolved.sort_by(|a, b| (a.2, a.0.post_id.as_str()).cmp(&(b.2, b.0.post_id.as_str())));
    let test_n = if resolved.len() < 2 {
        resolved.len()
    } else {
        (resolved.len() / 3).clamp(1, resolved.len() - 1)
    };
    let split = resolved.len() - test_n;
    let now = resolved.iter().map(|(_, _, t)| *t).max().unwrap_or(0);
    let events: Vec<bookmarks_agent::ranker::LabeledEvent> = resolved[..split]
        .iter()
        .map(|(b, s, t)| bookmarks_agent::ranker::LabeledEvent {
            bookmark: (*b).clone(),
            signal: *s,
            at: Some(*t),
        })
        .collect();
    let ranker = bookmarks_agent::ranker::Ranker::train(&events, now, min_records);
    let test_rows: Vec<Bookmark> = resolved[split..]
        .iter()
        .map(|(b, _, _)| (*b).clone())
        .collect();
    let test_ids: HashSet<&str> = test_rows.iter().map(|b| b.post_id.as_str()).collect();
    let n_test_worthy = worthy.intersection(&test_ids).count();
    let candidates = bookmarks_agent::brief::select_candidates(&test_rows, &BTreeSet::new());
    let baseline_picks = bookmarks_agent::brief::local_picks(&candidates);
    let baseline = precision_at_5(&candidates, &baseline_picks, &worthy);
    let ranked = if ranker.is_active() {
        let scores = ranker.score_all(test_rows.iter());
        let reranked =
            bookmarks_agent::brief::select_candidates_ranked(&test_rows, &BTreeSet::new(), &scores);
        let picks = bookmarks_agent::brief::local_picks(&reranked);
        precision_at_5(&reranked, &picks, &worthy)
    } else {
        baseline
    };
    Ok(RankingComparison {
        name: name.to_string(),
        n_verdicts: resolved.len(),
        n_unmatched: unmatched,
        n_train: split,
        n_test: test_rows.len(),
        n_test_worthy,
        baseline_precision_at_5: baseline,
        ranked_precision_at_5: ranked,
        ranker_active: ranker.is_active(),
    })
}

/// Render the ranking comparison as markdown. Aggregate numbers only: no
/// post text, ids or urls ever appear.
pub fn render_comparison_markdown(c: &RankingComparison) -> String {
    format!(
        "# Ranking comparison — {name}\n\
         \n\
         Leave-later-out by vote time: {n_train} earlier verdicts train the ranker, \
         {n_test} later posts score it ({n_worthy} brief-worthy). \
         {n_unmatched} verdicts matched no bookmark.\n\
         \n\
         | Picks | P@5 |\n\
         | --- | --- |\n\
         | Baseline (today's order) | {base:.3} |\n\
         | Ranked (personal ranker{inactive}) | {ranked:.3} |\n",
        name = c.name,
        n_train = c.n_train,
        n_test = c.n_test,
        n_worthy = c.n_test_worthy,
        n_unmatched = c.n_unmatched,
        base = c.baseline_precision_at_5,
        ranked = c.ranked_precision_at_5,
        inactive = if c.ranker_active { "" } else { ", inactive" },
    )
}

/// Load bookmarks and labels from JSONL files and score both tasks.
/// Translations come from `$TRANSLATIONS_CACHE_PATH` when set, else
/// `translations.json` next to the bookmarks file, else none (every
/// non-English match counts as untranslated). Reads only; no model calls.
pub fn run(
    bookmarks_path: impl AsRef<Path>,
    labels_path: impl AsRef<Path>,
    name: &str,
) -> Result<EvalReport, String> {
    let bookmarks_path = bookmarks_path.as_ref();
    let bookmarks = bookmarks_agent::fixture::load(bookmarks_path)
        .map_err(|e| format!("bookmarks {}: {e}", bookmarks_path.display()))?;
    let labels = load_labels(labels_path.as_ref())?;
    let cache_path = match std::env::var(bookmarks_agent::translate::CACHE_ENV_VAR)
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(p) => std::path::PathBuf::from(p),
        None => bookmarks_path
            .parent()
            .map(|d| {
                d.join(bookmarks_agent::translate::CACHE_FILE_NAME)
                    .to_path_buf()
            })
            .unwrap_or_else(|| {
                std::path::PathBuf::from(bookmarks_agent::translate::CACHE_FILE_NAME)
            }),
    };
    let cache = bookmarks_agent::translate::load_cache(&cache_path);
    Ok(evaluate_with_cache(&bookmarks, &labels, name, &cache))
}

/// Render the report as markdown. Aggregate numbers only: no post text, ids
/// or urls ever appear.
pub fn render_markdown(report: &EvalReport) -> String {
    let t = &report.try_it;
    let r = &report.ranking;
    format!(
        "# Eval report — {name}\n\
         \n\
         Labels: {n_labels} total, {n_matched} matched, {n_unmatched} unmatched, {n_non_en} non-English ({n_tr} translated, {n_un} untranslated).\n\
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
        n_non_en = report.n_non_english,
        n_tr = report.n_translated,
        n_un = report.n_untranslated,
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
    fn ranking_comparison_runs_offline_on_synthetic_verdicts() {
        let (bookmarks, labels) = load_synthetic();
        let verdicts = load_verdicts(fixture_path("synthetic_verdicts.jsonl")).unwrap();
        assert_eq!(verdicts.len(), 24);
        let c = compare_ranking(&bookmarks, &labels, &verdicts, "synthetic", 10).unwrap();
        assert_eq!(c.n_verdicts, 24);
        assert_eq!(c.n_unmatched, 0);
        assert_eq!(c.n_train, 16);
        assert_eq!(c.n_test, 8);
        assert_eq!(c.n_test_worthy, 4);
        assert!(c.ranker_active);
        assert!((0.0..=1.0).contains(&c.baseline_precision_at_5));
        assert!((0.0..=1.0).contains(&c.ranked_precision_at_5));
        // Deterministic: same inputs give the same comparison.
        let again = compare_ranking(&bookmarks, &labels, &verdicts, "synthetic", 10).unwrap();
        assert_eq!(c, again);
        let md = render_comparison_markdown(&c);
        assert!(md.contains("Baseline (today's order)"), "{md}");
        assert!(md.contains("Ranked (personal ranker)"), "{md}");
        assert!(!md.contains("eval-"), "{md}");
        for b in &bookmarks {
            assert!(!md.contains(&b.text), "leaked post text");
            assert!(!md.contains(&b.url), "leaked url");
        }
    }

    #[test]
    fn ranking_comparison_skips_unmatched_and_unknown_labels() {
        let (bookmarks, labels) = load_synthetic();
        let mut verdicts = load_verdicts(fixture_path("synthetic_verdicts.jsonl")).unwrap();
        verdicts.push(VerdictEntry {
            post_id: "ghost".to_string(),
            verdict: "approve".to_string(),
            voted_at: 9_999_999_999,
        });
        // A hide-undo carries no preference: skipped, not an error.
        verdicts.push(VerdictEntry {
            post_id: "eval-1".to_string(),
            verdict: "unhide_author".to_string(),
            voted_at: 9_999_999_998,
        });
        let c = compare_ranking(&bookmarks, &labels, &verdicts, "synthetic", 10).unwrap();
        assert_eq!(c.n_unmatched, 1);
        assert_eq!(c.n_verdicts, 24);
        let bad = vec![VerdictEntry {
            post_id: "eval-1".to_string(),
            verdict: "maybe".to_string(),
            voted_at: 1,
        }];
        assert!(compare_ranking(&bookmarks, &labels, &bad, "synthetic", 10).is_err());
    }

    #[test]
    fn ranking_comparison_fails_closed_on_small_inputs() {
        let (bookmarks, labels) = load_synthetic();
        // Too few labels to activate: ranked equals baseline.
        let verdicts = load_verdicts(fixture_path("synthetic_verdicts.jsonl")).unwrap();
        let c = compare_ranking(&bookmarks, &labels, &verdicts[..5], "tiny", 10).unwrap();
        assert!(!c.ranker_active);
        assert_eq!(c.ranked_precision_at_5, c.baseline_precision_at_5);
        // Empty verdicts: zeros, no panic.
        let c = compare_ranking(&bookmarks, &labels, &[], "empty", 10).unwrap();
        assert_eq!(c.n_train, 0);
        assert_eq!(c.n_test, 0);
        assert_eq!(c.baseline_precision_at_5, 0.0);
        assert_eq!(c.ranked_precision_at_5, 0.0);
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
    fn non_english_bookmarks_are_sampled_and_scored_on_translation() {
        fn bm(id: &str, text: &str, lang: Option<&str>) -> Bookmark {
            Bookmark {
                post_id: id.to_string(),
                url: format!("https://x.com/i/status/{id}"),
                text: text.to_string(),
                author_username: None,
                created_at: None,
                folders: None,
                label: None,
                had_note_tweet: false,
                media: Vec::new(),
                lang: lang.map(str::to_string),
                urls: Vec::new(),
            }
        }
        let en_text =
            "[synthetic] The government presented an economic plan to support small local businesses.";
        let es_text =
            "[synthetic] El gobierno presentó un plan económico para apoyar pequeñas empresas locales.";
        // Hands-on English: the gold try-it label applies to the translation.
        let es_english = "[synthetic] Try it: 1. Do A 2. Do B";
        let bookmarks = vec![
            bm("syn-en", en_text, None),
            bm("syn-es-yes", es_text, Some("es")),
            bm("syn-es-no", es_text, Some("es")),
        ];
        let labels = vec![
            Label {
                post_id: "syn-en".to_string(),
                try_it: false,
                brief_worthy: true,
            },
            Label {
                post_id: "syn-es-yes".to_string(),
                try_it: true,
                brief_worthy: true,
            },
            Label {
                post_id: "syn-es-no".to_string(),
                try_it: false,
                brief_worthy: false,
            },
        ];
        // Cached translation for syn-es-yes only; syn-es-no stays untranslated.
        let mut cache = HashMap::new();
        cache.insert(
            bookmarks_agent::translate::cache_key("syn-es-yes", es_text),
            bookmarks_agent::translate::CacheEntry {
                english: es_english.to_string(),
                source_lang: Some("es".to_string()),
            },
        );
        let report = evaluate_with_cache(&bookmarks, &labels, "synthetic", &cache);
        // Nothing hidden for language: all three sampled.
        assert_eq!(report.n_labels, 3);
        assert_eq!(report.n_matched, 3);
        assert_eq!(report.n_unmatched_labels, 0);
        assert_eq!(report.n_non_english, 2);
        assert_eq!(report.n_translated, 1);
        assert_eq!(report.n_untranslated, 1);
        // syn-es-yes scores on its translation (hands-on => predicted yes).
        assert_eq!(
            report.try_it,
            Confusion {
                tp: 1,
                fp: 0,
                r#fn: 0,
                tn: 2,
            },
            "translation must flip the prediction to yes"
        );
        assert_eq!(report.ranking.n_brief_worthy, 2);
        assert_eq!(report.ranking.n_candidates, 3);
        let md = render_markdown(&report);
        assert!(
            md.contains("2 non-English (1 translated, 1 untranslated)"),
            "{md}"
        );
        assert!(!md.contains("syn-es"), "{md}");
        assert!(!md.contains(es_text), "leaked original");
        assert!(!md.contains(es_english), "leaked translation");
        // Without the cache both non-English posts score on the original
        // (still sampled, both untranslated): syn-es-yes becomes a miss.
        let plain = evaluate(&bookmarks, &labels, "synthetic");
        assert_eq!(plain.n_matched, 3);
        assert_eq!(plain.n_translated, 0);
        assert_eq!(plain.n_untranslated, 2);
        assert_eq!(plain.try_it.tp, 0);
        assert_eq!(plain.try_it.r#fn, 1);
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
