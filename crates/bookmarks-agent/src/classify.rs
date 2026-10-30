//! Classify-on-import: score every new bookmark against the private label
//! queries and run media posts through `analyze_media`, before the labels
//! page ever shows them.
//!
//! Flow ([`classify_new_posts`], called after every `sync` import):
//! 1. Load the category queries from [`label_queries_path`]. A missing or
//!    unparseable file records a per-post [`ClassifyStatus::Error`] (never
//!    classified, never billed).
//! 2. Load the [`ClassifyStore`]. An unreadable file refuses the whole run
//!    with no model calls: idempotency is unprovable, so fail closed.
//! 3. Skip posts already [`ClassifyStatus::Classified`] (idempotent: never
//!    re-billed). Muted, pending, and error posts are re-evaluated; only
//!    changed outcomes are appended (append-only, last wins).
//! 4. Per post ([`classify_post`]): mute pre-check (muted posts make no model
//!    call), paid gate (pending posts make no call), egress/class gate,
//!    one scoring call, mute post-check on the raw reply, strict parse, then
//!    `analyze_media` for posts with stored media.
//!
//! Mute checks mirror [`crate::analyze_media::apply_mute_check`]: a missing
//! or unreadable mute config withholds every score and flags the post
//! `mute_check_failed`. The config is read BEFORE any model call, so a post
//! that would be withheld is never billed. A configured-but-empty list (for
//! example `ANCIENTOS_MUTED_TERMS=""`) mutes nothing.
//!
//! Privacy: only [`DataClass::Public`] content reaches the hosted scorer,
//! and only as sanitized text ([`crate::sanitize::sanitize_uncapped`]); the
//! class comes from the ORIGINAL text ([`crate::translate::class_of`]), and
//! every routing decision is logged by [`check_egress`]. Records persist in
//! `eval-private/classify.jsonl` (mode 0600, append-only, keyed by post id).
//! Decision-log entries carry post ids and counts only, never post text,
//! scores, findings, or mute terms.
use crate::analyze_media::{Finding, MediaBackend, MediaRequest, MediaRow, PaidConfig, Variant};
use crate::fixture::Bookmark;
use ancientos_core::log::DecisionLog;
use ancientos_router::data_class::{check_egress, max_of, DataClass, Route};
use ancientos_router::muse::MuseClient;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Score at or above which a query counts as a matched category.
pub const MATCH_THRESHOLD: f64 = 0.5;
/// Classify records file inside [`EVAL_PRIVATE_DIR_NAME`].
pub const CLASSIFY_FILE_NAME: &str = "classify.jsonl";
/// Private dir under the data dir holding labels and classify records.
pub const EVAL_PRIVATE_DIR_NAME: &str = "eval-private";
/// Env var overriding the label-queries path.
pub const LABEL_QUERIES_ENV_VAR: &str = "ANCIENTOS_LABEL_QUERIES";
/// Default label-queries path relative to the data dir.
pub const LABEL_QUERIES_DEFAULT_REL_PATH: &str = "eval-private/nathan/label-queries.json";
/// Caller name on egress decisions for the scoring call.
pub const CALLER: &str = "classify";
/// Decision-log kind for a muted post (counts only, never text or terms).
pub const MUTED_KIND: &str = "classify.muted";
/// Decision-log kind for a post that failed to classify (content-free reason).
pub const ERROR_KIND: &str = "classify.error";
/// Completion-token cap per scoring call (five scores as JSON is tiny).
pub const SCORE_MAX_COMPLETION_TOKENS: u32 = 512;
/// Error reason when the label queries cannot be loaded at all.
pub const QUERIES_UNAVAILABLE_REASON: &str = "label queries unavailable";
/// Error reason when a mute claim cannot be logged (fail closed to error).
const LOG_FAILED_REASON: &str = "decision log append failed";
/// Error reason for media posts when the media path is unwired.
const MEDIA_UNAVAILABLE_REASON: &str =
    "media analysis unavailable (needs ANCIENTOS_MEDIA_MAX_ITEMS and a media backend)";

/// One category query from the private label-queries file (`id` and `q`;
/// other fields such as `posts` are ignored).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LabelQuery {
    pub id: String,
    pub q: String,
}

/// Shape of the label-queries file: `{"queries":[{id, q, ...}], ...}`.
/// Unknown top-level fields (`source`, `posts`) are ignored.
#[derive(Debug, Deserialize)]
struct LabelQueriesFile {
    queries: Vec<LabelQuery>,
}

/// Load and validate the label queries: non-empty file, at least one query,
/// every entry with a non-empty `id` and `q`, ids unique. Reasons are short
/// and stable across runs (no paths, no file content).
pub fn load_label_queries(path: &Path) -> Result<Vec<LabelQuery>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "label queries file missing".to_string()
        } else {
            format!("label queries file unreadable ({})", e.kind())
        }
    })?;
    let file: LabelQueriesFile = serde_json::from_str(&text).map_err(|_| {
        "label queries file is not {\"queries\":[{\"id\",\"q\"},...]} JSON".to_string()
    })?;
    if file.queries.is_empty() {
        return Err("label queries file has no queries".to_string());
    }
    let mut seen: Vec<&str> = Vec::with_capacity(file.queries.len());
    for (i, q) in file.queries.iter().enumerate() {
        if q.id.trim().is_empty() || q.q.trim().is_empty() {
            return Err(format!(
                "label queries entry {} needs a non-empty id and q",
                i + 1
            ));
        }
        if seen.contains(&q.id.as_str()) {
            return Err(format!("label queries has a duplicate id {:?}", q.id));
        }
        seen.push(q.id.as_str());
    }
    Ok(file.queries)
}

/// Label-queries path: `$ANCIENTOS_LABEL_QUERIES`, else the default
/// `eval-private/nathan/label-queries.json` under the data dir.
pub fn label_queries_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var(LABEL_QUERIES_ENV_VAR) {
        if !p.trim().is_empty() {
            return Ok(PathBuf::from(p.trim()));
        }
    }
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(LABEL_QUERIES_DEFAULT_REL_PATH))
}

/// Classify records path: `eval-private/classify.jsonl` under the data dir.
pub fn classify_path() -> Result<PathBuf, String> {
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(EVAL_PRIVATE_DIR_NAME)
        .join(CLASSIFY_FILE_NAME))
}

/// Prompt sent to the scoring model: strict JSON scores only, one per query
/// id (`0` unrelated, `1` exactly about it). `text` must already be
/// sanitized for hosted backends.
pub fn score_prompt(text: &str, queries: &[LabelQuery]) -> String {
    let mut prompt = format!(
        "You score one public social post against {} topic queries. Reply with JSON only, no prose: {{\"scores\":{{<id>:0-1, ...}}}}. Each score is how relevant the post is to the query (0 = unrelated, 1 = exactly about it).\n\nQueries:\n",
        queries.len()
    );
    for q in queries {
        prompt.push_str(&format!("{}: {}\n", q.id, q.q));
    }
    prompt.push_str("\nPost (sanitized):\n");
    prompt.push_str(text);
    prompt
}

/// Strict scores envelope: exactly the `scores` map, nothing else.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoresEnvelope {
    scores: BTreeMap<String, f64>,
}

/// Parse the scoring reply into per-query scores. Accepts a bare JSON object
/// or one wrapped in a ```json fence (like
/// [`crate::analyze_media_live::parse_findings`]); rejects prose, missing or
/// unknown query ids, and scores outside 0..=1. Reasons never echo the reply
/// (it may carry muted terms).
pub fn parse_scores(reply: &str, queries: &[LabelQuery]) -> Result<BTreeMap<String, f64>, String> {
    let mut s = reply.trim();
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        s = rest
            .strip_suffix("```")
            .ok_or("scoring reply has an unterminated code fence")?
            .trim();
    }
    let env: ScoresEnvelope = serde_json::from_str(s)
        .map_err(|_| "scoring reply is not strict scores JSON".to_string())?;
    if env.scores.len() != queries.len() || queries.iter().any(|q| !env.scores.contains_key(&q.id))
    {
        return Err("scoring reply is not strict scores JSON".to_string());
    }
    for v in env.scores.values() {
        if !v.is_finite() || !(0.0..=1.0).contains(v) {
            return Err("scoring reply has a score outside 0..=1".to_string());
        }
    }
    Ok(env.scores)
}

/// Query ids scoring at or above [`MATCH_THRESHOLD`], in queries-file order.
pub fn matched_categories(scores: &BTreeMap<String, f64>, queries: &[LabelQuery]) -> Vec<String> {
    queries
        .iter()
        .filter(|q| scores.get(&q.id).is_some_and(|s| *s >= MATCH_THRESHOLD))
        .map(|q| q.id.clone())
        .collect()
}

/// Build `x_bookmark_media`-shaped rows plus the request for one post's
/// stored media. Media keys are synthetic (`import-0`, ...): the store keeps
/// no X media keys, and [`crate::analyze_media::resolve`] only needs the
/// request and the rows to agree. Videos/GIFs map `preview_url` to the
/// poster frame and `video_url` to one mp4 variant; unknown kinds and unsafe
/// URLs are left for `resolve` to skip and log.
pub fn media_rows_for(post: &Bookmark) -> (MediaRequest, Vec<MediaRow>) {
    let mut req = MediaRequest {
        post_id: post.post_id.clone(),
        media_keys: Vec::with_capacity(post.media.len()),
    };
    let mut rows = Vec::with_capacity(post.media.len());
    for (i, m) in post.media.iter().enumerate() {
        let key = format!("import-{i}");
        req.media_keys.push(key.clone());
        rows.push(MediaRow {
            post_id: post.post_id.clone(),
            media_key: key,
            kind: m.kind.clone(),
            url: m.url.clone(),
            preview_image_url: m.preview_url.clone(),
            variants: m
                .video_url
                .clone()
                .map(|u| {
                    vec![Variant {
                        url: u,
                        content_type: Some("video/mp4".to_string()),
                        bit_rate: None,
                    }]
                })
                .unwrap_or_default(),
            width: None,
            height: None,
            duration_ms: None,
            alt_text: m.alt_text.clone(),
        });
    }
    (req, rows)
}

/// Outcome of classifying one post.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifyStatus {
    /// Scores (and, for media posts, findings) recorded. Terminal: the post
    /// is never re-classified or re-billed.
    Classified,
    /// Paid calls are off: nothing billed, nothing dropped. Retried on the
    /// next import.
    Pending,
    /// A muted term matched the post text (pre-check) or the model output
    /// (post-check), or the mute config was missing/unreadable. No scores
    /// are kept. Re-evaluated on the next import (the pre-check is free).
    Muted,
    /// The post could not be classified (missing queries, refused egress,
    /// model error, parse error, unwired media path). Retried on the next
    /// import.
    Error,
}

/// Persisted per-post classification record: one JSONL line in
/// `eval-private/classify.jsonl`, keyed by `post_id` (last wins). Scores and
/// findings are present only on [`ClassifyStatus::Classified`]; `muted`,
/// `muted_findings`, and `mute_check_failed` mirror
/// [`crate::analyze_media::Report`] (a classified post may still have muted
/// findings); `error` carries a content-free reason on
/// [`ClassifyStatus::Error`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifyRecord {
    pub post_id: String,
    pub status: ClassifyStatus,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scores: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// True when any mute check fired for this post (pre-check match,
    /// post-check match, findings dropped, or config unreadable).
    #[serde(default)]
    pub muted: bool,
    /// Findings dropped by the post-model mute check.
    #[serde(default)]
    pub muted_findings: usize,
    /// True when the mute list could not be read, so output was withheld.
    #[serde(default)]
    pub mute_check_failed: bool,
    /// Content-free failure reason (error status only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Fresh record shell for `post_id` with `status` and empty details.
fn record_shell(post_id: &str, status: ClassifyStatus) -> ClassifyRecord {
    ClassifyRecord {
        post_id: post_id.to_string(),
        status,
        scores: BTreeMap::new(),
        matched: Vec::new(),
        findings: Vec::new(),
        muted: false,
        muted_findings: 0,
        mute_check_failed: false,
        error: None,
    }
}

/// Append-only classify store: `post_id` to the latest [`ClassifyRecord`]
/// (last wins). The file is created with mode 0600 under a 0700 parent and
/// is never rewritten or truncated.
#[derive(Debug, Default)]
pub struct ClassifyStore {
    by_id: BTreeMap<String, ClassifyRecord>,
}

impl ClassifyStore {
    /// Load from an explicit JSONL path. A missing file means an empty store;
    /// any other read error or a malformed line fails closed (the run must
    /// not proceed when idempotency is unprovable).
    pub fn load_from(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("classify store unreadable ({})", e.kind())),
            Ok(text) => text,
        };
        let mut store = Self::default();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record: ClassifyRecord = serde_json::from_str(line)
                .map_err(|_| format!("classify store line {} is not a record", i + 1))?;
            store.by_id.insert(record.post_id.clone(), record);
        }
        Ok(store)
    }

    /// Latest record for `post_id`, if any.
    pub fn latest(&self, post_id: &str) -> Option<&ClassifyRecord> {
        self.by_id.get(post_id)
    }

    /// Number of posts with a record.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// True when no post has a record.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Append `record` unless the latest record for the post is identical.
    /// Returns true when a line was appended. Creates missing parents (0700)
    /// and enforces mode 0600 on the file.
    pub fn append_to(&mut self, path: &Path, record: &ClassifyRecord) -> Result<bool, String> {
        if self.by_id.get(&record.post_id) == Some(record) {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("classify dir failed: {e}"))?;
        }
        append_line_0600(path, record)?;
        self.by_id.insert(record.post_id.clone(), record.clone());
        Ok(true)
    }
}

/// Append one record line to `path` (created 0600; the mode is enforced on
/// every append, like [`crate::token_store`]).
fn append_line_0600(path: &Path, record: &ClassifyRecord) -> Result<(), String> {
    use std::io::Write;
    let line = serde_json::to_string(record).map_err(|e| format!("classify encode failed: {e}"))?;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| format!("classify append failed: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("classify append failed: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("classify append failed: {e}"))?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("classify append failed: {e}"))?;
    Ok(())
}

/// Scores one post: sanitized prompt in, raw model reply out. Parsing and
/// the post-model mute check happen in [`classify_post`], not here.
pub trait ScoreBackend {
    /// Score `prompt` (already sanitized for hosted backends) under `class`.
    fn score(&self, class: DataClass, prompt: &str) -> Result<String, String>;
}

/// Muse Spark scoring backend (hosted, paid). Holds no secrets.
pub struct MuseScoreBackend {
    client: MuseClient,
}

impl MuseScoreBackend {
    /// Build from env with the classify token cap and low reasoning effort.
    /// Construction reads env only and never touches the network.
    pub fn from_env() -> Self {
        Self {
            client: MuseClient::from_env()
                .with_max_completion_tokens(SCORE_MAX_COMPLETION_TOKENS)
                .with_reasoning_effort("low"),
        }
    }

    /// Wrap an explicit client (callers that already read env).
    pub fn new(client: MuseClient) -> Self {
        Self { client }
    }
}

impl ScoreBackend for MuseScoreBackend {
    /// One paid call; re-checks the class without logging (the routing
    /// decision was already logged by [`check_egress`] in [`classify_post`]).
    fn score(&self, class: DataClass, prompt: &str) -> Result<String, String> {
        self.client
            .chat_with_class(class, CALLER, None, prompt)
            .map(|reply| reply.content)
            .map_err(|e| e.to_string())
    }
}

/// Test/offline scoring backend: canned outcomes in call order, records
/// prompts. Panics when called with no outcome left (fail loud in tests,
/// never silent wrong data).
#[derive(Debug, Default)]
pub struct MockScoreBackend {
    outcomes: RefCell<VecDeque<Result<String, String>>>,
    seen: RefCell<Vec<String>>,
}

impl MockScoreBackend {
    /// Canned successful replies in call order.
    pub fn with_replies(replies: Vec<String>) -> Self {
        Self::with_outcomes(replies.into_iter().map(Ok).collect())
    }

    /// Canned outcomes (`Ok` reply / `Err` model error) in call order.
    pub fn with_outcomes(outcomes: Vec<Result<String, String>>) -> Self {
        Self {
            outcomes: RefCell::new(outcomes.into()),
            seen: RefCell::new(Vec::new()),
        }
    }

    /// Prompts received, in call order.
    pub fn seen(&self) -> Vec<String> {
        self.seen.borrow().clone()
    }

    /// Number of calls made.
    pub fn calls(&self) -> usize {
        self.seen.borrow().len()
    }
}

impl ScoreBackend for MockScoreBackend {
    /// Record the prompt and return the next canned outcome.
    fn score(&self, _class: DataClass, prompt: &str) -> Result<String, String> {
        self.seen.borrow_mut().push(prompt.to_string());
        self.outcomes
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| panic!("mock score backend called with no outcome left"))
    }
}

/// Gates and model callers for one classify run.
pub struct ClassifyGates<'a> {
    /// Paid switch; false records [`ClassifyStatus::Pending`] with no calls.
    pub allow_paid: bool,
    /// Scoring backend; required when `allow_paid`.
    pub score: Option<&'a dyn ScoreBackend>,
    /// Media backend; required when `allow_paid` and the post has media.
    pub media_backend: Option<&'a dyn MediaBackend>,
    /// Paid cap for the media path; required with `media_backend`.
    pub media_paid: Option<PaidConfig>,
}

/// Append a counts-only mute entry: post id and stage only, never post text,
/// scores, findings, or mute terms.
fn log_muted(
    log: &mut DecisionLog,
    post_id: &str,
    stage: &str,
    check_failed: bool,
) -> Result<(), String> {
    log.append(
        MUTED_KIND,
        serde_json::json!({
            "post_id": post_id,
            "stage": stage,
            "check_failed": check_failed,
        }),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Append a content-free error entry. Reasons never echo model output.
fn log_error(log: &mut DecisionLog, post_id: &str, reason: &str) -> Result<(), String> {
    log.append(
        ERROR_KIND,
        serde_json::json!({"post_id": post_id, "reason": reason}),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Error record for `post_id` with a content-free `reason`, best-effort
/// logged (the record itself is the error report).
fn error_record(post_id: &str, reason: String, log: &mut DecisionLog) -> ClassifyRecord {
    let _ = log_error(log, post_id, &reason);
    ClassifyRecord {
        error: Some(reason),
        ..record_shell(post_id, ClassifyStatus::Error)
    }
}

/// Muted record for `post_id`: no scores kept. The mute claim is logged, or
/// fails closed to an error record when the log write fails.
fn muted_record(
    post_id: &str,
    stage: &str,
    check_failed: bool,
    log: &mut DecisionLog,
) -> ClassifyRecord {
    if log_muted(log, post_id, stage, check_failed).is_err() {
        return error_record(post_id, LOG_FAILED_REASON.to_string(), log);
    }
    ClassifyRecord {
        muted: true,
        mute_check_failed: check_failed,
        ..record_shell(post_id, ClassifyStatus::Muted)
    }
}

/// Classify one post. Never fails: every outcome (including config and model
/// failures) is a [`ClassifyRecord`], so the import never crashes.
///
/// Order: mute pre-check (muted posts make no model call), paid gate
/// (pending posts make no call), media-path availability (free, before any
/// billed call), egress/class gate (logged), one scoring call over sanitized
/// text, mute post-check on the raw reply (scores withheld on a match), strict
/// parse, then `analyze_media` for posts with stored media (its own evidence
/// and post-model mute checks apply; per-item backend errors degrade to
/// skips, not post errors).
pub fn classify_post(
    post: &Bookmark,
    queries: &[LabelQuery],
    gates: &ClassifyGates<'_>,
    log: &mut DecisionLog,
) -> ClassifyRecord {
    if queries.is_empty() {
        return error_record(
            post.post_id.as_str(),
            QUERIES_UNAVAILABLE_REASON.into(),
            log,
        );
    }
    // Mute pre-check, before any model call. A missing or unreadable config
    // fails closed (no call, output would be withheld anyway).
    let mute = match crate::topic_mute::configured() {
        Ok(Some(list)) => list,
        Ok(None) | Err(_) => {
            return muted_record(&post.post_id, "pre", true, log);
        }
    };
    if mute.is_muted(post, &[]) {
        return muted_record(&post.post_id, "pre", false, log);
    }
    if !gates.allow_paid {
        return record_shell(&post.post_id, ClassifyStatus::Pending);
    }
    let Some(score_backend) = gates.score else {
        return error_record(
            &post.post_id,
            "scoring backend unavailable although paid calls are on".to_string(),
            log,
        );
    };
    // Media availability is free to check: a media post that could not run
    // findings must not bill a scoring call first.
    if !post.media.is_empty() && (gates.media_backend.is_none() || gates.media_paid.is_none()) {
        return error_record(&post.post_id, MEDIA_UNAVAILABLE_REASON.to_string(), log);
    }
    // Egress gate over the ORIGINAL text (sanitized text would hide emails
    // and phones); the decision is logged, denied or allowed.
    let mut classes = vec![crate::translate::class_of(&post.text)];
    if let Some(quoted) = post.quoted_text.as_deref() {
        classes.push(crate::translate::class_of(quoted));
    }
    let class = max_of(classes);
    if let Err(e) = check_egress(class, Route::MuseSpark, CALLER, Some(log)) {
        return error_record(&post.post_id, format!("hosted scoring refused: {e}"), log);
    }
    let mut text = crate::sanitize::sanitize_uncapped(&post.text);
    if let Some(quoted) = post.quoted_text.as_deref().filter(|s| !s.trim().is_empty()) {
        text.push_str("\nQuoted:\n");
        text.push_str(&crate::sanitize::sanitize_uncapped(quoted));
    }
    let reply = match score_backend.score(class, &score_prompt(&text, queries)) {
        Ok(reply) => reply,
        Err(e) => return error_record(&post.post_id, format!("scoring model error: {e}"), log),
    };
    // Mute post-check on the raw reply: a match withholds the scores, which
    // are never parsed.
    if mute.matches_any(&[reply.as_str()]) {
        return muted_record(&post.post_id, "post", false, log);
    }
    let scores = match parse_scores(&reply, queries) {
        Ok(scores) => scores,
        Err(reason) => return error_record(&post.post_id, reason, log),
    };
    let matched = matched_categories(&scores, queries);
    let (findings, muted, muted_findings, mute_check_failed) = if post.media.is_empty() {
        (Vec::new(), false, 0, false)
    } else if let (Some(backend), Some(paid)) = (gates.media_backend, gates.media_paid) {
        let (req, rows) = media_rows_for(post);
        match crate::analyze_media::analyze(log, &req, &rows, backend, paid) {
            Ok(report) => (
                report.accepted,
                report.muted,
                report.muted_findings,
                report.mute_check_failed,
            ),
            Err(e) => {
                return error_record(&post.post_id, format!("media analysis failed: {e}"), log)
            }
        }
    } else {
        // Unreachable: the availability check above returned already. Fail
        // closed without panicking in case the checks ever diverge.
        return error_record(&post.post_id, MEDIA_UNAVAILABLE_REASON.to_string(), log);
    };
    ClassifyRecord {
        scores,
        matched,
        findings,
        muted,
        muted_findings,
        mute_check_failed,
        ..record_shell(&post.post_id, ClassifyStatus::Classified)
    }
}

/// Outcome counts for one classify run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ClassifyReport {
    /// Posts scanned.
    pub total: usize,
    /// Posts newly classified.
    pub classified: usize,
    /// Posts recorded pending (paid off).
    pub pending: usize,
    /// Posts recorded muted.
    pub muted: usize,
    /// Posts recorded error.
    pub errors: usize,
    /// Posts skipped as already classified (never re-billed).
    pub skipped: usize,
    /// Lines appended to the classify file (unchanged outcomes append nothing).
    pub appended: usize,
}

/// Classify every post in `posts` that has no classified record yet, appending
/// changed outcomes to `classify_path`. A missing or unparseable queries file
/// records a per-post error (never classified, never billed). Returns `Err`
/// with no model calls and no records when the classify file exists but
/// cannot be read: idempotency is unprovable, so fail closed.
pub fn classify_new_posts(
    posts: &[Bookmark],
    queries_path: &Path,
    classify_path: &Path,
    gates: &ClassifyGates<'_>,
    log: &mut DecisionLog,
) -> Result<ClassifyReport, String> {
    let mut store = ClassifyStore::load_from(classify_path)?;
    let queries = load_label_queries(queries_path);
    let mut report = ClassifyReport {
        total: posts.len(),
        ..ClassifyReport::default()
    };
    for post in posts {
        if store
            .latest(&post.post_id)
            .is_some_and(|r| r.status == ClassifyStatus::Classified)
        {
            report.skipped += 1;
            continue;
        }
        let record = match &queries {
            Ok(loaded) => classify_post(post, loaded, gates, log),
            Err(reason) => error_record(&post.post_id, reason.clone(), log),
        };
        match record.status {
            ClassifyStatus::Classified => report.classified += 1,
            ClassifyStatus::Pending => report.pending += 1,
            ClassifyStatus::Muted => report.muted += 1,
            ClassifyStatus::Error => report.errors += 1,
        }
        if store.append_to(classify_path, &record)? {
            report.appended += 1;
        }
    }
    Ok(report)
}

/// Classify-after-import entry point for `sync`: default paths, live backends
/// when the paid switch is on (Muse Spark scoring plus the
/// `ANCIENTOS_MEDIA_BACKEND` media backend), no backends when off. Returns
/// the run report; setup failures (no home, unreadable store or classify
/// file, unopenable log) are `Err`, and the caller must keep the import
/// successful (warn only, never fail the sync).
pub fn classify_after_import() -> Result<ClassifyReport, String> {
    let allow_paid = crate::translate::is_paid_allowed();
    let store = crate::ingest::Store::load()?;
    let posts: Vec<Bookmark> = store.by_id.into_values().collect();
    let queries_path = label_queries_path()?;
    let path = classify_path()?;
    let log_path = crate::brief::decision_log_path()?;
    let mut log = DecisionLog::open_with_key_env(&log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    if !allow_paid {
        let gates = ClassifyGates {
            allow_paid: false,
            score: None,
            media_backend: None,
            media_paid: None,
        };
        return classify_new_posts(&posts, &queries_path, &path, &gates, &mut log);
    }
    let score_backend = MuseScoreBackend::from_env();
    let media: Option<(Box<dyn MediaBackend>, PaidConfig)> =
        match crate::analyze_media::PaidConfig::from_env() {
            Ok(paid) => match crate::analyze_media_cli::live_backend_from_env(paid) {
                Ok(backend) => Some((backend, paid)),
                Err(_) => None,
            },
            Err(_) => None,
        };
    let gates = ClassifyGates {
        allow_paid: true,
        score: Some(&score_backend),
        media_backend: media.as_ref().map(|(b, _)| b.as_ref()),
        media_paid: media.as_ref().map(|(_, paid)| *paid),
    };
    classify_new_posts(&posts, &queries_path, &path, &gates, &mut log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze_media::{FindingKind, RawFinding};

    /// Test helper: plain synthetic bookmark.
    fn bm(id: &str, text: &str) -> Bookmark {
        Bookmark {
            quoted_text: None,
            x_bookmark_present: None,
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: text.into(),
            author_username: None,
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: None,
            urls: Vec::new(),
        }
    }

    /// Test helper: synthetic bookmark with one stored photo.
    fn bm_photo(id: &str, text: &str) -> Bookmark {
        let mut b = bm(id, text);
        b.media = vec![crate::media::Media {
            kind: "photo".into(),
            url: Some("https://pbs.twimg.com/media/synthetic_a.jpg".into()),
            preview_url: None,
            video_url: None,
            alt_text: Some("synthetic chart".into()),
            caption_url: None,
        }];
        b
    }

    /// Test helper: three synthetic label queries (same shape as q1-q5).
    fn test_queries() -> Vec<LabelQuery> {
        vec![
            LabelQuery {
                id: "q1".into(),
                q: "synthetic Rust tooling".into(),
            },
            LabelQuery {
                id: "q2".into(),
                q: "synthetic sleep tips".into(),
            },
            LabelQuery {
                id: "q3".into(),
                q: "synthetic note apps".into(),
            },
        ]
    }

    /// Test helper: scores reply JSON for the test queries.
    fn scores_reply(q1: f64, q2: f64, q3: f64) -> String {
        format!("{{\"scores\":{{\"q1\":{q1},\"q2\":{q2},\"q3\":{q3}}}}}")
    }

    /// Test helper: temp decision log.
    fn open_log() -> (tempfile::TempDir, DecisionLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path().join("log.jsonl")).unwrap();
        (dir, log)
    }

    /// Test helper: write a label-queries file, return its path.
    fn write_queries(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("label-queries.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Test helper: label-queries file body with the test queries plus the
    /// real file's extra fields (`source`, `posts` per query, `posts` map).
    fn queries_body() -> String {
        serde_json::json!({
            "source": "synthetic",
            "queries": [
                {"id": "q1", "q": "synthetic Rust tooling", "posts": [1]},
                {"id": "q2", "q": "synthetic sleep tips", "posts": [2]},
                {"id": "q3", "q": "synthetic note apps", "posts": [3]},
            ],
            "posts": {"1": {"a": "syn_a", "t": "synthetic", "u": "https://x.com/i/status/1"}},
        })
        .to_string()
    }

    /// Test helper: paid gates over a mock scorer, no media path.
    fn paid_gates<'a>(score: &'a dyn ScoreBackend) -> ClassifyGates<'a> {
        ClassifyGates {
            allow_paid: true,
            score: Some(score),
            media_backend: None,
            media_paid: None,
        }
    }

    /// Test helper: mock media backend returning one evidenced text finding.
    fn mock_media() -> crate::analyze_media::MockBackend {
        crate::analyze_media::MockBackend {
            findings: vec![RawFinding {
                kind: FindingKind::Text,
                text: "[synthetic] 10k steps".into(),
                quote: Some("10k".into()),
                region: None,
            }],
        }
    }

    /// Test helper: media backend that fails the test if called.
    struct Panics;
    impl MediaBackend for Panics {
        /// Fails the test if called.
        fn analyze(
            &self,
            _item: &crate::analyze_media::ResolvedMedia,
        ) -> Result<Vec<RawFinding>, String> {
            panic!("media backend must not be called")
        }
    }

    /// Test helper: log entry kinds in order.
    fn kinds(log: &DecisionLog) -> Vec<String> {
        log.verify_or_empty()
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    /// Queries load with the real file's extra fields ignored; every
    /// malformed shape is refused with a stable reason.
    #[test]
    fn queries_parse_ignores_extra_fields_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_queries(&dir, &queries_body());
        let q = load_label_queries(&path).unwrap();
        assert_eq!(q, test_queries());
        assert!(load_label_queries(&dir.path().join("missing.json")).is_err());
        for (name, body) in [
            ("bad json", "{oops"),
            ("no queries key", "{\"source\":\"synthetic\"}"),
            ("empty list", "{\"queries\":[]}"),
            ("blank id", "{\"queries\":[{\"id\":\" \",\"q\":\"x\"}]}"),
            ("blank q", "{\"queries\":[{\"id\":\"q1\",\"q\":\"  \"}]}"),
            (
                "duplicate id",
                "{\"queries\":[{\"id\":\"q1\",\"q\":\"a\"},{\"id\":\"q1\",\"q\":\"b\"}]}",
            ),
        ] {
            let p = write_queries(&dir, body);
            assert!(load_label_queries(&p).is_err(), "{name}");
        }
    }

    /// The prompt lists every query id with its text plus the post text.
    #[test]
    fn score_prompt_lists_queries_and_text() {
        let p = score_prompt("synthetic post", &test_queries());
        assert!(p.contains("JSON only"), "{p}");
        for q in test_queries() {
            assert!(p.contains(&q.id), "{p}");
            assert!(p.contains(&q.q), "{p}");
        }
        assert!(p.contains("synthetic post"), "{p}");
    }

    /// Bare and fenced scores JSON both parse to the per-query map.
    #[test]
    fn parse_scores_accepts_bare_and_fenced_json() {
        let queries = test_queries();
        let bare = scores_reply(0.9, 0.5, 0.0);
        let fenced = format!("```json\n{bare}\n```");
        for reply in [bare, fenced] {
            let s = parse_scores(&reply, &queries).unwrap();
            assert_eq!(s.len(), 3);
            assert_eq!(s["q1"], 0.9);
            assert_eq!(s["q2"], 0.5);
            assert_eq!(s["q3"], 0.0);
        }
    }

    /// Prose, fences without closers, missing or unknown ids, extra envelope
    /// fields, and out-of-range scores are all refused without echoing the
    /// reply (it may carry muted terms).
    #[test]
    fn parse_scores_rejects_prose_missing_unknown_and_out_of_range() {
        let queries = test_queries();
        for reply in [
            "q1 looks relevant".to_string(),
            "```json\n{\"scores\":{\"q1\":0.9}}".to_string(),
            "{\"scores\":{\"q1\":0.9,\"q2\":0.1}}".to_string(),
            "{\"scores\":{\"q1\":0.9,\"q2\":0.1,\"q3\":0.2,\"q4\":0.0}}".to_string(),
            "{\"scores\":{\"q1\":0.9,\"q2\":0.1,\"q3\":0.2},\"note\":\"hi\"}".to_string(),
            scores_reply(1.5, 0.1, 0.2),
            scores_reply(0.9, -0.1, 0.2),
        ] {
            let err = parse_scores(&reply, &queries).unwrap_err();
            assert!(!err.contains("looks relevant"), "{reply}: {err}");
            assert!(!err.contains("1.5"), "{reply}: {err}");
        }
    }

    /// The named threshold decides matches: 0.5 matches, below does not, and
    /// order follows the queries file (not the score map).
    #[test]
    fn matched_categories_uses_named_threshold_in_query_order() {
        assert_eq!(MATCH_THRESHOLD, 0.5);
        let queries = test_queries();
        let scores: BTreeMap<String, f64> = [("q3", 0.99), ("q1", 0.5), ("q2", 0.499)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        assert_eq!(
            matched_categories(&scores, &queries),
            vec!["q1".to_string(), "q3".to_string()]
        );
    }

    /// Stored photo/video media maps to import-derived rows the existing
    /// `resolve` accepts: photo url, video poster frame, mp4 variant.
    #[test]
    fn media_rows_for_maps_stored_media_and_resolves() {
        let mut post = bm("m1", "synthetic with media");
        post.media = vec![
            crate::media::Media {
                kind: "photo".into(),
                url: Some("https://pbs.twimg.com/media/synthetic_a.jpg".into()),
                preview_url: None,
                video_url: None,
                alt_text: Some("synthetic chart".into()),
                caption_url: None,
            },
            crate::media::Media {
                kind: "video".into(),
                url: None,
                preview_url: Some("https://pbs.twimg.com/media/synthetic_thumb.jpg".into()),
                video_url: Some("https://video.twimg.com/synthetic/v.mp4".into()),
                alt_text: None,
                caption_url: None,
            },
        ];
        let (req, rows) = media_rows_for(&post);
        assert_eq!(req.post_id, "m1");
        assert_eq!(req.media_keys, vec!["import-0", "import-1"]);
        assert_eq!(rows[1].variants.len(), 1);
        assert_eq!(
            rows[1].variants[0].content_type.as_deref(),
            Some("video/mp4")
        );
        let (ok, skipped) = crate::analyze_media::resolve(&req, &rows);
        assert_eq!((ok.len(), skipped.len()), (2, 0));
        assert_eq!(ok[0].provenance.frame_idx, None);
        assert_eq!(ok[1].provenance.frame_idx, Some(0));
    }

    /// A muted post (text match, pre-check) makes no model call at all: no
    /// scoring call and no media call even though the post has media. The
    /// mute claim is logged counts-only.
    #[test]
    fn pre_check_muted_post_makes_no_model_call() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (_d, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![]);
        let media = Panics;
        let gates = ClassifyGates {
            allow_paid: true,
            score: Some(&score),
            media_backend: Some(&media),
            media_paid: Some(PaidConfig { max_items: 5 }),
        };
        let post = bm_photo("p1", "synthetic post about ZORBLAX tooling");
        let r = classify_post(&post, &test_queries(), &gates, &mut log);
        assert_eq!(r.status, ClassifyStatus::Muted);
        assert!(r.muted && !r.mute_check_failed);
        assert!(r.scores.is_empty() && r.matched.is_empty() && r.findings.is_empty());
        assert_eq!(score.calls(), 0);
        assert_eq!(kinds(&log), vec![MUTED_KIND.to_string()]);
        let entry = &log.verify_or_empty().unwrap()[0];
        assert_eq!(entry.payload["stage"], "pre");
        assert!(!entry.payload.to_string().to_lowercase().contains("zorblax"));
    }

    /// Holds the crate-wide env lock with the mute-config vars cleared,
    /// restoring them on drop (EnvGuard pattern).
    struct MuteEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl MuteEnvGuard {
        /// Test helper: clear the mute-config vars under the crate env lock.
        fn cleared() -> Self {
            let lock = crate::test_support::lock_env();
            let keys = [
                crate::topic_mute::TERMS_ENV_VAR,
                crate::topic_mute::TERMS_FILE_ENV_VAR,
            ];
            let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for MuteEnvGuard {
        /// Test helper: restore the cleared vars and release the lock.
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// A missing mute config fails closed with no model call: scores are
    /// withheld and the post is flagged `mute_check_failed` (same shape as
    /// the post-model check), before any billing.
    #[test]
    fn missing_mute_config_fails_closed_without_model_call() {
        let _env = MuteEnvGuard::cleared();
        let (_d, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![]);
        let gates = paid_gates(&score);
        let r = classify_post(
            &bm("p1", "synthetic plain post"),
            &test_queries(),
            &gates,
            &mut log,
        );
        assert_eq!(r.status, ClassifyStatus::Muted);
        assert!(r.muted && r.mute_check_failed);
        assert!(r.scores.is_empty());
        assert_eq!(score.calls(), 0);
        let entry = &log.verify_or_empty().unwrap()[0];
        assert_eq!(entry.kind, MUTED_KIND);
        assert_eq!(entry.payload["check_failed"], true);
    }

    /// With the paid switch off the post is recorded pending, not dropped:
    /// no model call, no log entry, empty details.
    #[test]
    fn paid_off_records_pending_without_model_call() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (_d, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![]);
        let gates = ClassifyGates {
            allow_paid: false,
            score: Some(&score),
            media_backend: None,
            media_paid: None,
        };
        let r = classify_post(
            &bm("p1", "synthetic plain post"),
            &test_queries(),
            &gates,
            &mut log,
        );
        assert_eq!(r.status, ClassifyStatus::Pending);
        assert!(r.scores.is_empty() && r.error.is_none() && !r.muted);
        assert_eq!(score.calls(), 0);
        assert!(log.verify_or_empty().unwrap().is_empty());
    }

    /// Non-public content never reaches the hosted scorer: the egress gate
    /// refuses (and logs the denial) before any model call.
    #[test]
    fn non_public_content_refused_before_model_call() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (_d, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![]);
        let gates = paid_gates(&score);
        let post = bm(
            "p1",
            "synthetic post by syn-test@example.invalid about tooling",
        );
        let r = classify_post(&post, &test_queries(), &gates, &mut log);
        assert_eq!(r.status, ClassifyStatus::Error);
        assert!(r.scores.is_empty());
        assert!(
            r.error.as_deref().unwrap().contains("refused"),
            "{:?}",
            r.error
        );
        assert_eq!(score.calls(), 0);
        assert_eq!(
            kinds(&log),
            vec![
                ancientos_router::data_class::EGRESS_DENIED_KIND.to_string(),
                ERROR_KIND.to_string(),
            ]
        );
    }

    /// Paid on but no scoring backend wired is a config error, not a silent
    /// skip: an error state is recorded.
    #[test]
    fn scoring_backend_missing_with_paid_on_records_error() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (_d, mut log) = open_log();
        let gates = ClassifyGates {
            allow_paid: true,
            score: None,
            media_backend: None,
            media_paid: None,
        };
        let r = classify_post(
            &bm("p1", "synthetic plain post"),
            &test_queries(),
            &gates,
            &mut log,
        );
        assert_eq!(r.status, ClassifyStatus::Error);
        assert!(r.error.is_some());
    }

    /// A media post with no media path wired errors before any billed call;
    /// a plain post in the same run still classifies (per-post isolation).
    #[test]
    fn media_unavailable_errors_media_posts_before_scoring() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (_d, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![scores_reply(0.9, 0.1, 0.2)]);
        let gates = paid_gates(&score);
        let media_post = bm_photo("m1", "synthetic post with a photo");
        let r = classify_post(&media_post, &test_queries(), &gates, &mut log);
        assert_eq!(r.status, ClassifyStatus::Error);
        assert_eq!(r.error.as_deref(), Some(MEDIA_UNAVAILABLE_REASON));
        assert_eq!(score.calls(), 0);
        let plain = bm("p1", "synthetic plain post");
        let r = classify_post(&plain, &test_queries(), &gates, &mut log);
        assert_eq!(r.status, ClassifyStatus::Classified);
        assert_eq!(score.calls(), 1);
    }

    /// A model error records an error state: never classified, reason kept,
    /// scores empty.
    #[test]
    fn model_error_records_error_state() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (_d, mut log) = open_log();
        let score = MockScoreBackend::with_outcomes(vec![Err("synthetic outage".to_string())]);
        let gates = paid_gates(&score);
        let r = classify_post(
            &bm("p1", "synthetic plain post"),
            &test_queries(),
            &gates,
            &mut log,
        );
        assert_eq!(r.status, ClassifyStatus::Error);
        assert!(r.scores.is_empty() && r.matched.is_empty());
        assert!(
            r.error.as_deref().unwrap().contains("model error"),
            "{:?}",
            r.error
        );
        assert_eq!(kinds(&log).last().map(String::as_str), Some(ERROR_KIND));
    }

    /// An unparseable reply records an error state without echoing the reply
    /// into the record or the log (it may carry muted terms).
    #[test]
    fn parse_error_records_error_state_without_echoing_reply() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let reply = "here are my scores, roughly zorblax-ish";
        let score = MockScoreBackend::with_replies(vec![reply.to_string()]);
        let gates = paid_gates(&score);
        // The reply mentions the muted term, so this run mutes post-call...
        let r = classify_post(
            &bm("p1", "synthetic plain post"),
            &test_queries(),
            &gates,
            &mut log,
        );
        assert_eq!(r.status, ClassifyStatus::Muted);
        // ...while a term-free but unparseable reply records a parse error
        // whose reason never echoes the reply, in the record or the log.
        let score = MockScoreBackend::with_replies(vec!["just prose, no JSON".to_string()]);
        let gates = paid_gates(&score);
        let r = classify_post(
            &bm("p2", "synthetic plain post"),
            &test_queries(),
            &gates,
            &mut log,
        );
        assert_eq!(r.status, ClassifyStatus::Error);
        assert!(
            !r.error.as_deref().unwrap().contains("prose"),
            "{:?}",
            r.error
        );
        let text = std::fs::read_to_string(dir.path().join("log.jsonl")).unwrap();
        assert!(!text.contains("prose"), "{text}");
        assert!(!text.to_lowercase().contains("zorblax"), "{text}");
    }

    /// A muted term in the model output withholds the scores after the call
    /// (one call made): never parsed, never stored, media never runs, the
    /// post is flagged muted.
    #[test]
    fn post_check_muted_output_withholds_scores() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (_d, mut log) = open_log();
        let reply = format!("{}\n[synthetic] zorblax note", scores_reply(0.9, 0.9, 0.9));
        let score = MockScoreBackend::with_replies(vec![reply]);
        let media = Panics;
        let gates = ClassifyGates {
            allow_paid: true,
            score: Some(&score),
            media_backend: Some(&media),
            media_paid: Some(PaidConfig { max_items: 5 }),
        };
        let post = bm_photo("p1", "synthetic plain post with a photo");
        let r = classify_post(&post, &test_queries(), &gates, &mut log);
        assert_eq!(r.status, ClassifyStatus::Muted);
        assert!(r.muted && !r.mute_check_failed);
        assert!(r.scores.is_empty() && r.matched.is_empty() && r.findings.is_empty());
        assert_eq!(score.calls(), 1);
        let entries = log.verify_or_empty().unwrap();
        let mute = entries.iter().find(|e| e.kind == MUTED_KIND).unwrap();
        assert_eq!(mute.payload["stage"], "post");
    }

    /// Happy path with media: scores plus threshold matches persist, the
    /// prompt carries only sanitized text, findings come from the existing
    /// media path, and the allowed egress is logged.
    #[test]
    fn classified_post_persists_scores_matched_and_findings() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![scores_reply(0.9, 0.5, 0.49)]);
        let media = mock_media();
        let gates = ClassifyGates {
            allow_paid: true,
            score: Some(&score),
            media_backend: Some(&media),
            media_paid: Some(PaidConfig { max_items: 5 }),
        };
        let post = bm_photo(
            "p1",
            "synthetic Rust tooling post https://example.invalid/x by @syn_handle",
        );
        let queries_path = write_queries(&dir, &queries_body());
        let classify_path = dir.path().join("eval-private").join(CLASSIFY_FILE_NAME);
        let report = classify_new_posts(
            std::slice::from_ref(&post),
            &queries_path,
            &classify_path,
            &gates,
            &mut log,
        )
        .unwrap();
        assert_eq!(
            report,
            ClassifyReport {
                total: 1,
                classified: 1,
                pending: 0,
                muted: 0,
                errors: 0,
                skipped: 0,
                appended: 1,
            }
        );
        let prompt = &score.seen()[0];
        assert!(!prompt.contains("example.invalid"), "{prompt}");
        assert!(!prompt.contains("@syn_handle"), "{prompt}");
        assert!(prompt.contains("synthetic Rust tooling post"), "{prompt}");
        let back = ClassifyStore::load_from(&classify_path).unwrap();
        let r = back.latest("p1").unwrap();
        assert_eq!(r.status, ClassifyStatus::Classified);
        assert_eq!(r.scores.len(), 3);
        assert_eq!(r.matched, vec!["q1".to_string(), "q2".to_string()]);
        assert_eq!(r.findings.len(), 1);
        assert!(!r.muted && !r.mute_check_failed);
        assert_eq!(
            kinds(&log),
            vec![ancientos_router::data_class::EGRESS_ALLOWED_KIND.to_string()]
        );
    }

    /// A re-import never re-classifies or re-bills a classified post: the
    /// second run skips it with no model call and no new line.
    #[test]
    fn idempotent_rerun_never_rebills_classified() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![scores_reply(0.9, 0.1, 0.2)]);
        let gates = paid_gates(&score);
        let queries_path = write_queries(&dir, &queries_body());
        let classify_path = dir.path().join(CLASSIFY_FILE_NAME);
        let posts = vec![bm("p1", "synthetic plain post")];
        let first =
            classify_new_posts(&posts, &queries_path, &classify_path, &gates, &mut log).unwrap();
        assert_eq!((first.classified, first.appended), (1, 1));
        let second =
            classify_new_posts(&posts, &queries_path, &classify_path, &gates, &mut log).unwrap();
        assert_eq!(second.skipped, 1);
        assert_eq!(second.classified, 0);
        assert_eq!(second.appended, 0);
        assert_eq!(score.calls(), 1);
        let lines = std::fs::read_to_string(&classify_path)
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, 1);
    }

    /// A pending post is retried on the next import: paid off appends
    /// pending, paid on appends classified (last wins), and a third run
    /// skips without duplicating.
    #[test]
    fn pending_then_paid_classifies_without_duplicates() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let queries_path = write_queries(&dir, &queries_body());
        let classify_path = dir.path().join(CLASSIFY_FILE_NAME);
        let posts = vec![bm("p1", "synthetic plain post")];
        let off = ClassifyGates {
            allow_paid: false,
            score: None,
            media_backend: None,
            media_paid: None,
        };
        let r = classify_new_posts(&posts, &queries_path, &classify_path, &off, &mut log).unwrap();
        assert_eq!((r.pending, r.appended), (1, 1));
        let score = MockScoreBackend::with_replies(vec![scores_reply(0.9, 0.1, 0.2)]);
        let on = paid_gates(&score);
        let r = classify_new_posts(&posts, &queries_path, &classify_path, &on, &mut log).unwrap();
        assert_eq!((r.classified, r.appended), (1, 1));
        let back = ClassifyStore::load_from(&classify_path).unwrap();
        assert_eq!(
            back.latest("p1").unwrap().status,
            ClassifyStatus::Classified
        );
        let r = classify_new_posts(&posts, &queries_path, &classify_path, &on, &mut log).unwrap();
        assert_eq!((r.skipped, r.appended), (1, 0));
        let lines = std::fs::read_to_string(&classify_path)
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, 2);
    }

    /// An error post is retried: the error appends once, the later success
    /// appends once more, and the latest record wins.
    #[test]
    fn error_then_success_appends_once_each() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let queries_path = write_queries(&dir, &queries_body());
        let classify_path = dir.path().join(CLASSIFY_FILE_NAME);
        let posts = vec![bm("p1", "synthetic plain post")];
        let failing = MockScoreBackend::with_outcomes(vec![Err("synthetic outage".to_string())]);
        let r = classify_new_posts(
            &posts,
            &queries_path,
            &classify_path,
            &paid_gates(&failing),
            &mut log,
        )
        .unwrap();
        assert_eq!((r.errors, r.appended), (1, 1));
        let passing = MockScoreBackend::with_replies(vec![scores_reply(0.9, 0.1, 0.2)]);
        let r = classify_new_posts(
            &posts,
            &queries_path,
            &classify_path,
            &paid_gates(&passing),
            &mut log,
        )
        .unwrap();
        assert_eq!((r.classified, r.appended), (1, 1));
        let lines = std::fs::read_to_string(&classify_path)
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, 2);
    }

    /// An unchanged outcome appends nothing: two paid-off runs leave one
    /// pending line, and a repeated identical error leaves one error line.
    #[test]
    fn unchanged_outcomes_do_not_duplicate() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let queries_path = write_queries(&dir, &queries_body());
        let classify_path = dir.path().join(CLASSIFY_FILE_NAME);
        let posts = vec![bm("p1", "synthetic plain post")];
        let off = ClassifyGates {
            allow_paid: false,
            score: None,
            media_backend: None,
            media_paid: None,
        };
        for _ in 0..2 {
            let r =
                classify_new_posts(&posts, &queries_path, &classify_path, &off, &mut log).unwrap();
            assert_eq!(r.pending, 1);
        }
        let lines = std::fs::read_to_string(&classify_path)
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, 1);
    }

    /// A missing queries file records a per-post error, never classified,
    /// with no model call and no crash.
    #[test]
    fn missing_queries_file_records_error_per_post() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![]);
        let gates = paid_gates(&score);
        let classify_path = dir.path().join(CLASSIFY_FILE_NAME);
        let posts = vec![
            bm("p1", "synthetic plain post"),
            bm("p2", "synthetic other post"),
        ];
        let missing = dir.path().join("no-queries.json");
        let r = classify_new_posts(&posts, &missing, &classify_path, &gates, &mut log).unwrap();
        assert_eq!((r.total, r.errors, r.appended), (2, 2, 2));
        assert_eq!(score.calls(), 0);
        let back = ClassifyStore::load_from(&classify_path).unwrap();
        for id in ["p1", "p2"] {
            let rec = back.latest(id).unwrap();
            assert_eq!(rec.status, ClassifyStatus::Error);
            assert!(rec.scores.is_empty());
        }
    }

    /// An unreadable classify file refuses the whole run: no model calls, no
    /// records, an error for the caller to warn about.
    #[test]
    fn unreadable_classify_store_refuses_run() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![]);
        let gates = paid_gates(&score);
        let queries_path = write_queries(&dir, &queries_body());
        let classify_path = dir.path().join(CLASSIFY_FILE_NAME);
        std::fs::write(&classify_path, "not a record\n").unwrap();
        let posts = vec![bm("p1", "synthetic plain post")];
        assert!(
            classify_new_posts(&posts, &queries_path, &classify_path, &gates, &mut log).is_err()
        );
        assert_eq!(score.calls(), 0);
    }

    /// The classify file is created with mode 0600.
    #[cfg(unix)]
    #[test]
    fn classify_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (dir, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![scores_reply(0.9, 0.1, 0.2)]);
        let gates = paid_gates(&score);
        let queries_path = write_queries(&dir, &queries_body());
        let classify_path = dir.path().join("nested").join(CLASSIFY_FILE_NAME);
        let posts = vec![bm("p1", "synthetic plain post")];
        classify_new_posts(&posts, &queries_path, &classify_path, &gates, &mut log).unwrap();
        let mode = std::fs::metadata(&classify_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "classify file mode is {mode:o}");
        let dir_mode = std::fs::metadata(classify_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "classify dir mode is {dir_mode:o}");
    }

    /// Empty queries fail closed even when called directly: an error state,
    /// never classified.
    #[test]
    fn empty_queries_fail_closed() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new(["zorblax"]));
        let (_d, mut log) = open_log();
        let score = MockScoreBackend::with_replies(vec![]);
        let gates = paid_gates(&score);
        let r = classify_post(&bm("p1", "synthetic plain post"), &[], &gates, &mut log);
        assert_eq!(r.status, ClassifyStatus::Error);
        assert_eq!(r.error.as_deref(), Some(QUERIES_UNAVAILABLE_REASON));
        assert_eq!(score.calls(), 0);
    }
}
