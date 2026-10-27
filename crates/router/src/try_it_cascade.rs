//! "Try it" cascade: cheap rules first, then Jev, then Muse Spark.
//!
//! Stage 0 (topic filter) runs before everything: posts about ecosystems the
//! reader has moved past (see [`crate::topic_filter`]) answer no with no API
//! call, even when the rules verdict is `ConfidentYes`.
//!
//! Stage 1 (rules) is an INPUT ([`RulesVerdict`]) because the router cannot
//! depend on bookmarks-agent: `ConfidentYes` answers yes with no API call.
//! Anything else goes through the paid/privacy gates — both clients present,
//! both paid-allowed, [`DataClass::Public`](crate::data_class::DataClass) —
//! and then:
//!
//! - Stage 2 (Jev): one `noul` question with the same try-it wording the eval
//!   uses ([`TRY_IT_INSTRUCTIONS`]); `noul >=` threshold answers yes
//!   (default [`TRY_IT_THRESHOLD`], overridable per [`Cascade`]).
//! - Stage 3 (Muse Spark): strict-JSON `{"try_it":true|false}` prompt with the
//!   same definition; the reply is parsed leniently (first `{...}`). An
//!   empty/null content with `finish_reason` "length" still fails closed but
//!   is flagged as truncated instead of an ordinary Muse "no" (see
//!   [`CascadeDecision::truncated`] and [`CascadeStats::muse_truncated`]).
//!
//! Fail-closed everywhere: gates-off items fall back to the rules verdict
//! with no call and no error, while backend errors and unparseable replies
//! fall back to the rules verdict with the error recorded. [`Cascade::decide`]
//! never fails, so one bad item (or a rate limit) never aborts a run;
//! callers that must stop on HTTP 429 check
//! [`CascadeDecision::is_rate_limited`].
use crate::data_class::{check_egress, DataClass, Route};
use crate::jev_client::JevClient;
use crate::muse::MuseClient;
use crate::topic_filter::excluded_topic;
use crate::RouterError;
use ancientos_core::log::DecisionLog;

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
/// NaN, or outside [0,1] falls back to [`TRY_IT_THRESHOLD`] (see
/// [`jev_threshold_from_env`]).
pub const ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV: &str = "ANCIENTOS_JEV_TRY_IT_THRESHOLD";

/// Read [`ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV`]: a number in [0,1] wins;
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
/// content, `finish_reason` "length"), while real answers used up to 672
/// completion tokens. The brief (700), deeper, and delegate caps are
/// separate and unchanged.
pub const TRY_IT_MUSE_MAX_COMPLETION_TOKENS: u32 = 1024;

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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One item's decision: the answer, the stage that gave it, the backend
/// error when the stage is [`CascadeStage::FailClosed`], and whether the
/// Muse reply was length-truncated (empty/null content with `finish_reason`
/// "length"). A truncation still fails closed exactly like any other error;
/// the flag only records it distinctly instead of an ordinary Muse "no".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeDecision {
    pub try_it: bool,
    pub stage: CascadeStage,
    pub error: Option<String>,
    pub truncated: bool,
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
}

impl<'a> Cascade<'a> {
    pub fn new(jev: Option<&'a JevClient>, muse: Option<&'a MuseClient>) -> Self {
        Self {
            jev,
            muse,
            jev_threshold: TRY_IT_THRESHOLD,
        }
    }

    /// Override the Jev yes threshold (`noul >= threshold` answers yes).
    /// NaN or outside [0,1] falls back to [`TRY_IT_THRESHOLD`] (fail closed).
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
    // The log is lent to two sequential hosted calls; each `as_deref_mut`
    // reborrow is required (a move would not compile) despite the lint name.
    #[allow(clippy::needless_option_as_deref)]
    pub fn decide(
        &self,
        text: &str,
        class: DataClass,
        rules: RulesVerdict,
        stats: &mut CascadeStats,
        mut log: Option<&mut DecisionLog>,
    ) -> CascadeDecision {
        // Stage 0: excluded topics are never try-it, with no API call —
        // before rules, so even a confident rules yes cannot tag them.
        if excluded_topic(text).is_some() {
            stats.topic_filtered += 1;
            return CascadeDecision {
                try_it: false,
                stage: CascadeStage::TopicFilter,
                error: None,
                truncated: false,
            };
        }
        // Stage 1: confident rules answer yes with no API call.
        if rules == RulesVerdict::ConfidentYes {
            stats.rules_yes += 1;
            return CascadeDecision {
                try_it: true,
                stage: CascadeStage::Rules,
                error: None,
                truncated: false,
            };
        }
        // Gates: both clients present, both paid-allowed, Public only.
        // Anything else falls back to the rules verdict (Unsure => no).
        // A class refusal is logged (route jev: the first hosted stage that
        // would have run) but still returns the rules verdict, never an error.
        let (Some(jev), Some(muse)) = (self.jev, self.muse) else {
            return rules_only(rules);
        };
        if !jev.allow_paid() || !muse.allow_paid() {
            return rules_only(rules);
        }
        if class > DataClass::Public {
            let _ = check_egress(class, Route::Jev, "cascade", log);
            return rules_only(rules);
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
        // Stage 3: Muse Spark. Final answer. `chat` (not `complete`) so a
        // length-truncated reply keeps its finish reason and is flagged as
        // truncated instead of an ordinary Muse "no".
        stats.muse_calls += 1;
        match muse.chat_with_class(class, "cascade", log.as_deref_mut(), &muse_prompt(text)) {
            Ok(reply) => match parse_muse_try_it(&reply.content) {
                Some(answer) => {
                    if answer {
                        stats.muse_yes += 1;
                    }
                    CascadeDecision {
                        try_it: answer,
                        stage: CascadeStage::Muse,
                        error: None,
                        truncated: false,
                    }
                }
                None => {
                    let truncated = reply.finish_reason.as_deref() == Some("length")
                        && reply.content.trim().is_empty();
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

/// Rules-alone decision: no call was made, so no error is recorded.
fn rules_only(rules: RulesVerdict) -> CascadeDecision {
    CascadeDecision {
        try_it: rules.answer(),
        stage: CascadeStage::Rules,
        error: None,
        truncated: false,
    }
}

/// Error fallback: rules verdict with the backend error recorded. A Muse
/// truncation fails closed exactly like any other error; it is only
/// additionally flagged on the decision and counted in
/// [`CascadeStats::muse_truncated`].
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
    CascadeDecision {
        try_it: rules.answer(),
        stage: CascadeStage::FailClosed,
        error: Some(error.to_string()),
        truncated,
    }
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
pub fn muse_prompt(text: &str) -> String {
    format!(
        "Decide whether this X bookmark is try-it for the reader: hands-on practice to try now, \
         research for the reader's personal AI OS project (AncientOS), \
         something to hand off to an agent or bot to review or explore, \
         or learning content whose actionable thing is in the linked page or video. \
         Definition: {TRY_IT_INSTRUCTIONS} \
         Reply with strict JSON {{\"try_it\":true}} or {{\"try_it\":false}} and nothing else.\n\nBookmark text: {text}"
    )
}

/// Lenient strict-JSON parse: the first `{...}` substring (from any `{` to a
/// later `}`) that parses with a boolean `try_it` field wins.
pub fn parse_muse_try_it(content: &str) -> Option<bool> {
    for (start, _) in content.match_indices('{') {
        for (end, _) in content.match_indices('}') {
            if end <= start {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content[start..=end]) {
                if let Some(answer) = value.get("try_it").and_then(|v| v.as_bool()) {
                    return Some(answer);
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
    use crate::jev_client::JevBackend;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Process-wide env guard: every env-mutating test in the crate shares it.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    /// Set process env vars for a test, restoring previous values on drop.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
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
            seen_thread
                .lock()
                .expect("seen lock")
                .push(String::from_utf8_lossy(&rest).into_owned());
            let reason = match status {
                200 => "OK",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Error",
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
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

    fn jev_noul_body(noul: f64) -> String {
        format!(
            "{{\"model\":\"m\",\"answers\":{{\"try_it\":{{\"type\":\"noul\",\"noul\":{noul}}}}},\
             \"usage\":{{\"input_tokens\":10,\"output_tokens\":2}}}}"
        )
    }

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
            }
        );
        assert_eq!(stats.rules_yes, 1);
        assert_eq!(stats.jev_calls, 0);
        assert_eq!(stats.muse_calls, 0);
        assert_eq!(stats.fail_closed, 0);
        assert_eq!(stats.errors, 0);
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
        assert_eq!(TRY_IT_MUSE_MAX_COMPLETION_TOKENS, 1024);
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
        assert_eq!(stats.fail_closed, 1);
    }

    #[test]
    fn muse_empty_length_reply_is_truncated_fail_closed() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[("META_API_KEY", Some("test-key"))]);
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
        let empty_length = muse_body_full(serde_json::json!(""), "length");
        let (muse_url, _, muse_handle) = mock_server(200, &empty_length);
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
        assert_eq!(stats.muse_calls, 1);
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
        // only "length" means the completion budget ran out.
        for (reason, expected) in [("length", true), ("stop", false)] {
            let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.1));
            let null_body = muse_body_full(serde_json::Value::Null, reason);
            let (muse_url, _, muse_handle) = mock_server(200, &null_body);
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
            assert_eq!(stats.muse_calls, 1);
            assert_eq!(stats.muse_truncated, u64::from(expected), "reason {reason}");
            assert_eq!(stats.fail_closed, 1);
        }
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
        // Live mock servers that must see no request: both backends paid
        // and available, yet stage 0 answers before any call. The servers
        // block on accept, so they are left unjoined (no request arrives).
        let (jev_url, jev_seen, _jev_handle) = mock_server(200, &jev_noul_body(0.9));
        let (muse_url, muse_seen, _muse_handle) = mock_server(200, &muse_body("{\"try_it\":true}"));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(muse_url, true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        for text in [
            "OpenClaw setup you can try now: 1) install 2) run it",
            "Try this Hermes Agent quickstart today",
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
        // Bare "Hermes" is not an excluded topic: the cascade proceeds
        // (here, Jev answers yes).
        let (jev_url, _, jev_handle) = mock_server(200, &jev_noul_body(0.9));
        let jev = jev_client(jev_url, true);
        let muse = muse_client(closed_url(), true);
        let cascade = Cascade::new(Some(&jev), Some(&muse));
        let mut stats = zero_stats();
        let d = cascade.decide(
            "Hermes bag review",
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
            assert_eq!(e.kind, crate::data_class::EGRESS_DENIED_KIND);
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
        assert_eq!(entries[0].kind, crate::data_class::EGRESS_ALLOWED_KIND);
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
        assert_eq!(entries[0].kind, crate::data_class::EGRESS_ALLOWED_KIND);
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "jev");
    }
}
