//! Offline demo flow: sync from a synthetic fixture, build a deterministic
//! brief, record feedback, show the threshold shift and the decision log.
//! Uses mock model clients only; never makes network or paid calls.
use ancientos_core::feedback::{self, FeedbackRecord, ThresholdPolicy, Verdict};
use ancientos_core::log::DecisionLog;
use ancientos_router::dispatch::{dispatch, Backends};
use ancientos_router::{Destination, MockClient, Sensitivity};
use bookmarks_agent::fixture::{self, Bookmark};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Categories whose content is treated as sensitive (stays on local Llama).
pub const SENSITIVE_CATEGORIES: &[&str] = &["Health & Personal"];
pub const BRIEF_SIZE: usize = 5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    pub pick_id: String,
    pub category: String,
    pub text: String,
    /// Router destination, e.g. `LocalLlama` (kept as text for the brief file).
    pub routed_to: String,
    pub auto_approved: bool,
}

pub struct Workspace {
    dir: PathBuf,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

pub fn category(b: &Bookmark) -> String {
    b.label
        .as_ref()
        .and_then(|l| l.parent_label.clone())
        .filter(|c| !c.trim().is_empty())
        .unwrap_or_else(|| "unknown".into())
}

pub fn sensitivity(cat: &str) -> Sensitivity {
    if cat == "unknown" || SENSITIVE_CATEGORIES.contains(&cat) {
        Sensitivity::Sensitive
    } else {
        Sensitivity::Sanitized
    }
}

/// Deterministic pick selection: round-robin over sorted categories,
/// rotated by `day`, taking the `day`-th item in each category.
pub fn select(rows: &[Bookmark], day: usize, n: usize) -> Vec<&Bookmark> {
    let cats: BTreeSet<String> = rows.iter().map(category).collect();
    let cats: Vec<String> = cats.into_iter().collect();
    if cats.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..cats.len().min(n) {
        let cat = &cats[(day + i) % cats.len()];
        let mut items: Vec<&Bookmark> = rows.iter().filter(|b| &category(b) == cat).collect();
        items.sort_by(|a, b| a.post_id.cmp(&b.post_id));
        out.push(items[day % items.len()]);
    }
    out
}

impl Workspace {
    pub fn new(dir: impl AsRef<Path>) -> Result<Self, String> {
        std::fs::create_dir_all(dir.as_ref()).map_err(err)?;
        Ok(Self {
            dir: dir.as_ref().into(),
        })
    }
    fn store(&self) -> PathBuf {
        self.dir.join("store.jsonl")
    }
    fn brief_path(&self) -> PathBuf {
        self.dir.join("brief.json")
    }
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("decision_log.jsonl")
    }
    fn log(&self) -> Result<DecisionLog, String> {
        DecisionLog::open(self.log_path()).map_err(err)
    }

    /// Offline "sync": copy new rows from a fixture into the store. Never drops rows.
    pub fn sync(&self, fixture_path: &Path) -> Result<usize, String> {
        let incoming = fixture::load(fixture_path).map_err(err)?;
        let mut rows = if self.store().exists() {
            fixture::load(self.store()).map_err(err)?
        } else {
            Vec::new()
        };
        let have: BTreeSet<String> = rows.iter().map(|b| b.post_id.clone()).collect();
        let new: Vec<Bookmark> = incoming
            .into_iter()
            .filter(|b| !have.contains(&b.post_id))
            .collect();
        let n = new.len();
        rows.extend(new);
        let body: Vec<String> = rows
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .map_err(err)?;
        std::fs::write(self.store(), body.join("\n") + "\n").map_err(err)?;
        self.log()?
            .append(
                "sync",
                serde_json::json!({"source": "synthetic_fixture", "new": n, "total": rows.len()}),
            )
            .map_err(err)?;
        Ok(n)
    }

    pub fn auto_approved(&self, policy: ThresholdPolicy) -> Result<BTreeSet<String>, String> {
        let log = self.log()?;
        Ok(feedback::auto_approved(
            &feedback::records_from_log(&log).map_err(err)?,
            policy,
        ))
    }

    /// Build the day's brief. Each pick's summary is routed through the
    /// privacy router (mock clients) and the route is logged.
    pub fn brief(&self, day: usize, policy: ThresholdPolicy) -> Result<Vec<Pick>, String> {
        let rows = fixture::load(self.store()).map_err(err)?;
        let auto = self.auto_approved(policy)?;
        let (local, spark, wonder) = (
            MockClient(Destination::LocalLlama),
            MockClient(Destination::MuseSpark),
            MockClient(Destination::MuseWonder),
        );
        let b = Backends {
            local: &local,
            muse_spark: &spark,
            muse_wonder: &wonder,
        };
        let mut log = self.log()?;
        let mut picks = Vec::new();
        for bm in select(&rows, day, BRIEF_SIZE) {
            let cat = category(bm);
            let (dest, _summary) = dispatch(&b, sensitivity(&cat), false, &bm.text).map_err(err)?;
            log.append(
                "route",
                serde_json::json!({"pick_id": bm.post_id, "category": cat, "to": dest}),
            )
            .map_err(err)?;
            picks.push(Pick {
                pick_id: bm.post_id.clone(),
                auto_approved: auto.contains(&cat),
                category: cat,
                text: bm.text.clone(),
                routed_to: format!("{dest:?}"),
            });
        }
        log.append(
            "brief",
            serde_json::json!({"day": day, "picks": picks.iter().map(|p| &p.pick_id).collect::<Vec<_>>()}),
        )
        .map_err(err)?;
        std::fs::write(
            self.brief_path(),
            serde_json::to_string_pretty(&picks).map_err(err)?,
        )
        .map_err(err)?;
        Ok(picks)
    }

    /// Record a verdict on a pick from the latest brief. Unknown pick ids fail closed.
    pub fn feedback(
        &self,
        pick_id: &str,
        verdict: Verdict,
        policy: ThresholdPolicy,
    ) -> Result<feedback::ThresholdShift, String> {
        let picks: Vec<Pick> =
            serde_json::from_str(&std::fs::read_to_string(self.brief_path()).map_err(err)?)
                .map_err(err)?;
        let pick = picks
            .iter()
            .find(|p| p.pick_id == pick_id)
            .ok_or_else(|| format!("pick {pick_id} is not in the current brief"))?;
        let mut log = self.log()?;
        let (_, shift) = feedback::record(
            &mut log,
            FeedbackRecord {
                pick_id: pick.pick_id.clone(),
                category: pick.category.clone(),
                verdict,
            },
            policy,
        )
        .map_err(err)?;
        Ok(shift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/demo/synthetic_bookmarks.jsonl"
        ))
    }

    #[test]
    fn fixture_is_synthetic_only() {
        let rows = fixture::load(fixture_path()).unwrap();
        assert_eq!(rows.len(), 15);
        assert!(rows
            .iter()
            .all(|b| b.text.starts_with("[synthetic]")
                && b.url.starts_with("https://example.invalid/")));
    }

    #[test]
    fn end_to_end_threshold_shift() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let p = ThresholdPolicy::default();
        assert_eq!(ws.sync(&fixture_path()).unwrap(), 15);
        assert_eq!(ws.sync(&fixture_path()).unwrap(), 0);
        let mut added = Vec::new();
        for day in 0..3 {
            let picks = ws.brief(day, p).unwrap();
            assert_eq!(picks.len(), 5);
            for pk in &picks {
                if pk.category == "Health & Personal" {
                    assert_eq!(pk.routed_to, "LocalLlama");
                }
                let v = if pk.category == "Rust & Systems" {
                    Verdict::Approve
                } else {
                    Verdict::Skip
                };
                added.extend(ws.feedback(&pk.pick_id, v, p).unwrap().added);
            }
        }
        assert_eq!(added, vec!["Rust & Systems"]);
        let picks = ws.brief(3, p).unwrap();
        assert!(picks
            .iter()
            .all(|pk| pk.auto_approved == (pk.category == "Rust & Systems")));
        assert!(ws.feedback("not-a-pick", Verdict::Approve, p).is_err());
        DecisionLog::open(ws.log_path()).unwrap().verify().unwrap();
    }

    #[test]
    fn select_is_deterministic() {
        let rows = fixture::load(fixture_path()).unwrap();
        let a: Vec<_> = select(&rows, 2, 5)
            .iter()
            .map(|b| b.post_id.clone())
            .collect();
        let b: Vec<_> = select(&rows, 2, 5)
            .iter()
            .map(|b| b.post_id.clone())
            .collect();
        assert_eq!(a, b);
        assert!(select(&[], 0, 5).is_empty());
    }
}
