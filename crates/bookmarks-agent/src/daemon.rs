//! Agent scheduler daemon: six small agents ticking on a schedule.
//!
//! [`DaemonConfig::build_scheduler`] builds a [`Scheduler`] with one
//! [`Agent`] impl per job; `bookmarks-agent run` ticks it once (`--once`)
//! or in a loop. Every agent run appends a [`DAEMON_TICK_KIND`] entry
//! (agent name, ok/error, no secrets); an agent error never stops the
//! others (each tick result is collected, and the agent fails closed).
//!
//! Paid work is never executed by the daemon directly: when
//! `ANCIENTOS_ALLOW_PAID=1` the brief agent proposes a `brief.paid`
//! approval and the deeper-queue agent proposes `deeper.paid` approvals
//! (both [`Risk::High`](ancientos_core::gate::Risk) in the single
//! [`app_gate`](crate::app_gate) policy) instead of
//! calling Muse. A paid walkthrough runs only after a human approves the
//! exact payload: the daemon claims the release token
//! ([`claim_release_token`](ancientos_core::approvals::claim_release_token))
//! and moves it into the paid executor, which releases it
//! ([`release`](ancientos_core::approvals::release), exactly once) before
//! running the existing deeper paid path (which still requires
//! `ANCIENTOS_ALLOW_PAID=1`). Denied, expired or already-released proposals
//! never execute; a released-but-unfinished job needs a fresh approval.
//!
//! Clocks: `--now <rfc3339>` fixes the wall clock for deterministic runs
//! (log timestamps, brief dates, due checks); without it the system clock
//! is read per tick. Scheduler due checks use a monotonic clock.
//!
//! Intervals and loop pacing come from env with sane defaults
//! ([`DaemonConfig::from_paths`]); `DAEMON_MAX_TICKS` bounds the loop for
//! tests. Ctrl-C stops the loop via the default disposition (there is no
//! signal handler without new crates); the loop also checks a stop flag.

use crate::{BRIEF_PAID_ACTION, DEEPER_PAID_ACTION};
use ancientos_core::approvals;
use ancientos_core::log::{DecisionLog, Entry};
use ancientos_core::loop_guard::{self, Budgets, LoopGuard};
use ancientos_core::scheduler::{Agent, Scheduler};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Agent name recorded for daemon proposals (the bookmarks agent proposes;
/// a human approves).
pub use crate::brief::AGENT_NAME as DAEMON_AGENT_NAME;
/// Log kind appended once per agent run: `{agent, ok, at, error?}`.
pub const DAEMON_TICK_KIND: &str = "daemon.tick";
/// Log kind appended by the try-followup agent: `{count, post_ids, at}`.
pub const TRY_FOLLOWUP_KIND: &str = "daemon.try_followup";
/// Log kind appended before a released paid job executes:
/// `{approval_id, payload_hash, post_id, at, started_at}`.
pub const JOB_STARTED_KIND: &str = "job.started";
/// Log kind appended after a released paid job completes:
/// `{approval_id, payload_hash, post_id, at, done_at}`.
pub const JOB_DONE_KIND: &str = "job.done";

/// Env switches for intervals (seconds), loop sleep and the test tick cap.
pub const BRIEF_SECS_ENV: &str = "DAEMON_BRIEF_SECS";
pub const DIGEST_SECS_ENV: &str = "DAEMON_DIGEST_SECS";
pub const TRY_FOLLOWUP_SECS_ENV: &str = "DAEMON_TRY_FOLLOWUP_SECS";
pub const DEEPER_QUEUE_SECS_ENV: &str = "DAEMON_DEEPER_QUEUE_SECS";
pub const SWEEP_SECS_ENV: &str = "DAEMON_SWEEP_SECS";
pub const WIKI_LINT_SECS_ENV: &str = "DAEMON_WIKI_LINT_SECS";
pub const TICK_SLEEP_SECS_ENV: &str = "DAEMON_TICK_SLEEP_SECS";
pub const MAX_TICKS_ENV: &str = "DAEMON_MAX_TICKS";

/// Sane defaults: brief and try-followup daily, digest and wiki lint
/// weekly, queued go-deeper jobs every 15 minutes, expiry sweep hourly, 60s
/// between ticks.
pub const DEFAULT_BRIEF_SECS: u64 = 24 * 60 * 60;
pub const DEFAULT_DIGEST_SECS: u64 = 7 * 24 * 60 * 60;
pub const DEFAULT_TRY_FOLLOWUP_SECS: u64 = 24 * 60 * 60;
pub const DEFAULT_DEEPER_QUEUE_SECS: u64 = 15 * 60;
pub const DEFAULT_SWEEP_SECS: u64 = 60 * 60;
pub const DEFAULT_WIKI_LINT_SECS: u64 = 7 * 24 * 60 * 60;
pub const DEFAULT_TICK_SLEEP_SECS: u64 = 60;

/// Explicit file paths shared by every agent. Production fills these from
/// the standard locations; tests point them at temp dirs (no env reads).
#[derive(Debug, Clone)]
pub struct AgentCtx {
    pub log_path: PathBuf,
    pub cards_path: PathBuf,
    pub deeper_dir: PathBuf,
    pub live_store: PathBuf,
    pub fixture: Option<PathBuf>,
    /// Fixed wall clock (unix secs) for deterministic runs; `None` reads
    /// the system clock.
    pub now_override: Option<u64>,
}

impl AgentCtx {
    /// Current unix seconds: the fixed test clock when set, else the system clock.
    pub fn now_secs(&self) -> u64 {
        self.now_override.unwrap_or_else(crate::idea_card::now_secs)
    }
}

/// Full daemon configuration: paths plus schedule. Production builds this
/// with [`DaemonConfig::from_paths`] (intervals from env); tests build it
/// literally with `..Default::default()`.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub ctx: AgentCtx,
    pub brief_secs: u64,
    pub digest_secs: u64,
    pub try_followup_secs: u64,
    pub deeper_queue_secs: u64,
    pub sweep_secs: u64,
    pub wiki_lint_secs: u64,
    pub tick_sleep_secs: u64,
    /// Loop bound for tests (`None` loops until stopped).
    pub max_ticks: Option<u64>,
}

impl Default for DaemonConfig {
    /// Default daemon intervals and settings.
    fn default() -> Self {
        Self {
            ctx: AgentCtx {
                log_path: PathBuf::new(),
                cards_path: PathBuf::new(),
                deeper_dir: PathBuf::new(),
                live_store: PathBuf::new(),
                fixture: None,
                now_override: None,
            },
            brief_secs: DEFAULT_BRIEF_SECS,
            digest_secs: DEFAULT_DIGEST_SECS,
            try_followup_secs: DEFAULT_TRY_FOLLOWUP_SECS,
            deeper_queue_secs: DEFAULT_DEEPER_QUEUE_SECS,
            sweep_secs: DEFAULT_SWEEP_SECS,
            wiki_lint_secs: DEFAULT_WIKI_LINT_SECS,
            tick_sleep_secs: DEFAULT_TICK_SLEEP_SECS,
            max_ticks: None,
        }
    }
}

/// Read a seconds value from env var `var`, falling back to `default`; errors on junk.
fn secs_from_env(var: &str, default: u64) -> Result<u64, String> {
    match std::env::var(var).ok().filter(|v| !v.is_empty()) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|_| format!("invalid {var} value (want seconds): {raw}")),
    }
}

impl DaemonConfig {
    /// Attach the schedule from env (`DAEMON_*_SECS`, `DAEMON_MAX_TICKS`)
    /// to explicit paths. Unset or empty values take the defaults; anything
    /// unparseable is an error (fail closed at startup).
    pub fn from_paths(ctx: AgentCtx) -> Result<Self, String> {
        Ok(Self {
            ctx,
            brief_secs: secs_from_env(BRIEF_SECS_ENV, DEFAULT_BRIEF_SECS)?,
            digest_secs: secs_from_env(DIGEST_SECS_ENV, DEFAULT_DIGEST_SECS)?,
            try_followup_secs: secs_from_env(TRY_FOLLOWUP_SECS_ENV, DEFAULT_TRY_FOLLOWUP_SECS)?,
            deeper_queue_secs: secs_from_env(DEEPER_QUEUE_SECS_ENV, DEFAULT_DEEPER_QUEUE_SECS)?,
            sweep_secs: secs_from_env(SWEEP_SECS_ENV, DEFAULT_SWEEP_SECS)?,
            wiki_lint_secs: secs_from_env(WIKI_LINT_SECS_ENV, DEFAULT_WIKI_LINT_SECS)?,
            tick_sleep_secs: secs_from_env(TICK_SLEEP_SECS_ENV, DEFAULT_TICK_SLEEP_SECS)?,
            max_ticks: match std::env::var(MAX_TICKS_ENV).ok().filter(|v| !v.is_empty()) {
                None => None,
                Some(raw) => Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid {MAX_TICKS_ENV} value: {raw}"))?,
                ),
            },
        })
    }

    /// Current unix seconds: the fixed test clock when set, else the system clock.
    pub fn now_secs(&self) -> u64 {
        self.ctx.now_secs()
    }

    /// Build the scheduler with the six daemon agents. Order is fixed
    /// (brief, digest, try_followup, deeper_queue, approvals_sweep,
    /// wiki_lint) so `--once` logs read deterministically.
    pub fn build_scheduler(&self) -> Scheduler {
        let mut s = Scheduler::new();
        s.add(Box::new(BriefAgent::new(
            self.ctx.clone(),
            Duration::from_secs(self.brief_secs),
        )));
        s.add(Box::new(DigestAgent::new(
            self.ctx.clone(),
            Duration::from_secs(self.digest_secs),
        )));
        s.add(Box::new(TryFollowupAgent::new(
            self.ctx.clone(),
            Duration::from_secs(self.try_followup_secs),
        )));
        s.add(Box::new(DeeperQueueAgent::new(
            self.ctx.clone(),
            Duration::from_secs(self.deeper_queue_secs),
        )));
        s.add(Box::new(ApprovalsSweepAgent::new(
            self.ctx.clone(),
            Duration::from_secs(self.sweep_secs),
        )));
        s.add(Box::new(WikiLintAgent::new(
            self.ctx.clone(),
            Duration::from_secs(self.wiki_lint_secs),
        )));
        s
    }
}

/// True when paid calls are enabled (`ANCIENTOS_ALLOW_PAID=1`).
pub fn allow_paid() -> bool {
    std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Exact payload proposed (and later claim-matched) for a paid brief.
pub fn brief_paid_payload(date: &str, candidate_count: usize) -> serde_json::Value {
    serde_json::json!({"date": date, "candidate_count": candidate_count})
}

/// Exact payload proposed (and later claim-matched) for a paid walkthrough.
pub fn deeper_paid_payload(post_id: &str, at: u64) -> serde_json::Value {
    serde_json::json!({"post_id": post_id, "at": at})
}

/// Outcome of one brief tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefTick {
    pub picks: usize,
    pub candidates: usize,
    /// Approval id proposed when paid is on (dedupe may return an existing
    /// pending request for the same payload).
    pub proposed: Option<String>,
}

/// Daily brief, local path only: runs [`crate::brief::run_local_full`]
/// offline with no hydrator/expander (no network is possible) and, when
/// paid is on, proposes a `brief.paid` approval instead of calling Muse.
/// Metadata only in the log; never post text, never secrets.
pub fn brief_tick(ctx: &AgentCtx, now: u64, paid: bool) -> Result<BriefTick, String> {
    let live = crate::ingest::Store::load_from(&ctx.live_store)?;
    let live_rows: Vec<crate::fixture::Bookmark> = live.by_id.into_values().collect();
    let fixture_rows = match &ctx.fixture {
        None => Vec::new(),
        Some(path) => crate::fixture::load(path).map_err(|e| e.to_string())?,
    };
    let merged = crate::merge::merge(&live_rows, &fixture_rows);
    let date = crate::brief::date_from_days((now / 86400) as i64);
    let result =
        crate::brief::run_local_full(&merged, &ctx.log_path, &date, None, None, true, None, now)?;
    let proposed = if paid {
        let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
            .map_err(|e| e.to_string())?;
        let req = approvals::request(
            &mut log,
            &crate::app_gate(),
            DAEMON_AGENT_NAME,
            BRIEF_PAID_ACTION,
            &format!(
                "Paid brief ranking for {date} ({} candidates)",
                result.candidates.len()
            ),
            brief_paid_payload(&date, result.candidates.len()),
            now,
            None,
        )
        .map_err(|e| e.to_string())?;
        Some(req.id)
    } else {
        None
    };
    Ok(BriefTick {
        picks: result.picks.len(),
        candidates: result.candidates.len(),
        proposed,
    })
}

/// Outcome of one digest tick. The digest is computed but never recorded
/// (dry-run-safe); the weekly-due rule is [`crate::digest::digest_due`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestTick {
    pub due: bool,
    pub cards: usize,
    pub nudges: usize,
    pub rendered: String,
}

/// Weekly digest in dry-run-safe form: same inputs and weekly-due rule as
/// the `digest` command, but [`crate::digest::record_shown`] is never
/// called, so a daemon tick never resets the due check.
pub fn digest_tick(ctx: &AgentCtx, now: u64) -> Result<DigestTick, String> {
    let cards = crate::idea_card::load(&ctx.cards_path)?;
    let log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    let pending = crate::try_it::pending_followups(&log);
    // Unreadable store means no filtering (read-only listing never fails).
    let bookmarks: Vec<crate::fixture::Bookmark> = crate::ingest::Store::load_from(&ctx.live_store)
        .map(|s| s.by_id.into_values().collect())
        .unwrap_or_default();
    let d = crate::digest::weekly_digest_for_bookmarks(&cards, &pending, &entries, now, &bookmarks);
    Ok(DigestTick {
        due: d.due,
        cards: d.cards.len(),
        nudges: d.nudges.len(),
        rendered: crate::digest::render(&d),
    })
}

/// Daily next-day check-in: the pending try-it queue
/// ([`crate::try_it::pending_followups`]) plus a [`TRY_FOLLOWUP_KIND`]
/// entry carrying the count. Returns the due post ids.
pub fn try_followup_tick(ctx: &AgentCtx, now: u64) -> Result<Vec<String>, String> {
    let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    let pending = crate::try_it::pending_followups(&log);
    log.append(
        TRY_FOLLOWUP_KIND,
        serde_json::json!({"count": pending.len(), "post_ids": pending, "at": now}),
    )
    .map_err(|e| e.to_string())?;
    Ok(pending)
}

/// Expiry sweep: [`approvals::sweep_expired`]. Returns entries appended.
pub fn approvals_sweep_tick(ctx: &AgentCtx, now: u64) -> Result<usize, String> {
    let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    approvals::sweep_expired(&mut log, now).map_err(|e| e.to_string())
}

/// Weekly wiki lint: [`crate::wiki::lint_and_propose`] against the wiki dir
/// alongside the decision log. Findings become `wiki.edit` approvals; pages
/// are never modified.
pub fn wiki_lint_tick(ctx: &AgentCtx, now: u64) -> Result<crate::wiki::LintReport, String> {
    let wiki = crate::wiki::wiki_dir_for_log(&ctx.log_path);
    crate::wiki::lint_and_propose(&wiki, &ctx.log_path, now)
}

/// A paid job whose approval was released but whose completion was never
/// recorded (`job.started` without `job.done` for the approval id). The side
/// effect may or may not have run, so the daemon never retries it silently;
/// a human must resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedNotDone {
    /// Queued job the release belonged to.
    pub post_id: String,
    /// Approval id that was released but never completed.
    pub approval_id: String,
}

/// Outcome of one deeper-queue tick: post ids per path taken.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeeperQueueTick {
    pub ran_local: Vec<String>,
    pub proposed: Vec<String>,
    pub executed_paid: Vec<String>,
    pub waiting: Vec<String>,
    pub skipped_denied: Vec<String>,
    /// Jobs released but never recorded done (see [`ReleasedNotDone`]).
    pub released_not_done: Vec<ReleasedNotDone>,
    /// Resume approval proposed when this tick's loop guard tripped; the
    /// remaining paid jobs wait for the next tick (fail closed).
    pub guard_resume: Option<String>,
}

/// Loop-guard name for the paid deeper-queue loop.
pub const DEEPER_LOOP_NAME: &str = "deeper_queue";

/// Payload for a [`JOB_STARTED_KIND`] entry: the approval id and payload
/// hash key the event; post id, queue time and start time are metadata.
pub fn job_started_payload(
    approval_id: &str,
    payload_hash: &str,
    post_id: &str,
    at: u64,
    now: u64,
) -> serde_json::Value {
    serde_json::json!({
        "approval_id": approval_id, "payload_hash": payload_hash,
        "post_id": post_id, "at": at, "started_at": now,
    })
}

/// Payload for a [`JOB_DONE_KIND`] entry: same keys as
/// [`job_started_payload`], with the completion time instead of the start.
pub fn job_done_payload(
    approval_id: &str,
    payload_hash: &str,
    post_id: &str,
    at: u64,
    now: u64,
) -> serde_json::Value {
    serde_json::json!({
        "approval_id": approval_id, "payload_hash": payload_hash,
        "post_id": post_id, "at": at, "done_at": now,
    })
}

/// Lifecycle of one released paid job, folded from [`JOB_STARTED_KIND`] /
/// [`JOB_DONE_KIND`] entries keyed by approval id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobRunState {
    /// No `job.started` for this approval id: the side effect never ran.
    NotStarted,
    /// `job.started` without a matching `job.done`: the side effect may or
    /// may not have run (crash or failure mid-execution) — never retry it
    /// silently.
    StartedNotDone,
    /// `job.done` for this approval id: the side effect already ran.
    Done,
}

/// Fold [`JOB_STARTED_KIND`] / [`JOB_DONE_KIND`] entries into the run state
/// for one approval id. Only entries whose `approval_id` AND `payload_hash`
/// both match count: anything else proves nothing about this job (fail
/// closed — a mismatched hash never marks the job done or started).
pub fn job_run_state(entries: &[Entry], approval_id: &str, payload_hash: &str) -> JobRunState {
    let mut started = false;
    for e in entries {
        let keyed = e.payload.get("approval_id").and_then(|v| v.as_str()) == Some(approval_id)
            && e.payload.get("payload_hash").and_then(|v| v.as_str()) == Some(payload_hash);
        if !keyed {
            continue;
        }
        match e.kind.as_str() {
            JOB_STARTED_KIND => started = true,
            JOB_DONE_KIND => return JobRunState::Done,
            _ => {}
        }
    }
    if started {
        JobRunState::StartedNotDone
    } else {
        JobRunState::NotStarted
    }
}

/// True when a daemon approval matches the job's exact payload.
fn is_job_approval(a: &approvals::Approval, action: &str, payload_hash: &str) -> bool {
    a.request.agent == DAEMON_AGENT_NAME
        && a.request.action == action
        && a.request.payload_hash == payload_hash
}

/// Run queued go-deeper jobs whose time has come, with injected fetchers
/// and Muse caller (unit tests pass mocks; production passes the real
/// ones via [`deeper_queue_tick`]).
///
/// - Paid off: the free local fallback runs directly
///   ([`crate::deeper::run_with`] with `allow_paid=false`, offline when
///   the ctx fixture is set) and the job is marked
///   [`DEEPER_QUEUE_DONE_KIND`](crate::deeper::DEEPER_QUEUE_DONE_KIND).
/// - Paid on: a paid walkthrough is proposed as a `deeper.paid` approval
///   for the job's exact payload (no fetch, no model call) and marked
///   [`DEEPER_PROPOSED_KIND`](crate::deeper::DEEPER_PROPOSED_KIND). It
///   executes only later, when an approved request for that exact payload
///   exists: the daemon claims the [`ReleaseToken`](approvals::ReleaseToken)
///   from the human decision and moves it into the paid executor, which
///   calls [`release`](approvals::release) (exactly once per approval),
///   logs `job.started` keyed by approval id, runs the existing deeper paid
///   path (which still requires `ANCIENTOS_ALLOW_PAID=1`) and logs
///   `job.done`. A request that is already released never executes again:
///   a release with `job.started` but no `job.done` is surfaced as
///   released-not-done (never re-executed silently); a release with
///   `job.done` but no queue marker only gets the marker; a release that
///   never started needs a fresh approval (and a fresh release token), like
///   an expired one. Denied proposals are skipped forever.
///
/// Every due job is attempted; failures are collected and returned as one
/// `Err` listing the failed post ids (other jobs and agents still run).
#[allow(clippy::too_many_arguments)]
pub fn deeper_queue_tick_with(
    ctx: &AgentCtx,
    now: u64,
    paid: bool,
    fetch_thread: &crate::delegate::ThreadFetcher<'_>,
    fetch_post: &crate::deeper::PostFetcher<'_>,
    call: &crate::deeper::DeeperMuseCaller<'_>,
) -> Result<DeeperQueueTick, String> {
    let log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    let jobs = crate::deeper::due_jobs(&entries, now);
    let mut out = DeeperQueueTick::default();
    let mut errors: Vec<String> = Vec::new();
    let mut guard = LoopGuard::new(DEEPER_LOOP_NAME, Budgets::default(), now);
    for job in jobs {
        let payload = deeper_paid_payload(&job.post_id, job.at);
        if !paid {
            match crate::deeper::run_with(
                &job.post_id,
                false,
                ctx.fixture.as_deref(),
                fetch_thread,
                fetch_post,
                call,
                &ctx.log_path,
                &ctx.deeper_dir,
                None,
            ) {
                Ok(_) => {
                    // run_with appended through its own handle; reopen before
                    // marking done so the chain stays contiguous.
                    let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| {
                        std::env::var(key).ok()
                    })
                    .map_err(|e| e.to_string())?;
                    log.append(
                        crate::deeper::DEEPER_QUEUE_DONE_KIND,
                        serde_json::json!({
                            "post_id": job.post_id, "at": job.at,
                            "mode": "local", "done_at": now,
                        }),
                    )
                    .map_err(|e| e.to_string())?;
                    out.ran_local.push(job.post_id.clone());
                }
                Err(e) => errors.push(format!("{}: {e}", job.post_id)),
            }
            continue;
        }
        if guard.tripped().is_some() || guard.step(&job.post_id, true, now).is_err() {
            if out.guard_resume.is_none() {
                let mut log =
                    DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
                        .map_err(|e| e.to_string())?;
                let req = loop_guard::propose_resume(
                    &mut log,
                    &crate::app_gate(),
                    DAEMON_AGENT_NAME,
                    &guard,
                    now,
                )
                .map_err(|e| e.to_string())?;
                out.guard_resume = req.map(|r| r.id);
            }
            out.waiting.push(job.post_id.clone());
            continue;
        }
        match paid_job_step(ctx, &job, &payload, now, fetch_thread, fetch_post, call)? {
            PaidStep::Proposed => out.proposed.push(job.post_id.clone()),
            PaidStep::Executed => out.executed_paid.push(job.post_id.clone()),
            PaidStep::Waiting => out.waiting.push(job.post_id.clone()),
            PaidStep::SkippedDenied => out.skipped_denied.push(job.post_id.clone()),
            PaidStep::ReleasedNotDone(approval_id) => out.released_not_done.push(ReleasedNotDone {
                post_id: job.post_id.clone(),
                approval_id,
            }),
            PaidStep::Failed(e) => errors.push(format!("{}: {e}", job.post_id)),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(format!("deeper jobs failed: {}", errors.join("; ")))
    }
}

/// One paid job's path through a tick: propose, wait, claim+release+execute,
/// skip, surface released-not-done, or fail.
#[derive(Debug, PartialEq, Eq)]
enum PaidStep {
    /// A fresh approval was proposed for the job.
    Proposed,
    /// The paid walkthrough completed (or was already done and only the
    /// queue marker was still missing); the side effect ran at most once.
    Executed,
    /// An approval is still pending a human decision.
    Waiting,
    /// A human denied the job: never execute, never re-propose.
    SkippedDenied,
    /// The approval was released but no `job.done` exists for its id
    /// (carries the approval id); surfaced for a human, never retried
    /// silently.
    ReleasedNotDone(String),
    /// The step failed with the given reason; other jobs still run.
    Failed(String),
}

/// One paid job: propose, wait, claim+release+execute, skip, or surface
/// released-not-done. `Ok` always (failures come back as
/// [`PaidStep::Failed`]); `Err` only when the log itself cannot be read.
#[allow(clippy::too_many_arguments)]
fn paid_job_step(
    ctx: &AgentCtx,
    job: &crate::deeper::QueuedJob,
    payload: &serde_json::Value,
    now: u64,
    fetch_thread: &crate::delegate::ThreadFetcher<'_>,
    fetch_post: &crate::deeper::PostFetcher<'_>,
    call: &crate::deeper::DeeperMuseCaller<'_>,
) -> Result<PaidStep, String> {
    let log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    let payload_hash = approvals::canonical_hash(&approvals::redact(payload));
    let pending: Vec<approvals::Approval> = approvals::pending(&log, now)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|a| is_job_approval(a, DEEPER_PAID_ACTION, &payload_hash))
        .collect();
    let decided: Vec<approvals::Approval> = approvals::decided(&log, now)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|a| is_job_approval(a, DEEPER_PAID_ACTION, &payload_hash))
        .collect();
    let approved = decided
        .iter()
        .find(|a| a.status == approvals::Status::Approved);
    let released = decided
        .iter()
        .find(|a| a.status == approvals::Status::Released);
    let denied = decided
        .iter()
        .any(|a| a.status == approvals::Status::Denied);
    if let Some(a) = released {
        // Already released: a previous tick released then crashed before
        // executing, or executed then crashed before marking done. The
        // job.started/job.done events keyed by approval id tell which: a
        // job started but not done is surfaced for a human (never silently
        // re-executed); a job done but missing its queue marker gets the
        // marker without re-running the side effect; a job never
        // started needs a fresh approval.
        let id = a.request.id.clone();
        let hash = a.request.payload_hash.clone();
        let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
        match job_run_state(&entries, &id, &hash) {
            JobRunState::Done => return mark_paid_done(ctx, job, now),
            JobRunState::StartedNotDone => return Ok(PaidStep::ReleasedNotDone(id)),
            // Released but never started: the release token is spent and a
            // side effect needs a token by value, so fall through and
            // propose a fresh approval instead of executing.
            JobRunState::NotStarted => {}
        }
    }
    if let Some(a) = approved {
        let id = a.request.id.clone();
        let hash = a.request.payload_hash.clone();
        let log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
            .map_err(|e| e.to_string())?;
        let token = match approvals::claim_release_token(&log, &id, &hash) {
            Ok(t) => t,
            Err(e) => return Ok(PaidStep::Failed(format!("claim failed: {e}"))),
        };
        return execute_paid(
            token,
            ctx,
            job,
            payload,
            &id,
            &hash,
            now,
            fetch_thread,
            fetch_post,
            call,
        );
    }
    if !pending.is_empty() {
        ensure_proposed(ctx, job, pending[0].request.id.clone(), now)?;
        return Ok(PaidStep::Waiting);
    }
    if denied {
        // A human said no: never execute, never re-propose.
        return Ok(PaidStep::SkippedDenied);
    }
    // Nothing usable (a new job, an expired request, or a released request
    // whose job never finished): propose a fresh approval. The old request
    // itself is never executed: every execution consumes a fresh token in
    // `execute_paid`, so a retry after a crash or a failed paid run needs a
    // fresh human approval.
    let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    let req = match approvals::request(
        &mut log,
        &crate::app_gate(),
        DAEMON_AGENT_NAME,
        DEEPER_PAID_ACTION,
        &format!("Paid go-deeper walkthrough for {}", job.post_id),
        payload.clone(),
        now,
        None,
    ) {
        Ok(r) => r,
        Err(e) => return Ok(PaidStep::Failed(format!("proposal failed: {e}"))),
    };
    ensure_proposed(ctx, job, req.id, now)?;
    Ok(PaidStep::Proposed)
}

/// Append a `deeper.proposed` marker unless the job already shows one.
fn ensure_proposed(
    ctx: &AgentCtx,
    job: &crate::deeper::QueuedJob,
    approval_id: String,
    now: u64,
) -> Result<(), String> {
    if job.status == crate::deeper::QueueStatus::Proposed {
        return Ok(());
    }
    let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    log.append(
        crate::deeper::DEEPER_PROPOSED_KIND,
        serde_json::json!({
            "post_id": job.post_id, "at": job.at,
            "approval_id": approval_id, "proposed_at": now,
        }),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Record a paid job as done without running the side effect: the log
/// already shows `job.done` for the approval id, so a previous tick ran the
/// walkthrough but crashed before marking the queue done. Appends only the
/// `deeper.done` queue marker; never touches fetchers or the model.
/// Returns [`PaidStep::Executed`] or [`PaidStep::Failed`].
fn mark_paid_done(
    ctx: &AgentCtx,
    job: &crate::deeper::QueuedJob,
    now: u64,
) -> Result<PaidStep, String> {
    let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    if let Err(e) = log.append(
        crate::deeper::DEEPER_QUEUE_DONE_KIND,
        serde_json::json!({
            "post_id": job.post_id, "at": job.at,
            "mode": "paid", "done_at": now,
        }),
    ) {
        return Ok(PaidStep::Failed(format!("done marker failed: {e}")));
    }
    Ok(PaidStep::Executed)
}

/// Run the paid walkthrough for one approved job and mark the job done.
/// Consumes the job approval's [`ReleaseToken`](approvals::ReleaseToken) by
/// value together with the exact approved `payload`: the token is released
/// first (exactly once per approval, so a second execution with another
/// token fails here), and only then does the paid path run. Without a token
/// this function cannot be called. Logs [`JOB_STARTED_KIND`] before the
/// side effect and [`JOB_DONE_KIND`] after it (both keyed by approval id and
/// payload hash), so a crash in between surfaces as released-not-done
/// instead of silently re-executing. Fails closed: when the release or the
/// started event cannot be logged the side effect never runs. Returns
/// [`PaidStep::Executed`] or
/// [`PaidStep::Failed`].
#[allow(clippy::too_many_arguments)]
fn execute_paid(
    token: approvals::ReleaseToken,
    ctx: &AgentCtx,
    job: &crate::deeper::QueuedJob,
    payload: &serde_json::Value,
    approval_id: &str,
    payload_hash: &str,
    now: u64,
    fetch_thread: &crate::delegate::ThreadFetcher<'_>,
    fetch_post: &crate::deeper::PostFetcher<'_>,
    call: &crate::deeper::DeeperMuseCaller<'_>,
) -> Result<PaidStep, String> {
    let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    if let Err(e) = approvals::release(&mut log, token, payload) {
        return Ok(PaidStep::Failed(format!("release failed: {e}")));
    }
    if let Err(e) = log.append(
        JOB_STARTED_KIND,
        job_started_payload(approval_id, payload_hash, &job.post_id, job.at, now),
    ) {
        return Ok(PaidStep::Failed(format!("job.started failed: {e}")));
    }
    if let Err(e) = crate::deeper::run_with(
        &job.post_id,
        true,
        None,
        fetch_thread,
        fetch_post,
        call,
        &ctx.log_path,
        &ctx.deeper_dir,
        None,
    ) {
        return Ok(PaidStep::Failed(format!("paid run failed: {e}")));
    }
    // run_with appended through its own handle; reopen before marking done
    // so the chain stays contiguous. job.done first: a crash before the
    // queue marker still heals without re-running the side effect.
    let mut log = DecisionLog::open_with_key_env(&ctx.log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    if let Err(e) = log.append(
        JOB_DONE_KIND,
        job_done_payload(approval_id, payload_hash, &job.post_id, job.at, now),
    ) {
        return Ok(PaidStep::Failed(format!("job.done failed: {e}")));
    }
    mark_paid_done(ctx, job, now)
}

/// Production deeper-queue tick: real thread/post fetchers and a real Muse
/// caller. Paid execution still requires `ANCIENTOS_ALLOW_PAID=1` (checked
/// again inside the deeper paid path).
pub fn deeper_queue_tick(ctx: &AgentCtx, now: u64, paid: bool) -> Result<DeeperQueueTick, String> {
    let client = ancientos_router::muse::MuseClient::from_env()
        .with_max_completion_tokens(crate::deeper::DEEPER_MAX_COMPLETION_TOKENS)
        .with_reasoning_effort(crate::deeper::DEEPER_REASONING_EFFORT);
    let call = crate::deeper::production_muse_caller(&client);
    deeper_queue_tick_with(
        ctx,
        now,
        paid,
        &crate::x_api::fetch_thread,
        &crate::x_api::fetch_post,
        &call,
    )
}

/// Daily brief agent: local path only (see [`brief_tick`]).
pub struct BriefAgent {
    ctx: AgentCtx,
    interval: Duration,
}

impl BriefAgent {
    /// Create the wiki lint agent with its tick interval.
    pub fn new(ctx: AgentCtx, interval: Duration) -> Self {
        Self { ctx, interval }
    }
}

impl Agent for BriefAgent {
    /// Scheduler name of this agent.
    fn name(&self) -> &str {
        "brief"
    }

    /// How often the scheduler runs this agent.
    fn interval(&self) -> Duration {
        self.interval
    }

    /// Run one tick: lint the wiki and propose fixes through approvals.
    fn tick(&mut self) -> Result<(), String> {
        let now = self.ctx.now_secs();
        let summary = brief_tick(&self.ctx, now, allow_paid())?;
        match summary.proposed {
            Some(id) => println!(
                "brief: {} picks ({} candidates), proposed paid brief {id}",
                summary.picks, summary.candidates
            ),
            None => println!(
                "brief: {} picks ({} candidates, local)",
                summary.picks, summary.candidates
            ),
        }
        Ok(())
    }
}

/// Weekly digest agent: dry-run-safe (see [`digest_tick`]).
pub struct DigestAgent {
    ctx: AgentCtx,
    interval: Duration,
}

impl DigestAgent {
    /// Create the wiki lint agent with its tick interval.
    pub fn new(ctx: AgentCtx, interval: Duration) -> Self {
        Self { ctx, interval }
    }
}

impl Agent for DigestAgent {
    /// Scheduler name of this agent.
    fn name(&self) -> &str {
        "digest"
    }

    /// How often the scheduler runs this agent.
    fn interval(&self) -> Duration {
        self.interval
    }

    /// Run one tick: lint the wiki and propose fixes through approvals.
    fn tick(&mut self) -> Result<(), String> {
        let now = self.ctx.now_secs();
        let summary = digest_tick(&self.ctx, now)?;
        if summary.due {
            print!("{}", summary.rendered);
        } else {
            println!("digest: not due");
        }
        Ok(())
    }
}

/// Daily try-it check-in agent (see [`try_followup_tick`]).
pub struct TryFollowupAgent {
    ctx: AgentCtx,
    interval: Duration,
}

impl TryFollowupAgent {
    /// Create the wiki lint agent with its tick interval.
    pub fn new(ctx: AgentCtx, interval: Duration) -> Self {
        Self { ctx, interval }
    }
}

impl Agent for TryFollowupAgent {
    /// Scheduler name of this agent.
    fn name(&self) -> &str {
        "try_followup"
    }

    /// How often the scheduler runs this agent.
    fn interval(&self) -> Duration {
        self.interval
    }

    /// Run one tick: lint the wiki and propose fixes through approvals.
    fn tick(&mut self) -> Result<(), String> {
        let now = self.ctx.now_secs();
        let pending = try_followup_tick(&self.ctx, now)?;
        println!("try_followup: {} pending", pending.len());
        for id in &pending {
            println!("  {id}");
        }
        Ok(())
    }
}

/// Queued go-deeper agent (see [`deeper_queue_tick`]).
pub struct DeeperQueueAgent {
    ctx: AgentCtx,
    interval: Duration,
}

impl DeeperQueueAgent {
    /// Create the wiki lint agent with its tick interval.
    pub fn new(ctx: AgentCtx, interval: Duration) -> Self {
        Self { ctx, interval }
    }
}

impl Agent for DeeperQueueAgent {
    /// Scheduler name of this agent.
    fn name(&self) -> &str {
        "deeper_queue"
    }

    /// How often the scheduler runs this agent.
    fn interval(&self) -> Duration {
        self.interval
    }

    /// Run one tick: lint the wiki and propose fixes through approvals.
    fn tick(&mut self) -> Result<(), String> {
        let now = self.ctx.now_secs();
        let summary = deeper_queue_tick(&self.ctx, now, allow_paid())?;
        println!(
            "deeper_queue: local={} proposed={} paid={} waiting={} denied={} released_not_done={}",
            summary.ran_local.len(),
            summary.proposed.len(),
            summary.executed_paid.len(),
            summary.waiting.len(),
            summary.skipped_denied.len(),
            summary.released_not_done.len()
        );
        Ok(())
    }
}

/// Approval expiry sweep agent (see [`approvals_sweep_tick`]).
pub struct ApprovalsSweepAgent {
    ctx: AgentCtx,
    interval: Duration,
}

impl ApprovalsSweepAgent {
    /// Create the wiki lint agent with its tick interval.
    pub fn new(ctx: AgentCtx, interval: Duration) -> Self {
        Self { ctx, interval }
    }
}

impl Agent for ApprovalsSweepAgent {
    /// Scheduler name of this agent.
    fn name(&self) -> &str {
        "approvals_sweep"
    }

    /// How often the scheduler runs this agent.
    fn interval(&self) -> Duration {
        self.interval
    }

    /// Run one tick: lint the wiki and propose fixes through approvals.
    fn tick(&mut self) -> Result<(), String> {
        let now = self.ctx.now_secs();
        let swept = approvals_sweep_tick(&self.ctx, now)?;
        println!("approvals_sweep: swept {swept}");
        Ok(())
    }
}

/// Weekly wiki-lint agent (see [`wiki_lint_tick`]).
pub struct WikiLintAgent {
    ctx: AgentCtx,
    interval: Duration,
}

impl WikiLintAgent {
    /// Create the wiki lint agent with its tick interval.
    pub fn new(ctx: AgentCtx, interval: Duration) -> Self {
        Self { ctx, interval }
    }
}

impl Agent for WikiLintAgent {
    /// Scheduler name of this agent.
    fn name(&self) -> &str {
        "wiki_lint"
    }

    /// How often the scheduler runs this agent.
    fn interval(&self) -> Duration {
        self.interval
    }

    /// Run one tick: lint the wiki and propose fixes through approvals.
    fn tick(&mut self) -> Result<(), String> {
        let now = self.ctx.now_secs();
        let report = wiki_lint_tick(&self.ctx, now)?;
        println!(
            "wiki_lint: {} findings, {} approvals",
            report.findings.len(),
            report.approvals.len()
        );
        Ok(())
    }
}

/// Agent names in scheduler order.
pub const AGENT_NAMES: &[&str] = &[
    "brief",
    "digest",
    "try_followup",
    "deeper_queue",
    "approvals_sweep",
    "wiki_lint",
];

/// Append one [`DAEMON_TICK_KIND`] entry: agent name, ok/error and the wall
/// clock — never payloads, post text or secrets.
pub fn log_tick(
    log_path: &Path,
    agent: &str,
    result: &Result<(), String>,
    now: u64,
) -> Result<(), String> {
    let mut log = DecisionLog::open_with_key_env(log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    let mut payload = serde_json::json!({"agent": agent, "ok": result.is_ok(), "at": now});
    if let Err(e) = result {
        payload["error"] = serde_json::json!(e);
    }
    log.append(DAEMON_TICK_KIND, payload)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// One agent's outcome in a scheduler pass: its tick result plus whether
/// its [`DAEMON_TICK_KIND`] entry was recorded.
#[derive(Debug)]
pub struct AgentOutcome {
    pub agent: String,
    pub result: Result<(), String>,
    pub tick_logged: Result<(), String>,
}

/// Every due agent's outcome in one scheduler pass.
#[derive(Debug, Default)]
pub struct RunReport {
    pub outcomes: Vec<AgentOutcome>,
}

impl RunReport {
    /// True when every due agent succeeded and every tick entry logged.
    pub fn ok(&self) -> bool {
        self.outcomes
            .iter()
            .all(|o| o.result.is_ok() && o.tick_logged.is_ok())
    }
}

/// Run every due agent once against `sched`, logging one
/// [`DAEMON_TICK_KIND`] entry per run. Agent errors are collected (never
/// stop the other agents); the report tells which failed.
pub fn run_due_once(sched: &mut Scheduler, config: &DaemonConfig) -> RunReport {
    let now = config.now_secs();
    let mut report = RunReport::default();
    for (name, result) in sched.run_due(Instant::now()) {
        let tick_logged = log_tick(&config.ctx.log_path, &name, &result, now);
        report.outcomes.push(AgentOutcome {
            agent: name,
            result,
            tick_logged,
        });
    }
    report
}

/// Build a fresh scheduler and run every due agent once (on a fresh
/// scheduler every agent is due). Used by `run --once`.
pub fn run_once(config: &DaemonConfig) -> RunReport {
    let mut sched = config.build_scheduler();
    run_due_once(&mut sched, config)
}

/// Tick in a loop until `stop` is set or `max_ticks` is reached, sleeping
/// `tick_sleep_secs` between ticks. Ctrl-C stops the process via the
/// default disposition (no signal handler without new crates); tests bound
/// the loop with `DAEMON_MAX_TICKS`. Returns one report per tick.
pub fn run_loop(config: &DaemonConfig, stop: &AtomicBool) -> Vec<RunReport> {
    let mut sched = config.build_scheduler();
    let mut reports = Vec::new();
    let mut tick_no: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        reports.push(run_due_once(&mut sched, config));
        tick_no += 1;
        if config.max_ticks.is_some_and(|m| tick_no >= m) {
            break;
        }
        std::thread::sleep(Duration::from_secs(config.tick_sleep_secs));
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancientos_core::log::Entry;
    use std::cell::Cell;

    const NOW: u64 = 1_791_000_000;

    /// Test helper: synthetic bookmark with standard label and metadata.
    fn bookmark(id: &str, text: &str) -> crate::fixture::Bookmark {
        crate::fixture::Bookmark {
            quoted_text: None,
            x_bookmark_present: None,
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: text.into(),
            author_username: Some(format!("syn_{id}")),
            created_at: Some("2026-10-20T00:00:00+00:00".into()),
            folders: None,
            label: Some(crate::fixture::Label {
                parent_label: Some("Local tooling".into()),
                confidence: Some(0.5),
                method: None,
                is_foldered: None,
            }),
            had_note_tweet: false,
            media: Vec::new(),
            lang: Some("en".into()),
            urls: Vec::new(),
        }
    }

    /// Test helper: seed the path with three synthetic bookmark rows.
    fn write_brief_fixture(path: &Path) {
        let rows = [
            bookmark("b1", "synthetic note about local file tooling"),
            bookmark("b2", "synthetic note about neighborhood libraries"),
            bookmark("b3", "synthetic note about paper maps"),
        ];
        let body: Vec<String> = rows
            .iter()
            .map(|b| serde_json::to_string(b).unwrap())
            .collect();
        std::fs::write(path, body.join("\n") + "\n").unwrap();
    }

    /// Test helper: agent context rooted at the temp dir, optionally seeded.
    fn test_ctx(dir: &tempfile::TempDir, fixture: bool) -> AgentCtx {
        let fixture_path = dir.path().join("brief.jsonl");
        if fixture {
            write_brief_fixture(&fixture_path);
        }
        AgentCtx {
            log_path: dir.path().join("decision_log.jsonl"),
            cards_path: dir.path().join("idea_cards.jsonl"),
            deeper_dir: dir.path().join("deeper"),
            live_store: dir.path().join("bookmarks.jsonl"),
            fixture: fixture.then_some(fixture_path),
            now_override: Some(NOW),
        }
    }

    /// Test helper: default daemon settings over a seeded temp context.
    fn test_config(dir: &tempfile::TempDir) -> DaemonConfig {
        DaemonConfig {
            ctx: test_ctx(dir, true),
            ..DaemonConfig::default()
        }
    }

    /// Test helper: verified decision log records stored at the path.
    fn entries_of(path: &Path) -> Vec<Entry> {
        DecisionLog::open(path).unwrap().verify_or_empty().unwrap()
    }

    /// Test helper: entry kinds from the decision log at the path.
    fn kinds_of(path: &Path) -> Vec<String> {
        entries_of(path).iter().map(|e| e.kind.clone()).collect()
    }

    #[test]
    fn config_defaults_and_env_override() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let _lock = crate::test_support::lock_env();
        for var in [
            BRIEF_SECS_ENV,
            DIGEST_SECS_ENV,
            TRY_FOLLOWUP_SECS_ENV,
            DEEPER_QUEUE_SECS_ENV,
            SWEEP_SECS_ENV,
            WIKI_LINT_SECS_ENV,
            TICK_SLEEP_SECS_ENV,
            MAX_TICKS_ENV,
        ] {
            std::env::remove_var(var);
        }
        let dir = tempfile::tempdir().unwrap();
        let cfg = DaemonConfig::from_paths(test_ctx(&dir, false)).unwrap();
        assert_eq!(cfg.brief_secs, DEFAULT_BRIEF_SECS);
        assert_eq!(cfg.digest_secs, DEFAULT_DIGEST_SECS);
        assert_eq!(cfg.try_followup_secs, DEFAULT_TRY_FOLLOWUP_SECS);
        assert_eq!(cfg.deeper_queue_secs, DEFAULT_DEEPER_QUEUE_SECS);
        assert_eq!(cfg.sweep_secs, DEFAULT_SWEEP_SECS);
        assert_eq!(cfg.wiki_lint_secs, DEFAULT_WIKI_LINT_SECS);
        assert_eq!(cfg.tick_sleep_secs, DEFAULT_TICK_SLEEP_SECS);
        assert_eq!(cfg.max_ticks, None);
        std::env::set_var(BRIEF_SECS_ENV, "7");
        std::env::set_var(MAX_TICKS_ENV, "3");
        let cfg = DaemonConfig::from_paths(test_ctx(&dir, false)).unwrap();
        assert_eq!(cfg.brief_secs, 7);
        assert_eq!(cfg.max_ticks, Some(3));
        std::env::set_var(BRIEF_SECS_ENV, "soon");
        assert!(DaemonConfig::from_paths(test_ctx(&dir, false)).is_err());
        std::env::remove_var(BRIEF_SECS_ENV);
        std::env::remove_var(MAX_TICKS_ENV);
    }

    #[test]
    fn scheduler_runs_due_and_skips_not_due() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let _env = crate::test_support::EnvHold::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(&dir);
        let mut sched = config.build_scheduler();
        let t = Instant::now();
        let first = sched.run_due(t);
        let names: Vec<&str> = first.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, AGENT_NAMES);
        assert!(first.iter().all(|(_, r)| r.is_ok()), "{first:?}");
        // Immediately again: nothing is due.
        assert!(sched.run_due(t).is_empty());
        // Past every interval: all due again.
        let later = t + Duration::from_secs(DEFAULT_DIGEST_SECS + 1);
        assert_eq!(sched.run_due(later).len(), 6);
    }

    #[test]
    fn once_is_deterministic_across_dirs() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let _env = crate::test_support::EnvHold::paid_off();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ra = run_once(&test_config(&a));
        let rb = run_once(&test_config(&b));
        assert!(ra.ok(), "{ra:?}");
        assert!(rb.ok(), "{rb:?}");
        let la = std::fs::read(a.path().join("decision_log.jsonl")).unwrap();
        let lb = std::fs::read(b.path().join("decision_log.jsonl")).unwrap();
        assert_eq!(la, lb, "same fixture + same --now must log identically");
        assert_eq!(
            kinds_of(&a.path().join("decision_log.jsonl"))
                .iter()
                .filter(|k| k.as_str() == DAEMON_TICK_KIND)
                .count(),
            6
        );
    }

    #[test]
    fn agent_error_isolated_and_logged() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let _env = crate::test_support::EnvHold::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(&dir, false);
        // Missing fixture file: the brief agent fails closed.
        ctx.fixture = Some(dir.path().join("missing.jsonl"));
        let config = DaemonConfig {
            ctx,
            ..DaemonConfig::default()
        };
        let report = run_once(&config);
        assert!(!report.ok());
        assert_eq!(report.outcomes.len(), 6);
        for o in &report.outcomes {
            assert!(o.tick_logged.is_ok(), "{}: {:?}", o.agent, o.tick_logged);
            if o.agent == "brief" {
                assert!(o.result.is_err(), "brief must fail");
            } else {
                assert!(o.result.is_ok(), "{}: {:?}", o.agent, o.result);
            }
        }
        let ticks: Vec<Entry> = entries_of(&config.ctx.log_path)
            .into_iter()
            .filter(|e| e.kind == DAEMON_TICK_KIND)
            .collect();
        assert_eq!(ticks.len(), 6);
        let brief = ticks
            .iter()
            .find(|e| e.payload["agent"] == "brief")
            .unwrap();
        assert_eq!(brief.payload["ok"], false);
        assert!(brief.payload.get("error").is_some());
    }

    #[test]
    fn brief_paid_proposes_without_model_call() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, true);
        // run_local_full takes no model at all: paid mode cannot call one.
        let tick = brief_tick(&ctx, NOW, true).unwrap();
        assert_eq!(tick.candidates, 3);
        assert!(!tick.proposed.is_none());
        let entries = entries_of(&ctx.log_path);
        let req = entries
            .iter()
            .find(|e| e.kind == approvals::REQUESTED_KIND)
            .expect("proposal appended");
        assert_eq!(req.payload["action"], BRIEF_PAID_ACTION);
        let date = crate::brief::date_from_days((NOW / 86400) as i64);
        assert_eq!(req.payload["payload"], brief_paid_payload(&date, 3));
        let model_call = entries
            .iter()
            .find(|e| e.kind == "brief.model_call")
            .unwrap();
        assert_eq!(model_call.payload["fallback"], true);
        assert_eq!(model_call.payload["model"], "local");
    }

    /// Test helper: closures proving the propose path makes no calls.
    #[allow(clippy::type_complexity)]
    fn panicking_fetchers() -> (
        impl Fn(&str) -> Result<crate::x_api::ThreadFetch, String>,
        impl Fn(&str) -> Result<crate::unroll::ThreadPost, String>,
        impl Fn(
            &str,
            Option<&str>,
        ) -> Result<
            (
                String,
                String,
                Option<ancientos_router::muse::MuseUsage>,
                Option<String>,
            ),
            String,
        >,
    ) {
        let ft = |_: &str| -> Result<crate::x_api::ThreadFetch, String> {
            panic!("fetch_thread must not run on the propose path")
        };
        let fp = |_: &str| -> Result<crate::unroll::ThreadPost, String> {
            panic!("fetch_post must not run on the propose path")
        };
        let call = |_: &str, _: Option<&str>| panic!("model call must not run on the propose path");
        (ft, fp, call)
    }

    /// Test helper: schedule a deeper follow-up for the post at the given time.
    fn queue_job(ctx: &AgentCtx, post_id: &str, at: u64, now: u64) {
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        crate::deeper::queue_later(&mut log, post_id, at, now).unwrap();
    }

    #[test]
    fn deeper_paid_propose_makes_no_fetch_or_model_call() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        queue_job(&ctx, "100", NOW, NOW);
        let (ft, fp, call) = panicking_fetchers();
        let tick = deeper_queue_tick_with(&ctx, NOW, true, &ft, &fp, &call).unwrap();
        assert_eq!(tick.proposed, vec!["100".to_string()]);
        assert!(tick.executed_paid.is_empty());
        let entries = entries_of(&ctx.log_path);
        assert!(entries.iter().any(|e| e.kind == approvals::REQUESTED_KIND));
        assert!(entries
            .iter()
            .any(|e| e.kind == crate::deeper::DEEPER_PROPOSED_KIND));
        assert!(!entries
            .iter()
            .any(|e| e.kind == crate::deeper::DEEPER_QUEUE_DONE_KIND));
    }

    /// Test helper: synthetic single-post fetch result for the given id.
    fn mock_thread(id: &str) -> crate::x_api::ThreadFetch {
        let post = crate::unroll::ThreadPost {
            id: id.into(),
            author_id: "syn_author".into(),
            text: "[synthetic] Install WordCraft and open a page daily".into(),
            created_at: None,
            conversation_id: Some(id.into()),
            referenced_tweets: Vec::new(),
            quoted: None,
        };
        crate::x_api::ThreadFetch {
            root: post.clone(),
            username: Some("syn_author".into()),
            posts: vec![post],
            search_ok: true,
            search_note: None,
        }
    }

    #[test]
    fn approved_released_once_then_executed_via_mock() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        queue_job(&ctx, "100", NOW, NOW);
        let calls = Cell::new(0usize);
        let fetch_thread = |id: &str| Ok(mock_thread(id));
        let fetch_post = |id: &str| Ok(mock_thread(id).posts[0].clone());
        let call = |_: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Ok((
                "1. Install WordCraft now\n2. Open a page daily\n".to_string(),
                "mock-spark".to_string(),
                None,
                None,
            ))
        };
        // Tick 1 proposes; the mock is never called.
        let t1 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t1.proposed, vec!["100".to_string()]);
        assert_eq!(calls.get(), 0);
        // A human approves (CLI-style: the token is dropped).
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        let req = approvals::pending(&log, NOW).unwrap();
        assert_eq!(req.len(), 1);
        approvals::approve(
            &mut log,
            &req[0].request.id,
            &req[0].request.payload_hash,
            "human",
            NOW,
        )
        .unwrap();
        drop(log);
        // Tick 2 claims, releases exactly once, and executes via the mock.
        let t2 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t2.executed_paid, vec!["100".to_string()]);
        assert_eq!(calls.get(), 1);
        let entries = entries_of(&ctx.log_path);
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind == approvals::RELEASED_KIND)
                .count(),
            1
        );
        assert!(entries.iter().any(|e| {
            e.kind == crate::deeper::DEEPER_QUEUE_DONE_KIND && e.payload["mode"] == "paid"
        }));
        assert!(ctx.deeper_dir.join("100.md").exists());
        // Tick 3 is a no-op: done jobs never run again.
        let t3 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t3, DeeperQueueTick::default());
        assert_eq!(calls.get(), 1);
        assert_eq!(
            entries_of(&ctx.log_path)
                .iter()
                .filter(|e| e.kind == approvals::RELEASED_KIND)
                .count(),
            1,
            "released exactly once"
        );
    }

    /// `execute_paid` takes the release token by value (compile-time).
    #[allow(clippy::type_complexity)]
    #[test]
    fn execute_paid_takes_release_token_by_value() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        // Compiles only if `execute_paid` takes `ReleaseToken` by value.
        let _sig: for<'a, 'b, 'c> fn(
            approvals::ReleaseToken,
            &AgentCtx,
            &crate::deeper::QueuedJob,
            &serde_json::Value,
            &str,
            &str,
            u64,
            &crate::delegate::ThreadFetcher<'a>,
            &crate::deeper::PostFetcher<'b>,
            &crate::deeper::DeeperMuseCaller<'c>,
        ) -> Result<PaidStep, String> = execute_paid;
    }

    /// Test helper: approve (without releasing) the job's single pending
    /// proposal, returning approval id and payload hash; the executor claims
    /// and releases the token itself.
    fn approve_only_for(ctx: &AgentCtx, now: u64) -> (String, String) {
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        let pending = approvals::pending(&log, now).unwrap();
        assert_eq!(pending.len(), 1);
        let id = pending[0].request.id.clone();
        let hash = pending[0].request.payload_hash.clone();
        approvals::approve(&mut log, &id, &hash, "human", now).unwrap();
        (id, hash)
    }

    /// Test helper: approve and release the job's pending proposal, returning approval id and payload hash.
    fn approve_and_release_for(
        ctx: &AgentCtx,
        post_id: &str,
        at: u64,
        now: u64,
    ) -> (String, String) {
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        let pending = approvals::pending(&log, now).unwrap();
        assert_eq!(pending.len(), 1);
        let id = pending[0].request.id.clone();
        let hash = pending[0].request.payload_hash.clone();
        approvals::approve(&mut log, &id, &hash, "human", now).unwrap();
        let token = approvals::claim_release_token(&log, &id, &hash).unwrap();
        approvals::release(&mut log, token, &deeper_paid_payload(post_id, at)).unwrap();
        (id, hash)
    }

    /// Paid execution logs job.started then job.done keyed by approval id.
    #[test]
    fn paid_execution_logs_started_then_done_for_approval() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        queue_job(&ctx, "100", NOW, NOW);
        let fetch_thread = |id: &str| Ok(mock_thread(id));
        let fetch_post = |id: &str| Ok(mock_thread(id).posts[0].clone());
        let call = |_: &str, _: Option<&str>| {
            Ok((
                "1. Install WordCraft now\n".to_string(),
                "mock".to_string(),
                None,
                None,
            ))
        };
        let t1 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t1.proposed, vec!["100".to_string()]);
        let (id, hash) = approve_only_for(&ctx, NOW);
        let t2 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t2.executed_paid, vec!["100".to_string()]);
        assert!(t2.released_not_done.is_empty());
        let entries = entries_of(&ctx.log_path);
        let started = entries
            .iter()
            .position(|e| e.kind == JOB_STARTED_KIND)
            .expect("job.started logged");
        let done = entries
            .iter()
            .position(|e| e.kind == JOB_DONE_KIND)
            .expect("job.done logged");
        assert!(started < done, "started precedes done");
        for e in [&entries[started], &entries[done]] {
            assert_eq!(e.payload["approval_id"], id);
            assert_eq!(e.payload["payload_hash"], hash);
            assert_eq!(e.payload["post_id"], "100");
        }
        assert_eq!(job_run_state(&entries, &id, &hash), JobRunState::Done);
    }

    /// A released-but-unfinished job needs a fresh approval to execute.
    #[test]
    fn released_without_done_needs_fresh_approval_to_execute() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        queue_job(&ctx, "100", NOW, NOW);
        let calls = Cell::new(0usize);
        let fetch_thread = |id: &str| Ok(mock_thread(id));
        let fetch_post = |id: &str| Ok(mock_thread(id).posts[0].clone());
        let call = |_: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Ok((
                "1. Install WordCraft now\n2. Open a page daily\n".to_string(),
                "mock-spark".to_string(),
                None,
                None,
            ))
        };
        // Tick 1 proposes.
        let t1 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t1.proposed, vec!["100".to_string()]);
        // A human approves; the executor releases, then the process dies
        // before marking done (simulated by releasing directly).
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        let req = approvals::pending(&log, NOW).unwrap();
        assert_eq!(req.len(), 1);
        let first_id = req[0].request.id.clone();
        let hash = req[0].request.payload_hash.clone();
        let payload = req[0].request.payload.clone();
        let approved = approvals::approve(&mut log, &first_id, &hash, "human", NOW).unwrap();
        drop(approved);
        let token = approvals::claim_release_token(&log, &first_id, &hash).unwrap();
        approvals::release(&mut log, token, &payload).unwrap();
        drop(log);
        // Tick 2 (a second later, so the fresh request gets a new id) must
        // not execute the released request: it proposes a fresh approval
        // and the mock is never called.
        let t2 =
            deeper_queue_tick_with(&ctx, NOW + 1, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert!(t2.executed_paid.is_empty());
        assert_eq!(t2.proposed, vec!["100".to_string()]);
        assert_eq!(calls.get(), 0);
        assert_eq!(
            entries_of(&ctx.log_path)
                .iter()
                .filter(|e| e.kind == approvals::RELEASED_KIND)
                .count(),
            1,
            "no second release of the old request"
        );
        assert!(
            !entries_of(&ctx.log_path)
                .iter()
                .any(|e| e.kind == crate::deeper::DEEPER_QUEUE_DONE_KIND),
            "unfinished jobs are never marked done"
        );
        // Approving the fresh request executes exactly once via the mock.
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        let pending = approvals::pending(&log, NOW + 1).unwrap();
        assert_eq!(pending.len(), 1);
        assert_ne!(pending[0].request.id, first_id);
        let fresh_id = pending[0].request.id.clone();
        let fresh_hash = pending[0].request.payload_hash.clone();
        approvals::approve(&mut log, &fresh_id, &fresh_hash, "human", NOW + 1).unwrap();
        drop(log);
        let t3 =
            deeper_queue_tick_with(&ctx, NOW + 1, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t3.executed_paid, vec!["100".to_string()]);
        assert_eq!(calls.get(), 1);
        assert!(ctx.deeper_dir.join("100.md").exists());
    }

    /// A release with job.started but no job.done surfaces as
    /// released-not-done and never retries silently.
    #[test]
    fn crash_between_release_and_done_surfaces_released_not_done() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        queue_job(&ctx, "100", NOW, NOW);
        let calls = Cell::new(0usize);
        let fetch_thread = |id: &str| Ok(mock_thread(id));
        let fetch_post = |id: &str| Ok(mock_thread(id).posts[0].clone());
        let call = |_: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Ok((
                "1. Install WordCraft now\n".to_string(),
                "mock".to_string(),
                None,
                None,
            ))
        };
        let t1 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t1.proposed, vec!["100".to_string()]);
        assert_eq!(calls.get(), 0);
        let (id, hash) = approve_and_release_for(&ctx, "100", NOW, NOW);
        // Simulate a crash after the started event: execution began (or
        // never did) but completion was never recorded.
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        log.append(
            JOB_STARTED_KIND,
            job_started_payload(&id, &hash, "100", NOW, NOW),
        )
        .unwrap();
        drop(log);
        for _ in 0..2 {
            let tick =
                deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
            assert_eq!(
                tick.released_not_done,
                vec![ReleasedNotDone {
                    post_id: "100".to_string(),
                    approval_id: id.clone(),
                }]
            );
            assert!(tick.executed_paid.is_empty());
        }
        assert_eq!(calls.get(), 0, "released-not-done job never re-executes");
        assert!(!entries_of(&ctx.log_path)
            .iter()
            .any(|e| e.kind == crate::deeper::DEEPER_QUEUE_DONE_KIND));
    }

    /// A job.done without the queue marker completes bookkeeping without
    /// re-running the side effect.
    #[test]
    fn done_job_without_queue_marker_never_executes_twice() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        queue_job(&ctx, "100", NOW, NOW);
        let calls = Cell::new(0usize);
        let fetch_thread = |id: &str| Ok(mock_thread(id));
        let fetch_post = |id: &str| Ok(mock_thread(id).posts[0].clone());
        let call = |_: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Ok((
                "1. Install WordCraft now\n".to_string(),
                "mock".to_string(),
                None,
                None,
            ))
        };
        let t1 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t1.proposed, vec!["100".to_string()]);
        let (id, hash) = approve_and_release_for(&ctx, "100", NOW, NOW);
        // Simulate a crash after the side effect completed but before the
        // queue marker was written: started and done exist, deeper.done
        // does not.
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        log.append(
            JOB_STARTED_KIND,
            job_started_payload(&id, &hash, "100", NOW, NOW),
        )
        .unwrap();
        log.append(JOB_DONE_KIND, job_done_payload(&id, &hash, "100", NOW, NOW))
            .unwrap();
        drop(log);
        let tick =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(tick.executed_paid, vec!["100".to_string()]);
        assert!(tick.released_not_done.is_empty());
        assert_eq!(calls.get(), 0, "side effect already ran: no second run");
        assert!(entries_of(&ctx.log_path).iter().any(|e| {
            e.kind == crate::deeper::DEEPER_QUEUE_DONE_KIND && e.payload["mode"] == "paid"
        }));
        // The healed job leaves the queue: the next tick is a no-op.
        let again =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(again, DeeperQueueTick::default());
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn denied_and_expired_never_execute() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        queue_job(&ctx, "100", NOW, NOW);
        queue_job(&ctx, "101", NOW, NOW);
        let calls = Cell::new(0usize);
        let fetch_thread = |id: &str| Ok(mock_thread(id));
        let fetch_post = |id: &str| Ok(mock_thread(id).posts[0].clone());
        let call = |_: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Ok((
                "1. Install WordCraft now\n".to_string(),
                "mock".to_string(),
                None,
                None,
            ))
        };
        let t1 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(t1.proposed.len(), 2);
        // Deny the first job's proposal.
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        let pending = approvals::pending(&log, NOW).unwrap();
        let hash = pending[0].request.payload_hash.clone();
        let id = pending[0].request.id.clone();
        approvals::deny(&mut log, &id, &hash, "human", "no", NOW).unwrap();
        drop(log);
        let t2 =
            deeper_queue_tick_with(&ctx, NOW, true, &fetch_thread, &fetch_post, &call).unwrap();
        assert_eq!(calls.get(), 0, "denied job never executes");
        assert!(
            t2.skipped_denied.contains(&"100".to_string())
                || t2.skipped_denied.contains(&"101".to_string())
        );
        assert!(t2.executed_paid.is_empty());
        let requested = entries_of(&ctx.log_path)
            .iter()
            .filter(|e| e.kind == approvals::REQUESTED_KIND)
            .count();
        assert_eq!(requested, 2, "denied jobs are never re-proposed");
        // Expired proposals never execute either: seed a short-lived request
        // for a fresh job, then tick past its expiry.
        let dir2 = tempfile::tempdir().unwrap();
        let ctx2 = test_ctx(&dir2, false);
        queue_job(&ctx2, "102", NOW, NOW);
        let mut log2 = DecisionLog::open(&ctx2.log_path).unwrap();
        approvals::request(
            &mut log2,
            &crate::app_gate(),
            DAEMON_AGENT_NAME,
            DEEPER_PAID_ACTION,
            "synthetic short-lived",
            deeper_paid_payload("102", NOW),
            NOW,
            Some(10),
        )
        .unwrap();
        drop(log2);
        let calls2 = Cell::new(0usize);
        let call2 = |_: &str, _: Option<&str>| {
            calls2.set(calls2.get() + 1);
            Ok((
                "1. Install WordCraft now\n".to_string(),
                "mock".to_string(),
                None,
                None,
            ))
        };
        let t3 = deeper_queue_tick_with(&ctx2, NOW + 100, true, &fetch_thread, &fetch_post, &call2)
            .unwrap();
        assert_eq!(calls2.get(), 0, "expired proposal never executes");
        assert!(t3.executed_paid.is_empty());
        assert!(!entries_of(&ctx2.log_path)
            .iter()
            .any(|e| e.kind == crate::deeper::DEEPER_QUEUE_DONE_KIND));
    }

    /// Test helper: seed the path with a synthetic two-post thread.
    fn write_thread_fixture(path: &Path) {
        let row = |id: &str, text: &str, conv: &str| {
            serde_json::json!({
                "post_id": id,
                "url": format!("https://example.invalid/syn/status/{id}"),
                "text": text,
                "author_username": "syn_author",
                "created_at": "2026-10-20T14:00:00+00:00",
                "conversation_id": conv,
            })
            .to_string()
        };
        let body = [
            row("100", "[synthetic] Start a page for the garden plan", "100"),
            row("104", "[synthetic] Then list the seeds to order", "100"),
        ]
        .join("\n")
            + "\n";
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn queued_local_job_runs_only_after_its_time() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(&dir, false);
        let fixture = dir.path().join("thread.jsonl");
        write_thread_fixture(&fixture);
        ctx.fixture = Some(fixture);
        queue_job(&ctx, "100", NOW, NOW - 10);
        let (ft, fp, call) = panicking_fetchers();
        // Before its time: not due, nothing runs (fetchers would panic).
        let early = deeper_queue_tick_with(&ctx, NOW - 1, false, &ft, &fp, &call).unwrap();
        assert_eq!(early, DeeperQueueTick::default());
        assert!(!entries_of(&ctx.log_path)
            .iter()
            .any(|e| e.kind == crate::deeper::DEEPER_QUEUE_DONE_KIND));
        // At its time: the free local fallback runs directly from the
        // fixture (still no fetch or model call).
        let due = deeper_queue_tick_with(&ctx, NOW, false, &ft, &fp, &call).unwrap();
        assert_eq!(due.ran_local, vec!["100".to_string()]);
        assert!(ctx.deeper_dir.join("100.md").exists());
        let entries = entries_of(&ctx.log_path);
        assert!(entries.iter().any(|e| {
            e.kind == crate::deeper::DEEPER_QUEUE_DONE_KIND && e.payload["mode"] == "local"
        }));
        assert!(!entries.iter().any(|e| e.kind == approvals::REQUESTED_KIND));
    }

    #[test]
    fn digest_never_records_and_follows_weekly_rule() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        crate::digest::record_shown(&mut log, NOW - crate::digest::WEEK_SECS).unwrap();
        drop(log);
        let before = entries_of(&ctx.log_path).len();
        // Last showing exactly a week ago: due, and the tick records nothing.
        let tick = digest_tick(&ctx, NOW).unwrap();
        assert!(tick.due);
        assert!(tick.rendered.contains("Status: DUE"));
        assert_eq!(entries_of(&ctx.log_path).len(), before);
        // A fresh showing: not due.
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        crate::digest::record_shown(&mut log, NOW).unwrap();
        drop(log);
        let tick = digest_tick(&ctx, NOW).unwrap();
        assert!(!tick.due);
    }

    #[test]
    fn try_followup_logs_count_and_ids() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, false);
        let mut log = DecisionLog::open(&ctx.log_path).unwrap();
        crate::try_it::record_shown(&mut log, "p1", NOW - 100).unwrap();
        crate::try_it::record_shown(&mut log, "p2", NOW - 100).unwrap();
        crate::try_it::record_followup(&mut log, "p2", crate::try_it::Tried::Yes, None).unwrap();
        drop(log);
        let pending = try_followup_tick(&ctx, NOW).unwrap();
        assert_eq!(pending, vec!["p1".to_string()]);
        let entry = entries_of(&ctx.log_path)
            .into_iter()
            .find(|e| e.kind == TRY_FOLLOWUP_KIND)
            .unwrap();
        assert_eq!(entry.payload["count"], 1);
        assert_eq!(entry.payload["post_ids"], serde_json::json!(["p1"]));
        assert_eq!(entry.payload["at"], NOW);
    }

    #[test]
    fn tick_entries_carry_no_secrets_or_post_text() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let _env = crate::test_support::EnvHold::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let marker = "synthetic-secret-marker-zzz";
        std::fs::write(
            dir.path().join("brief.jsonl"),
            serde_json::to_string(&bookmark("b1", &format!("note with {marker} inside"))).unwrap()
                + "\n",
        )
        .unwrap();
        let mut ctx = test_ctx(&dir, false);
        ctx.fixture = Some(dir.path().join("brief.jsonl"));
        let config = DaemonConfig {
            ctx,
            ..DaemonConfig::default()
        };
        let report = run_once(&config);
        assert!(report.ok(), "{report:?}");
        let raw = std::fs::read_to_string(&config.ctx.log_path).unwrap();
        assert!(!raw.contains(marker), "post text leaked into the log");
        for secret_word in ["META_API_KEY", "TYPESAFE_API_KEY", "Bearer "] {
            assert!(!raw.contains(secret_word), "{secret_word} leaked");
        }
        for e in entries_of(&config.ctx.log_path) {
            if e.kind != DAEMON_TICK_KIND {
                continue;
            }
            let keys: Vec<&str> = e
                .payload
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert!(
                keys.iter()
                    .all(|k| ["agent", "ok", "at", "error"].contains(k)),
                "tick carries only status fields: {keys:?}"
            );
        }
    }

    #[test]
    fn loop_respects_max_ticks_and_stop_flag() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let _env = crate::test_support::EnvHold::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(&dir);
        config.tick_sleep_secs = 0;
        config.max_ticks = Some(2);
        let stop = AtomicBool::new(false);
        let reports = run_loop(&config, &stop);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].outcomes.len(), 6);
        // Second tick runs nothing: no interval elapsed.
        assert!(reports[1].outcomes.is_empty());
        let stop = AtomicBool::new(true);
        assert!(run_loop(&config, &stop).is_empty());
    }

    #[test]
    fn wiki_lint_tick_proposes_without_editing() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let _env = crate::test_support::EnvHold::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&dir, true);
        let wiki = crate::wiki::wiki_dir_for_log(&ctx.log_path);
        crate::wiki::write_raw(
            &wiki,
            "p1",
            "https://x.com/i/status/p1",
            ancientos_router::data_class::DataClass::Public,
            "2026-10-27",
            "synthetic source",
            &[],
            None,
        )
        .unwrap();
        let pages = wiki.join(crate::wiki::PAGES_DIR_NAME);
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(
            pages.join("lonely.md"),
            "# Lonely\nslug: lonely\nsources: p1\nclass: public\nsummary: Alone.\n\n## claims\n- Alone claim. [sources: p1]\n\n## links\nnone\n",
        )
        .unwrap();
        let before = std::fs::read(pages.join("lonely.md")).unwrap();
        let report = wiki_lint_tick(&ctx, NOW).unwrap();
        assert!(!report.findings.is_empty());
        assert_eq!(report.findings.len(), report.approvals.len());
        assert_eq!(
            std::fs::read(pages.join("lonely.md")).unwrap(),
            before,
            "lint never edits pages"
        );
        assert!(
            entries_of(&ctx.log_path)
                .iter()
                .any(|e| e.kind == crate::wiki::WIKI_LINT_KIND),
            "lint run is logged"
        );
    }

    #[test]
    fn daemon_paid_actions_share_the_single_gate_policy() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        use ancientos_core::gate::{Proposal, Verdict};
        // The daemon imports the gate's constants (one place): both actions
        // need a human, and anything unregistered fails closed.
        assert_eq!(BRIEF_PAID_ACTION, "brief.paid");
        assert_eq!(DEEPER_PAID_ACTION, "deeper.paid");
        let gate = crate::app_gate();
        for action in [BRIEF_PAID_ACTION, DEEPER_PAID_ACTION] {
            assert_eq!(
                gate.evaluate(&Proposal {
                    agent: DAEMON_AGENT_NAME.into(),
                    action: action.into(),
                }),
                Verdict::NeedsHuman
            );
        }
        assert!(matches!(
            gate.evaluate(&Proposal {
                agent: DAEMON_AGENT_NAME.into(),
                action: "daemon.anything_else".into(),
            }),
            Verdict::Deny { .. }
        ));
    }
}
