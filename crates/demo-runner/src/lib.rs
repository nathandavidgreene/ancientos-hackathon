//! Offline demo flow: sync from a synthetic fixture, build a deterministic
//! brief, record feedback, show the threshold shift and the decision log.
//! Uses mock model clients only; never makes network or paid calls.
use ancientos_core::approvals;
use ancientos_core::feedback::{self, FeedbackRecord, ThresholdPolicy, Verdict};
use ancientos_core::gate::DEEPER_PAID_ACTION;
use ancientos_core::log::DecisionLog;
use ancientos_router::dispatch::{dispatch, Backends};
use ancientos_router::muse::MuseUsage;
use ancientos_router::{Destination, MockClient, ModelClient, RouterError, Sensitivity};
use bookmarks_agent::fixture::{self, Bookmark};
use bookmarks_agent::{
    author_filter, brief, daemon, deeper, idea_card, media, translate, try_it, unroll,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
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
            jev_typesafe: None,
            jev_local: None,
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
        jev_typesafe: None,
        jev_local: None,
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

/// Fixed clock for the approvals demo: far future so the CLI's system-clock
/// pending check stays pending, deterministic across runs.
pub const APPROVALS_DEMO_NOW_RFC3339: &str = "2030-01-01T00:00:00Z";
/// Go-deeper post queued and released in the approvals demo.
pub const APPROVALS_DEMO_POST_ID: &str = "demo-3001";

/// Unix secs for [`APPROVALS_DEMO_NOW_RFC3339`]. Panics only when the fixed
/// constant fails to parse (a programming error, never user input).
pub fn approvals_demo_now() -> u64 {
    idea_card::parse_rfc3339(APPROVALS_DEMO_NOW_RFC3339).expect("fixed demo clock parses")
}

/// New demos are paid-off only: the real paid switch must stay unset while
/// paid work is simulated through mocks that can never touch the network.
fn require_paid_off() -> Result<(), String> {
    if std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return Err("demo is paid-off only (ANCIENTOS_ALLOW_PAID must be unset)".to_string());
    }
    Ok(())
}

/// Synthetic thread returned by the approvals-demo mock fetcher: two posts,
/// no quoted refs and no `/status/` links, so the post fetcher is never
/// called. Pure, no network.
fn mock_approvals_posts() -> Vec<unroll::ThreadPost> {
    vec![
        unroll::ThreadPost {
            id: "demo-3001".to_string(),
            author_id: "demo_threadsmith".to_string(),
            text: "[synthetic] Kanban review: our queue drains 40 percent faster when agents pick cards in batches of five."
                .to_string(),
            created_at: Some("2030-01-01T00:00:00+00:00".to_string()),
            conversation_id: Some("demo-3001".to_string()),
            referenced_tweets: Vec::new(),
            quoted: None,
        },
        unroll::ThreadPost {
            id: "demo-3002".to_string(),
            author_id: "demo_threadsmith".to_string(),
            text: "[synthetic] Friday follow-up: audits take time across thirty cards, so the log stays the source of truth."
                .to_string(),
            created_at: Some("2030-01-01T00:01:00+00:00".to_string()),
            conversation_id: Some("demo-3001".to_string()),
            referenced_tweets: vec![unroll::ReferencedTweet {
                kind: "replied_to".to_string(),
                id: "demo-3001".to_string(),
            }],
            quoted: None,
        },
    ]
}

/// Canned walkthrough returned by the approvals-demo mock Muse caller.
/// Imperative steps plus a Mapping section; no URLs, no network.
fn mock_approvals_walkthrough() -> String {
    "1. List due cards in batches of five\n\
     2. Write results back as a comment\n\
     3. Review the log each Friday\n\
     \n\
     ## Mapping\n\
     Maps to the scheduler and the decision log.\n"
        .to_string()
}

/// Proposal shown by the approvals demo: the queued job plus the pending
/// `deeper.paid` request the daemon proposed for it.
#[derive(Debug, Clone)]
pub struct ApprovalsPropose {
    pub post_id: String,
    pub at: u64,
    pub approval_id: String,
    pub expires_at: u64,
    pub proposed: usize,
    pub waiting: usize,
}

/// Paid execution shown by the approvals demo: the released approval plus
/// the walkthrough file the mock wrote. The file name is the basename only
/// (no temp dir) so demo output stays byte-identical.
#[derive(Debug, Clone)]
pub struct ApprovalsRelease {
    pub approval_id: String,
    pub executed: Vec<String>,
    pub walkthrough_file: String,
    pub walkthrough_head: Vec<String>,
}

/// Approval log excerpt: requested/approved/released kinds in log order plus
/// the verified total. No payloads or hashes (the deeper payload carries a
/// temp-dir path), so the output stays byte-identical.
#[derive(Debug, Clone)]
pub struct ApprovalsChain {
    pub kinds: Vec<(String, String)>,
    pub total: usize,
}

/// One cascade decision with the stage that produced it.
#[derive(Debug, Clone)]
pub struct CascadeItem {
    pub post_id: String,
    pub try_it: bool,
    pub stage: String,
    pub note: String,
}

/// Try-it cascade demo: three synthetic posts decided in stages with mock
/// Jev and mock Muse (no calls leave the process, paid stays off).
#[derive(Debug, Clone)]
pub struct CascadeDemo {
    pub items: Vec<CascadeItem>,
    pub topic_filtered: u64,
    pub rules_yes: u64,
    pub jev_calls: u64,
    pub jev_yes: u64,
    pub muse_calls: u64,
}

/// Translate demo: one synthetic Spanish post rendered in English with the
/// original kept, via a mock local model (sensitive text never reaches a
/// hosted model; paid stays off).
#[derive(Debug, Clone)]
pub struct TranslateDemo {
    pub post_id: String,
    pub original: String,
    pub english: String,
    pub marker: String,
}

/// Author demo: one synthetic author hidden, then unhidden. Counts come from
/// the synced store; the store itself is never modified.
#[derive(Debug, Clone)]
pub struct AuthorDemo {
    pub author: String,
    pub total: usize,
    pub hidden_count: usize,
    pub visible_after_hide: usize,
    pub visible_after_unhide: usize,
}

/// Fixed-reply mock model client: returns one canned translation with no
/// network. Used only for the translate demo's local path.
struct FixedReply {
    dest: Destination,
    reply: String,
}

impl ModelClient for FixedReply {
    fn destination(&self) -> Destination {
        self.dest
    }

    fn complete(&self, _prompt: &str) -> Result<String, RouterError> {
        Ok(self.reply.clone())
    }
}

impl Workspace {
    /// Walkthrough directory for the approvals demo (inside the temp data
    /// dir, never `ANCIENTOS_HOME`).
    pub fn deeper_dir(&self) -> PathBuf {
        self.dir.join("deeper")
    }

    fn approvals_ctx(&self, now: u64) -> daemon::AgentCtx {
        daemon::AgentCtx {
            log_path: self.log_path(),
            cards_path: self.idea_cards_path(),
            deeper_dir: self.deeper_dir(),
            live_store: self.dir.join("bookmarks.jsonl"),
            fixture: None,
            now_override: Some(now),
        }
    }

    /// First approvals tick: queue the demo go-deeper job (due now) and run
    /// the daemon's deeper queue with paid simulated ON through mocks that
    /// can never touch the network. The real `ANCIENTOS_ALLOW_PAID` switch
    /// must stay unset. Returns the pending `deeper.paid` approval.
    pub fn approvals_propose(&self) -> Result<ApprovalsPropose, String> {
        require_paid_off()?;
        let now = approvals_demo_now();
        let post_id = APPROVALS_DEMO_POST_ID;
        {
            let mut log = self.log()?;
            let entries = log.verify_or_empty().map_err(err)?;
            let already = deeper::list_queue(&entries)
                .iter()
                .any(|j| j.post_id == post_id && j.at == now);
            if !already {
                deeper::queue_later(&mut log, post_id, now, now)?;
            }
        }
        let ctx = self.approvals_ctx(now);
        let posts = mock_approvals_posts();
        let fetch_thread = |_id: &str| -> Result<bookmarks_agent::x_api::ThreadFetch, String> {
            Ok(bookmarks_agent::x_api::ThreadFetch {
                root: posts[0].clone(),
                username: Some("demo_threadsmith".to_string()),
                posts: posts.clone(),
                search_ok: true,
                search_note: None,
            })
        };
        let fetch_post = |id: &str| -> Result<unroll::ThreadPost, String> {
            Err(format!(
                "mock has no quoted/linked post {id} (demo thread has none)"
            ))
        };
        let call = |_prompt: &str,
                    _prior: Option<&str>|
         -> Result<(String, String, Option<MuseUsage>, Option<String>), String> {
            Ok((
                mock_approvals_walkthrough(),
                "mock-muse".to_string(),
                None,
                None,
            ))
        };
        let tick =
            daemon::deeper_queue_tick_with(&ctx, now, true, &fetch_thread, &fetch_post, &call)?;
        if tick.proposed.is_empty() && tick.waiting.is_empty() {
            return Err(format!("approvals demo: expected a proposal, got {tick:?}"));
        }
        let log = DecisionLog::open(self.log_path()).map_err(err)?;
        let payload = daemon::deeper_paid_payload(post_id, now);
        let payload_hash = approvals::canonical_hash(&approvals::redact(&payload));
        let pending = approvals::pending(&log, now).map_err(err)?;
        let found = pending
            .iter()
            .find(|a| {
                a.request.action == DEEPER_PAID_ACTION && a.request.payload_hash == payload_hash
            })
            .ok_or_else(|| {
                "approvals demo: no pending deeper.paid found after propose".to_string()
            })?;
        Ok(ApprovalsPropose {
            post_id: post_id.to_string(),
            at: now,
            approval_id: found.request.id.clone(),
            expires_at: found.request.expires_at,
            proposed: tick.proposed.len(),
            waiting: tick.waiting.len(),
        })
    }

    /// Second approvals tick: claim and release the human-approved request
    /// exactly once and run the walkthrough through the mock. Fails closed
    /// when nothing is approved yet (the CLI approve step runs first).
    pub fn approvals_release(&self) -> Result<ApprovalsRelease, String> {
        require_paid_off()?;
        let now = approvals_demo_now();
        let post_id = APPROVALS_DEMO_POST_ID;
        let ctx = self.approvals_ctx(now);
        let posts = mock_approvals_posts();
        let fetch_thread = |_id: &str| -> Result<bookmarks_agent::x_api::ThreadFetch, String> {
            Ok(bookmarks_agent::x_api::ThreadFetch {
                root: posts[0].clone(),
                username: Some("demo_threadsmith".to_string()),
                posts: posts.clone(),
                search_ok: true,
                search_note: None,
            })
        };
        let fetch_post = |id: &str| -> Result<unroll::ThreadPost, String> {
            Err(format!(
                "mock has no quoted/linked post {id} (demo thread has none)"
            ))
        };
        let call = |_prompt: &str,
                    _prior: Option<&str>|
         -> Result<(String, String, Option<MuseUsage>, Option<String>), String> {
            Ok((
                mock_approvals_walkthrough(),
                "mock-muse".to_string(),
                None,
                None,
            ))
        };
        let tick =
            daemon::deeper_queue_tick_with(&ctx, now, true, &fetch_thread, &fetch_post, &call)?;
        if tick.executed_paid.len() != 1 || tick.executed_paid[0] != post_id {
            return Err(format!(
                "approvals demo: expected exactly one paid execution, got {tick:?}"
            ));
        }
        let log = DecisionLog::open(self.log_path()).map_err(err)?;
        let payload = daemon::deeper_paid_payload(post_id, now);
        let payload_hash = approvals::canonical_hash(&approvals::redact(&payload));
        let decided = approvals::decided(&log, now).map_err(err)?;
        let found = decided
            .iter()
            .find(|a| {
                a.request.action == DEEPER_PAID_ACTION
                    && a.request.payload_hash == payload_hash
                    && a.status == approvals::Status::Released
            })
            .ok_or_else(|| {
                "approvals demo: no released deeper.paid found after release".to_string()
            })?;
        let path = self.deeper_dir().join(format!("{post_id}.md"));
        let body = std::fs::read_to_string(&path).map_err(err)?;
        let head: Vec<String> = body.lines().take(6).map(str::to_string).collect();
        if head.is_empty() {
            return Err("approvals demo: mock walkthrough is empty".to_string());
        }
        Ok(ApprovalsRelease {
            approval_id: found.request.id.clone(),
            executed: tick.executed_paid,
            walkthrough_file: format!("{post_id}.md"),
            walkthrough_head: head,
        })
    }

    /// Approval log excerpt for the demo: the requested/approved/released
    /// triple in log order plus the verified total.
    pub fn approvals_chain(&self) -> Result<ApprovalsChain, String> {
        let entries = DecisionLog::open(self.log_path())
            .and_then(|l| l.verify())
            .map_err(err)?;
        let total = entries.len();
        let mut kinds = Vec::new();
        for e in &entries {
            if e.kind == approvals::REQUESTED_KIND
                || e.kind == approvals::APPROVED_KIND
                || e.kind == approvals::RELEASED_KIND
            {
                let id = e
                    .payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                kinds.push((e.kind.clone(), id));
            }
        }
        if kinds.len() != 3 {
            return Err(format!(
                "approvals demo: expected 3 approval entries, got {}",
                kinds.len()
            ));
        }
        Ok(ApprovalsChain { kinds, total })
    }

    /// Try-it cascade on three synthetic posts with mock Jev and mock Muse:
    /// a stage-0 topic-filter no, a rules-confident yes, and a Jev yes. No
    /// calls leave the process; the mock noul and threshold are fixed.
    pub fn cascade_demo(&self) -> Result<CascadeDemo, String> {
        let _ = self;
        require_paid_off()?;
        let text1 = "[synthetic] OpenClaw setup guide: 1) install 2) run. Try it now.";
        let topic2 = "Health & Wellness";
        let text2 = "[synthetic] Desk stretch: 1) roll shoulders 10x 2) neck tilt 20s each side 3) seated twist 30s each side. Try it now.";
        let topic3 = "Local tooling";
        let text3 = "[synthetic] A quiet note about local file tooling and paper maps.";
        let excluded = ancientos_router::topic_filter::excluded_topic(text1)
            .ok_or_else(|| "cascade demo: demo-4001 must match an excluded topic".to_string())?;
        if ancientos_router::topic_filter::excluded_topic(text2).is_some() {
            return Err("cascade demo: demo-4002 must not be topic-filtered".to_string());
        }
        if !try_it::rules_confident(topic2, text2) {
            return Err("cascade demo: demo-4002 must be rules-confident".to_string());
        }
        if ancientos_router::topic_filter::excluded_topic(text3).is_some() {
            return Err("cascade demo: demo-4003 must not be topic-filtered".to_string());
        }
        if try_it::rules_confident(topic3, text3) {
            return Err("cascade demo: demo-4003 must be rules-unsure".to_string());
        }
        let threshold = ancientos_router::try_it_cascade::TRY_IT_THRESHOLD;
        let mock_noul = 0.90;
        if mock_noul < threshold {
            return Err("cascade demo: mock Jev noul must clear the threshold".to_string());
        }
        Ok(CascadeDemo {
            items: vec![
                CascadeItem {
                    post_id: "demo-4001".to_string(),
                    try_it: false,
                    stage: "topic_filter".to_string(),
                    note: format!("excluded topic '{excluded}', no call"),
                },
                CascadeItem {
                    post_id: "demo-4002".to_string(),
                    try_it: true,
                    stage: "rules".to_string(),
                    note: "rules-confident yes, no call".to_string(),
                },
                CascadeItem {
                    post_id: "demo-4003".to_string(),
                    try_it: true,
                    stage: "jev".to_string(),
                    note: format!("mock Jev noul={mock_noul:.2} >= {threshold:.2}"),
                },
            ],
            topic_filtered: 1,
            rules_yes: 1,
            jev_calls: 1,
            jev_yes: 1,
            muse_calls: 0,
        })
    }

    /// Translate one synthetic Spanish post through a mock local model,
    /// keeping the original. Sensitive text stays local; paid stays off.
    pub fn translate_demo(&self) -> Result<TranslateDemo, String> {
        let _ = self;
        require_paid_off()?;
        let post_id = "demo-5001";
        let original = "[synthetic] El gobierno presentó un plan económico para apoyar pequeñas empresas locales. Detalles en https://example.invalid/demo_plan"
            .to_string();
        let english_expected = "[synthetic] The government presented an economic plan to support small local businesses. Details at the demo plan page."
            .to_string();
        let b = Bookmark {
            post_id: post_id.to_string(),
            url: format!("https://example.invalid/demo_translator/status/{post_id}"),
            text: original.clone(),
            author_username: Some("demo_translator".to_string()),
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: Some("es".to_string()),
            urls: Vec::new(),
        };
        if bookmarks_agent::language::is_english(&b) {
            return Err("translate demo: demo-5001 must not be English".to_string());
        }
        if translate::sensitivity_of(&b.text) != Sensitivity::Sensitive {
            return Err(
                "translate demo: demo-5001 must be sensitive (URL) for the local mock".to_string(),
            );
        }
        let mock = FixedReply {
            dest: Destination::LocalLlama,
            reply: english_expected.clone(),
        };
        let mut cache: HashMap<String, translate::CacheEntry> = HashMap::new();
        let t = translate::translate_one(&b, &mut cache, None, Some(&mock));
        if t.status != translate::TranslationStatus::Translated {
            return Err("translate demo: mock local must translate".to_string());
        }
        let marker =
            translate::marker_for(&b, &t).ok_or_else(|| "translate demo: no marker".to_string())?;
        let shown = translate::english_for(&b, &t).to_string();
        if shown != english_expected {
            return Err("translate demo: english mismatch".to_string());
        }
        if t.original != original {
            return Err("translate demo: original must be kept".to_string());
        }
        Ok(TranslateDemo {
            post_id: post_id.to_string(),
            original,
            english: shown,
            marker,
        })
    }

    /// Hide one synthetic author, then undo the hide. Reports store counts;
    /// the store itself is never modified.
    pub fn author_demo(&self) -> Result<AuthorDemo, String> {
        require_paid_off()?;
        let author = "demo_author_0".to_string();
        let rows = fixture::load(self.store()).map_err(err)?;
        if rows.is_empty() {
            return Err("no bookmarks in synced store; sync first".to_string());
        }
        let total = rows.len();
        let policy = ThresholdPolicy::default();
        {
            let mut log = self.log()?;
            feedback::record(
                &mut log,
                FeedbackRecord {
                    pick_id: author.clone(),
                    category: "Rust & Systems".to_string(),
                    verdict: Verdict::NeverAuthor,
                    tags: Vec::new(),
                },
                policy,
            )
            .map_err(err)?;
        }
        let hidden_after_hide = {
            let log = self.log()?;
            let records = feedback::records_from_log(&log).map_err(err)?;
            feedback::hidden_authors(&records)
        };
        if !hidden_after_hide.contains(&author_filter::normalize(&author)) {
            return Err("author demo: hide failed".to_string());
        }
        let visible_after_hide = author_filter::filter_visible(&rows, &hidden_after_hide).len();
        let hidden_count = total.saturating_sub(visible_after_hide);
        {
            let mut log = self.log()?;
            feedback::record(
                &mut log,
                FeedbackRecord {
                    pick_id: author.clone(),
                    category: "Rust & Systems".to_string(),
                    verdict: Verdict::UnhideAuthor,
                    tags: Vec::new(),
                },
                policy,
            )
            .map_err(err)?;
        }
        let hidden_after_unhide = {
            let log = self.log()?;
            let records = feedback::records_from_log(&log).map_err(err)?;
            feedback::hidden_authors(&records)
        };
        if hidden_after_unhide.contains(&author_filter::normalize(&author)) {
            return Err("author demo: unhide failed".to_string());
        }
        let visible_after_unhide = author_filter::filter_visible(&rows, &hidden_after_unhide).len();
        Ok(AuthorDemo {
            author,
            total,
            hidden_count,
            visible_after_hide,
            visible_after_unhide,
        })
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

    fn second_fixture_path() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/demo/try_it_and_thread_bookmarks.jsonl"
        ))
    }

    #[test]
    fn cascade_demo_decides_in_stages() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let demo = ws.cascade_demo().unwrap();
        assert_eq!(demo.items.len(), 3);
        assert_eq!(demo.items[0].stage, "topic_filter");
        assert!(!demo.items[0].try_it);
        assert!(demo.items[0].note.contains("openclaw"));
        assert_eq!(demo.items[1].stage, "rules");
        assert!(demo.items[1].try_it);
        assert_eq!(demo.items[2].stage, "jev");
        assert!(demo.items[2].try_it);
        assert!(demo.items[2].note.contains("0.90"));
        assert_eq!(
            (
                demo.topic_filtered,
                demo.rules_yes,
                demo.jev_calls,
                demo.jev_yes,
                demo.muse_calls
            ),
            (1, 1, 1, 1, 0)
        );
    }

    #[test]
    fn translate_demo_keeps_original() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let demo = ws.translate_demo().unwrap();
        assert_eq!(demo.post_id, "demo-5001");
        assert!(demo.original.starts_with("[synthetic]"));
        assert!(demo.english.starts_with("[synthetic]"));
        assert_eq!(demo.marker, "Translated from es");
        assert!(demo.original.contains("example.invalid"));
    }

    #[test]
    fn author_demo_hides_then_undoes_without_touching_store() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        assert_eq!(ws.sync(&fixture_path()).unwrap(), 15);
        assert_eq!(ws.sync(&second_fixture_path()).unwrap(), 5);
        let before = std::fs::read_to_string(dir.path().join("store.jsonl")).unwrap();
        let demo = ws.author_demo().unwrap();
        assert_eq!(demo.author, "demo_author_0");
        assert_eq!((demo.total, demo.hidden_count), (20, 3));
        assert_eq!(demo.visible_after_hide, 17);
        assert_eq!(demo.visible_after_unhide, 20);
        let after = std::fs::read_to_string(dir.path().join("store.jsonl")).unwrap();
        assert_eq!(before, after, "author demo must never modify the store");
        assert!(
            ws.author_demo().is_ok(),
            "second run still hides then undoes"
        );
    }

    #[test]
    fn approvals_demo_proposes_releases_once_with_mock() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let proposed = ws.approvals_propose().unwrap();
        assert_eq!(proposed.post_id, APPROVALS_DEMO_POST_ID);
        assert_eq!(proposed.at, approvals_demo_now());
        assert_eq!(proposed.expires_at, approvals_demo_now() + 24 * 60 * 60);
        assert_eq!(proposed.approval_id.len(), 16);
        // Human decision with the fixed demo clock (the CLI `--now` path).
        {
            let mut log = DecisionLog::open(ws.log_path()).unwrap();
            let hash = approvals::get(&log, &proposed.approval_id)
                .unwrap()
                .request
                .payload_hash;
            approvals::approve(
                &mut log,
                &proposed.approval_id,
                &hash,
                "cli",
                approvals_demo_now() + 1,
            )
            .unwrap();
        }
        let released = ws.approvals_release().unwrap();
        assert_eq!(released.approval_id, proposed.approval_id);
        assert_eq!(released.executed, vec![APPROVALS_DEMO_POST_ID.to_string()]);
        assert_eq!(released.walkthrough_file, "demo-3001.md");
        assert!(released
            .walkthrough_head
            .iter()
            .any(|l| l.contains("List due cards")));
        assert!(dir.path().join("deeper").join("demo-3001.md").is_file());
        let chain = ws.approvals_chain().unwrap();
        let kinds: Vec<&str> = chain.kinds.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "approval.requested",
                "approval.approved",
                "approval.released"
            ]
        );
        assert!(chain
            .kinds
            .iter()
            .all(|(_, id)| id == &proposed.approval_id));
        assert_eq!(chain.total, 7);
        // Exactly once: the job is done, so a second release tick finds
        // nothing due and appends nothing (never a second release entry).
        assert!(ws.approvals_release().is_err());
        let entries = DecisionLog::open(ws.log_path()).unwrap().verify().unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind == approvals::RELEASED_KIND)
                .count(),
            1
        );
    }

    #[test]
    fn approvals_release_fails_closed_without_approval() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        ws.approvals_propose().unwrap();
        let err = ws.approvals_release().unwrap_err();
        assert!(
            err.contains("exactly one paid execution"),
            "unexpected: {err}"
        );
    }
}
