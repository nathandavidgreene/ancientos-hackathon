//! Real Meta Model API client for Muse Spark (OpenAI-compatible chat API).
//!
//! The API key is read from `META_API_KEY` at call time and never stored on
//! the client, logged, or printed. Calls fail closed: they are refused unless
//! the paid-call switch `ANCIENTOS_ALLOW_PAID` equals `"1"` and a key is set.

use crate::{Destination, ModelClient, RouterError};

/// Default API base; override with `META_API_BASE`.
pub const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";
/// Default model; override with `MUSE_MODEL`.
pub const DEFAULT_MODEL: &str = "muse-spark-1.3";
/// Default cap for completion tokens when not configured.
pub const DEFAULT_MAX_COMPLETION_TOKENS: u32 = 256;
/// Default reasoning effort. Muse Spark always reasons, so this is never "none".
pub const DEFAULT_REASONING_EFFORT: &str = "low";

/// Token usage reported by a chat completion response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuseUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl std::fmt::Display for MuseUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "prompt={} completion={} total={}",
            fmt_opt(self.prompt_tokens),
            fmt_opt(self.completion_tokens),
            fmt_opt(self.total_tokens),
        )
    }
}

fn fmt_opt(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}

/// Parsed chat completion: reply text plus model id, usage, and finish reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuseChatResponse {
    pub content: String,
    pub model: String,
    pub usage: Option<MuseUsage>,
    /// `choices[0].finish_reason` (`"stop"`, `"length"`, ...); `None` when
    /// missing or null. Callers that only need text keep using
    /// [`ModelClient::complete`]; truncation handling reads this field.
    pub finish_reason: Option<String>,
}

/// Muse Spark client. Holds no secrets, so `Debug` is safe to print.
#[derive(Debug, Clone)]
pub struct MuseClient {
    model: String,
    base_url: String,
    max_completion_tokens: u32,
    reasoning_effort: String,
    allow_paid: bool,
}

impl MuseClient {
    /// Build from env: `MUSE_MODEL`, `META_API_BASE`, `MUSE_REASONING_EFFORT`,
    /// `ANCIENTOS_ALLOW_PAID`. The key itself is read lazily per call, never here.
    pub fn from_env() -> Self {
        Self {
            model: get_env("MUSE_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into()),
            base_url: get_env("META_API_BASE").unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            max_completion_tokens: DEFAULT_MAX_COMPLETION_TOKENS,
            reasoning_effort: normalize_reasoning_effort(get_env("MUSE_REASONING_EFFORT")),
            allow_paid: is_paid_allowed(),
        }
    }

    /// Build with explicit settings (tests, callers that already read env).
    /// Reasoning effort defaults to [`DEFAULT_REASONING_EFFORT`]; use
    /// [`Self::with_reasoning_effort`] to override.
    pub fn new(
        model: impl Into<String>,
        base_url: impl Into<String>,
        max_completion_tokens: u32,
        allow_paid: bool,
    ) -> Self {
        Self {
            model: model.into(),
            base_url: base_url.into(),
            max_completion_tokens,
            reasoning_effort: DEFAULT_REASONING_EFFORT.into(),
            allow_paid,
        }
    }

    /// Override the completion-token cap.
    pub fn with_max_completion_tokens(mut self, n: u32) -> Self {
        self.max_completion_tokens = n;
        self
    }

    /// Override the reasoning effort. `"none"` is rejected (Muse Spark always
    /// reasons) and falls back to [`DEFAULT_REASONING_EFFORT`].
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = normalize_reasoning_effort(Some(effort.into()));
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn max_completion_tokens(&self) -> u32 {
        self.max_completion_tokens
    }

    pub fn reasoning_effort(&self) -> &str {
        &self.reasoning_effort
    }

    pub fn allow_paid(&self) -> bool {
        self.allow_paid
    }

    /// Full chat-completions endpoint for the configured base URL.
    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Request body for a single user prompt.
    pub fn request_body(&self, prompt: &str) -> serde_json::Value {
        self.request_body_messages(serde_json::json!([{"role": "user", "content": prompt}]))
    }

    /// Request body for an explicit multi-turn message list (continuations).
    pub fn request_body_messages(&self, messages: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "messages": messages,
            "max_completion_tokens": self.max_completion_tokens,
            "reasoning_effort": self.reasoning_effort,
        })
    }

    /// Send one prompt and parse the full response (content, model, usage).
    /// Fails closed without the paid switch or the API key; never touches
    /// the network in those cases.
    pub fn chat(&self, prompt: &str) -> Result<MuseChatResponse, RouterError> {
        self.chat_messages(serde_json::json!([{"role": "user", "content": prompt}]))
    }

    /// Send an explicit multi-turn message list and parse the full response.
    /// Same fail-closed rules as [`Self::chat`]; existing single-prompt
    /// callers keep using [`Self::chat`] unchanged.
    pub fn chat_messages(
        &self,
        messages: serde_json::Value,
    ) -> Result<MuseChatResponse, RouterError> {
        if !self.allow_paid {
            return Err(RouterError::PaidCallsDisabled);
        }
        // Key lives in env only; an empty value counts as missing.
        let api_key = get_env("META_API_KEY").ok_or(RouterError::MissingEnv("META_API_KEY"))?;
        let body = self.request_body_messages(messages);
        let raw: serde_json::Value = ureq::post(&self.endpoint())
            .timeout(std::time::Duration::from_secs(60))
            .set("Authorization", &format!("Bearer {api_key}"))
            .send_json(&body)
            .map_err(map_ureq_error)?
            .into_json()
            .map_err(|e| RouterError::Http(e.to_string()))?;
        Self::parse_chat_response(&raw)
    }

    /// Parse an OpenAI-compatible chat completion value.
    /// Reads `choices[0].message.content` plus `model`, `usage`, and
    /// `finish_reason`. A null content (reasoning tokens consumed the budget,
    /// typically with `finish_reason` "length") is a clear `BadResponse`
    /// naming the reason.
    pub fn parse_chat_response(value: &serde_json::Value) -> Result<MuseChatResponse, RouterError> {
        let choice = value
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| RouterError::BadResponse("missing choices[0]".into()))?;
        let content = match choice.get("message").and_then(|m| m.get("content")) {
            Some(v) => v.as_str().ok_or_else(|| {
                let reason = choice
                    .get("finish_reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown");
                if v.is_null() {
                    RouterError::BadResponse(format!(
                        "response content is null (finish_reason: \"{reason}\"); \
                         reasoning tokens may have exhausted max_completion_tokens; \
                         raise the cap or lower reasoning_effort"
                    ))
                } else {
                    RouterError::BadResponse(format!(
                        "choices[0].message.content is not a string (finish_reason: \"{reason}\")"
                    ))
                }
            })?,
            None => {
                let reason = choice
                    .get("finish_reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown");
                return Err(RouterError::BadResponse(format!(
                    "missing choices[0].message.content (finish_reason: \"{reason}\")"
                )));
            }
        };
        let model = value
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let usage = value.get("usage").map(|u| MuseUsage {
            prompt_tokens: u.get("prompt_tokens").and_then(as_u64),
            completion_tokens: u.get("completion_tokens").and_then(as_u64),
            total_tokens: u.get("total_tokens").and_then(as_u64),
        });
        let finish_reason = choice
            .get("finish_reason")
            .and_then(|r| r.as_str())
            .map(str::to_string);
        Ok(MuseChatResponse {
            content: content.to_string(),
            model,
            usage,
            finish_reason,
        })
    }
}

fn as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
}

impl ModelClient for MuseClient {
    fn destination(&self) -> Destination {
        Destination::MuseSpark
    }

    fn complete(&self, prompt: &str) -> Result<String, RouterError> {
        self.chat(prompt).map(|r| r.content)
    }
}

fn get_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Sanitize a reasoning-effort value. Empty, missing, or "none" (any case)
/// falls back to [`DEFAULT_REASONING_EFFORT`]; Muse Spark always reasons.
fn normalize_reasoning_effort(value: Option<String>) -> String {
    match value.map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("none") => s,
        _ => DEFAULT_REASONING_EFFORT.to_string(),
    }
}

fn is_paid_allowed() -> bool {
    std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn map_ureq_error(e: ureq::Error) -> RouterError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(500).collect();
            RouterError::Http(format!("http status {code}: {snippet}"))
        }
        other => RouterError::Http(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serialise tests that mutate process env vars.
    fn env_lock() -> MutexGuard<'static, ()> {
        static ENV_MUTEX: Mutex<()> = Mutex::new(());
        ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_client(allow_paid: bool) -> MuseClient {
        MuseClient::new("test-model", "http://127.0.0.1:9", 16, allow_paid)
    }

    #[test]
    fn paid_calls_disabled_by_default() {
        let _guard = env_lock();
        let saved = std::env::var("ANCIENTOS_ALLOW_PAID").ok();
        std::env::remove_var("ANCIENTOS_ALLOW_PAID");

        let client = MuseClient::from_env();
        assert!(!client.allow_paid());
        assert!(matches!(
            client.complete("hi"),
            Err(RouterError::PaidCallsDisabled)
        ));
        assert!(matches!(
            client.chat("hi"),
            Err(RouterError::PaidCallsDisabled)
        ));
        assert!(matches!(
            test_client(false).complete("hi"),
            Err(RouterError::PaidCallsDisabled)
        ));

        if let Some(v) = saved {
            std::env::set_var("ANCIENTOS_ALLOW_PAID", v);
        }
    }

    #[test]
    fn missing_key_fails_closed_without_network() {
        let _guard = env_lock();
        let saved = std::env::var("META_API_KEY").ok();
        std::env::remove_var("META_API_KEY");

        // Unroutable base URL proves no network is attempted: a missing key
        // must fail before any HTTP call.
        let err = test_client(true).complete("hi").unwrap_err();
        assert!(
            matches!(err, RouterError::MissingEnv("META_API_KEY")),
            "unexpected: {err:?}"
        );

        if let Some(v) = saved {
            std::env::set_var("META_API_KEY", v);
        }
    }

    #[test]
    fn parses_sample_chat_response() {
        let raw = serde_json::json!({
            "id": "chatcmpl-123",
            "model": "muse-spark-1.3",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 2,
                "total_tokens": 11
            }
        });
        let resp = MuseClient::parse_chat_response(&raw).unwrap();
        assert_eq!(resp.content, "ok");
        assert_eq!(resp.model, "muse-spark-1.3");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            resp.usage,
            Some(MuseUsage {
                prompt_tokens: Some(9),
                completion_tokens: Some(2),
                total_tokens: Some(11),
            })
        );
    }

    #[test]
    fn exposes_finish_reason_for_truncation() {
        let length = serde_json::json!({
            "model": "muse-spark-1.3",
            "choices": [{
                "message": {"role": "assistant", "content": "part one "},
                "finish_reason": "length"
            }],
        });
        let resp = MuseClient::parse_chat_response(&length).unwrap();
        assert_eq!(resp.finish_reason.as_deref(), Some("length"));

        let missing = serde_json::json!({
            "model": "muse-spark-1.3",
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
        });
        let resp = MuseClient::parse_chat_response(&missing).unwrap();
        assert_eq!(resp.finish_reason, None);

        let null_reason = serde_json::json!({
            "model": "muse-spark-1.3",
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": null
            }],
        });
        let resp = MuseClient::parse_chat_response(&null_reason).unwrap();
        assert_eq!(resp.finish_reason, None);
    }

    #[test]
    fn rejects_response_without_choices() {
        for raw in [
            serde_json::json!({"model": "m"}),
            serde_json::json!({"choices": []}),
            serde_json::json!({"choices": [{"message": {}}]}),
            serde_json::json!({"choices": [{"message": {"content": 42}}]}),
        ] {
            assert!(
                matches!(
                    MuseClient::parse_chat_response(&raw),
                    Err(RouterError::BadResponse(_))
                ),
                "unexpected ok for {raw}"
            );
        }
    }

    #[test]
    fn builds_openai_compatible_body() {
        let body = test_client(true).request_body("hello");
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert_eq!(body["max_completion_tokens"], 16);
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn builds_multi_turn_continuation_body() {
        let messages = serde_json::json!([
            {"role": "user", "content": "original"},
            {"role": "assistant", "content": "part one "},
            {"role": "user", "content": "Continue exactly where you stopped."},
        ]);
        let body = test_client(true).request_body_messages(messages);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"], "part one ");
        assert_eq!(
            body["messages"][2]["content"],
            "Continue exactly where you stopped."
        );
        assert_eq!(body["max_completion_tokens"], 16);
    }

    #[test]
    fn reasoning_effort_defaults_to_low() {
        let _guard = env_lock();
        let saved = std::env::var("MUSE_REASONING_EFFORT").ok();
        std::env::remove_var("MUSE_REASONING_EFFORT");

        assert_eq!(MuseClient::from_env().reasoning_effort(), "low");
        assert_eq!(test_client(true).reasoning_effort(), "low");

        if let Some(v) = saved {
            std::env::set_var("MUSE_REASONING_EFFORT", v);
        }
    }

    #[test]
    fn reasoning_effort_env_and_none_sanitized() {
        let _guard = env_lock();
        let saved = std::env::var("MUSE_REASONING_EFFORT").ok();

        std::env::set_var("MUSE_REASONING_EFFORT", "medium");
        assert_eq!(MuseClient::from_env().reasoning_effort(), "medium");

        for bad in ["none", "NONE", " None ", ""] {
            if bad.is_empty() {
                std::env::remove_var("MUSE_REASONING_EFFORT");
            } else {
                std::env::set_var("MUSE_REASONING_EFFORT", bad);
            }
            assert_eq!(
                MuseClient::from_env().reasoning_effort(),
                "low",
                "input {bad:?} must fall back to low"
            );
        }

        assert_eq!(
            test_client(true)
                .with_reasoning_effort("none")
                .reasoning_effort(),
            "low"
        );
        let body = test_client(true)
            .with_reasoning_effort("high")
            .request_body("hi");
        assert_eq!(body["reasoning_effort"], "high");

        match saved {
            Some(v) => std::env::set_var("MUSE_REASONING_EFFORT", v),
            None => std::env::remove_var("MUSE_REASONING_EFFORT"),
        }
    }

    #[test]
    fn null_content_mentions_finish_reason() {
        let raw = serde_json::json!({
            "model": "muse-spark-1.3",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": null},
                "finish_reason": "length"
            }],
            "usage": {"prompt_tokens": 9, "completion_tokens": 16, "total_tokens": 25}
        });
        let err = MuseClient::parse_chat_response(&raw).unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, RouterError::BadResponse(_)),
            "unexpected: {msg}"
        );
        assert!(msg.contains("finish_reason"), "missing reason: {msg}");
        assert!(msg.contains("length"), "missing length: {msg}");
    }

    #[test]
    fn missing_content_without_reason_still_fails_closed() {
        let raw = serde_json::json!({
            "choices": [{"message": {}}],
        });
        let err = MuseClient::parse_chat_response(&raw).unwrap_err();
        assert!(matches!(err, RouterError::BadResponse(_)));
        assert!(err.to_string().contains("finish_reason"));
    }

    #[test]
    fn endpoint_appends_chat_completions() {
        assert_eq!(
            test_client(true).endpoint(),
            "http://127.0.0.1:9/chat/completions"
        );
        let trailing = MuseClient::new("m", "https://api.meta.ai/v1/", 8, true);
        assert_eq!(
            trailing.endpoint(),
            "https://api.meta.ai/v1/chat/completions"
        );
    }

    #[test]
    fn reports_muse_spark_destination() {
        assert_eq!(test_client(false).destination(), Destination::MuseSpark);
    }

    #[test]
    fn debug_output_holds_no_key_material() {
        // The client stores no key, so Debug can never leak one.
        let dbg = format!("{:?}", test_client(true));
        assert!(!dbg.contains("META_API_KEY"));
        assert!(!dbg.contains("Bearer"));
    }
}
