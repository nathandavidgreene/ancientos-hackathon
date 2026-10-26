//! Feedback on brief picks, recorded in the append-only decision log, plus
//! a deterministic learning threshold that decides which pick categories
//! may be auto-approved.
//!
//! Rules (fail closed):
//! - A category is auto-approved only after `min_votes` verdicts and an
//!   approval share of at least `approve_pct` percent.
//! - Any `NeverCategory` verdict blocks auto-approval for that category.
//! - `NeverPost` hides one post forever and is excluded from category
//!   stats entirely (it neither approves nor counts against the category).
//! - `Delegate` is neutral: it counts toward the vote total (like `Skip`)
//!   but is neither an approval nor a block.
//! - Unknown or empty categories are never auto-approved.
use crate::log::{DecisionLog, Entry, LogError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const FEEDBACK_KIND: &str = "feedback";
pub const THRESHOLD_KIND: &str = "threshold_change";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    Skip,
    NeverPost,
    #[serde(alias = "never")]
    NeverCategory,
    Delegate,
}

impl Verdict {
    /// Canonical snake_case spelling, matching serde.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Skip => "skip",
            Self::NeverPost => "never_post",
            Self::NeverCategory => "never_category",
            Self::Delegate => "delegate",
        }
    }
}

impl std::str::FromStr for Verdict {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "approve" => Ok(Self::Approve),
            "skip" => Ok(Self::Skip),
            "never_post" => Ok(Self::NeverPost),
            "never_category" | "never" => Ok(Self::NeverCategory),
            "delegate" => Ok(Self::Delegate),
            other => Err(format!("unknown verdict: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackRecord {
    pub pick_id: String,
    pub category: String,
    pub verdict: Verdict,
    /// Free-form tags from `vote --tag` (absent in old log entries).
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThresholdPolicy {
    pub min_votes: u32,
    pub approve_pct: u32,
}

impl Default for ThresholdPolicy {
    fn default() -> Self {
        Self {
            min_votes: 3,
            approve_pct: 80,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CategoryStats {
    pub approve: u32,
    pub skip: u32,
    pub never_category: u32,
    pub delegate: u32,
}

impl CategoryStats {
    pub fn total(&self) -> u32 {
        self.approve + self.skip + self.never_category + self.delegate
    }
}

fn valid_category(c: &str) -> bool {
    let c = c.trim();
    !c.is_empty() && !c.eq_ignore_ascii_case("unknown")
}

/// Tally verdicts per category (BTreeMap for deterministic order).
/// `NeverPost` verdicts are excluded: they hide one post but say nothing
/// about the category. `Delegate` verdicts count as neutral.
pub fn tally(records: &[FeedbackRecord]) -> BTreeMap<String, CategoryStats> {
    let mut m: BTreeMap<String, CategoryStats> = BTreeMap::new();
    for r in records {
        if r.verdict == Verdict::NeverPost {
            continue;
        }
        let s = m.entry(r.category.clone()).or_default();
        match r.verdict {
            Verdict::Approve => s.approve += 1,
            Verdict::Skip => s.skip += 1,
            Verdict::NeverCategory => s.never_category += 1,
            Verdict::Delegate => s.delegate += 1,
            Verdict::NeverPost => {}
        }
    }
    m
}

/// Post ids hidden forever by a `NeverPost` verdict. Once hidden, a post
/// stays hidden: later verdicts do not un-hide it.
pub fn hidden_posts(records: &[FeedbackRecord]) -> BTreeSet<String> {
    records
        .iter()
        .filter(|r| r.verdict == Verdict::NeverPost)
        .map(|r| r.pick_id.clone())
        .collect()
}

/// Categories that may be auto-approved under `policy`.
pub fn auto_approved(records: &[FeedbackRecord], policy: ThresholdPolicy) -> BTreeSet<String> {
    tally(records)
        .into_iter()
        .filter(|(cat, s)| {
            valid_category(cat)
                && s.never_category == 0
                && s.total() >= policy.min_votes
                && s.total() > 0
                && u64::from(s.approve) * 100
                    >= u64::from(policy.approve_pct) * u64::from(s.total())
        })
        .map(|(c, _)| c)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThresholdShift {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl ThresholdShift {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

pub fn diff(before: &BTreeSet<String>, after: &BTreeSet<String>) -> ThresholdShift {
    ThresholdShift {
        added: after.difference(before).cloned().collect(),
        removed: before.difference(after).cloned().collect(),
    }
}

/// Read all feedback records back out of a decision log.
pub fn records_from_log(log: &DecisionLog) -> Result<Vec<FeedbackRecord>, LogError> {
    let mut out = Vec::new();
    for e in log.verify_or_empty()? {
        if e.kind == FEEDBACK_KIND {
            out.push(serde_json::from_value(e.payload)?);
        }
    }
    Ok(out)
}

/// Append one verdict, and a `threshold_change` entry if the auto-approve
/// set shifted. Returns the shift (possibly empty).
pub fn record(
    log: &mut DecisionLog,
    rec: FeedbackRecord,
    policy: ThresholdPolicy,
) -> Result<(Entry, ThresholdShift), LogError> {
    let mut records = records_from_log(log)?;
    let before = auto_approved(&records, policy);
    let entry = log.append(FEEDBACK_KIND, serde_json::to_value(&rec)?)?;
    records.push(rec);
    let after = auto_approved(&records, policy);
    let shift = diff(&before, &after);
    if !shift.is_empty() {
        log.append(
            THRESHOLD_KIND,
            serde_json::json!({
                "added": shift.added,
                "removed": shift.removed,
                "auto_approved": after,
                "min_votes": policy.min_votes,
                "approve_pct": policy.approve_pct,
            }),
        )?;
    }
    Ok((entry, shift))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(cat: &str, v: Verdict) -> FeedbackRecord {
        FeedbackRecord {
            pick_id: format!("{cat}-{v:?}"),
            category: cat.into(),
            verdict: v,
            tags: Vec::new(),
        }
    }
    const P: ThresholdPolicy = ThresholdPolicy {
        min_votes: 3,
        approve_pct: 80,
    };

    #[test]
    fn needs_min_votes() {
        let recs = vec![r("Rust", Verdict::Approve), r("Rust", Verdict::Approve)];
        assert!(auto_approved(&recs, P).is_empty());
    }

    #[test]
    fn approves_after_threshold() {
        let recs = vec![r("Rust", Verdict::Approve); 3];
        assert_eq!(
            auto_approved(&recs, P),
            BTreeSet::from(["Rust".to_string()])
        );
    }

    #[test]
    fn ratio_boundary_is_inclusive() {
        let mut recs = vec![r("A", Verdict::Approve); 4];
        recs.push(r("A", Verdict::Skip));
        assert!(auto_approved(&recs, P).contains("A")); // 80%
        recs.push(r("A", Verdict::Skip));
        assert!(!auto_approved(&recs, P).contains("A")); // 66%
    }

    #[test]
    fn never_category_blocks_category() {
        let mut recs = vec![r("A", Verdict::Approve); 10];
        recs.push(r("A", Verdict::NeverCategory));
        assert!(auto_approved(&recs, P).is_empty());
    }

    #[test]
    fn never_post_is_excluded_from_stats_and_blocking() {
        // NeverPost alone: no votes counted, nothing auto-approved.
        let recs = vec![r("A", Verdict::NeverPost); 5];
        assert!(auto_approved(&recs, P).is_empty());
        assert!(!tally(&recs).contains_key("A"));
        // NeverPost alongside approvals: approvals still carry the category.
        let mut recs = vec![r("A", Verdict::Approve); 3];
        recs.push(r("A", Verdict::NeverPost));
        assert_eq!(auto_approved(&recs, P), BTreeSet::from(["A".to_string()]));
        let stats = tally(&recs);
        assert_eq!(stats["A"].total(), 3);
    }

    #[test]
    fn delegate_is_neutral_not_approval_or_block() {
        // Delegates alone never approve.
        let recs = vec![r("A", Verdict::Delegate); 3];
        assert!(auto_approved(&recs, P).is_empty());
        // A delegate counts toward the total but never vetoes: 4 approvals
        // plus 1 delegate still clears 80%.
        let mut recs = vec![r("A", Verdict::Approve); 4];
        recs.push(r("A", Verdict::Delegate));
        assert_eq!(auto_approved(&recs, P), BTreeSet::from(["A".to_string()]));
        assert_eq!(tally(&recs)["A"].delegate, 1);
    }

    #[test]
    fn hidden_posts_collects_only_never_post_ids() {
        let recs = vec![
            r("A", Verdict::Approve),
            FeedbackRecord {
                pick_id: "hidden-1".into(),
                category: "A".into(),
                verdict: Verdict::NeverPost,
                tags: Vec::new(),
            },
            r("A", Verdict::NeverCategory),
            r("A", Verdict::Delegate),
            FeedbackRecord {
                pick_id: "hidden-2".into(),
                category: "B".into(),
                verdict: Verdict::NeverPost,
                tags: Vec::new(),
            },
        ];
        assert_eq!(
            hidden_posts(&recs),
            BTreeSet::from(["hidden-1".to_string(), "hidden-2".to_string()])
        );
    }

    #[test]
    fn unknown_and_empty_never_auto() {
        let mut recs = vec![r("unknown", Verdict::Approve); 5];
        recs.extend(vec![r("  ", Verdict::Approve); 5]);
        assert!(auto_approved(&recs, P).is_empty());
    }

    #[test]
    fn deterministic_regardless_of_order() {
        let mut recs = vec![
            r("B", Verdict::Approve),
            r("A", Verdict::Approve),
            r("B", Verdict::Approve),
            r("A", Verdict::Approve),
            r("B", Verdict::Approve),
            r("A", Verdict::Approve),
        ];
        let a = auto_approved(&recs, P);
        recs.reverse();
        assert_eq!(a, auto_approved(&recs, P));
        assert_eq!(a.into_iter().collect::<Vec<_>>(), vec!["A", "B"]);
    }

    #[test]
    fn verdict_parses_snake_case_with_never_alias() {
        assert_eq!("approve".parse::<Verdict>().unwrap(), Verdict::Approve);
        assert_eq!("skip".parse::<Verdict>().unwrap(), Verdict::Skip);
        assert_eq!("never_post".parse::<Verdict>().unwrap(), Verdict::NeverPost);
        assert_eq!(
            "never_category".parse::<Verdict>().unwrap(),
            Verdict::NeverCategory
        );
        assert_eq!("delegate".parse::<Verdict>().unwrap(), Verdict::Delegate);
        // Backward compat with old log entries / CLI input.
        assert_eq!("never".parse::<Verdict>().unwrap(), Verdict::NeverCategory);
        assert!("maybe".parse::<Verdict>().is_err());
        assert!("Never".parse::<Verdict>().is_err());
    }

    #[test]
    fn verdict_serde_roundtrips_and_reads_old_never() {
        for (v, s) in [
            (Verdict::Approve, "approve"),
            (Verdict::Skip, "skip"),
            (Verdict::NeverPost, "never_post"),
            (Verdict::NeverCategory, "never_category"),
            (Verdict::Delegate, "delegate"),
        ] {
            assert_eq!(serde_json::to_value(v).unwrap(), serde_json::json!(s));
            assert_eq!(v.as_str(), s);
            assert_eq!(
                serde_json::from_value::<Verdict>(serde_json::json!(s)).unwrap(),
                v
            );
        }
        // Old log entries stored plain "never".
        assert_eq!(
            serde_json::from_value::<Verdict>(serde_json::json!("never")).unwrap(),
            Verdict::NeverCategory
        );
    }

    #[test]
    fn old_records_without_tags_still_parse() {
        let rec: FeedbackRecord = serde_json::from_value(serde_json::json!({
            "pick_id": "p1",
            "category": "Rust",
            "verdict": "never",
        }))
        .unwrap();
        assert_eq!(rec.verdict, Verdict::NeverCategory);
        assert!(rec.tags.is_empty());
    }

    #[test]
    fn record_appends_and_logs_shift() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        for i in 0..2 {
            let (_, s) = record(&mut log, r("Rust", Verdict::Approve), P).unwrap();
            assert!(s.is_empty(), "no shift at vote {i}");
        }
        let (_, s) = record(&mut log, r("Rust", Verdict::Approve), P).unwrap();
        assert_eq!(s.added, vec!["Rust"]);
        let (_, s) = record(&mut log, r("Rust", Verdict::NeverCategory), P).unwrap();
        assert_eq!(s.removed, vec!["Rust"]);
        let entries = log.verify().unwrap();
        let kinds: Vec<_> = entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                FEEDBACK_KIND,
                FEEDBACK_KIND,
                FEEDBACK_KIND,
                THRESHOLD_KIND,
                FEEDBACK_KIND,
                THRESHOLD_KIND
            ]
        );
        // survives reopen, history intact
        drop(log);
        let log = DecisionLog::open(&p).unwrap();
        assert_eq!(records_from_log(&log).unwrap().len(), 4);
    }

    #[test]
    fn empty_log_has_no_records() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path().join("none.jsonl")).unwrap();
        assert!(records_from_log(&log).unwrap().is_empty());
    }
}
