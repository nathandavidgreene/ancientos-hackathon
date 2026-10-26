//! Feedback on brief picks: approve / skip / never, recorded in the
//! append-only decision log, plus a deterministic learning threshold that
//! decides which pick categories may be auto-approved.
//!
//! Rules (fail closed):
//! - A category is auto-approved only after `min_votes` verdicts and an
//!   approval share of at least `approve_pct` percent.
//! - Any `Never` verdict blocks auto-approval for that category.
//! - Unknown or empty categories are never auto-approved.
use crate::log::{DecisionLog, Entry, LogError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const FEEDBACK_KIND: &str = "feedback";
pub const THRESHOLD_KIND: &str = "threshold_change";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Approve,
    Skip,
    Never,
}

impl std::str::FromStr for Verdict {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "approve" => Ok(Self::Approve),
            "skip" => Ok(Self::Skip),
            "never" => Ok(Self::Never),
            other => Err(format!("unknown verdict: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackRecord {
    pub pick_id: String,
    pub category: String,
    pub verdict: Verdict,
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
    pub never: u32,
}

impl CategoryStats {
    pub fn total(&self) -> u32 {
        self.approve + self.skip + self.never
    }
}

fn valid_category(c: &str) -> bool {
    let c = c.trim();
    !c.is_empty() && !c.eq_ignore_ascii_case("unknown")
}

/// Tally verdicts per category (BTreeMap for deterministic order).
pub fn tally(records: &[FeedbackRecord]) -> BTreeMap<String, CategoryStats> {
    let mut m: BTreeMap<String, CategoryStats> = BTreeMap::new();
    for r in records {
        let s = m.entry(r.category.clone()).or_default();
        match r.verdict {
            Verdict::Approve => s.approve += 1,
            Verdict::Skip => s.skip += 1,
            Verdict::Never => s.never += 1,
        }
    }
    m
}

/// Categories that may be auto-approved under `policy`.
pub fn auto_approved(records: &[FeedbackRecord], policy: ThresholdPolicy) -> BTreeSet<String> {
    tally(records)
        .into_iter()
        .filter(|(cat, s)| {
            valid_category(cat)
                && s.never == 0
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
    fn never_blocks_category() {
        let mut recs = vec![r("A", Verdict::Approve); 10];
        recs.push(r("A", Verdict::Never));
        assert!(auto_approved(&recs, P).is_empty());
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
    fn verdict_parses() {
        assert_eq!("never".parse::<Verdict>().unwrap(), Verdict::Never);
        assert!("maybe".parse::<Verdict>().is_err());
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
        let (_, s) = record(&mut log, r("Rust", Verdict::Never), P).unwrap();
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
