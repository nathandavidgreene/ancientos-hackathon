//! Live [`MediaBackend`] for `analyze_media` over the Meta Model API.
//!
//! Reuses [`MuseClient`] (key from `META_API_KEY` at call time, never
//! stored). Needs `ANCIENTOS_MEDIA_MODEL` and `ANCIENTOS_MEDIA_API_BASE`
//! (no defaults are guessed) plus a [`PaidConfig`], so construction itself
//! proves the paid switch and cap were checked. Missing env fails closed.
//!
//! The model must answer with JSON only:
//! `{"findings":[{"kind":"text|claim|description","text":"…",
//! "quote":"…"?,"region":{"x":…,"y":…,"w":…,"h":…}?}]}`. Anything else is a
//! backend error (the item is skipped and logged); evidence is then enforced
//! by [`crate::analyze_media::check`].
use crate::analyze_media::{MediaBackend, PaidConfig, RawFinding, ResolvedMedia};
use ancientos_router::data_class::DataClass;
use ancientos_router::muse::MuseClient;
use serde::Deserialize;

/// Model id for the media route; required.
pub const MODEL_ENV_VAR: &str = "ANCIENTOS_MEDIA_MODEL";
/// API base (e.g. `https://…/v1`) for the media route; required.
pub const BASE_ENV_VAR: &str = "ANCIENTOS_MEDIA_API_BASE";
/// Completion-token cap per media item.
pub const MAX_COMPLETION_TOKENS: u32 = 800;
/// Most findings accepted from one response.
pub const MAX_FINDINGS: usize = 20;

/// Instruction sent with every image.
pub const PROMPT: &str = "You read one image from a public social post. Reply with JSON only, \
no prose: {\"findings\":[{\"kind\":\"text|claim|description\",\"text\":string,\
\"quote\":string|null,\"region\":{\"x\":0-1,\"y\":0-1,\"w\":0-1,\"h\":0-1}|null}]}. \
Every text or claim finding MUST include the exact words visible in the image as \
quote, or a region box around them. Omit anything you cannot point to.";

/// Live backend: one chat call per media item with the image URL attached.
#[derive(Debug, Clone)]
pub struct LiveBackend {
    client: MuseClient,
}

/// Build the live backend from env, failing closed on a missing model id or
/// API base. `paid` must come from [`PaidConfig::from_env`].
pub fn live_backend(
    env: &dyn Fn(&str) -> Option<String>,
    _paid: PaidConfig,
) -> Result<LiveBackend, String> {
    let get = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    let model = get(MODEL_ENV_VAR)
        .ok_or_else(|| format!("set {MODEL_ENV_VAR} to the vision model id (fail closed)"))?;
    let base = get(BASE_ENV_VAR)
        .ok_or_else(|| format!("set {BASE_ENV_VAR} to the API base URL (fail closed)"))?;
    if !base.starts_with("https://") {
        return Err(format!("{BASE_ENV_VAR} must be https (fail closed)"));
    }
    Ok(LiveBackend {
        client: MuseClient::new(model, base, MAX_COMPLETION_TOKENS, true),
    })
}

impl LiveBackend {
    /// Model id this backend calls.
    pub fn model(&self) -> &str {
        self.client.model()
    }

    /// Endpoint this backend calls.
    pub fn endpoint(&self) -> String {
        self.client.endpoint()
    }
}

/// Chat messages for one item: prompt text plus the image URL part.
pub fn messages_for(item: &ResolvedMedia) -> serde_json::Value {
    serde_json::json!([{
        "role": "user",
        "content": [
            {"type": "text", "text": PROMPT},
            {"type": "image_url", "image_url": {"url": item.provenance.media_url}}
        ]
    }])
}

/// Strict response shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    findings: Vec<RawFinding>,
}

/// Parse the model's reply into raw findings. Accepts a bare JSON object or
/// one wrapped in a ```json fence; rejects prose, unknown fields, and more
/// than [`MAX_FINDINGS`] findings.
pub fn parse_findings(content: &str) -> Result<Vec<RawFinding>, String> {
    let mut s = content.trim();
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        s = rest
            .strip_suffix("```")
            .ok_or("unterminated code fence")?
            .trim();
    }
    let env: Envelope =
        serde_json::from_str(s).map_err(|e| format!("response is not findings JSON: {e}"))?;
    if env.findings.len() > MAX_FINDINGS {
        return Err(format!("more than {MAX_FINDINGS} findings"));
    }
    Ok(env.findings)
}

impl MediaBackend for LiveBackend {
    /// One paid call for the item; the reply must parse as findings JSON.
    fn analyze(&self, item: &ResolvedMedia) -> Result<Vec<RawFinding>, String> {
        let resp = self
            .client
            .chat_messages_with_class(DataClass::Public, "analyze_media", None, messages_for(item))
            .map_err(|e| e.to_string())?;
        parse_findings(&resp.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze_media::{check, FindingKind, Provenance};

    /// Test helper: env lookup from pairs.
    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(a, _)| *a == k)
                .map(|(_, v)| v.to_string())
        }
    }

    /// Test helper: paid config.
    fn paid() -> PaidConfig {
        PaidConfig { max_items: 1 }
    }

    #[test]
    fn missing_env_fails_closed() {
        assert!(live_backend(&env_of(&[]), paid())
            .unwrap_err()
            .contains(MODEL_ENV_VAR));
        assert!(live_backend(&env_of(&[(MODEL_ENV_VAR, "m")]), paid())
            .unwrap_err()
            .contains(BASE_ENV_VAR));
        assert!(live_backend(
            &env_of(&[(MODEL_ENV_VAR, "m"), (BASE_ENV_VAR, "http://x")]),
            paid()
        )
        .is_err());
        let b = live_backend(
            &env_of(&[
                (MODEL_ENV_VAR, "synthetic-vision"),
                (BASE_ENV_VAR, "https://api.invalid/v1"),
            ]),
            paid(),
        )
        .unwrap();
        assert_eq!(b.model(), "synthetic-vision");
        assert_eq!(b.endpoint(), "https://api.invalid/v1/chat/completions");
    }

    #[test]
    fn messages_carry_image_url() {
        let item = ResolvedMedia {
            provenance: Provenance {
                post_id: "demo-1".into(),
                media_key: "3_k".into(),
                media_url: "https://pbs.twimg.com/media/demo.jpg".into(),
                frame_idx: None,
            },
            kind: "photo".into(),
            alt_text: None,
        };
        let m = messages_for(&item);
        assert_eq!(
            m[0]["content"][1]["image_url"]["url"],
            "https://pbs.twimg.com/media/demo.jpg"
        );
    }

    #[test]
    fn parses_recorded_style_reply_and_check_filters_evidence() {
        // Synthetic reply in the recorded chat-completions content shape.
        let reply = "```json\n{\"findings\":[\
            {\"kind\":\"text\",\"text\":\"[synthetic] 10,000 steps\",\"quote\":\"10,000 steps\",\"region\":null},\
            {\"kind\":\"claim\",\"text\":\"[synthetic] walking cures all\",\"quote\":null,\"region\":null},\
            {\"kind\":\"text\",\"text\":\"[synthetic] label\",\"region\":{\"x\":0.1,\"y\":0.2,\"w\":0.3,\"h\":0.1}},\
            {\"kind\":\"description\",\"text\":\"[synthetic] a bar chart\"}]}\n```";
        let f = parse_findings(reply).unwrap();
        assert_eq!(f.len(), 4);
        assert_eq!(f[0].kind, FindingKind::Text);
        let ok: Vec<bool> = f.iter().map(|x| check(x).is_ok()).collect();
        assert_eq!(ok, vec![true, false, true, true]);
    }

    #[test]
    fn rejects_prose_unknown_fields_and_floods() {
        assert!(parse_findings("Sure! Here are the findings.").is_err());
        assert!(parse_findings("{\"findings\":[],\"extra\":1}").is_err());
        assert!(parse_findings("```json\n{\"findings\":[]}").is_err());
        let many = format!(
            "{{\"findings\":[{}]}}",
            vec!["{\"kind\":\"description\",\"text\":\"x\"}"; MAX_FINDINGS + 1].join(",")
        );
        assert!(parse_findings(&many).is_err());
        assert!(parse_findings("{\"findings\":[]}").unwrap().is_empty());
    }
}
