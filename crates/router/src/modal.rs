//! Modal-hosted Llama client stub: an optional future target, not used by
//! default dispatch (sensitive data routes to [`crate::local_llm`]).
//!
//! Config comes only from `MODAL_*` env vars (`MODAL_LLAMA_URL`,
//! `MODAL_TOKEN_ID`, `MODAL_TOKEN_SECRET`) plus the paid switch
//! `ANCIENTOS_ALLOW_PAID` (default off). Fails closed: content above
//! [`DataClass::Public`](crate::data_class::DataClass) is refused before
//! anything else, then paid switch off, any missing var, or a non-https URL
//! is an error. The HTTP call itself is not wired yet, so a fully configured
//! client still returns NotImplemented and never touches the network.
//!
//! Note: this client declares `Destination::LocalLlama` as its dispatch slot
//! but enforces the hosted [`Route::Modal`](crate::data_class::Route)
//! ceiling (`Public`): Modal is a hosted, paid route.
use crate::data_class::{check_egress, DataClass, Route};
use crate::{Destination, ModelClient, RouterError, Sensitivity};
use ancientos_core::log::DecisionLog;

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
    /// Fail-closed: the class is unknown here, so this always refuses with
    /// [`RouterError::EgressDenied`]. Classified callers use
    /// [`Self::complete_with_class`].
    fn complete(&self, _prompt: &str) -> Result<String, RouterError> {
        self.complete_with_class(DataClass::Restricted, "modal.complete", None, _prompt)
    }
    fn complete_with_sensitivity(
        &self,
        s: Sensitivity,
        prompt: &str,
    ) -> Result<String, RouterError> {
        self.complete_with_class(DataClass::from(s), "modal", None, prompt)
    }
    fn complete_with_class(
        &self,
        class: DataClass,
        caller: &str,
        log: Option<&mut DecisionLog>,
        prompt: &str,
    ) -> Result<String, RouterError> {
        let _ = prompt;
        check_egress(class, Route::Modal, caller, log)?;
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

    /// Classified call with Public content and no log.
    fn call_public(c: &ModalLlamaClient) -> Result<String, RouterError> {
        ModelClient::complete_with_class(c, DataClass::Public, "test", None, "x")
    }

    #[test]
    fn paid_switch_defaults_off() {
        let c = client(&FULL[..3]);
        assert!(!c.allow_paid());
        assert!(matches!(
            call_public(&c),
            Err(RouterError::PaidCallsDisabled)
        ));
    }

    #[test]
    fn missing_vars_fail_closed() {
        let c = client(&[("ANCIENTOS_ALLOW_PAID", "1")]);
        assert!(matches!(
            call_public(&c),
            Err(RouterError::MissingEnv("MODAL_LLAMA_URL"))
        ));
        let c = client(&[FULL[0], FULL[3]]);
        assert!(matches!(
            call_public(&c),
            Err(RouterError::MissingEnv("MODAL_TOKEN_ID"))
        ));
        let c = client(&[FULL[0], FULL[1], ("MODAL_TOKEN_SECRET", " "), FULL[3]]);
        assert!(matches!(
            call_public(&c),
            Err(RouterError::MissingEnv("MODAL_TOKEN_SECRET"))
        ));
    }

    #[test]
    fn rejects_plain_http() {
        let c = client(&[("MODAL_LLAMA_URL", "http://x"), FULL[1], FULL[2], FULL[3]]);
        assert!(matches!(call_public(&c), Err(RouterError::BadResponse(_))));
    }

    #[test]
    fn configured_stub_is_not_implemented_and_local() {
        let c = client(&FULL);
        assert_eq!(c.destination(), Destination::LocalLlama);
        assert!(c.check().is_ok());
        assert!(matches!(
            call_public(&c),
            Err(RouterError::NotImplemented(Destination::LocalLlama))
        ));
    }

    #[test]
    fn paid_switch_requires_exact_one() {
        let c = client(&[FULL[0], FULL[1], FULL[2], ("ANCIENTOS_ALLOW_PAID", "true")]);
        assert!(matches!(
            call_public(&c),
            Err(RouterError::PaidCallsDisabled)
        ));
    }

    #[test]
    fn over_ceiling_and_bare_calls_refuse_with_denied_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        // Fully configured: only the class check can refuse.
        let c = client(&FULL);
        for class in [
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            let err =
                ModelClient::complete_with_class(&c, class, "test-modal", Some(&mut log), "canary")
                    .unwrap_err();
            assert!(
                matches!(err, RouterError::EgressDenied { .. }),
                "unexpected: {err:?}"
            );
        }
        // Bare complete() has unknown class: always refused.
        assert!(matches!(
            c.complete("x"),
            Err(RouterError::EgressDenied { .. })
        ));
        assert!(matches!(
            ModelClient::complete_with_sensitivity(&c, Sensitivity::Sensitive, "x"),
            Err(RouterError::EgressDenied { .. })
        ));
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 3);
        for (e, class) in entries
            .iter()
            .zip(["internal", "confidential", "restricted"])
        {
            assert_eq!(e.kind, crate::data_class::EGRESS_DENIED_KIND);
            assert_eq!(e.payload["class"], class);
            assert_eq!(e.payload["route"], "modal");
            assert_eq!(e.payload["caller"], "test-modal");
            let raw = serde_json::to_string(&e.payload).unwrap();
            assert!(!raw.contains("canary"), "{raw}");
        }
    }

    #[test]
    fn public_call_logs_allowed_before_stub_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        let c = client(&FULL);
        // Egress allowed (logged); the stub itself is still unwired.
        assert!(matches!(
            ModelClient::complete_with_class(
                &c,
                DataClass::Public,
                "test-modal",
                Some(&mut log),
                "x"
            ),
            Err(RouterError::NotImplemented(_))
        ));
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, crate::data_class::EGRESS_ALLOWED_KIND);
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "modal");
    }
}
