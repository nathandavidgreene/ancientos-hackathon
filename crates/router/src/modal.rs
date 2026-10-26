//! Modal-hosted Llama client stub: an optional future target, not used by
//! default dispatch (sensitive data routes to [`crate::local_llm`]).
//!
//! Config comes only from `MODAL_*` env vars (`MODAL_LLAMA_URL`,
//! `MODAL_TOKEN_ID`, `MODAL_TOKEN_SECRET`) plus the paid switch
//! `ANCIENTOS_ALLOW_PAID` (default off). Fails closed: paid switch off,
//! any missing var, or a non-https URL is an error. The HTTP call itself is
//! not wired yet, so a fully configured client still returns NotImplemented
//! and never touches the network.
use crate::{Destination, ModelClient, RouterError};

#[derive(Debug, Clone)]
pub struct ModalLlamaClient {
    url: Option<String>,
    token_id_set: bool,
    token_secret_set: bool,
    allow_paid: bool,
}

impl ModalLlamaClient {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build from any key lookup (tests use this to avoid touching env).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let nonempty = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        Self {
            url: nonempty("MODAL_LLAMA_URL"),
            token_id_set: nonempty("MODAL_TOKEN_ID").is_some(),
            token_secret_set: nonempty("MODAL_TOKEN_SECRET").is_some(),
            allow_paid: get("ANCIENTOS_ALLOW_PAID").as_deref() == Some("1"),
        }
    }

    pub fn allow_paid(&self) -> bool {
        self.allow_paid
    }

    /// Check config in fail-closed order; returns the endpoint when usable.
    pub fn check(&self) -> Result<&str, RouterError> {
        if !self.allow_paid {
            return Err(RouterError::PaidCallsDisabled);
        }
        let url = self
            .url
            .as_deref()
            .ok_or(RouterError::MissingEnv("MODAL_LLAMA_URL"))?;
        if !url.starts_with("https://") {
            return Err(RouterError::BadResponse(
                "MODAL_LLAMA_URL must be https".into(),
            ));
        }
        if !self.token_id_set {
            return Err(RouterError::MissingEnv("MODAL_TOKEN_ID"));
        }
        if !self.token_secret_set {
            return Err(RouterError::MissingEnv("MODAL_TOKEN_SECRET"));
        }
        Ok(url)
    }
}

impl ModelClient for ModalLlamaClient {
    fn destination(&self) -> Destination {
        Destination::LocalLlama
    }
    fn complete(&self, _prompt: &str) -> Result<String, RouterError> {
        self.check()?;
        // TODO: POST to the Modal endpoint once deployed (paid, behind switch).
        Err(RouterError::NotImplemented(Destination::LocalLlama))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn client(pairs: &[(&str, &str)]) -> ModalLlamaClient {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ModalLlamaClient::from_lookup(|k| m.get(k).cloned())
    }

    const FULL: [(&str, &str); 4] = [
        ("MODAL_LLAMA_URL", "https://example.invalid/llama"),
        ("MODAL_TOKEN_ID", "id"),
        ("MODAL_TOKEN_SECRET", "secret"),
        ("ANCIENTOS_ALLOW_PAID", "1"),
    ];

    #[test]
    fn paid_switch_defaults_off() {
        let c = client(&FULL[..3]);
        assert!(!c.allow_paid());
        assert!(matches!(
            c.complete("x"),
            Err(RouterError::PaidCallsDisabled)
        ));
    }

    #[test]
    fn missing_vars_fail_closed() {
        let c = client(&[("ANCIENTOS_ALLOW_PAID", "1")]);
        assert!(matches!(
            c.complete("x"),
            Err(RouterError::MissingEnv("MODAL_LLAMA_URL"))
        ));
        let c = client(&[FULL[0], FULL[3]]);
        assert!(matches!(
            c.complete("x"),
            Err(RouterError::MissingEnv("MODAL_TOKEN_ID"))
        ));
        let c = client(&[FULL[0], FULL[1], ("MODAL_TOKEN_SECRET", " "), FULL[3]]);
        assert!(matches!(
            c.complete("x"),
            Err(RouterError::MissingEnv("MODAL_TOKEN_SECRET"))
        ));
    }

    #[test]
    fn rejects_plain_http() {
        let c = client(&[("MODAL_LLAMA_URL", "http://x"), FULL[1], FULL[2], FULL[3]]);
        assert!(matches!(c.complete("x"), Err(RouterError::BadResponse(_))));
    }

    #[test]
    fn configured_stub_is_not_implemented_and_local() {
        let c = client(&FULL);
        assert_eq!(c.destination(), Destination::LocalLlama);
        assert!(c.check().is_ok());
        assert!(matches!(
            c.complete("x"),
            Err(RouterError::NotImplemented(Destination::LocalLlama))
        ));
    }

    #[test]
    fn paid_switch_requires_exact_one() {
        let c = client(&[FULL[0], FULL[1], FULL[2], ("ANCIENTOS_ALLOW_PAID", "true")]);
        assert!(matches!(
            c.complete("x"),
            Err(RouterError::PaidCallsDisabled)
        ));
    }
}
