//! Baseten [`MediaBackend`]: Muse Spark Open (`meta-models/muse-spark-open-1026`)
//! over Baseten's OpenAI-compatible chat API, one image per call.
//!
//! Needs [`PaidConfig`] (so `ANCIENTOS_ALLOW_PAID=1`), `BASETEN_API_KEY`
//! (sent as `Authorization: Api-Key …`, never logged), `ANCIENTOS_MEDIA_MODEL`,
//! an https `ANCIENTOS_MEDIA_API_BASE`, and `ANCIENTOS_MEDIA_MAX_TOKENS`; any
//! missing value fails closed. The model always reasons: the answer is
//! `message.content` (reasoning arrives separately and is ignored), parsed by
//! [`parse_findings`]. `reasoning_effort` is `low`, which in a live test cut
//! output from ~1.5K to ~1.1K tokens and latency from 7.7s to 5.3s with the
//! same findings. Usage (prompt + completion tokens) adds to a running total;
//! once it reaches the cap, further items fail before any call.
use crate::analyze_media::{MediaBackend, PaidConfig, RawFinding, ResolvedMedia};
use crate::analyze_media_live::{parse_findings, BASE_ENV_VAR, MODEL_ENV_VAR};
use crate::analyze_media_muse::{parse_token_cap, Usage, MAX_TOKENS_ENV_VAR};
use std::cell::Cell;

/// Env var holding the Baseten API key.
pub const KEY_ENV_VAR: &str = "BASETEN_API_KEY";
/// Completion-token cap per item (reasoning counts toward it).
pub const MAX_COMPLETION_TOKENS: u32 = 4000;
/// Reasoning effort sent with every call.
pub const REASONING_EFFORT: &str = "low";

/// Posts a JSON body to an endpoint with an API key; returns the JSON reply.
pub type Post = Box<dyn Fn(&str, &str, &serde_json::Value) -> Result<serde_json::Value, String>>;

/// The Baseten backend with a running token total.
pub struct BasetenBackend {
    model: String,
    base: String,
    key: String,
    max_tokens: u64,
    used: Cell<Usage>,
    calls: Cell<u64>,
    post: Post,
}

impl std::fmt::Debug for BasetenBackend {
    /// Debug output that never shows the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasetenBackend")
            .field("model", &self.model)
            .field("base", &self.base)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Build the backend from env with real HTTP, failing closed on any missing
/// value. `paid` must come from [`PaidConfig::from_env`].
pub fn baseten_backend(
    env: &dyn Fn(&str) -> Option<String>,
    paid: PaidConfig,
) -> Result<BasetenBackend, String> {
    BasetenBackend::with_post(env, paid, Box::new(http_post))
}

impl BasetenBackend {
    /// Build from env with an injected `post` (tests use a fake).
    pub fn with_post(
        env: &dyn Fn(&str) -> Option<String>,
        _paid: PaidConfig,
        post: Post,
    ) -> Result<Self, String> {
        let get = |k: &str| env(k).filter(|v| !v.trim().is_empty());
        let key = get(KEY_ENV_VAR).ok_or_else(|| format!("set {KEY_ENV_VAR} (fail closed)"))?;
        let model = get(MODEL_ENV_VAR)
            .ok_or_else(|| format!("set {MODEL_ENV_VAR} to the model id (fail closed)"))?;
        let base = get(BASE_ENV_VAR)
            .ok_or_else(|| format!("set {BASE_ENV_VAR} to the API base URL (fail closed)"))?;
        if !base.starts_with("https://") {
            return Err(format!("{BASE_ENV_VAR} must be https (fail closed)"));
        }
        let max_tokens = parse_token_cap(env(MAX_TOKENS_ENV_VAR).as_deref())?;
        Ok(Self {
            model,
            base: base.trim_end_matches('/').to_string(),
            key,
            max_tokens,
            used: Cell::new(Usage::default()),
            calls: Cell::new(0),
            post,
        })
    }

    /// Chat completions endpoint.
    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base)
    }

    /// Tokens used so far.
    pub fn used(&self) -> Usage {
        self.used.get()
    }

    /// Request body for one item: prompt plus image URL, low reasoning effort.
    pub fn body_for(&self, item: &ResolvedMedia) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_COMPLETION_TOKENS,
            "reasoning_effort": REASONING_EFFORT,
            "messages": crate::analyze_media_live::messages_for(item),
        })
    }
}

/// Pull `(content, usage)` from a chat completion; a truncated reply
/// (`finish_reason` = `length`) or empty content is an error.
pub fn parse_reply(v: &serde_json::Value) -> Result<(String, Usage), String> {
    let u = &v["usage"];
    let usage = Usage {
        input: u["prompt_tokens"].as_u64().unwrap_or(0),
        output: u["completion_tokens"].as_u64().unwrap_or(0),
    };
    let choice = &v["choices"][0];
    if choice["finish_reason"].as_str() == Some("length") {
        return Err("reply cut off at the token limit".into());
    }
    let content = choice["message"]["content"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("no content in reply")?;
    Ok((content.to_string(), usage))
}

impl MediaBackend for BasetenBackend {
    /// Check the cap, make one call, add usage, parse findings.
    fn analyze(&self, item: &ResolvedMedia) -> Result<Vec<RawFinding>, String> {
        let before = self.used.get();
        if before.input + before.output >= self.max_tokens {
            return Err(format!("token cap {} reached", self.max_tokens));
        }
        let reply = (self.post)(&self.endpoint(), &self.key, &self.body_for(item))?;
        self.calls.set(self.calls.get() + 1);
        let u = Usage {
            input: reply["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output: reply["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        };
        self.used.set(Usage {
            input: before.input + u.input,
            output: before.output + u.output,
        });
        parse_findings(&parse_reply(&reply)?.0)
    }

    /// Calls and tokens used so far.
    fn usage_summary(&self) -> Option<String> {
        let u = self.used.get();
        Some(format!(
            "{} call(s), {} input + {} output tokens",
            self.calls.get(),
            u.input,
            u.output
        ))
    }
}

/// POST with `Authorization: Api-Key`; errors never include the key.
fn http_post(url: &str, key: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let resp = ureq::post(url)
        .timeout(std::time::Duration::from_secs(120))
        .set("Authorization", &format!("Api-Key {key}"))
        .send_json(body.clone());
    match resp {
        Ok(r) => r.into_json().map_err(|e| format!("bad JSON reply: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("Baseten HTTP {code}")),
        Err(e) => Err(format!("Baseten request failed: {}", e.kind())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze_media::Provenance;
    use std::rc::Rc;

    /// Test helper: env with all required values.
    fn env(k: &str) -> Option<String> {
        match k {
            KEY_ENV_VAR => Some("sk-test".into()),
            MODEL_ENV_VAR => Some("meta-models/muse-spark-open-1026".into()),
            BASE_ENV_VAR => Some("https://inference.baseten.co/v1/".into()),
            MAX_TOKENS_ENV_VAR => Some("2000".into()),
            _ => None,
        }
    }

    /// Test helper: paid config.
    fn paid() -> PaidConfig {
        PaidConfig::from_values(Some("1"), Some("5")).unwrap()
    }

    /// Test helper: one resolved image.
    fn item() -> ResolvedMedia {
        ResolvedMedia {
            provenance: Provenance {
                post_id: "1".into(),
                media_key: "3_1".into(),
                media_url: "https://pbs.twimg.com/media/a.jpg".into(),
                frame_idx: None,
            },
            kind: "photo".into(),
            alt_text: None,
        }
    }

    /// Test helper: a completion reply.
    fn reply(content: &str, finish: &str) -> serde_json::Value {
        serde_json::json!({
            "choices": [{"finish_reason": finish, "message": {"content": content, "reasoning_content": "thinking"}}],
            "usage": {"prompt_tokens": 1590, "completion_tokens": 1149}
        })
    }

    #[test]
    /// Each required env value missing fails closed; http base refused.
    fn missing_env_fails_closed() {
        for k in [KEY_ENV_VAR, MODEL_ENV_VAR, BASE_ENV_VAR, MAX_TOKENS_ENV_VAR] {
            let e = BasetenBackend::with_post(
                &|x: &str| if x == k { None } else { env(x) },
                paid(),
                Box::new(|_, _, _| panic!("no call")),
            )
            .unwrap_err();
            assert!(e.contains("fail closed"), "{k}: {e}");
        }
        let e = BasetenBackend::with_post(
            &|x: &str| if x == BASE_ENV_VAR { Some("http://x".into()) } else { env(x) },
            paid(),
            Box::new(|_, _, _| panic!("no call")),
        )
        .unwrap_err();
        assert!(e.contains("https"));
    }

    #[test]
    /// Body carries model, low effort, image URL; key goes to post, not Debug.
    fn sends_image_and_parses_content_not_reasoning() {
        let seen = Rc::new(std::cell::RefCell::new(None));
        let s2 = seen.clone();
        let b = BasetenBackend::with_post(
            &env,
            paid(),
            Box::new(move |url, key, body| {
                *s2.borrow_mut() = Some((url.to_string(), key.to_string(), body.clone()));
                Ok(reply(
                    r#"{"findings":[{"kind":"text","text":"Hi","quote":"Hi","region":null}]}"#,
                    "stop",
                ))
            }),
        )
        .unwrap();
        assert!(!format!("{b:?}").contains("sk-test"));
        let f = b.analyze(&item()).unwrap();
        assert_eq!(f.len(), 1);
        let (url, key, body) = seen.borrow().clone().unwrap();
        assert_eq!(url, "https://inference.baseten.co/v1/chat/completions");
        assert_eq!(key, "sk-test");
        assert_eq!(body["model"], "meta-models/muse-spark-open-1026");
        assert_eq!(body["reasoning_effort"], "low");
        assert!(body.to_string().contains("pbs.twimg.com/media/a.jpg"));
        assert_eq!(b.used(), Usage { input: 1590, output: 1149 });
    }

    #[test]
    /// Truncated or empty replies are errors; cap stops the next call.
    fn truncation_errors_and_cap_stops() {
        assert!(parse_reply(&reply("{", "length")).is_err());
        assert!(parse_reply(&reply("", "stop")).is_err());
        let b = BasetenBackend::with_post(
            &env,
            paid(),
            Box::new(|_, _, _| Ok(reply(r#"{"findings":[]}"#, "stop"))),
        )
        .unwrap();
        assert!(b.analyze(&item()).unwrap().is_empty());
        let e = b.analyze(&item()).unwrap_err();
        assert!(e.contains("token cap"));
    }
}
