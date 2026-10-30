//! "Try it" cascade: cheap rules first, then Jev, then Muse Spark.
//!
//! Stage 0 (topic filter) runs before everything: posts mentioning a term on
//! the user's private mute list (see [`crate::topic_mute`]) answer no with no API
//! call, even when the rules verdict is `ConfidentYes`.
//!
//! Stage 1 (rules) is an INPUT ([`RulesVerdict`]) because the router cannot
//! depend on bookmarks-agent: `ConfidentYes` answers yes with no API call.
//! Anything else goes through the paid/privacy gates — both clients present,
//! both paid-allowed, [`DataClass::Public`](ancientos_router::data_class::DataClass) —
//! and then:
//!
//! - Stage 2 (Jev): one `noul` question with the same try-it wording the eval
//!   uses ([`TRY_IT_INSTRUCTIONS`]); `noul >=` threshold answers yes
//!   (default [`TRY_IT_THRESHOLD`], overridable per [`Cascade`]).
//! - Stage 3 (Muse Spark): strict-JSON `{"try_it":true|false}` prompt with the
//!   same definition; the reply is parsed leniently (first `{...}`). An
//!   empty/null content with `finish_reason` "length" is retried exactly
//!   once with the doubled cap [`TRY_IT_MUSE_RETRY_MAX_COMPLETION_TOKENS`]
//!   (counted in [`CascadeStats::muse_retries`]); only a second truncation
//!   still fails closed and is flagged as truncated instead of an ordinary
//!   Muse "no" (see [`CascadeDecision::truncated`] and
//!   [`CascadeStats::muse_truncated`]).
//!
//! Fail-closed everywhere: gates-off items fall back to the rules verdict
//! with no call and no error, while backend errors and unparseable replies
//! fall back to the rules verdict with the error recorded. [`Cascade::decide`]
//! never fails, so one bad item (or a rate limit) never aborts a run;
//! callers that must stop on HTTP 429 check
//! [`CascadeDecision::is_rate_limited`].
use crate::topic_mute::excluded_topic;
use ancientos_core::log::DecisionLog;
use ancientos_core::loop_guard::LoopGuard;
use ancientos_router::data_class::{check_egress, DataClass, Route};
use ancientos_router::jev_client::JevClient;
use ancientos_router::muse::{MuseChatResponse, MuseClient};
use ancientos_router::RouterError;
use std::cell::RefCell;

/// Try-it definition shared by the Jev question and the Muse prompt. The eval
/// crate's `bookmark_questions()` carries the same wording; the router cannot
/// depend on the eval crate, so the string lives here and an eval test asserts
/// the two match.
pub const TRY_IT_INSTRUCTIONS: &str = "This X bookmark is try-it the reader would act on: hands-on practice to try now with actionable steps (exercise, stretch, breathing, meditation, recipe, concrete routine, tool walkthrough); research for the reader's personal AI OS project (AncientOS) to learn from to build their agent system better (how LLMs, agents and harnesses work, architecture write-ups, popular or growing GitHub repos worth reviewing for ideas); something to hand off to an agent or bot to review or explore (a repo, a list of tools or APIs, a course, a long thread); or learning content whose actionable thing is in the linked page or video (a tutorial, a tool to install, shortcuts to learn). Not news, hype, opinion, drama, general advice lists, or things the reader already uses.";

/// Default Jev `noul` threshold: `>= 0.3` answers yes. Tuned against real
/// labels for recall; override per cascade with
/// [`Cascade::with_jev_threshold`] or process-wide with
/// [`ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV`].
pub const TRY_IT_THRESHOLD: f64 = 0.3;

/// Env var overriding the Jev try-it threshold. Unset, empty, unparseable,
/// NaN, or outside \[0, 1\] falls back to [`TRY_IT_THRESHOLD`] (see
/// [`jev_threshold_from_env`]).
pub const ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV: &str = "ANCIENTOS_JEV_TRY_IT_THRESHOLD";

/// Read [`ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV`]: a number in \[0, 1\] wins;
/// anything else (unset, empty, unparseable, NaN, out of range) falls back
/// to [`TRY_IT_THRESHOLD`]. Never panics.
pub fn jev_threshold_from_env() -> f64 {
    std::env::var(ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|v| !v.is_nan() && (0.0..=1.0).contains(v))
        .unwrap_or(TRY_IT_THRESHOLD)
}

/// Muse Spark completion-token cap for stage 3 (a classify-style yes/no
/// call). Single source of truth for the cascade and the eval bin. Muse
/// Spark's reasoning shares this budget: 200 starved every answer (empty
/// content, `finish_reason` "length"), 1024 still truncated about 1 in 7
/// answers at reasoning effort low, while real answers used up to 672
/// completion tokens. The brief (700), deeper, and delegate caps are
/// separate and unchanged.
pub const TRY_IT_MUSE_MAX_COMPLETION_TOKENS: u32 = 2048;

/// Doubled cap for stage 3's single length-truncation retry. A first reply
/// that is empty/unparseable with `finish_reason` "length" is retried
/// exactly once through the same [`MuseClient::chat_with_class`] path (same
/// data class, caller, paid gate, and egress checks) with this cap; only a
/// second truncation counts as truncated.
pub const TRY_IT_MUSE_RETRY_MAX_COMPLETION_TOKENS: u32 = 4096;

/// Rules verdict for one item, computed by the caller (bookmarks-agent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesVerdict {
    /// Very confident hands-on content: answers yes with no API call.
    ConfidentYes,
    /// Anything else: gates decide whether Jev/Muse may weigh in.
    Unsure,
}

impl RulesVerdict {
    /// Rules-alone answer: yes only when confident, fail closed otherwise.
    pub fn answer(self) -> bool {
        matches!(self, RulesVerdict::ConfidentYes)
    }
}

/// Which stage produced a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeStage {
    /// Stage 0: excluded topic (reader moved past it): always no, no call.
    TopicFilter,
    /// Rules alone (confident yes, or gates off: unpaid, non-sanitized,
    /// or a client missing).
    Rules,
    /// Jev `noul >=` threshold (default 0.3).
    Jev,
    /// Muse Spark strict-JSON answer.
    Muse,
    /// Backend error or unparseable reply: rules verdict with error recorded.
    FailClosed,
}

impl CascadeStage {
    /// Stable lowercase name for logs and JSONL output.
    pub fn as_str(self) -> &'static str {
        match self {
            CascadeStage::TopicFilter => "topic_filter",
            CascadeStage::Rules => "rules",
            CascadeStage::Jev => "jev",
            CascadeStage::Muse => "muse",
            CascadeStage::FailClosed => "fail_closed",
        }
    }
}

impl std::fmt::Display for CascadeStage {
    /// Stage name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why one post was (or was not) tagged try-it: the deciding stage, a
/// human-readable reason, and the model score when the stage has one.
/// Reasons never carry post text, secrets, or raw HTTP bodies.
#[derive(Debug, Clone, PartialEq)]
pub struct TryItExplanation {
    pub stage: CascadeStage,
    pub reason: String,
    pub confidence: Option<f32>,
}

impl TryItExplanation {
    /// Build an explanation directly (args: stage, reason, score; never fails).
    pub fn new(stage: CascadeStage, reason: impl Into<String>, confidence: Option<f32>) -> Self {
        Self {
            stage,
            reason: reason.into(),
            confidence,
        }
    }

    /// Stage 0 explanation: the matched muted term (args: topic id; never fails).
    pub fn topic_filter(topic_id: &str) -> Self {
        Self {
            stage: CascadeStage::TopicFilter,
            reason: format!("muted term '{topic_id}'"),
            confidence: None,
        }
    }

    /// Rules-stage explanation: the matched-cue text from the local
    /// classifier (args: cue text; never fails).
    pub fn rules(matched_cues: &str) -> Self {
        Self {
            stage: CascadeStage::Rules,
            reason: matched_cues.to_string(),
            confidence: None,
        }
    }

    /// Jev-stage explanation: the `noul` score and the threshold it cleared
    /// (args: score, threshold; never fails).
    pub fn jev(noul: f64, threshold: f64) -> Self {
        Self {
            stage: CascadeStage::Jev,
            reason: format!("jev noul={noul:.2} >= {threshold:.2}"),
            confidence: Some(noul as f32),
        }
    }

    /// Muse-stage explanation: the model's own short reason when it gave
    /// one (sanitized, capped), else what Muse answered (args: the answer
    /// and the raw reason; never fails).
    pub fn muse(answer: bool, raw_reason: Option<&str>) -> Self {
        let reason = raw_reason
            .map(sanitize_muse_reason)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if answer {
                    "muse answered try_it=true".to_string()
                } else {
                    "muse answered try_it=false".to_string()
                }
            });
        Self {
            stage: CascadeStage::Muse,
            reason,
            confidence: None,
        }
    }

    /// Fail-closed explanation: the error class only, never the message
    /// body (args: the backend error; never fails).
    pub fn fail_closed(error: &RouterError) -> Self {
        Self {
            stage: CascadeStage::FailClosed,
            reason: fail_closed_reason(error),
            confidence: None,
        }
    }
}

/// Decision-log kind for try-it decisions: `{post_id, stage, reason}`
/// (never post text). Counted by `/stats` per stage.
pub const DECIDED_KIND: &str = "try_it.decided";

/// Maximum reason chars kept from a Muse reply (sanitized, one line).
pub const MUSE_REASON_MAX_CHARS: usize = 160;

/// Rules-stage fallback when the caller has no matched-cue text: a
/// confident yes with no API call.
pub const RULES_CONFIDENT_REASON: &str = "rules-confident yes";
/// Rules-stage fallback when the local classifier found no cues.
pub const RULES_NO_CUES_REASON: &str = "no try-it cues";

/// Sanitize a free-form Muse reason for display and logging: redact
/// secret-shaped values, drop URL/email/handle tokens, collapse to one
/// line, and cap at [`MUSE_REASON_MAX_CHARS`] chars. Never fails.
pub fn sanitize_muse_reason(raw: &str) -> String {
    let redacted = ancientos_core::redact::redact_text(raw).text;
    let kept: Vec<&str> = redacted
        .split_whitespace()
        .filter(|t| !is_url_or_email_token(t) && !is_handle_token(t))
        .collect();
    kept.join(" ")
        .chars()
        .take(MUSE_REASON_MAX_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// True for whitespace-separated tokens that are URLs or emails (dropped
/// from Muse reasons so no hostname or address is logged).
fn is_url_or_email_token(token: &str) -> bool {
    let lower = token.to_lowercase();
    if lower.contains("://") || lower.starts_with("www.") {
        return true;
    }
    let t = token.trim_matches(|c: char| {
        c.is_ascii_punctuation() && c != '@' && c != '.' && c != '_' && c != '-' && c != '+'
    });
    let Some(at) = t.find('@') else {
        return false;
    };
    t[at + 1..].contains('.')
}

/// True for `@handle` tokens (dropped from Muse reasons).
fn is_handle_token(token: &str) -> bool {
    let t = token.trim_start_matches(|c: char| {
        c == '(' || c == '[' || c == '{' || c == '"' || c == '\'' || c == '<'
    });
    t.starts_with('@')
        && t.chars()
            .nth(1)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The error class of a backend failure for [`TryItExplanation`] reasons:
/// `http status <code>` when the message carries one, else the variant
/// name (`http error`, `bad response`, ...). Never the message body, so no
/// secret or raw HTTP payload can leak into a reason or log entry.
pub fn fail_closed_reason(error: &RouterError) -> String {
    match error {
        RouterError::MissingEnv(_) => "missing env".to_string(),
        RouterError::NotImplemented(_) => "backend not implemented".to_string(),
        RouterError::PaidCallsDisabled => "paid calls disabled".to_string(),
        RouterError::Http(msg) => match http_status_of(msg) {
            Some(code) => format!("http status {code}"),
            None => "http error".to_string(),
        },
        RouterError::BadResponse(_) => "bad response".to_string(),
        RouterError::Refused(_) => "refused".to_string(),
        RouterError::EgressDenied { .. } => "egress denied".to_string(),
    }
}

/// Append a [`DECIDED_KIND`] entry (`{post_id, stage, reason}`, never post
/// text) for one try-it explanation. An empty post id is an error (fail
/// closed); anything else appends and returns the entry.
pub fn record_decision(
    log: &mut DecisionLog,
    post_id: &str,
    explanation: &TryItExplanation,
) -> Result<ancientos_core::log::Entry, String> {
    if post_id.trim().is_empty() {
        return Err("post id must be non-empty".to_string());
    }
    log.append(
        DECIDED_KIND,
        serde_json::json!({
            "post_id": post_id,
            "stage": explanation.stage.as_str(),
            "reason": explanation.reason,
        }),
    )
    .map_err(|e| e.to_string())
}

/// One item's decision: the answer, the stage that gave it, the
/// [`TryItExplanation`] behind it, the backend error when the stage is
/// [`CascadeStage::FailClosed`], and whether the Muse reply was
/// length-truncated (empty/null content with `finish_reason` "length"). A
/// truncation still fails closed exactly like any other error; the flag
/// only records it distinctly instead of an ordinary Muse "no".
#[derive(Debug, Clone, PartialEq)]
pub struct CascadeDecision {
    pub try_it: bool,
    pub stage: CascadeStage,
    pub error: Option<String>,
    pub truncated: bool,
    pub explanation: TryItExplanation,
}

impl CascadeDecision {
    /// True when the recorded error is an HTTP 429 rate limit. The cascade
    /// itself never stops; callers that must stop check this per item.
    pub fn is_rate_limited(&self) -> bool {
        self.error.as_deref().is_some_and(is_rate_limit_error)
    }
}

/// True when a backend error message reports HTTP 429 (ureq surfaces it as
/// `http status 429: ...` via both clients' mappers).
pub fn is_rate_limit_error(message: &str) -> bool {
    http_status_of(message) == Some(429)
}

/// The HTTP status code from a mapped error (`http status <code>: ...`,
/// optionally wrapped as `http error: http status <code>: ...`), or `None`.
/// A "429" anywhere else (body, token counts, post text) does not count.
pub fn http_status_of(message: &str) -> Option<u16> {
    let rest = message
        .strip_prefix("http error: ")
        .unwrap_or(message)
        .strip_prefix("http status ")?;
    let (code, _) = rest.split_once(':')?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    code.parse().ok()
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn rate_limit_matches_http_status_only() {
        assert!(is_rate_limit_error("http status 429: slow down"));
        assert!(is_rate_limit_error("http error: http status 429: {}"));
        assert!(!is_rate_limit_error("http status 500: used 429 tokens"));
        assert!(!is_rate_limit_error("bad response: 429 in body"));
        assert!(!is_rate_limit_error("http status 4290: x"));
        assert!(!is_rate_limit_error("timeout after 429ms"));
        assert_eq!(http_status_of("http status 503: down"), Some(503));
        assert_eq!(http_status_of("refused: no"), None);
    }
}

/// Run counters. Call counts record attempts (including ones that errored);
/// token counts record successful Jev replies only. `muse_truncated` counts
/// the Muse calls that failed closed on a length truncation; those calls
/// are also counted in `muse_calls`, `fail_closed`, and `errors`.
/// `muse_retries` counts the single length-truncation retries (at most one
/// per item); each retry is also counted in `muse_calls`.
/// `topic_filtered` counts stage-0 answers (never try-it, never a call).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CascadeStats {
    pub topic_filtered: u64,
    pub rules_yes: u64,
    pub jev_calls: u64,
    pub jev_yes: u64,
    pub muse_calls: u64,
    pub muse_yes: u64,
    pub muse_truncated: u64,
    pub muse_retries: u64,
    pub fail_closed: u64,
    pub errors: u64,
    pub jev_input_tokens: u64,
    pub jev_output_tokens: u64,
}

/// The cascade: injectable clients so tests can point at mock servers.
/// `None` clients mean that backend is unavailable (rules alone).
#[derive(Debug, Clone, Copy)]
pub struct Cascade<'a> {
    jev: Option<&'a JevClient>,
    muse: Option<&'a MuseClient>,
    jev_threshold: f64,
    guard: Option<&'a RefCell<LoopGuard>>,
}

impl<'a> Cascade<'a> {
    /// Test helper: cascade with default yes threshold over the given backends.
    pub fn new(jev: Option<&'a JevClient>, muse: Option<&'a MuseClient>) -> Self {
        Self {
            jev,
            muse,
            jev_threshold: TRY_IT_THRESHOLD,
            guard: None,
        }
    }

    /// Bound the hosted stages with a loop guard: each item that would reach
    /// Jev/Muse first takes one paid step (keyed by the item text). A refused
    /// step answers with the rules verdict and makes no call (fail closed).
    pub fn with_guard(mut self, guard: &'a RefCell<LoopGuard>) -> Self {
        self.guard = Some(guard);
        self
    }

    /// Override the Jev yes threshold (`noul >= threshold` answers yes).
    /// NaN or outside \[0, 1\] falls back to [`TRY_IT_THRESHOLD`] (fail closed).
    pub fn with_jev_threshold(mut self, threshold: f64) -> Self {
        if !threshold.is_nan() && (0.0..=1.0).contains(&threshold) {
            self.jev_threshold = threshold;
        }
        self
    }

    /// The Jev yes threshold this cascade answers against.
    pub fn jev_threshold(&self) -> f64 {
        self.jev_threshold
    }

    /// Decide one item, updating `stats`. Never fails: see the module docs.
    /// Content above [`DataClass::Public`] never reaches the hosted stages:
    /// the refusal is logged as `router.egress_denied` (route `jev`, caller
    /// `cascade`) when `log` is given, and the rules verdict is returned.
    /// Rules-stage reasons use [`RULES_CONFIDENT_REASON`]; callers with the
    /// local classifier's matched-cue text use
    /// [`decide_with_rules_reason`](Self::decide_with_rules_reason) instead.
    pub fn decide(
        &self,
        text: &str,
        class: DataClass,
        rules: RulesVerdict,
        stats: &mut CascadeStats,
        log: Option<&mut DecisionLog>,
    ) -> CascadeDecision {
        self.decide_with_rules_reason(text, class, rules, None, stats, log)
    }

    /// Decide one item with the rules classifier's matched-cue text for the
    /// rules-stage reason (`None` falls back to [`RULES_CONFIDENT_REASON`]).
    /// Otherwise identical to [`decide`](Self::decide): never fails.
    // The log is lent to two sequential hosted calls; each `as_deref_mut`
    // reborrow is required (a move would not compile) despite the lint name.
    #[allow(clippy::needless_option_as_deref)]
    pub fn decide_with_rules_reason(
        &self,
        text: &str,
        class: DataClass,
        rules: RulesVerdict,
        rules_reason: Option<&str>,
        stats: &mut CascadeStats,
        mut log: Option<&mut DecisionLog>,
    ) -> CascadeDecision {
        // Stage 0: excluded topics are never try-it, with no API call —
        // before rules, so even a confident rules yes cannot tag them.
        if let Some(topic) = excluded_topic(text) {
            stats.topic_filtered += 1;
            return CascadeDecision {
                try_it: false,
                stage: CascadeStage::TopicFilter,
                error: None,
                truncated: false,
                explanation: TryItExplanation::topic_filter(&topic),
            };
        }
        // Stage 1: confident rules answer yes with no API call.
        if rules == RulesVerdict::ConfidentYes {
            stats.rules_yes += 1;
            let reason = rules_reason.unwrap_or(RULES_CONFIDENT_REASON);
            return CascadeDecision {
                try_it: true,
                stage: CascadeStage::Rules,
                error: None,
                truncated: false,
                explanation: TryItExplanation::rules(reason),
            };
        }
        // Gates: both clients present, both paid-allowed, Public only.
        // Anything else falls back to the rules verdict (Unsure => no).
        // A class refusal is logged (route jev: the first hosted stage that
        // would have run) but still returns the rules verdict, never an error.
        let (Some(jev), Some(muse)) = (self.jev, self.muse) else {
            return rules_only(rules, "rules unsure (backend unavailable)");
        };
        if !jev.allow_paid() || !muse.allow_paid() {
            return rules_only(rules, "rules unsure (paid off)");
        }
        if class > DataClass::Public {
            let _ = check_egress(class, Route::Jev, "cascade", log);
            return rules_only(rules, "rules unsure (non-public)");
        }
        if let Some(g) = self.guard {
            if g.borrow_mut().step(text, true, now_secs()).is_err() {
                return rules_only(rules, "rules unsure (loop guard tripped)");
            }
        }
        // Stage 2: Jev. Yes stops here; no falls through to Muse.
        stats.jev_calls += 1;
        match jev.ask_with_class(
            class,
            "cascade",
            log.as_deref_mut(),
            text,
            &try_it_questions(),
        ) {
            Ok(reply) => {
                stats.jev_input_tokens += reply.input_tokens;
                stats.jev_output_tokens += reply.output_tokens;
                match reply.noul("try_it") {
                    Some(p) if p >= self.jev_threshold => {
                        stats.jev_yes += 1;
                        return CascadeDecision {
                            try_it: true,
                            stage: CascadeStage::Jev,
                            error: None,
                            truncated: false,
                            explanation: TryItExplanation::jev(p, self.jev_threshold),
                        };
                    }
                    Some(_) => {}
                    None => {
                        return fail_closed(
                            rules,
                            stats,
                            RouterError::BadResponse("jev reply has no try_it noul".into()),
                            false,
                        );
                    }
                }
            }
            Err(e) => return fail_closed(rules, stats, e, false),
        }
        // Stage 3: Muse Spark. Final answer. `chat_with_class` (not
        // `complete`) so a length-truncated reply keeps its finish reason. A
        // first reply that is empty/unparseable with `finish_reason` "length"
        // is retried exactly once with the doubled cap, through the same
        // `chat_with_class` path (same class, caller, paid gate, and egress
        // checks); only a second truncation is flagged as truncated instead
        // of an ordinary Muse "no". Non-length failures never retry.
        let prompt = muse_prompt(text);
        stats.muse_calls += 1;
        let first = muse.chat_with_class(class, "cascade", log.as_deref_mut(), &prompt);
        let retry = match &first {
            Ok(reply) => parse_muse_try_it(&reply.content).is_none() && is_truncated_reply(reply),
            Err(e) => is_truncation_error(e),
        };
        let result = if retry {
            stats.muse_retries += 1;
            stats.muse_calls += 1;
            muse.clone()
                .with_max_completion_tokens(TRY_IT_MUSE_RETRY_MAX_COMPLETION_TOKENS)
                .chat_with_class(class, "cascade", log.as_deref_mut(), &prompt)
        } else {
            first
        };
        match result {
            Ok(reply) => match parse_muse_try_it_with_reason(&reply.content) {
                Some((answer, reason)) => {
                    if answer {
                        stats.muse_yes += 1;
                    }
                    CascadeDecision {
                        try_it: answer,
                        stage: CascadeStage::Muse,
                        error: None,
                        truncated: false,
                        explanation: TryItExplanation::muse(answer, reason.as_deref()),
                    }
                }
                None => {
                    let truncated = is_truncated_reply(&reply);
                    fail_closed(
                        rules,
                        stats,
                        RouterError::BadResponse(format!(
                            "muse reply is not strict try_it JSON: {}",
                            snippet(&reply.content)
                        )),
                        truncated,
                    )
                }
            },
            Err(e) => {
                let truncated = is_truncation_error(&e);
                fail_closed(rules, stats, e, truncated)
            }
        }
    }
}

/// Wall clock in unix seconds (0 if the clock is before the epoch).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Rules-alone decision: no call was made, so no error is recorded.
/// `reason` names why the paid stages did not run (gates off).
fn rules_only(rules: RulesVerdict, reason: &str) -> CascadeDecision {
    CascadeDecision {
        try_it: rules.answer(),
        stage: CascadeStage::Rules,
        error: None,
        truncated: false,
        explanation: TryItExplanation::rules(reason),
    }
}

/// Error fallback: rules verdict with the backend error recorded. A Muse
/// truncation fails closed exactly like any other error; it is only
/// additionally flagged on the decision and counted in
/// [`CascadeStats::muse_truncated`]. The explanation carries the error
/// class only, never the message body.
fn fail_closed(
    rules: RulesVerdict,
    stats: &mut CascadeStats,
    error: RouterError,
    truncated: bool,
) -> CascadeDecision {
    stats.fail_closed += 1;
    stats.errors += 1;
    if truncated {
        stats.muse_truncated += 1;
    }
    let explanation = TryItExplanation::fail_closed(&error);
    CascadeDecision {
        try_it: rules.answer(),
        stage: CascadeStage::FailClosed,
        error: Some(error.to_string()),
        truncated,
        explanation,
    }
}

/// True when a parsed Muse reply is a length truncation: empty content
/// with `finish_reason` "length". Callers check unparseability separately:
/// the retry trigger needs both, while the final fail-closed path only
/// reaches here after parsing already failed.
fn is_truncated_reply(reply: &MuseChatResponse) -> bool {
    reply.finish_reason.as_deref() == Some("length") && reply.content.trim().is_empty()
}

/// True when a Muse error is a length truncation. [`MuseClient`] reports a
/// null, non-string, or missing content as `BadResponse` naming the
/// `finish_reason`; only `"length"` means the completion budget ran out. A
/// null content with any other reason keeps its existing handling.
fn is_truncation_error(error: &RouterError) -> bool {
    matches!(error, RouterError::BadResponse(msg) if msg.contains("finish_reason: \"length\""))
}

/// The single Jev System One question for stage 2.
pub fn try_it_questions() -> serde_json::Value {
    serde_json::json!({"try_it": {"type": "noul", "instructions": TRY_IT_INSTRUCTIONS}})
}

/// Stage 3 prompt: strict JSON with the same try-it definition. The lead
/// sentence states the yes signals explicitly; the full definition follows.
/// An optional short `reason` string is accepted and surfaced (sanitized)
/// in the decision explanation.
pub fn muse_prompt(text: &str) -> String {
    format!(
        "Decide whether this X bookmark is try-it for the reader: hands-on practice to try now, \
         research for the reader's personal AI OS project (AncientOS), \
         something to hand off to an agent or bot to review or explore, \
         or learning content whose actionable thing is in the linked page or video. \
         Definition: {TRY_IT_INSTRUCTIONS} \
         Reply with strict JSON only: {{\"try_it\":true}} or {{\"try_it\":false}}, optionally with a short \"reason\" string (max 160 chars).\n\nBookmark text: {text}"
    )
}

/// Lenient strict-JSON parse: the first `{...}` substring (from any `{` to a
/// later `}`) that parses with a boolean `try_it` field wins.
pub fn parse_muse_try_it(content: &str) -> Option<bool> {
    parse_muse_try_it_with_reason(content).map(|(answer, _)| answer)
}

/// Lenient strict-JSON parse with the optional `reason` string: the first
/// `{...}` substring (from any `{` to a later `}`) that parses with a
/// boolean `try_it` field wins, carrying its `reason` when it is a string.
/// Callers sanitize the raw reason via [`TryItExplanation::muse`].
pub fn parse_muse_try_it_with_reason(content: &str) -> Option<(bool, Option<String>)> {
    for (start, _) in content.match_indices('{') {
        for (end, _) in content.match_indices('}') {
            if end <= start {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content[start..=end]) {
                if let Some(answer) = value.get("try_it").and_then(|v| v.as_bool()) {
                    let reason = value
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    return Some((answer, reason));
                }
            }
        }
    }
    None
}

/// Short preview for error messages (the muse reply, never a secret).
fn snippet(content: &str) -> String {
    content.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancientos_router::jev_client::JevBackend;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Process-wide env guard: every env-mutating test in the crate shares it.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::lock_env()
    }

    /// Set process env vars for a test, restoring previous values on drop.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        /// Test helper: apply env overrides, saving prior values for restore.
        fn set(pairs: &[(&'static str, Option<&str>)]) -> Self {
            let mut saved = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                saved.push((*key, std::env::var(key).ok()));
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        /// Test helper: restore saved env values on scope exit.
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// Captured request bodies, one per hit.
    type SeenBodies = Arc<Mutex<Vec<String>>>;

    /// One-shot mock HTTP server on 127.0.0.1: captures the request body,
    /// replies once with `status` + `body`, then exits. No external network.
    fn mock_server(status: u16, body: &str) -> (String, SeenBodies, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let seen: SeenBodies = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        let body = body.to_string();
        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            seen_thread
                .lock()
                .expect("seen lock")
                .push(read_request_body(&mut stream));
            let reason = reason(status);
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        (url, seen, handle)
    }

    /// Read one request: headers until the blank line, then Content-Length bytes.
    fn read_request_body(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut content_length = 0usize;
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        let mut raw_len = 0usize;
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    raw_len += 1;
                    line.push(buf[0]);
                    if buf[0] == b'\n' {
                        let text = String::from_utf8_lossy(&line);
                        if text.to_lowercase().starts_with("content-length:") {
                            content_length = text
                                .split(':')
                                .nth(1)
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                        }
                        if line == b"\r\n" || line == b"\n" {
                            break;
                        }
                        line.clear();
                    }
                    if raw_len > 65536 {
                        break;
                    }
                }
            }
        }
        let mut rest = vec![0u8; content_length.min(1 << 20)];
        let mut filled = 0;
        while filled < rest.len() {
            match stream.read(&mut rest[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        rest.truncate(filled);
        String::from_utf8_lossy(&rest).into_owned()
    }

    /// Reason phrase for the statuses the mock servers script.
    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Error",
        }
    }

    /// Multi-shot mock HTTP server on 127.0.0.1: serves the scripted
    /// `(status, body)` replies in order on one listener, captures request
    /// bodies, then exits. For the Muse retry tests (first try and retry hit
    /// the same base URL). No external network.
    fn mock_server_seq(
        replies: Vec<(u16, String)>,
    ) -> (String, SeenBodies, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let seen: SeenBodies = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        let handle = std::thread::spawn(move || {
            for (status, body) in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                seen_thread
                    .lock()
                    .expect("seen lock")
                    .push(read_request_body(&mut stream));
                let reason = reason(status);
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (url, seen, handle)
    }

    /// A base URL with nothing listening (connection refused, fast). Tests
    /// that must not touch the network use this to prove it.
    fn closed_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let url = format!("http://{}", listener.local_addr().expect("probe addr"));
        drop(listener);
        url
    }

    /// Jev client over a mock URL. LocalJev needs no key, so only the
    /// cascade's paid gate matters; the flag is set explicitly per test.
    fn jev_client(base_url: String, allow_paid: bool) -> JevClient {
        JevClient::new(JevBackend::LocalJev, base_url, "test-localjev", allow_paid)
    }

    /// Muse client with the cascade's stage-3 settings.
    fn muse_client(base_url: String, allow_paid: bool) -> MuseClient {
        MuseClient::new(
            "muse-spark-1.3",
            base_url,
            TRY_IT_MUSE_MAX_COMPLETION_TOKENS,
            allow_paid,
        )
        .with_reasoning_effort("low")
    }

    /// Test helper: mock Jev response carrying the given score.
    fn jev_noul_body(noul: f64) -> String {
        format!(
            "{{\"model\":\"m\",\"answers\":{{\"try_it\":{{\"type\":\"noul\",\"noul\":{noul}}}}},\
             \"usage\":{{\"input_tokens\":10,\"output_tokens\":2}}}}"
        )
    }

    /// Test helper: mock Muse chat response with the given content.
    fn muse_body(content: &str) -> String {
        serde_json::json!({
            "model": "muse-spark-1.3",
            "choices": [{"message": {"role": "assistant", "content": content},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
        })
        .to_string()
    }

    /// Muse reply with an explicit content value (string or null) and finish
    /// reason, for the truncation tests.
    fn muse_body_full(content: serde_json::Value, finish_reason: &str) -> String {
        serde_json::json!({
            "model": "muse-spark-1.3",
            "choices": [{"message": {"role": "assistant", "content": content},
                         "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
        })
        .to_string()
    }

    /// Test helper: blank cascade counters for a single decision.
    fn zero_stats() -> CascadeStats {
        CascadeStats::default()
    }

    #[test]
    fn confident_yes_answers_without_any_call() {
        // Closed URLs prove no network is attempted.
        let jev = jev_client(closed_url(), true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "try it",
            DataClass::Public,
            RulesVerdict::ConfidentYes,
            &mut stats,
            None,
        );
        assert_eq!(
            d,
            CascadeDecision {
                try_it: true,
                stage: CascadeStage::Rules,
                error: None,
                truncated: false,
                explanation: TryItExplanation::rules(RULES_CONFIDENT_REASON),
            }
        );
        assert_eq!(stats.rules_yes, 1);
        assert_eq!(stats.jev_calls, 0);
        assert_eq!(stats.muse_calls, 0);
        assert_eq!(stats.fail_closed, 0);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn tripped_loop_guard_makes_no_hosted_call() {
        use ancientos_core::loop_guard::{Budgets, LoopGuard};
        // Closed URLs prove no network is attempted once the paid budget is 0.
        let jev = jev_client(closed_url(), true);
        let muse = muse_client(closed_url(), true);
        let b = Budgets {
            max_paid: 0,
            ..Budgets::default()
        };
        let guard = RefCell::new(LoopGuard::new("cascade", b, now_secs()));
        let cascade = Cascade::new(Some(&jev), Some(&muse)).with_guard(&guard);
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        assert!(!d.try_it);
        assert_eq!(d.stage, CascadeStage::Rules);
        assert_eq!(stats.jev_calls + stats.muse_calls, 0);
        assert!(guard.borrow().tripped().is_some());
    }

    #[test]
    fn paid_off_is_rules_only_with_zero_calls() {
        // Either flag off gates the whole cascade; closed URLs prove it.
        for (jev_paid, muse_paid) in [(false, false), (false, true), (true, false)] {
            let jev = jev_client(closed_url(), jev_paid);
            let muse = muse_client(closed_url(), muse_paid);
            let cascade = Cascade::new(Some(&jev), Some(&muse));
            let mut stats = zero_stats();
            let d = cascade.decide(
                "x",
                DataClass::Public,
                RulesVerdict::Unsure,
                &mut stats,
                None,
            );
            assert!(!d.try_it);
            assert_eq!(d.stage, CascadeStage::Rules);
            assert_eq!(d.error, None);
            assert_eq!(stats, zero_stats(), "paid=({jev_paid},{muse_paid})");
        }
    }

    #[test]
    fn missing_client_is_rules_only() {
        let jev = jev_client(closed_url(), true);
        let muse = muse_client(closed_url(), true);
        for (jev_opt, muse_opt) in [(None, None), (Some(&jev), None), (None, Some(&muse))] {
            let cascade = Cascade::new(jev_opt, muse_opt);
            let mut stats = zero_stats();
            let d = cascade.decide(
                "x",
                DataClass::Public,
                RulesVerdict::Unsure,
                &mut stats,
                None,
            );
            assert!(!d.try_it);
            assert_eq!(d.stage, CascadeStage::Rules);
            assert_eq!(d.error, None);
            assert_eq!(stats, zero_stats());
        }
    }

    #[test]
    fn non_public_is_rules_only_with_zero_calls() {
        let jev = jev_client(closed_url(), true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        // Anything above Public fails closed to rules with zero calls.
        for class in [
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            let mut stats = zero_stats();
            let d = cascade.decide("private", class, RulesVerdict::Unsure, &mut stats, None);
            assert!(!d.try_it, "class {class:?}");
            assert_eq!(d.stage, CascadeStage::Rules);
            assert_eq!(d.error, None);
            assert_eq!(stats, zero_stats(), "class {class:?}");
        }
        // Confident rules still answer yes without touching the network.
        let mut stats = zero_stats();
        let d = cascade.decide(
            "private",
            DataClass::Confidential,
            RulesVerdict::ConfidentYes,
            &mut stats,
            None,
        );
        assert!(d.try_it);
        assert_eq!(d.stage, CascadeStage::Rules);
        assert_eq!(stats.rules_yes, 1);
        assert_eq!(stats.jev_calls, 0);
    }

    #[test]
    fn jev_yes_stops_before_muse() {
        let (jev_url, jev_seen, jev_handle) = mock_server(200, &jev_noul_body(0.9));
        let jev = jev_client(jev_url, true);
        // Closed muse URL proves stage 3 is never reached.
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "stretch now",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert!(d.try_it);
        assert_eq!(d.stage, CascadeStage::Jev);
        assert_eq!(d.error, None);
        assert_eq!(stats.jev_calls, 1);
        assert_eq!(stats.jev_yes, 1);
        assert_eq!(stats.muse_calls, 0);
        assert_eq!((stats.jev_input_tokens, stats.jev_output_tokens), (10, 2));
        // The Jev request carries the shared definition as a noul question.
        let seen = jev_seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        let sent: serde_json::Value = serde_json::from_str(&seen[0]).expect("request is json");
        assert_eq!(sent["state"], "stretch now");
        assert_eq!(sent["questions"]["try_it"]["type"], "noul");
        assert_eq!(
            sent["questions"]["try_it"]["instructions"],
            TRY_IT_INSTRUCTIONS
        );
        // The broadened definition carries the research and hand-off signals.
        let instructions = sent["questions"]["try_it"]["instructions"]
            .as_str()
            .expect("instructions is a string");
        assert!(instructions.contains("personal AI OS project (AncientOS)"));
        assert!(instructions.contains("hand off"));
    }

    #[test]
    fn jev_threshold_defaults_to_point_three() {
        assert_eq!(TRY_IT_THRESHOLD, 0.3);
        // Exactly 0.3 answers yes and never reaches muse.
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.3));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert_eq!((d.try_it, d.stage), (true, CascadeStage::Jev));
        assert_eq!(stats.muse_calls, 0);
    }

    #[test]
    fn just_below_default_falls_through_to_muse() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        // 0.29 is below the 0.3 default: Jev says no, Muse decides.
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.29));
        let (muse_url, _, muse_handle) = mock_server(200, &muse_body("{\"try_it\":false}"));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        assert_eq!(cascade.jev_threshold(), TRY_IT_THRESHOLD);
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        assert_eq!((d.try_it, d.stage), (false, CascadeStage::Muse));
        assert_eq!(stats.jev_calls, 1);
        assert_eq!(stats.jev_yes, 0);
        assert_eq!(stats.muse_calls, 1);
    }

    #[test]
    fn builder_overrides_threshold() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        // Invalid builder values fail closed to the default, never stored.
        let jev = jev_client(closed_url(), true);
        let muse = muse_client(closed_url(), true);
        for bad in [f64::NAN, -0.1, 1.5, f64::INFINITY] {
            let cascade = Cascade::new(Some(&jev), Some(&muse)).with_jev_threshold(bad);
            assert_eq!(cascade.jev_threshold(), TRY_IT_THRESHOLD, "input {bad}");
        }
        // A high threshold sends 0.5 to Muse instead of answering yes.
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.5));
        let (muse_url, _, muse_handle) = mock_server(200, &muse_body("{\"try_it\":true}"));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse)).with_jev_threshold(0.9);
        assert_eq!(cascade.jev_threshold(), 0.9);
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        assert_eq!((d.try_it, d.stage), (true, CascadeStage::Muse));
        assert_eq!(stats.jev_yes, 0);
        assert_eq!(stats.muse_calls, 1);
        // A low threshold answers yes at 0.2 without reaching Muse.
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.2));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse)).with_jev_threshold(0.1);
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert_eq!((d.try_it, d.stage), (true, CascadeStage::Jev));
        assert_eq!(stats.muse_calls, 0);
    }

    #[test]
    fn env_threshold_parsing_matrix() {
        let _guard = env_lock();
        // Unset, empty, and blank fall back to the default.
        for value in [None, Some(""), Some("   ")] {
            let _env = EnvGuard::set(&[(ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV, value)]);
            assert_eq!(
                jev_threshold_from_env(),
                TRY_IT_THRESHOLD,
                "input {value:?}"
            );
        }
        // Valid numbers in [0,1] win, endpoints included.
        for (value, expected) in [("0", 0.0), ("1", 1.0), ("0.3", 0.3), ("0.7", 0.7)] {
            let _env = EnvGuard::set(&[(ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV, Some(value))]);
            assert_eq!(jev_threshold_from_env(), expected, "input {value:?}");
        }
        // Invalid, NaN, and out-of-range fall back, never panic.
        for value in ["bogus", "NaN", "nan", "-0.1", "1.5", "inf", "-inf", "0.5.0"] {
            let _env = EnvGuard::set(&[(ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV, Some(value))]);
            assert_eq!(
                jev_threshold_from_env(),
                TRY_IT_THRESHOLD,
                "input {value:?}"
            );
        }
    }

    #[test]
    fn jev_no_falls_through_to_muse_yes_and_no() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        assert_eq!(TRY_IT_MUSE_MAX_COMPLETION_TOKENS, 2048);
        assert_eq!(TRY_IT_MUSE_RETRY_MAX_COMPLETION_TOKENS, 4096);
        for (muse_content, expected) in [("{\"try_it\":true}", true), ("{\"try_it\":false}", false)]
        {
            let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
            let (muse_url, muse_seen, muse_handle) = mock_server(200, &muse_body(muse_content));
            let jev = jev_client(jev_url, true);
            let muse = muse_client(muse_url, true);
            let cascade = Cascade::new(Some(&jev), Some(&muse));
            let mut stats = zero_stats();
            let d = cascade.decide(
                "news blip",
                DataClass::Public,
                RulesVerdict::Unsure,
                &mut stats,
                None,
            );
            jev_handle.join().expect("server thread");
            muse_handle.join().expect("server thread");
            assert_eq!(d.try_it, expected, "muse said {muse_content}");
            assert_eq!(d.stage, CascadeStage::Muse);
            assert_eq!(d.error, None);
            assert!(!d.is_rate_limited());
            // A normal Muse answer is never reported as truncated.
            assert!(!d.truncated, "muse said {muse_content}");
            assert_eq!(stats.jev_calls, 1);
            assert_eq!(stats.jev_yes, 0);
            assert_eq!(stats.muse_calls, 1);
            assert_eq!(stats.muse_yes, u64::from(expected));
            assert_eq!(stats.muse_truncated, 0);
            assert_eq!(stats.muse_retries, 0);
            // Stage-3 settings from the spec reach the wire.
            let seen = muse_seen.lock().expect("seen lock");
            assert_eq!(seen.len(), 1);
            let sent: serde_json::Value = serde_json::from_str(&seen[0]).expect("request is json");
            assert_eq!(sent["model"], "muse-spark-1.3");
            assert_eq!(
                sent["max_completion_tokens"],
                TRY_IT_MUSE_MAX_COMPLETION_TOKENS
            );
            assert_eq!(sent["reasoning_effort"], "low");
            assert!(sent["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains(TRY_IT_INSTRUCTIONS));
        }
    }

    #[test]
    fn jev_error_falls_back_without_reaching_muse() {
        let (jev_url, _, jev_handle) = mock_server(500, r#"{"error":"boom"}"#);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert!(!d.try_it);
        assert_eq!(d.stage, CascadeStage::FailClosed);
        let error = d.error.as_deref().expect("error recorded");
        assert!(error.contains("500"), "{error}");
        assert!(!d.is_rate_limited());
        assert_eq!(stats.jev_calls, 1);
        assert_eq!(stats.muse_calls, 0);
        assert_eq!(stats.fail_closed, 1);
        assert_eq!(stats.errors, 1);
        assert_eq!((stats.jev_input_tokens, stats.jev_output_tokens), (0, 0));
    }

    #[test]
    fn jev_missing_noul_falls_back() {
        let body = r#"{"model":"m","answers":{"try_it":{"type":"noul"}},"usage":{"input_tokens":7,"output_tokens":1}}"#;
        let (jev_url, _, jev_handle) = mock_server(200, body);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert!(!d.try_it);
        assert_eq!(d.stage, CascadeStage::FailClosed);
        assert!(d.error.unwrap().contains("noul"));
        assert_eq!(stats.jev_calls, 1);
        assert_eq!(stats.muse_calls, 0);
        // Tokens from the reply are still counted.
        assert_eq!((stats.jev_input_tokens, stats.jev_output_tokens), (7, 1));
    }

    #[test]
    fn muse_garbage_falls_back() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.2));
        let (muse_url, _, muse_handle) = mock_server(200, &muse_body("Sure, looks hands-on!"));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        assert!(!d.try_it);
        assert_eq!(d.stage, CascadeStage::FailClosed);
        let error = d.error.expect("error recorded");
        assert!(error.contains("strict try_it JSON"), "{error}");
        assert!(!d.truncated);
        assert_eq!(stats.muse_calls, 1);
        assert_eq!(stats.muse_yes, 0);
        assert_eq!(stats.muse_truncated, 0);
        assert_eq!(stats.muse_retries, 0);
        assert_eq!(stats.fail_closed, 1);
        assert_eq!(stats.errors, 1);
    }

    #[test]
    fn muse_error_falls_back() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.2));
        let (muse_url, _, muse_handle) = mock_server(500, r#"{"error":"boom"}"#);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        assert!(!d.try_it);
        assert_eq!(d.stage, CascadeStage::FailClosed);
        assert!(d.error.as_deref().unwrap().contains("500"));
        assert!(!d.is_rate_limited());
        assert!(!d.truncated);
        assert_eq!(stats.muse_calls, 1);
        assert_eq!(stats.muse_truncated, 0);
        assert_eq!(stats.muse_retries, 0);
        assert_eq!(stats.fail_closed, 1);
    }

    #[test]
    fn muse_empty_length_reply_is_truncated_fail_closed() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
        // Truncated twice (first try + the one retry): only the second
        // truncation counts as truncated.
        let empty_length = muse_body_full(serde_json::json!(""), "length");
        let (muse_url, _, muse_handle) =
            mock_server_seq(vec![(200, empty_length.clone()), (200, empty_length)]);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        // Fail closed exactly as before: not try-it, rules verdict, error kept.
        assert!(!d.try_it);
        assert_eq!(d.stage, CascadeStage::FailClosed);
        assert!(d.error.is_some());
        assert!(!d.is_rate_limited());
        // ...but reported as truncated, distinct from an ordinary Muse "no".
        assert!(d.truncated);
        assert_eq!(stats.muse_calls, 2);
        assert_eq!(stats.muse_retries, 1);
        assert_eq!(stats.muse_yes, 0);
        assert_eq!(stats.muse_truncated, 1);
        assert_eq!(stats.fail_closed, 1);
        assert_eq!(stats.errors, 1);
    }

    #[test]
    fn muse_null_content_is_truncated_only_for_length() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        // A null content surfaces as a BadResponse naming the finish reason;
        // only "length" means the completion budget ran out. The length case
        // retries once (two truncated replies); "stop" never retries.
        for (reason, expected) in [("length", true), ("stop", false)] {
            let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
            let null_body = muse_body_full(serde_json::Value::Null, reason);
            let replies = if expected {
                vec![(200, null_body.clone()), (200, null_body)]
            } else {
                vec![(200, null_body)]
            };
            let (muse_url, _, muse_handle) = mock_server_seq(replies);
            let jev = jev_client(jev_url, true);
            let muse = muse_client(muse_url, true);
            let cascade = Cascade::new(Some(&jev), Some(&muse));
            let mut stats = zero_stats();
            let d = cascade.decide(
                "x",
                DataClass::Public,
                RulesVerdict::Unsure,
                &mut stats,
                None,
            );
            jev_handle.join().expect("server thread");
            muse_handle.join().expect("server thread");
            assert!(!d.try_it, "reason {reason}");
            assert_eq!(d.stage, CascadeStage::FailClosed);
            assert_eq!(d.truncated, expected, "reason {reason}");
            assert_eq!(stats.muse_calls, 1 + u64::from(expected), "reason {reason}");
            assert_eq!(stats.muse_retries, u64::from(expected), "reason {reason}");
            assert_eq!(stats.muse_truncated, u64::from(expected), "reason {reason}");
            assert_eq!(stats.fail_closed, 1);
        }
    }

    #[test]
    fn muse_truncation_retries_once_with_doubled_cap_then_answers() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
        // First Muse reply is truncated (empty content + length); the retry
        // answers, so nothing fails closed and nothing counts as truncated.
        let (muse_url, muse_seen, muse_handle) = mock_server_seq(vec![
            (200, muse_body_full(serde_json::json!(""), "length")),
            (200, muse_body("{\"try_it\":true}")),
        ]);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        assert!(d.try_it);
        assert_eq!(d.stage, CascadeStage::Muse);
        assert_eq!(d.error, None);
        assert!(!d.truncated);
        assert!(!d.is_rate_limited());
        assert_eq!(stats.muse_calls, 2);
        assert_eq!(stats.muse_retries, 1);
        assert_eq!(stats.muse_yes, 1);
        assert_eq!(stats.muse_truncated, 0);
        assert_eq!(stats.fail_closed, 0);
        assert_eq!(stats.errors, 0);
        // First request carries 2048, the retry 4096; same model and effort.
        let seen = muse_seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 2);
        let first: serde_json::Value = serde_json::from_str(&seen[0]).expect("request is json");
        let second: serde_json::Value = serde_json::from_str(&seen[1]).expect("request is json");
        assert_eq!(
            first["max_completion_tokens"],
            TRY_IT_MUSE_MAX_COMPLETION_TOKENS
        );
        assert_eq!(first["max_completion_tokens"], 2048);
        assert_eq!(
            second["max_completion_tokens"],
            TRY_IT_MUSE_RETRY_MAX_COMPLETION_TOKENS
        );
        assert_eq!(second["max_completion_tokens"], 4096);
        assert_eq!(second["model"], "muse-spark-1.3");
        assert_eq!(second["reasoning_effort"], "low");
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
    }

    #[test]
    fn muse_double_truncation_retries_once_then_fails_closed_as_truncated() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
        // Null content + length (a truncation error) first, empty + length
        // on the retry: both truncation shapes, still exactly one retry.
        let (muse_url, muse_seen, muse_handle) = mock_server_seq(vec![
            (200, muse_body_full(serde_json::Value::Null, "length")),
            (200, muse_body_full(serde_json::json!(""), "length")),
        ]);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        assert!(!d.try_it);
        assert_eq!(d.stage, CascadeStage::FailClosed);
        assert!(d.error.is_some());
        assert!(!d.is_rate_limited());
        assert!(d.truncated);
        assert_eq!(stats.muse_calls, 2);
        assert_eq!(stats.muse_retries, 1);
        assert_eq!(stats.muse_yes, 0);
        assert_eq!(stats.muse_truncated, 1);
        assert_eq!(stats.fail_closed, 1);
        assert_eq!(stats.errors, 1);
        // First request carries 2048, the retry 4096.
        let seen = muse_seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 2);
        let first: serde_json::Value = serde_json::from_str(&seen[0]).expect("request is json");
        let second: serde_json::Value = serde_json::from_str(&seen[1]).expect("request is json");
        assert_eq!(first["max_completion_tokens"], 2048);
        assert_eq!(second["max_completion_tokens"], 4096);
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
    }

    #[test]
    fn muse_non_length_failures_are_never_retried() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        // Garbage with "stop", null content with "stop", HTTP 500, HTTP 429:
        // none is a length truncation, so none retries (429 stays flagged).
        let cases: Vec<(u16, String, bool)> = vec![
            (200, muse_body("Sure, looks hands-on!"), false),
            (200, muse_body_full(serde_json::Value::Null, "stop"), false),
            (500, r#"{"error":"boom"}"#.to_string(), false),
            (429, r#"{"error":"slow down"}"#.to_string(), true),
        ];
        for (status, body, rate_limited) in cases {
            let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
            let (muse_url, muse_seen, muse_handle) = mock_server(status, &body);
            let jev = jev_client(jev_url, true);
            let muse = muse_client(muse_url, true);
            let cascade = Cascade::new(Some(&jev), Some(&muse));
            let mut stats = zero_stats();
            let d = cascade.decide(
                "x",
                DataClass::Public,
                RulesVerdict::Unsure,
                &mut stats,
                None,
            );
            assert!(!d.try_it, "status {status}");
            assert_eq!(d.stage, CascadeStage::FailClosed, "status {status}");
            assert_eq!(d.is_rate_limited(), rate_limited, "status {status}");
            assert!(!d.truncated, "status {status}");
            assert_eq!(stats.muse_calls, 1, "status {status}");
            assert_eq!(stats.muse_retries, 0, "status {status}");
            assert_eq!(stats.muse_truncated, 0, "status {status}");
            assert_eq!(
                muse_seen.lock().expect("seen lock").len(),
                1,
                "status {status}"
            );
            jev_handle.join().expect("server thread");
            muse_handle.join().expect("server thread");
        }
    }

    #[test]
    fn refused_class_makes_zero_muse_calls_including_no_retry() {
        // Refused classes never reach the hosted stages at all: zero calls
        // and zero retries. Live mocks (one scripting a truncation first)
        // prove no request arrived.
        let (jev_url, jev_seen, _jev_handle) = mock_server(200, &jev_noul_body(0.1));
        let (muse_url, muse_seen, _muse_handle) = mock_server_seq(vec![
            (200, muse_body_full(serde_json::json!(""), "length")),
            (200, muse_body("{\"try_it\":true}")),
        ]);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        for class in [
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            let mut stats = zero_stats();
            let d = cascade.decide("private", class, RulesVerdict::Unsure, &mut stats, None);
            assert!(!d.try_it, "class {class:?}");
            assert_eq!(d.stage, CascadeStage::Rules, "class {class:?}");
            assert_eq!(d.error, None, "class {class:?}");
            assert_eq!(stats, zero_stats(), "class {class:?}");
        }
        assert!(
            jev_seen.lock().expect("seen lock").is_empty(),
            "jev must see no request"
        );
        assert!(
            muse_seen.lock().expect("seen lock").is_empty(),
            "muse must see no request"
        );
    }

    #[test]
    fn rate_limit_is_flagged_and_never_aborts_the_run() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        // Jev 429: flagged on the decision; the cascade still decides on.
        let (jev_url, _, jev_handle) = mock_server(429, r#"{"error":"slow down"}"#);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert_eq!(d.stage, CascadeStage::FailClosed);
        assert!(d.error.as_deref().unwrap().contains("429"));
        assert!(d.is_rate_limited());
        assert!(!d.truncated);
        // The same cascade keeps working: rules decide without the network.
        let d2 = cascade.decide(
            "y",
            DataClass::Public,
            RulesVerdict::ConfidentYes,
            &mut stats,
            None,
        );
        assert_eq!((d2.try_it, d2.stage), (true, CascadeStage::Rules));
        assert!(!d2.is_rate_limited());
        assert_eq!(stats.fail_closed, 1);

        // Muse 429 is flagged the same way.
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
        let (muse_url, _, muse_handle) = mock_server(429, r#"{"error":"slow down"}"#);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "x",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        assert_eq!(d.stage, CascadeStage::FailClosed);
        assert!(d.is_rate_limited());
        assert!(!d.truncated);
        assert_eq!(stats.muse_truncated, 0);
        assert_eq!(stats.muse_retries, 0);
    }

    #[test]
    fn muse_prompt_carries_definition_and_strict_shape() {
        let prompt = muse_prompt("do 10 push-ups");
        assert!(prompt.contains(TRY_IT_INSTRUCTIONS));
        assert!(prompt.contains("{\"try_it\":true}"));
        assert!(prompt.contains("{\"try_it\":false}"));
        assert!(prompt.contains("do 10 push-ups"));
        // Yes signals are stated explicitly in the prompt.
        assert!(prompt.contains("personal AI OS project (AncientOS)"));
        assert!(prompt.contains("hand off"));
        assert!(prompt.contains("linked page or video"));
        let questions = try_it_questions();
        assert_eq!(questions["try_it"]["type"], "noul");
        assert_eq!(questions["try_it"]["instructions"], TRY_IT_INSTRUCTIONS);
    }

    #[test]
    fn try_it_instructions_cover_broadened_signals() {
        // One paragraph, concise enough for a Jev noul question.
        assert!(!TRY_IT_INSTRUCTIONS.contains('\n'));
        assert!(
            TRY_IT_INSTRUCTIONS.len() < 900,
            "length {}",
            TRY_IT_INSTRUCTIONS.len()
        );
        // Yes signals: hands-on practice, AncientOS research, agent hand-off,
        // and learning content whose payoff is in the linked page or video.
        assert!(TRY_IT_INSTRUCTIONS.contains("hands-on practice"));
        assert!(TRY_IT_INSTRUCTIONS.contains("personal AI OS project (AncientOS)"));
        assert!(TRY_IT_INSTRUCTIONS.contains("hand off"));
        assert!(TRY_IT_INSTRUCTIONS.contains("agent or bot"));
        assert!(TRY_IT_INSTRUCTIONS.contains("linked page or video"));
        // Still not try-it: news, hype, opinion, drama, advice lists, and
        // things the reader already uses.
        for no in [
            "news",
            "hype",
            "opinion",
            "drama",
            "general advice lists",
            "already uses",
        ] {
            assert!(TRY_IT_INSTRUCTIONS.contains(no), "missing {no}");
        }
    }

    #[test]
    fn muse_parse_is_lenient_first_braces_only() {
        assert_eq!(parse_muse_try_it("{\"try_it\":true}"), Some(true));
        assert_eq!(parse_muse_try_it("{\"try_it\":false}"), Some(false));
        assert_eq!(
            parse_muse_try_it("Here you go: {\"try_it\": true} hope this helps."),
            Some(true)
        );
        assert_eq!(
            parse_muse_try_it("{\"try_it\":false}\nExtra prose after."),
            Some(false)
        );
        // First parseable object without the field is skipped for the next.
        assert_eq!(
            parse_muse_try_it("{\"note\":1} then {\"try_it\":true}"),
            Some(true)
        );
        for garbage in [
            "Sure, looks hands-on!",
            "",
            "{\"try_it\": \"yes\"}",
            "{\"try_it\": null}",
            "{\"other\": true}",
            "{oops",
            "{\"try_it\":true",
        ] {
            assert_eq!(parse_muse_try_it(garbage), None, "input {garbage:?}");
        }
    }

    #[test]
    fn excluded_topic_answers_no_with_zero_calls() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new([
            "zorblax",
            "quuxwidget",
        ]));
        // Live mock servers that must see no request: both backends paid
        // and available, yet stage 0 answers before any call. The servers
        // block on accept, so they are left unjoined (no request arrives).
        let (jev_url, jev_seen, _jev_handle) = mock_server(200, &jev_noul_body(0.9));
        let (muse_url, muse_seen, _muse_handle) = mock_server(200, &muse_body("{\"try_it\":true}"));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        for (text, topic) in [
            (
                "Zorblax setup you can try now: 1) install 2) run it",
                "zorblax",
            ),
            ("Try this QuuxWidget quickstart today", "quuxwidget"),
        ] {
            for rules in [RulesVerdict::Unsure, RulesVerdict::ConfidentYes] {
                let d = cascade.decide(text, DataClass::Public, rules, &mut stats, None);
                assert_eq!(
                    d,
                    CascadeDecision {
                        try_it: false,
                        stage: CascadeStage::TopicFilter,
                        error: None,
                        truncated: false,
                        explanation: TryItExplanation::topic_filter(topic),
                    },
                    "text {text:?} rules {rules:?}"
                );
                assert!(!d.is_rate_limited());
            }
        }
        assert_eq!(stats.topic_filtered, 4);
        assert_eq!(stats.jev_calls, 0);
        assert_eq!(stats.muse_calls, 0);
        assert_eq!(stats.rules_yes, 0);
        assert_eq!(stats.fail_closed, 0);
        assert_eq!(stats.errors, 0);
        assert_eq!(stats.jev_input_tokens, 0);
        assert_eq!(stats.jev_output_tokens, 0);
        assert!(
            jev_seen.lock().expect("seen lock").is_empty(),
            "jev must see no request"
        );
        assert!(
            muse_seen.lock().expect("seen lock").is_empty(),
            "muse must see no request"
        );
    }

    #[test]
    fn non_excluded_topics_reach_later_stages() {
        // A post with no muted term proceeds (here, Jev answers yes).
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new([
            "zorblax",
            "quuxwidget",
        ]));
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.9));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "Zorb lax bag review",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert_eq!((d.try_it, d.stage), (true, CascadeStage::Jev));
        assert_eq!(stats.topic_filtered, 0);
        assert_eq!(stats.jev_calls, 1);
    }

    #[test]
    fn stage_names_are_stable() {
        assert_eq!(CascadeStage::TopicFilter.as_str(), "topic_filter");
        assert_eq!(CascadeStage::Rules.as_str(), "rules");
        assert_eq!(CascadeStage::Jev.as_str(), "jev");
        assert_eq!(CascadeStage::Muse.as_str(), "muse");
        assert_eq!(CascadeStage::FailClosed.as_str(), "fail_closed");
        assert_eq!(CascadeStage::Muse.to_string(), "muse");
        assert!(RulesVerdict::ConfidentYes.answer());
        assert!(!RulesVerdict::Unsure.answer());
    }

    #[test]
    fn non_public_logs_denied_with_zero_mock_requests() {
        // Live mock servers that must see no request. They block on accept,
        // so they are left unjoined (no request arrives).
        let (jev_url, jev_seen, _jev_handle) = mock_server(200, &jev_noul_body(0.9));
        let (muse_url, muse_seen, _muse_handle) = mock_server(200, &muse_body("{\"try_it\":true}"));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for class in [
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            let mut stats = zero_stats();
            let d = cascade.decide(
                "canary private",
                class,
                RulesVerdict::Unsure,
                &mut stats,
                Some(&mut log),
            );
            assert!(!d.try_it, "class {class:?}");
            assert_eq!(d.stage, CascadeStage::Rules);
            assert_eq!(stats, zero_stats(), "class {class:?}");
        }
        assert!(
            jev_seen.lock().expect("seen lock").is_empty(),
            "jev must see no request"
        );
        assert!(
            muse_seen.lock().expect("seen lock").is_empty(),
            "muse must see no request"
        );
        // One denied entry per refusal: the cascade's first hosted stage.
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 3);
        for (e, class) in entries
            .iter()
            .zip(["internal", "confidential", "restricted"])
        {
            assert_eq!(e.kind, ancientos_router::data_class::EGRESS_DENIED_KIND);
            assert_eq!(e.payload["class"], class);
            assert_eq!(e.payload["route"], "jev");
            assert_eq!(e.payload["caller"], "cascade");
            let raw = serde_json::to_string(&e.payload).unwrap();
            assert!(!raw.contains("canary"), "{raw}");
        }
    }

    #[test]
    fn public_stages_log_allowed_entries() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[
            ("META_API_KEY", Some("test-key")),
            ("TYPESAFE_API_KEY", Some("test-key")),
        ]);
        // Jev says no, Muse says yes: the muse stage logs one allowed entry
        // (the LocalJev test client itself never logs: local, not egress).
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
        let (muse_url, muse_seen, muse_handle) = mock_server(200, &muse_body("{\"try_it\":true}"));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        let mut stats = zero_stats();
        let d = cascade.decide(
            "public post",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            Some(&mut log),
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        assert_eq!((d.try_it, d.stage), (true, CascadeStage::Muse));
        assert_eq!(muse_seen.lock().expect("seen lock").len(), 1);
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].kind,
            ancientos_router::data_class::EGRESS_ALLOWED_KIND
        );
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "muse_spark");
    }

    #[test]
    fn typesafe_jev_stage_logs_allowed_entry() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("TYPESAFE_API_KEY", Some("test-key"))]);
        // TypeSafe Jev answers yes: one allowed entry for route jev, and the
        // closed muse URL proves stage 3 is never reached.
        let (jev_url, jev_seen, jev_handle) = mock_server(200, &jev_noul_body(0.9));
        let jev = JevClient::new(JevBackend::TypeSafe, jev_url, "test-jevo", true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        let mut stats = zero_stats();
        let d = cascade.decide(
            "public post",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            Some(&mut log),
        );
        jev_handle.join().expect("server thread");
        assert_eq!((d.try_it, d.stage), (true, CascadeStage::Jev));
        assert_eq!(jev_seen.lock().expect("seen lock").len(), 1);
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].kind,
            ancientos_router::data_class::EGRESS_ALLOWED_KIND
        );
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "jev");
    }

    #[test]
    fn explanations_name_stage_reason_and_score() {
        let cascade = Cascade::new(None, None);
        let mut stats = zero_stats();
        // Topic filter: the matched muted term, no score.
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::new([
            "zorblax",
            "quuxwidget",
        ]));
        let d = cascade.decide(
            "Zorblax quickstart",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        assert_eq!(d.explanation.stage, CascadeStage::TopicFilter);
        assert_eq!(d.explanation.reason, "muted term 'zorblax'");
        assert_eq!(d.explanation.confidence, None);
        // Confident rules without cue text: the fallback reason, no score.
        let d = cascade.decide(
            "plain post",
            DataClass::Public,
            RulesVerdict::ConfidentYes,
            &mut stats,
            None,
        );
        assert_eq!(d.explanation.stage, CascadeStage::Rules);
        assert_eq!(d.explanation.reason, RULES_CONFIDENT_REASON);
        assert_eq!(d.explanation.confidence, None);
        // Confident rules with cue text: the cue text wins.
        let d = cascade.decide_with_rules_reason(
            "plain post",
            DataClass::Public,
            RulesVerdict::ConfidentYes,
            Some("practice cue; topic 'Health & Wellness'"),
            &mut stats,
            None,
        );
        assert_eq!(d.explanation.stage, CascadeStage::Rules);
        assert_eq!(
            d.explanation.reason,
            "practice cue; topic 'Health & Wellness'"
        );
        assert_eq!(d.explanation.confidence, None);
        // Gates-off rules: the gate that stopped the paid stages.
        let d = cascade.decide(
            "plain post",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        assert_eq!(d.explanation.stage, CascadeStage::Rules);
        assert_eq!(d.explanation.reason, "rules unsure (backend unavailable)");
        let jev = jev_client(closed_url(), false);
        let muse = muse_client(closed_url(), true);
        let gated = Cascade::new(Some(&jev), Some(&muse));
        let d = gated.decide(
            "plain post",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        assert_eq!(d.explanation.reason, "rules unsure (paid off)");
        let jev = jev_client(closed_url(), true);
        let muse = muse_client(closed_url(), true);
        let gated = Cascade::new(Some(&jev), Some(&muse));
        let d = gated.decide(
            "private",
            DataClass::Confidential,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        assert_eq!(d.explanation.reason, "rules unsure (non-public)");
    }

    #[test]
    fn jev_explanation_shows_score_and_threshold() {
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.9));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse)).with_jev_threshold(0.3);
        let mut stats = zero_stats();
        let d = cascade.decide(
            "stretch now",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert_eq!(d.explanation.stage, CascadeStage::Jev);
        assert_eq!(d.explanation.reason, "jev noul=0.90 >= 0.30");
        assert_eq!(d.explanation.confidence, Some(0.9f32));
    }

    /// Muse explanation is sanitized and capped; sets META_API_KEY via EnvGuard so the test cannot hang on a missing key.
    #[test]
    fn muse_explanation_uses_sanitized_capped_reason() {
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        // With a model reason: surfaced, one line.
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
        let (muse_url, _, muse_handle) = mock_server(
            200,
            &muse_body("{\"try_it\":true,\"reason\":\"hands-on  routine\\nwith steps\"}"),
        );
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "desk routine",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        assert_eq!(d.explanation.stage, CascadeStage::Muse);
        assert_eq!(d.explanation.reason, "hands-on routine with steps");
        assert_eq!(d.explanation.confidence, None);
        // Without a model reason: what Muse answered.
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
        let (muse_url, _, muse_handle) = mock_server(200, &muse_body("{\"try_it\":false}"));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "desk routine",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        muse_handle.join().expect("server thread");
        assert_eq!(d.explanation.reason, "muse answered try_it=false");
        // Long reasons cap at 160 chars.
        let long = "w ".repeat(200);
        let capped = sanitize_muse_reason(&long);
        assert!(capped.chars().count() <= MUSE_REASON_MAX_CHARS);
        assert_eq!(MUSE_REASON_MAX_CHARS, 160);
    }

    #[test]
    fn muse_reason_never_carries_secrets_or_links() {
        let token = format!("ghp_{}", "C".repeat(36));
        let raw = format!(
            "rotate {token} now see https://example.invalid/x mail a@b.example @syn_user ok"
        );
        let clean = sanitize_muse_reason(&raw);
        assert!(!clean.contains(&token), "{clean}");
        assert!(!clean.contains("https://"), "{clean}");
        assert!(!clean.contains("a@b.example"), "{clean}");
        assert!(!clean.contains("@syn_user"), "{clean}");
        assert!(clean.contains("[redacted:github_token]"), "{clean}");
        assert!(clean.contains("ok"), "{clean}");
        // A reason that is only a link degrades to the default, never empty.
        let expl = TryItExplanation::muse(true, Some("https://example.invalid/x"));
        assert_eq!(expl.reason, "muse answered try_it=true");
    }

    #[test]
    fn muse_reason_parse_matrix() {
        assert_eq!(
            parse_muse_try_it_with_reason("{\"try_it\":true,\"reason\":\"steps\"}"),
            Some((true, Some("steps".to_string())))
        );
        assert_eq!(
            parse_muse_try_it_with_reason("{\"try_it\":false}"),
            Some((false, None))
        );
        assert_eq!(
            parse_muse_try_it_with_reason("{\"try_it\":true,\"reason\":42}"),
            Some((true, None))
        );
        assert_eq!(parse_muse_try_it_with_reason("no json here"), None);
        // The legacy parser ignores the reason field.
        assert_eq!(
            parse_muse_try_it("{\"try_it\":true,\"reason\":\"steps\"}"),
            Some(true)
        );
    }

    #[test]
    fn fail_closed_explanation_is_error_class_only() {
        for (error, want) in [
            (
                RouterError::Http("http status 429: {\"detail\":\"slow\"}".into()),
                "http status 429",
            ),
            (
                RouterError::Http("http status 500: tokenizer exploded".into()),
                "http status 500",
            ),
            (RouterError::Http("connection refused".into()), "http error"),
            (
                RouterError::BadResponse("muse reply is not strict try_it JSON: nope".into()),
                "bad response",
            ),
            (RouterError::PaidCallsDisabled, "paid calls disabled"),
            (RouterError::MissingEnv("TYPESAFE_API_KEY"), "missing env"),
            (
                RouterError::Refused("JEV_BACKEND=auto needs more".into()),
                "refused",
            ),
        ] {
            let expl = TryItExplanation::fail_closed(&error);
            assert_eq!(expl.stage, CascadeStage::FailClosed);
            assert_eq!(expl.reason, want, "error {error:?}");
            assert_eq!(expl.confidence, None);
        }
        // End to end: a 500 with a body fails closed without the body.
        let (jev_url, _, jev_handle) =
            mock_server_seq(vec![(500, "{\"detail\":\"synthetic boom\"}".to_string())]);
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "stretch now",
            DataClass::Public,
            RulesVerdict::Unsure,
            &mut stats,
            None,
        );
        jev_handle.join().expect("server thread");
        assert_eq!(d.stage, CascadeStage::FailClosed);
        assert_eq!(d.explanation.reason, "http status 500");
        assert!(
            !d.explanation.reason.contains("boom"),
            "{}",
            d.explanation.reason
        );
    }

    #[test]
    fn record_decision_shape_has_no_post_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        let expl = TryItExplanation::jev(0.9, 0.3);
        let entry = record_decision(&mut log, "syn-1", &expl).unwrap();
        assert_eq!(entry.kind, DECIDED_KIND);
        assert_eq!(DECIDED_KIND, "try_it.decided");
        assert_eq!(entry.payload["post_id"], "syn-1");
        assert_eq!(entry.payload["stage"], "jev");
        assert_eq!(entry.payload["reason"], "jev noul=0.90 >= 0.30");
        let raw = entry.payload.to_string();
        assert!(!raw.contains("stretch"), "{raw}");
        assert!(record_decision(&mut log, "  ", &expl).is_err());
    }
}
