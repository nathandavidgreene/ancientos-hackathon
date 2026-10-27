//! Local stats for the `/stats` dashboard and the `stats` CLI command.
//!
//! [`collect`] folds the data dir's decision log (verified first, fail
//! closed) plus the repo's `docs/muse-code-log.md` into one [`Stats`]
//! value; [`render_text`] prints it as plain text for the CLI. The web UI
//! renders the same [`Stats`] as HTML with an inline-SVG chart. No network,
//! no paid calls: everything is computed from local files only.

use ancientos_core::feedback::{self, FeedbackRecord, ThresholdPolicy};
use ancientos_core::log::{DecisionLog, Entry, LogError};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Env var overriding the Muse Code run-log path (primary).
pub const MUSE_LOG_ENV_VAR: &str = "MUSE_CODE_LOG_PATH";
/// Env var fallback for the run-log path (same name `scripts/muse_run.sh` uses).
pub const MUSE_LOG_FALLBACK_ENV_VAR: &str = "MUSE_LOG";
/// Repo-relative default for the Muse Code run log.
pub const MUSE_LOG_DEFAULT_PATH: &str = "docs/muse-code-log.md";

/// Decision-log kinds counted as try-it cascade decisions. Any entry whose
/// kind contains `cascade` with a `stage` payload field is counted, plus
/// [`crate::try_it::DECIDED_KIND`] (`try_it.decided`, which carries no
/// `cascade` substring); unknown stages are ignored.
pub const CASCADE_KIND_SUBSTR: &str = "cascade";

/// Cascade stages in display order (matching
/// `ancientos_router::try_it_cascade::CascadeStage::as_str`).
pub const CASCADE_STAGES: [&str; 5] = ["topic_filter", "rules", "jev", "muse", "fail_closed"];

/// Maximum entry kinds listed in [`Stats::kinds_top`].
pub const TOP_KINDS: usize = 10;

/// Canonical verdict display order (matching serde spellings).
pub const VERDICT_ORDER: [&str; 7] = [
    "approve",
    "skip",
    "never_post",
    "never_category",
    "delegate",
    "never_author",
    "unhide_author",
];

/// Muse Code runs and summed estimated cost parsed from
/// `docs/muse-code-log.md`. Cost is integer cents (no float drift).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MuseStats {
    /// Rows matching the `demo.sh` run-count rule (see [`is_run_row`]).
    pub runs: usize,
    /// Summed `est. cost $X.YY` amounts in those rows, in cents.
    pub cost_cents: u64,
}

impl MuseStats {
    /// Format the summed cost as `$D.CC` (args: none; returns the string;
    /// never fails).
    pub fn formatted_cost(&self) -> String {
        format!("${}.{:02}", self.cost_cents / 100, self.cost_cents % 100)
    }
}

/// Approval counts folded from the log at one clock time (args: none on
/// the struct; built by [`collect`]; never fails on its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApprovalCounts {
    /// Requests still awaiting a decision and unexpired at `now`.
    pub pending: usize,
    /// Decided-approved requests not yet released.
    pub approved: usize,
    /// Decided-denied requests.
    pub denied: usize,
    /// Expired requests (swept entries plus time-expired, still undecided).
    pub expired: usize,
    /// Approved requests whose release token was consumed exactly once.
    pub released: usize,
}

/// Try-it funnel counts (args: none on the struct; built by [`collect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TryItCounts {
    /// `try_it.shown` entries (tagged picks rendered).
    pub shown: usize,
    /// `try_it.followup` entries with `tried: yes`.
    pub yes: usize,
    /// `try_it.followup` entries with `tried: no`.
    pub no: usize,
    /// `try_it.followup` entries with `tried: later` (still pending).
    pub later: usize,
    /// Shown posts with no yes/no follow-up yet (first-shown order).
    pub pending: usize,
}

/// Try-it cascade decisions by stage (args: none on the struct; built by
/// [`collect`]; `None` in [`Stats::cascade`] means no decisions logged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CascadeCounts {
    /// Stage 0: excluded topic, always no, no call.
    pub topic_filter: u64,
    /// Rules alone (confident yes, or gates off).
    pub rules: u64,
    /// Jev `noul >=` threshold.
    pub jev: u64,
    /// Muse Spark strict-JSON answer.
    pub muse: u64,
    /// Backend error or unparseable reply: rules verdict, error recorded.
    pub fail_closed: u64,
}

impl CascadeCounts {
    /// Sum over all five stages (args: none; returns the total; never fails).
    pub fn total(&self) -> u64 {
        self.topic_filter + self.rules + self.jev + self.muse + self.fail_closed
    }

    /// True when every stage count is zero (args: none; never fails).
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Count for one display-order stage name (args: `stage` is one of
    /// [`CASCADE_STAGES`]; returns 0 for anything else; never fails).
    pub fn by_stage(&self, stage: &str) -> u64 {
        match stage {
            "topic_filter" => self.topic_filter,
            "rules" => self.rules,
            "jev" => self.jev,
            "muse" => self.muse,
            "fail_closed" => self.fail_closed,
            _ => 0,
        }
    }
}

/// Last daemon tick per agent (args: none on the struct; built by
/// [`collect`] from `daemon.tick` entries; never fails on its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickSummary {
    /// Agent name (`brief`, `digest`, ...).
    pub agent: String,
    /// Whether that tick succeeded.
    pub ok: bool,
    /// Wall-clock unix secs recorded with the tick (0 when missing).
    pub at: u64,
    /// Single-line error message when `ok` is false.
    pub error: Option<String>,
}

/// Every number on the `/stats` dashboard and in `stats` CLI output.
///
/// When `chain_verified` is false, every log-derived field is empty
/// (fail closed: nothing else is trusted) and only `muse_log` — parsed
/// from an independent repo file — may carry data. Renderers must show
/// `withheld` for the log sections in that case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    /// True when the hash chain verified (a missing log file counts as
    /// verified-but-empty).
    pub chain_verified: bool,
    /// 0-based line index where the chain broke (`None` when verified, or
    /// when the file exists but cannot be read at all).
    pub broken_at: Option<usize>,
    /// Verified entry count (0 and untrusted when broken).
    pub entry_count: usize,
    /// Top [`TOP_KINDS`] entry kinds by count (count desc, kind asc).
    pub kinds_top: Vec<(String, usize)>,
    /// Approval queue counts at collection time.
    pub approvals: ApprovalCounts,
    /// Feedback counts by verdict, canonical [`VERDICT_ORDER`], nonzero only.
    pub verdicts_by_verdict: Vec<(String, u64)>,
    /// Feedback counts by category, count desc then name asc.
    pub verdicts_by_category: Vec<(String, u64)>,
    /// Categories auto-approved under [`ThresholdPolicy::default`], sorted.
    pub auto_approved: Vec<String>,
    /// Try-it funnel counts.
    pub try_it: TryItCounts,
    /// Cascade decisions by stage, or `None` when none are logged.
    pub cascade: Option<CascadeCounts>,
    /// Last tick per agent, sorted by agent name.
    pub ticks: Vec<TickSummary>,
    /// Go-deeper jobs not yet done (queued or proposed), by `(at, post_id)`.
    pub deeper_outstanding: Vec<crate::deeper::QueuedJob>,
    /// Go-deeper jobs already done.
    pub deeper_done: usize,
    /// Muse Code runs and cost, or `None` when the run log is missing or
    /// unreadable (`unavailable`, never an error).
    pub muse_log: Option<MuseStats>,
}

impl Stats {
    /// Blank stats for a broken or unreadable log: every log-derived field
    /// empty and only `chain_verified`, `broken_at` and `muse_log` set.
    /// Args: `broken_at` is the 0-based breaking line index (`None` when the
    /// file cannot be read at all); `muse_log` is the independently parsed
    /// run-log stats. Returns the struct; never fails.
    pub fn broken(broken_at: Option<usize>, muse_log: Option<MuseStats>) -> Self {
        Self {
            chain_verified: false,
            broken_at,
            entry_count: 0,
            kinds_top: Vec::new(),
            approvals: ApprovalCounts::default(),
            verdicts_by_verdict: Vec::new(),
            verdicts_by_category: Vec::new(),
            auto_approved: Vec::new(),
            try_it: TryItCounts::default(),
            cascade: None,
            ticks: Vec::new(),
            deeper_outstanding: Vec::new(),
            deeper_done: 0,
            muse_log,
        }
    }
}

/// Resolve the Muse Code run-log path: `$MUSE_CODE_LOG_PATH`, else
/// `$MUSE_LOG` (the `scripts/muse_run.sh` name), else
/// [`MUSE_LOG_DEFAULT_PATH`]. Empty values are ignored. Args: none;
/// returns the path; never fails, never reads the file.
pub fn muse_log_path_from_env() -> PathBuf {
    for var in [MUSE_LOG_ENV_VAR, MUSE_LOG_FALLBACK_ENV_VAR] {
        if let Ok(p) = std::env::var(var) {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    PathBuf::from(MUSE_LOG_DEFAULT_PATH)
}

/// True for Muse Code run-log data rows: the same rule `scripts/demo.sh`
/// shot 13 uses (`grep -E '^\\| [0-9]+[a-z]? '` minus `Not used` rows).
/// Args: one raw log line; returns whether it counts as a run; never fails.
pub fn is_run_row(line: &str) -> bool {
    if line.contains("Not used") {
        return false;
    }
    let Some(after) = line.strip_prefix("| ") else {
        return false;
    };
    let digits = after.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let mut rest = &after[digits..];
    if let Some(tail) = rest.strip_prefix(|c: char| c.is_ascii_lowercase()) {
        rest = tail;
    }
    rest.starts_with(' ')
}

/// Estimated-cost cents in one run-log row: sums every `est. cost $X.YY`
/// amount (at most one per row in practice). Args: one raw log line;
/// returns the cents found (0 when none or unparseable); never fails.
pub fn cost_cents_in_row(line: &str) -> u64 {
    let mut total = 0u64;
    let mut rest = line;
    while let Some(at) = rest.find("est. cost $") {
        rest = &rest[at + "est. cost $".len()..];
        let num: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut parts = num.split('.');
        let dollars: u64 = parts.next().unwrap_or("").parse().unwrap_or(0);
        let cents: u64 = match parts.next() {
            Some(frac) => {
                let two: String = frac.chars().take(2).collect();
                let padded = format!("{two:0<2}");
                padded.parse().unwrap_or(0)
            }
            None => 0,
        };
        if !num.is_empty() && dollars.checked_mul(100).is_some() {
            total = total.saturating_add(dollars.saturating_mul(100).saturating_add(cents));
        }
    }
    total
}

/// Parse run-log text into run count plus summed cost: rows matching
/// [`is_run_row`] count as runs, and their [`cost_cents_in_row`] amounts
/// are summed. Args: the full file text; returns the stats (possibly
/// zero); never fails.
pub fn parse_muse_log(text: &str) -> MuseStats {
    let mut out = MuseStats::default();
    for line in text.lines() {
        if is_run_row(line) {
            out.runs += 1;
            out.cost_cents = out.cost_cents.saturating_add(cost_cents_in_row(line));
        }
    }
    out
}

/// Read and parse the run log at `path`; a missing or unreadable file (or
/// non-UTF-8 bytes) yields `None` (`unavailable`), never an error or panic.
/// Args: the run-log path; returns the stats or `None`; never fails.
pub fn read_muse_log(path: &Path) -> Option<MuseStats> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(parse_muse_log(&text))
}

/// First 0-based line index in `path` that does not parse as a log
/// [`Entry`]. Used only to locate a JSON break the chain verifier already
/// reported. Args: the decision-log path; returns the index, or `None`
/// when the file cannot be read or every line parses (fail closed:
/// callers treat `None` as unreadable); never fails.
fn first_bad_line(path: &Path) -> Option<usize> {
    let text = std::fs::read_to_string(path).ok()?;
    for (i, line) in text.lines().enumerate() {
        if serde_json::from_str::<Entry>(line).is_err() {
            return Some(i);
        }
    }
    None
}

/// Open and verify the decision log: returns `(entries, verified,
/// broken_at)`. A missing file reads as verified-but-empty; a chain break
/// yields `broken_at` (0-based); an unreadable file yields unverified with
/// `broken_at: None`. Args: the decision-log path; returns the triple;
/// never fails.
fn read_verified_entries(log_path: &Path) -> (Vec<Entry>, bool, Option<usize>) {
    let log = match DecisionLog::open(log_path) {
        Ok(log) => log,
        Err(LogError::Chain(n)) => return (Vec::new(), false, Some(n)),
        Err(LogError::Json(_)) => return (Vec::new(), false, first_bad_line(log_path)),
        Err(LogError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), true, None)
        }
        Err(LogError::Io(_)) => return (Vec::new(), false, None),
    };
    match log.verify_or_empty() {
        Ok(entries) => (entries, true, None),
        Err(LogError::Chain(n)) => (Vec::new(), false, Some(n)),
        Err(LogError::Json(_)) => (Vec::new(), false, first_bad_line(log_path)),
        Err(LogError::Io(_)) => (Vec::new(), false, None),
    }
}

/// Normalize one cascade `stage` payload value to a [`CASCADE_STAGES`] name:
/// lowercased with separators stripped, so `Muse`, `muse_spark`,
/// `fail closed` and `failed_closed` all map. Args: the raw value;
/// returns the canonical stage or `None` for unknown values (fail closed:
/// callers ignore unknown stages); never fails.
fn normalize_stage(raw: &str) -> Option<&'static str> {
    let flat: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match flat.as_str() {
        "topicfilter" => Some("topic_filter"),
        "rules" | "rule" => Some("rules"),
        "jev" => Some("jev"),
        "muse" | "musespark" => Some("muse"),
        "failclosed" | "failedclosed" => Some("fail_closed"),
        _ => None,
    }
}

/// Collapse multi-line text to one line for single-line tick displays.
/// Args: the raw message; returns it with newlines as spaces, trimmed;
/// never fails.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Count verified entries by kind, sorted by count desc then kind asc and
/// capped at [`TOP_KINDS`]. Args: verified log entries; returns the
/// ranking; never fails.
fn top_kinds(entries: &[Entry]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for e in entries {
        *counts.entry(e.kind.as_str()).or_default() += 1;
    }
    let mut out: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(k, n)| (k.to_string(), n))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(TOP_KINDS);
    out
}

/// Fold approval entries into status counts at `now` via
/// `core::approvals` (`pending` plus `decided` with the time overlay, so
/// unswept time-expired requests count as expired). Args: an open verified
/// log and unix-secs `now`; returns the counts (all zero when the fold
/// errors, fail closed); never fails.
fn approval_counts(log: &DecisionLog, now: u64) -> ApprovalCounts {
    let mut out = ApprovalCounts::default();
    let pending = ancientos_core::approvals::pending(log, now).unwrap_or_default();
    out.pending = pending.len();
    for a in ancientos_core::approvals::decided(log, now).unwrap_or_default() {
        match a.status_at(now) {
            ancientos_core::approvals::Status::Approved => out.approved += 1,
            ancientos_core::approvals::Status::Denied => out.denied += 1,
            ancientos_core::approvals::Status::Expired => out.expired += 1,
            ancientos_core::approvals::Status::Released => out.released += 1,
            ancientos_core::approvals::Status::Pending => {}
        }
    }
    out
}

/// Parse `feedback` entries into records, skipping malformed payloads
/// individually (one bad record never zeroes the rest). Args: verified log
/// entries; returns the parsed records; never fails.
fn feedback_records(entries: &[Entry]) -> Vec<FeedbackRecord> {
    entries
        .iter()
        .filter(|e| e.kind == feedback::FEEDBACK_KIND)
        .filter_map(|e| serde_json::from_value(e.payload.clone()).ok())
        .collect()
}

/// Count records by verdict in canonical [`VERDICT_ORDER`], nonzero only.
/// Args: parsed feedback records; returns `(verdict, count)` pairs; never fails.
fn verdict_counts(records: &[FeedbackRecord]) -> Vec<(String, u64)> {
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for r in records {
        *counts.entry(r.verdict.as_str()).or_default() += 1;
    }
    VERDICT_ORDER
        .iter()
        .filter_map(|v| counts.get(v).map(|n| (v.to_string(), *n)))
        .collect()
}

/// Count records by category (all verdicts), sorted by count desc then name
/// asc. Args: parsed feedback records; returns `(category, count)` pairs;
/// never fails.
fn category_counts(records: &[FeedbackRecord]) -> Vec<(String, u64)> {
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for r in records {
        *counts.entry(r.category.as_str()).or_default() += 1;
    }
    let mut out: Vec<(String, u64)> = counts
        .into_iter()
        .map(|(k, n)| (k.to_string(), n))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Count try-it funnel entries: `try_it.shown` entries, `try_it.followup`
/// entries by tried value (unknown values ignored), and the pending queue
/// via `try_it::pending_followups`. Args: an open verified log plus its
/// entries; returns the counts; never fails.
fn try_it_counts(log: &DecisionLog, entries: &[Entry]) -> TryItCounts {
    let mut out = TryItCounts::default();
    for e in entries {
        if e.kind == crate::try_it::SHOWN_KIND {
            out.shown += 1;
        } else if e.kind == crate::try_it::FOLLOWUP_KIND {
            match e.payload.get("tried").and_then(|v| v.as_str()) {
                Some("yes") => out.yes += 1,
                Some("no") => out.no += 1,
                Some("later") => out.later += 1,
                _ => {}
            }
        }
    }
    out.pending = crate::try_it::pending_followups(log).len();
    out
}

/// Count cascade decisions by stage over entries whose kind contains
/// [`CASCADE_KIND_SUBSTR`] (or equals [`crate::try_it::DECIDED_KIND`])
/// with a `stage` payload that [`normalize_stage`] accepts. Args:
/// verified log entries; returns `Some` counts, or `None` when no cascade
/// decisions are logged; never fails.
fn cascade_counts(entries: &[Entry]) -> Option<CascadeCounts> {
    let mut out = CascadeCounts::default();
    for e in entries {
        if !e.kind.contains(CASCADE_KIND_SUBSTR) && e.kind != crate::try_it::DECIDED_KIND {
            continue;
        }
        let Some(stage) = e.payload.get("stage").and_then(|v| v.as_str()) else {
            continue;
        };
        match normalize_stage(stage) {
            Some("topic_filter") => out.topic_filter += 1,
            Some("rules") => out.rules += 1,
            Some("jev") => out.jev += 1,
            Some("muse") => out.muse += 1,
            Some("fail_closed") => out.fail_closed += 1,
            _ => {}
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Last `daemon.tick` entry per agent (log order wins), sorted by agent
/// name. Entries missing an agent name or the `ok` flag are skipped.
/// Args: verified log entries; returns the per-agent summaries; never fails.
fn tick_summaries(entries: &[Entry]) -> Vec<TickSummary> {
    let mut last: BTreeMap<&str, TickSummary> = BTreeMap::new();
    for e in entries {
        if e.kind != crate::daemon::DAEMON_TICK_KIND {
            continue;
        }
        let (Some(agent), Some(ok)) = (
            e.payload.get("agent").and_then(|v| v.as_str()),
            e.payload.get("ok").and_then(|v| v.as_bool()),
        ) else {
            continue;
        };
        if agent.is_empty() {
            continue;
        }
        let at = e.payload.get("at").and_then(|v| v.as_u64()).unwrap_or(0);
        let error = e
            .payload
            .get("error")
            .and_then(|v| v.as_str())
            .map(one_line)
            .filter(|s| !s.is_empty());
        last.insert(
            agent,
            TickSummary {
                agent: agent.to_string(),
                ok,
                at,
                error,
            },
        );
    }
    last.into_values().collect()
}

/// Collect every stat from the decision log at `log_path` (verified first)
/// and the Muse Code run log at `muse_log_path`, with approval/queue
/// states evaluated at unix-secs `now`. A broken chain yields
/// [`Stats::broken`] (nothing else trusted); a missing run log yields
/// `muse_log: None` (`unavailable`). Args: the two file paths and `now`;
/// returns the stats; never fails, never panics, no network, no paid calls.
pub fn collect(log_path: &Path, muse_log_path: &Path, now: u64) -> Stats {
    let muse_log = read_muse_log(muse_log_path);
    let (entries, verified, broken_at) = read_verified_entries(log_path);
    if !verified {
        return Stats::broken(broken_at, muse_log);
    }
    let Ok(log) = DecisionLog::open(log_path) else {
        return Stats::broken(first_bad_line(log_path), muse_log);
    };
    let records = feedback_records(&entries);
    let queue = crate::deeper::list_queue(&entries);
    let (outstanding, done) = queue
        .into_iter()
        .partition::<Vec<_>, _>(|j| j.status != crate::deeper::QueueStatus::Done);
    Stats {
        chain_verified: true,
        broken_at: None,
        entry_count: entries.len(),
        kinds_top: top_kinds(&entries),
        approvals: approval_counts(&log, now),
        verdicts_by_verdict: verdict_counts(&records),
        verdicts_by_category: category_counts(&records),
        auto_approved: feedback::auto_approved(&records, ThresholdPolicy::default())
            .into_iter()
            .collect(),
        try_it: try_it_counts(&log, &entries),
        cascade: cascade_counts(&entries),
        ticks: tick_summaries(&entries),
        deeper_outstanding: outstanding,
        deeper_done: done.len(),
        muse_log,
    }
}

/// Render one `name: value` list section: a `title:` header plus one line
/// per item, or `title: none` when empty. Args: the section title and
/// `(name, count)` items; returns the lines (each trailing-`\n`-free);
/// never fails.
fn list_section(title: &str, items: &[(String, u64)]) -> String {
    if items.is_empty() {
        return format!("{title}: none\n");
    }
    let mut out = format!("{title}:\n");
    for (name, n) in items {
        out.push_str(&format!("{name}: {n}\n"));
    }
    out
}

/// Render [`Stats`] as stable plain text for the `stats` CLI command (one
/// `key: value` or `key:`-plus-items section per line block). A broken
/// chain renders `withheld` for every log-derived section while the
/// independent run-log line still prints. Args: the collected stats;
/// returns the text (trailing newline); never fails.
pub fn render_text(stats: &Stats) -> String {
    let mut out = String::new();
    if !stats.chain_verified {
        match stats.broken_at {
            Some(n) => out.push_str(&format!("decision log: broken at line {n} (fail closed)\n")),
            None => out.push_str("decision log: unreadable (fail closed)\n"),
        }
        for section in [
            "entries",
            "top kinds",
            "approvals",
            "verdicts by verdict",
            "verdicts by category",
            "auto-approved categories",
            "try-it",
            "cascade",
            "daemon ticks",
            "deeper queue",
        ] {
            out.push_str(&format!("{section}: withheld\n"));
        }
    } else {
        out.push_str("decision log: verified\n");
        out.push_str(&format!("entries: {}\n", stats.entry_count));
        let kinds: Vec<(String, u64)> = stats
            .kinds_top
            .iter()
            .map(|(k, n)| (k.clone(), *n as u64))
            .collect();
        out.push_str(&list_section("top kinds", &kinds));
        let a = &stats.approvals;
        out.push_str(&format!(
            "approvals: pending={} approved={} denied={} expired={} released={}\n",
            a.pending, a.approved, a.denied, a.expired, a.released
        ));
        out.push_str(&list_section(
            "verdicts by verdict",
            &stats.verdicts_by_verdict,
        ));
        out.push_str(&list_section(
            "verdicts by category",
            &stats.verdicts_by_category,
        ));
        if stats.auto_approved.is_empty() {
            out.push_str("auto-approved categories: none\n");
        } else {
            out.push_str(&format!(
                "auto-approved categories: {}\n",
                stats.auto_approved.join(", ")
            ));
        }
        let t = &stats.try_it;
        out.push_str(&format!(
            "try-it: shown={} yes={} no={} later={} pending={}\n",
            t.shown, t.yes, t.no, t.later, t.pending
        ));
        match &stats.cascade {
            Some(c) => out.push_str(&format!(
                "cascade: topic_filter={} rules={} jev={} muse={} fail_closed={}\n",
                c.topic_filter, c.rules, c.jev, c.muse, c.fail_closed
            )),
            None => out.push_str("cascade: no cascade decisions logged\n"),
        }
        if stats.ticks.is_empty() {
            out.push_str("daemon ticks: none\n");
        } else {
            out.push_str("daemon ticks:\n");
            for tick in &stats.ticks {
                if tick.ok {
                    out.push_str(&format!("{}: ok at={}\n", tick.agent, tick.at));
                } else if let Some(error) = &tick.error {
                    out.push_str(&format!("{}: error at={} {error}\n", tick.agent, tick.at));
                } else {
                    out.push_str(&format!("{}: error at={}\n", tick.agent, tick.at));
                }
            }
        }
        out.push_str(&format!(
            "deeper queue: outstanding={} done={}\n",
            stats.deeper_outstanding.len(),
            stats.deeper_done
        ));
        for job in &stats.deeper_outstanding {
            out.push_str(&format!(
                "{}: at={} status={}\n",
                job.post_id,
                job.at,
                job.status.as_str()
            ));
        }
    }
    match &stats.muse_log {
        Some(m) => out.push_str(&format!(
            "built with meta: runs={} est_cost={}\n",
            m.runs,
            m.formatted_cost()
        )),
        None => out.push_str("built with meta: unavailable\n"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_row_rule_matches_demo_sh() {
        assert!(is_run_row("| 1 | 2026-10-26 | x | Done |"));
        assert!(is_run_row("| 2b | 2026-10-26 | x | Done |"));
        assert!(is_run_row("| 42 | 2026-10-27 | x | Done |"));
        assert!(!is_run_row("| 2b | 2026-10-26 | x | Not used: ... |"));
        assert!(!is_run_row("| # | Date | Prompt | Outcome |"));
        assert!(!is_run_row("|---|"));
        assert!(!is_run_row("Muse Code CLI 1.4.13"));
        assert!(!is_run_row("|  | 2026-10-26 | x | Done |"));
        assert!(!is_run_row(" | 3 | 2026-10-26 | x | Done |"));
    }

    #[test]
    fn cost_parsing_sums_cents() {
        assert_eq!(cost_cents_in_row("no cost here"), 0);
        assert_eq!(cost_cents_in_row("est. cost $1.79."), 179);
        assert_eq!(cost_cents_in_row("est. cost $0.41."), 41);
        assert_eq!(cost_cents_in_row("est. cost $12."), 1200);
        assert_eq!(cost_cents_in_row("est. cost $7.5."), 750);
        assert_eq!(
            cost_cents_in_row("est. cost $1.00 and est. cost $2.50"),
            350
        );
        assert_eq!(
            parse_muse_log("| 1 | d | p | est. cost $1.79. |\n| 2b | d | p | Not used |\n").runs,
            1
        );
        let m =
            parse_muse_log("| 1 | d | p | est. cost $1.79. |\n| 2 | d | p | est. cost $2.46. |\n");
        assert_eq!((m.runs, m.cost_cents), (2, 425));
        assert_eq!(m.formatted_cost(), "$4.25");
    }

    #[test]
    fn stage_normalization_accepts_variants() {
        assert_eq!(normalize_stage("topic_filter"), Some("topic_filter"));
        assert_eq!(normalize_stage("Rules"), Some("rules"));
        assert_eq!(normalize_stage("Jev"), Some("jev"));
        assert_eq!(normalize_stage("Muse"), Some("muse"));
        assert_eq!(normalize_stage("muse_spark"), Some("muse"));
        assert_eq!(normalize_stage("failed closed"), Some("fail_closed"));
        assert_eq!(normalize_stage("fail_closed"), Some("fail_closed"));
        assert_eq!(normalize_stage("nope"), None);
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn empty_dir_collects_verified_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("decision_log.jsonl");
        let muse = dir.path().join("missing.md");
        let s = collect(&log, &muse, 1_700_000_000);
        assert!(s.chain_verified);
        assert_eq!(s.entry_count, 0);
        assert!(s.kinds_top.is_empty());
        assert_eq!(s.approvals, ApprovalCounts::default());
        assert!(s.cascade.is_none());
        assert!(s.muse_log.is_none());
        let text = render_text(&s);
        assert!(text.contains("decision log: verified"), "{text}");
        assert!(
            text.contains("cascade: no cascade decisions logged"),
            "{text}"
        );
        assert!(text.contains("built with meta: unavailable"), "{text}");
    }

    #[test]
    fn broken_chain_collects_nothing_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("decision_log.jsonl");
        let mut open = DecisionLog::open(&log).unwrap();
        open.append("feedback", serde_json::json!({"x": 1}))
            .unwrap();
        open.append("feedback", serde_json::json!({"x": 2}))
            .unwrap();
        drop(open);
        let tampered = std::fs::read_to_string(&log)
            .unwrap()
            .replacen("\"x\":1", "\"x\":9", 1);
        write(&log, &tampered);
        let muse = dir.path().join("runs.md");
        write(&muse, "| 1 | d | p | est. cost $1.00. |\n");
        let s = collect(&log, &muse, 1_700_000_000);
        assert!(!s.chain_verified);
        assert_eq!(s.broken_at, Some(0));
        assert_eq!(s.entry_count, 0);
        assert_eq!(s.muse_log.unwrap().runs, 1);
        let text = render_text(&s);
        assert!(text.contains("broken at line 0"), "{text}");
        assert!(text.contains("approvals: withheld"), "{text}");
        assert!(!text.contains("verified"), "{text}");
    }

    #[test]
    fn decided_entries_count_by_stage_alongside_cascade_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("decision_log.jsonl");
        let mut open = DecisionLog::open(&log).unwrap();
        for (post_id, stage) in [
            ("syn-1", "rules"),
            ("syn-2", "jev"),
            ("syn-3", "muse"),
            ("syn-4", "topic_filter"),
            ("syn-5", "fail_closed"),
        ] {
            open.append(
                crate::try_it::DECIDED_KIND,
                serde_json::json!({
                    "post_id": post_id,
                    "stage": stage,
                    "reason": "synthetic reason",
                }),
            )
            .unwrap();
        }
        open.append(
            "try_it.cascade",
            serde_json::json!({"post_id": "syn-6", "stage": "rules"}),
        )
        .unwrap();
        // Unknown stages and unrelated kinds never count.
        open.append(
            crate::try_it::DECIDED_KIND,
            serde_json::json!({"post_id": "syn-7", "stage": "nope"}),
        )
        .unwrap();
        open.append("brief.pick", serde_json::json!({"stage": "rules"}))
            .unwrap();
        drop(open);
        let muse = dir.path().join("missing.md");
        let s = collect(&log, &muse, 1_700_000_000);
        let c = s.cascade.expect("decisions logged");
        assert_eq!(
            (c.topic_filter, c.rules, c.jev, c.muse, c.fail_closed),
            (1, 2, 1, 1, 1)
        );
        let text = render_text(&s);
        assert!(
            text.contains("cascade: topic_filter=1 rules=2 jev=1 muse=1 fail_closed=1"),
            "{text}"
        );
    }
}
