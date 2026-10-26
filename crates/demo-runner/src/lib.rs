//! Offline demo flow: sync from a synthetic fixture, build a deterministic
//! brief, record feedback, show the threshold shift and the decision log.
//! Uses mock model clients only; never makes network or paid calls.
use ancientos_core::feedback::{self, FeedbackRecord, ThresholdPolicy, Verdict};
use ancientos_core::log::DecisionLog;
use ancientos_router::dispatch::{dispatch, Backends};
use ancientos_router::{Destination, MockClient, Sensitivity};
use bookmarks_agent::fixture::{self, Bookmark};
use bookmarks_agent::{brief, idea_card, media, try_it, unroll};
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
                tags: Vec::new(),
            },
            policy,
        )
        .map_err(err)?;
        Ok(shift)
    }

    /// Path for demo idea cards (inside the temp data dir, never ANCIENTOS_HOME).
    pub fn idea_cards_path(&self) -> PathBuf {
        self.dir.join("idea_cards.jsonl")
    }

    /// Path for the demo backlog file (inside the temp data dir).
    pub fn backlog_path(&self) -> PathBuf {
        self.dir.join("backlog.md")
    }

    /// Path for the tampered log copy used by the tamper demo.
    pub fn tampered_path(&self) -> PathBuf {
        self.dir.join("tampered_log.jsonl")
    }

    /// Rich brief for the demo: normal picks plus the unrolled synthetic
    /// thread and a media example, all loaded from the synced store.
    pub fn brief_rich(
        &self,
        day: usize,
        policy: ThresholdPolicy,
        date: &str,
    ) -> Result<RichBrief, String> {
        let picks = self.brief(day, policy)?;
        let rows = fixture::load(self.store()).map_err(err)?;
        let thread = thread_chain_from_rows(&rows);
        if thread.is_empty() {
            return Err("thread demo-2001 not in synced store; sync first".to_string());
        }
        let (media_for, media_items) = media_example_from_rows(&rows, &picks);
        let media_markdown = media::render_markdown(&media_items);
        if media_markdown.is_empty() {
            return Err("no media in synced store; sync first".to_string());
        }
        Ok(RichBrief {
            picks,
            thread,
            media_for,
            media: media_items,
            media_markdown,
            date: date.to_string(),
        })
    }

    /// Voting demo: approve Rust on days 1-3, showcase every verdict on day 2,
    /// then return the threshold set and the final brief.
    pub fn vote_demo(&self, policy: ThresholdPolicy) -> Result<VoteDemo, String> {
        let mut votes = Vec::new();
        for day in [1usize, 2, 3] {
            let picks = self.brief(day, policy)?;
            // On day 2, reserve two non-Rust/non-Hot picks for never_post/delegate.
            let mut specials: Vec<String> = Vec::new();
            if day == 2 {
                for p in &picks {
                    if p.category != "Rust & Systems" && p.category != "Hot Takes" {
                        specials.push(p.pick_id.clone());
                    }
                }
            }
            let never_post_id = specials.first().cloned().unwrap_or_default();
            let delegate_id = specials.get(1).cloned().unwrap_or_default();
            for p in &picks {
                let verdict = if p.category == "Rust & Systems" {
                    Verdict::Approve
                } else if day == 2 && p.category == "Hot Takes" {
                    Verdict::NeverCategory
                } else if day == 2 && p.pick_id == never_post_id {
                    Verdict::NeverPost
                } else if day == 2 && p.pick_id == delegate_id {
                    Verdict::Delegate
                } else {
                    Verdict::Skip
                };
                let shift = self.feedback(&p.pick_id, verdict, policy)?;
                votes.push(VoteStep {
                    pick_id: p.pick_id.clone(),
                    category: p.category.clone(),
                    verdict,
                    shift,
                });
            }
        }
        let thresholds = self.auto_approved(policy)?;
        let final_picks = self.brief(4, policy)?;
        Ok(VoteDemo {
            votes,
            thresholds,
            final_picks,
        })
    }

    /// Offline delegate demo: unroll the synthetic thread, create idea cards
    /// from a mock delegate summary, list them, and adopt the first.
    pub fn delegate_demo(&self, post_id: &str, now: u64) -> Result<DelegateDemo, String> {
        let rows = fixture::load(self.store()).map_err(err)?;
        let thread = thread_chain_from_rows(&rows);
        if thread.is_empty() || thread[0].id != post_id {
            return Err(format!("thread {post_id} not in synced store; sync first"));
        }
        let source_url = rows
            .iter()
            .find(|b| b.post_id == post_id)
            .map(|b| b.url.clone())
            .unwrap_or_else(|| format!("https://example.invalid/demo/status/{post_id}"));
        let output = mock_delegate_output();
        let cards_path = self.idea_cards_path();
        let made = idea_card::capture_from_delegate(&cards_path, &source_url, &output, now)?;
        let ideas = idea_card::list_due_for_triage(&cards_path, now)?;
        let first_id = ideas
            .first()
            .map(|c| c.id.clone())
            .ok_or_else(|| "no idea cards due for triage".to_string())?;
        let adopted = idea_card::triage(
            &cards_path,
            &self.backlog_path(),
            &first_id,
            idea_card::Triage::Adopt,
            now,
        )?;
        let backlog_line = idea_card::backlog_item(&adopted);
        Ok(DelegateDemo {
            thread,
            output,
            made,
            ideas,
            adopted_id: first_id,
            backlog_line,
        })
    }

    /// Try-it demo: show tagged hands-on picks with steps, then pending/tried.
    pub fn try_demo(&self) -> Result<TryDemo, String> {
        let rows = fixture::load(self.store()).map_err(err)?;
        let mut tagged: Vec<TaggedPost> = Vec::new();
        for b in &rows {
            if b.post_id != "demo-2101" && b.post_id != "demo-2102" {
                continue;
            }
            if let Some(tag) = try_it::classify(b) {
                tagged.push(TaggedPost {
                    post_id: b.post_id.clone(),
                    reason: tag.reason,
                    steps: try_it::extract_steps(&b.text),
                });
            }
        }
        tagged.sort_by(|a, b| a.post_id.cmp(&b.post_id));
        if tagged.is_empty() {
            return Err("no hands-on posts in synced store; sync first".to_string());
        }
        let mut log = self.log()?;
        // Fixed demo clock: the log shot prints payloads, so wall time here
        // would break byte-identical demo output.
        let shown_at = demo_now_secs();
        for t in &tagged {
            try_it::record_shown(&mut log, &t.post_id, shown_at)?;
        }
        let pending_before = try_it::pending_followups(&log);
        let first = pending_before
            .first()
            .cloned()
            .unwrap_or_else(|| tagged[0].post_id.clone());
        try_it::record_followup(&mut log, &first, try_it::Tried::Yes, None)?;
        let pending_after = try_it::pending_followups(&log);
        Ok(TryDemo {
            tagged,
            pending_before,
            tried_id: first,
            pending_after,
        })
    }

    /// Verify the log, then tamper a copy and show verification fails closed.
    pub fn tamper_demo(&self) -> Result<TamperDemo, String> {
        let entries = DecisionLog::open(self.log_path())
            .and_then(|l| l.verify())
            .map_err(|e| e.to_string())?;
        let count = entries.len();
        let body = std::fs::read_to_string(self.log_path()).map_err(err)?;
        // Deterministic single-line tamper: flip the first kind value.
        let tampered = body.replacen("\"sync\"", "\"xync\"", 1);
        let tampered = if tampered == body {
            body.replacen("\"route\"", "\"xoute\"", 1)
        } else {
            tampered
        };
        let tampered = if tampered == body {
            body.replacen('a', "b", 1)
        } else {
            tampered
        };
        std::fs::write(self.tampered_path(), tampered).map_err(err)?;
        let failure = match DecisionLog::open(self.tampered_path()).and_then(|l| l.verify()) {
            Ok(_) => "unexpectedly verified".to_string(),
            Err(e) => e.to_string(),
        };
        Ok(TamperDemo {
            verified: count,
            failure,
        })
    }
}

/// Env var carrying the fixed demo clock (unix secs or RFC 3339 / YYYY-MM-DD).
pub const DEMO_FIXED_NOW_ENV: &str = "ANCIENTOS_DEMO_FIXED_NOW";

/// Fixed clock for deterministic demos. Falls back to real time when unset.
pub fn demo_now_secs() -> u64 {
    let raw = std::env::var(DEMO_FIXED_NOW_ENV).unwrap_or_default();
    let s = raw.trim();
    if s.is_empty() {
        return idea_card::now_secs();
    }
    if let Ok(n) = s.parse::<u64>() {
        return n;
    }
    if let Some(n) = idea_card::parse_rfc3339(s) {
        return n;
    }
    if s.len() >= 10 {
        let day = &s[..10];
        if let Some(n) = idea_card::parse_rfc3339(&format!("{day}T00:00:00Z")) {
            return n;
        }
    }
    idea_card::now_secs()
}

/// Fixed demo date (`YYYY-MM-DD`). Falls back to the real current date.
pub fn demo_date() -> String {
    let raw = std::env::var(DEMO_FIXED_NOW_ENV).unwrap_or_default();
    let s = raw.trim().to_string();
    if s.is_empty() {
        return brief::current_date();
    }
    if s.len() >= 10 {
        let day = &s[..10];
        let b = day.as_bytes();
        let is_day = b.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && b[0..4].iter().all(|c| c.is_ascii_digit())
            && b[5..7].iter().all(|c| c.is_ascii_digit())
            && b[8..10].iter().all(|c| c.is_ascii_digit());
        if is_day {
            return day.to_string();
        }
    }
    if let Ok(n) = s.parse::<u64>() {
        return brief::date_from_days((n / 86400) as i64);
    }
    if let Some(n) = idea_card::parse_rfc3339(&s) {
        return brief::date_from_days((n / 86400) as i64);
    }
    brief::current_date()
}

/// Rich brief content for shot 5.
#[derive(Debug, Clone)]
pub struct RichBrief {
    pub picks: Vec<Pick>,
    pub thread: Vec<unroll::ThreadPost>,
    pub media_for: String,
    pub media: Vec<media::Media>,
    pub media_markdown: String,
    pub date: String,
}

/// One recorded vote plus any threshold shift it caused.
#[derive(Debug, Clone)]
pub struct VoteStep {
    pub pick_id: String,
    pub category: String,
    pub verdict: Verdict,
    pub shift: feedback::ThresholdShift,
}

/// Voting demo content for shot 7.
#[derive(Debug, Clone)]
pub struct VoteDemo {
    pub votes: Vec<VoteStep>,
    pub thresholds: BTreeSet<String>,
    pub final_picks: Vec<Pick>,
}

/// Delegate demo content for shot 8.
#[derive(Debug, Clone)]
pub struct DelegateDemo {
    pub thread: Vec<unroll::ThreadPost>,
    pub output: String,
    pub made: Vec<idea_card::IdeaCard>,
    pub ideas: Vec<idea_card::IdeaCard>,
    pub adopted_id: String,
    pub backlog_line: String,
}

/// One hands-on post with its tag reason and steps.
#[derive(Debug, Clone)]
pub struct TaggedPost {
    pub post_id: String,
    pub reason: String,
    pub steps: Vec<String>,
}

/// Try-it demo content for shot 9.
#[derive(Debug, Clone)]
pub struct TryDemo {
    pub tagged: Vec<TaggedPost>,
    pub pending_before: Vec<String>,
    pub tried_id: String,
    pub pending_after: Vec<String>,
}

/// Tamper demo content for shot 10.
#[derive(Debug, Clone)]
pub struct TamperDemo {
    pub verified: usize,
    pub failure: String,
}

/// Privacy routing demo: one sensitive and one sanitized category.
pub fn routing_demo() -> Result<Vec<(String, Destination)>, String> {
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
    let mut out = Vec::new();
    for (cat, sens) in [
        ("Health & Personal", Sensitivity::Sensitive),
        ("Rust & Systems", Sensitivity::Sanitized),
    ] {
        let (dest, _) = dispatch(&b, sens, false, "synthetic demo text").map_err(err)?;
        out.push((cat.to_string(), dest));
    }
    Ok(out)
}

/// Deterministic mock delegate summary with gate + decision_log ideas.
pub fn mock_delegate_output() -> String {
    "## Setups\n\
     - Kanban board with an Agent column\n\
     ## Worth borrowing for AncientOS\n\
     - Drag a card to approve\n\
     - Shared memory log with history\n"
        .to_string()
}

/// Unrolled demo-2001 thread from synced store rows.
fn thread_chain_from_rows(rows: &[Bookmark]) -> Vec<unroll::ThreadPost> {
    let find = |id: &str| rows.iter().find(|b| b.post_id == id);
    let (Some(r1), Some(r2), Some(r3)) = (find("demo-2001"), find("demo-2002"), find("demo-2003"))
    else {
        return Vec::new();
    };
    let author = r1
        .author_username
        .clone()
        .unwrap_or_else(|| "demo_threadsmith".to_string());
    let mk = |b: &Bookmark, reply_to: Option<&str>| unroll::ThreadPost {
        id: b.post_id.clone(),
        author_id: author.clone(),
        text: b.text.clone(),
        created_at: b.created_at.clone(),
        conversation_id: Some("demo-2001".to_string()),
        referenced_tweets: reply_to
            .map(|p| {
                vec![unroll::ReferencedTweet {
                    kind: "replied_to".to_string(),
                    id: p.to_string(),
                }]
            })
            .unwrap_or_default(),
        quoted: None,
    };
    let root = mk(r1, None);
    let c2 = mk(r2, Some("demo-2001"));
    let c3 = mk(r3, Some("demo-2002"));
    unroll::unroll(&root, &[c2, c3])
}

/// Media example preferring the current picks, then the demo fixtures.
fn media_example_from_rows(rows: &[Bookmark], picks: &[Pick]) -> (String, Vec<media::Media>) {
    for p in picks {
        if let Some(b) = rows.iter().find(|b| b.post_id == p.pick_id) {
            if !b.media.is_empty() {
                return (b.post_id.clone(), b.media.clone());
            }
        }
    }
    for id in ["demo-2101", "demo-2102"] {
        if let Some(b) = rows.iter().find(|b| b.post_id == id) {
            if !b.media.is_empty() {
                return (b.post_id.clone(), b.media.clone());
            }
        }
    }
    if let Some(b) = rows.iter().find(|b| !b.media.is_empty()) {
        return (b.post_id.clone(), b.media.clone());
    }
    (String::new(), Vec::new())
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
