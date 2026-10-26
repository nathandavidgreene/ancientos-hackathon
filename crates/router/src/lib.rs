//! Privacy router: sensitive data stays on local Llama (Modal); only
//! sanitized data may reach hosted Muse models via the Meta Model API.
//! Network clients are stubs for now; endpoints come from env vars.
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Sensitivity {
    Sensitive,
    Sanitized,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Destination {
    LocalLlama,
    MuseSpark,
    MuseWonder,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("missing env var {0}")]
    MissingEnv(&'static str),
    #[error("backend not implemented: {0:?}")]
    NotImplemented(Destination),
}

pub trait ModelClient {
    fn destination(&self) -> Destination;
    fn complete(&self, prompt: &str) -> Result<String, RouterError>;
}

/// Config read from env: MODAL_LLAMA_URL, META_API_KEY, MUSE_MODEL.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub modal_llama_url: Option<String>,
    pub meta_api_key_set: bool,
    pub muse_model: String,
}

impl RouterConfig {
    pub fn from_env() -> Self {
        let get = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
        Self {
            modal_llama_url: get("MODAL_LLAMA_URL"),
            meta_api_key_set: get("META_API_KEY").is_some(),
            muse_model: get("MUSE_MODEL").unwrap_or_else(|| "muse-spark".into()),
        }
    }
}

/// Decide where data may go. Unknown sensitivity is treated as sensitive.
pub fn route(s: Sensitivity, wants_creative: bool) -> Destination {
    match s {
        Sensitivity::Sanitized if wants_creative => Destination::MuseWonder,
        Sensitivity::Sanitized => Destination::MuseSpark,
        Sensitivity::Sensitive | Sensitivity::Unknown => Destination::LocalLlama,
    }
}

/// Stub client: returns NotImplemented until wired to a real endpoint.
pub struct StubClient(pub Destination);

impl ModelClient for StubClient {
    fn destination(&self) -> Destination {
        self.0
    }
    fn complete(&self, _prompt: &str) -> Result<String, RouterError> {
        Err(RouterError::NotImplemented(self.0))
    }
}

/// Deterministic mock for tests and offline demos.
pub struct MockClient(pub Destination);

impl ModelClient for MockClient {
    fn destination(&self) -> Destination {
        self.0
    }
    fn complete(&self, prompt: &str) -> Result<String, RouterError> {
        Ok(format!("[{:?}] {}", self.0, prompt.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_stays_local() {
        assert_eq!(route(Sensitivity::Unknown, true), Destination::LocalLlama);
        assert_eq!(
            route(Sensitivity::Sensitive, false),
            Destination::LocalLlama
        );
    }

    #[test]
    fn sanitized_goes_to_muse() {
        assert_eq!(route(Sensitivity::Sanitized, false), Destination::MuseSpark);
        assert_eq!(route(Sensitivity::Sanitized, true), Destination::MuseWonder);
    }

    #[test]
    fn stub_is_not_implemented() {
        assert!(StubClient(Destination::MuseSpark).complete("hi").is_err());
    }
}
