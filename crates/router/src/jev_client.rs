//! Jev model backend: TypeSafe frontier API or a local Jev server.
//!
//! Backend selection uses the shared Luna env names. `JEV_BACKEND` picks
//! `auto` (default), `typesafe`, or `localjev`; under `auto`,
//! `LUNA_MODEL_MODE=frontier` selects TypeSafe and `LUNA_MODEL_MODE=local`
//! selects the local server. Any other value, or an unset mode under `auto`,
//! is an error (fail closed).
//!
//! TypeSafe (frontier) is a hosted, paid API: `TYPESAFE_BASE_URL` (default
//! `https://api.typesafe.ai`), `TYPESAFE_API_KEY` (required),
//! `TYPESAFE_DEFAULT_MODEL` (default `jev-preview`). Real calls need the paid
//! switch `ANCIENTOS_ALLOW_PAID=1`, and only `Public` content may go there —
//! anything above the hosted ceiling is refused before any network I/O.
//!
//! LocalJev is a local server in the style of [`crate::local_llm`]:
//! `LOCALJEV_BASE_URL` (default `http://127.0.0.1:8080`),
//! `LOCALJEV_DEFAULT_MODEL` (default `localjev-default`), optional
//! `LOCALJEV_API_KEY` Bearer token. It is not paid-gated, and data stays on
//! the device, so every data class is allowed.
//!
//! Keys are read from env at call time and never stored on the client,
//! logged, or printed. The HTTP request/response shape is shared by both
//! backends in the single function [`post_systemone`].
//!
//! VERIFIED 2026-10-26 against api.typesafe.ai (OpenAPI + one live call):
//! Jev is NOT a chat API. `POST /v1/systemone` takes
//! `{model, state, questions: {name: {type: noul|choice|score, instructions, criteria?}}}`
//! and returns `{model, answers: {name: {type, noul|choice,...}}, usage: {input_tokens, output_tokens}}`.
//! `/v1/chat/completions` returns 404. Use [`JevClient::ask`]; freeform
//! [`JevClient::chat`] fails closed after the usual gates.
use crate::data_class::{check_egress, DataClass, Route};
use crate::{Destination, ModelClient, RouterError, Sensitivity};
use ancientos_core::log::DecisionLog;

/// Backend selector: `auto` (default) | `typesafe` | `localjev`.
pub const JEV_BACKEND_ENV: &str = "JEV_BACKEND";
/// Model mode used when `JEV_BACKEND=auto`: `frontier` or `local`.
pub const LUNA_MODEL_MODE_ENV: &str = "LUNA_MODEL_MODE";

/// TypeSafe env var carrying the API base URL.
pub const TYPESAFE_BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
/// TypeSafe env var carrying the API key (required for real calls).
pub const TYPESAFE_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
/// TypeSafe env var overriding the chat model.
pub const TYPESAFE_MODEL_ENV: &str = "TYPESAFE_DEFAULT_MODEL";
/// Default TypeSafe API base; override with [`TYPESAFE_BASE_URL_ENV`].
pub const TYPESAFE_DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
/// Default TypeSafe model; override with [`TYPESAFE_MODEL_ENV`].
pub const TYPESAFE_DEFAULT_MODEL: &str = "jev-preview";

/// LocalJev env var carrying the server base URL.
pub const LOCALJEV_BASE_URL_ENV: &str = "LOCALJEV_BASE_URL";
/// LocalJev env var overriding the chat model.
pub const LOCALJEV_MODEL_ENV: &str = "LOCALJEV_DEFAULT_MODEL";
/// LocalJev env var carrying an optional Bearer token.
pub const LOCALJEV_API_KEY_ENV: &str = "LOCALJEV_API_KEY";
/// Default local Jev server base; override with [`LOCALJEV_BASE_URL_ENV`].
pub const LOCALJEV_DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";
/// Default local Jev model; override with [`LOCALJEV_MODEL_ENV`].
pub const LOCALJEV_DEFAULT_MODEL: &str = "localjev-default";

/// Paid-call switch shared with the other hosted backends.
const ALLOW_PAID_ENV: &str = "ANCIENTOS_ALLOW_PAID";
const TIMEOUT_SECS: u64 = 60;

/// Which Jev backend a client talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevBackend {
    /// Hosted TypeSafe frontier API. Paid-gated, key required, sanitized only.
    TypeSafe,
    /// Local Jev server. Free, key optional, any sensitivity (stays local).
    LocalJev,
}

impl JevBackend {
    /// Router destination for this backend.
    fn destination(self) -> Destination {
        match self {
            JevBackend::TypeSafe => Destination::Jev,
            JevBackend::LocalJev => Destination::LocalJev,
        }
    }
}

/// Resolve the backend from raw selector values (pure helper; [`resolve_backend`]
/// reads the same values from process env). Empty values count as unset.
/// Explicit `typesafe`/`localjev` wins over any model mode; under `auto` an
/// unset or other mode is an error, as is any other backend value.
pub fn resolve_backend_from(
    jev_backend: Option<&str>,
    model_mode: Option<&str>,
) -> Result<JevBackend, RouterError> {
    let backend = jev_backend
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("auto");
    match backend {
        "typesafe" => Ok(JevBackend::TypeSafe),
        "localjev" => Ok(JevBackend::LocalJev),
        "auto" => match model_mode.map(str::trim).filter(|s| !s.is_empty()) {
            Some("frontier") => Ok(JevBackend::TypeSafe),
            Some("local") => Ok(JevBackend::LocalJev),
            other => Err(RouterError::Refused(format!(
                "JEV_BACKEND=auto needs LUNA_MODEL_MODE=frontier|local, got {}",
                other
                    .map(|s| format!("{s:?}"))
                    .unwrap_or_else(|| "unset".to_string()),
            ))),
        },
        other => Err(RouterError::Refused(format!(
            "unknown JEV_BACKEND={other:?} (want auto|typesafe|localjev)"
        ))),
    }
}

/// Resolve the backend from `JEV_BACKEND` / `LUNA_MODEL_MODE`. Fails closed
/// on any missing or unexpected value.
pub fn resolve_backend() -> Result<JevBackend, RouterError> {
    resolve_backend_from(
        get_env(JEV_BACKEND_ENV).as_deref(),
        get_env(LUNA_MODEL_MODE_ENV).as_deref(),
    )
}

/// Jev client for one resolved backend. Holds no secrets, so `Debug` is safe
/// to print.
#[derive(Debug, Clone)]
pub struct JevClient {
    backend: JevBackend,
    base_url: String,
    model: String,
    allow_paid: bool,
}

impl JevClient {
    /// Build from env: backend selection plus the selected backend's base URL
    /// and model (with defaults). Returns `Err` when backend selection fails;
    /// the key itself is read lazily per call, never here.
    pub fn from_env() -> Result<Self, RouterError> {
        let backend = resolve_backend()?;
        let (base_url, model) = match backend {
            JevBackend::TypeSafe => (
                get_env(TYPESAFE_BASE_URL_ENV).unwrap_or_else(|| TYPESAFE_DEFAULT_BASE_URL.into()),
                get_env(TYPESAFE_MODEL_ENV).unwrap_or_else(|| TYPESAFE_DEFAULT_MODEL.into()),
            ),
            JevBackend::LocalJev => (
                get_env(LOCALJEV_BASE_URL_ENV).unwrap_or_else(|| LOCALJEV_DEFAULT_BASE_URL.into()),
                get_env(LOCALJEV_MODEL_ENV).unwrap_or_else(|| LOCALJEV_DEFAULT_MODEL.into()),
            ),
        };
        Ok(Self {
            backend,
            base_url,
            model,
            allow_paid: is_paid_allowed(),
        })
    }

    /// Build with explicit settings (tests, callers that already read env).
    pub fn new(
        backend: JevBackend,
        base_url: impl Into<String>,
        model: impl Into<String>,
        allow_paid: bool,
    ) -> Self {
        Self {
            backend,
            base_url: base_url.into(),
            model: model.into(),
            allow_paid,
        }
    }

    /// Configured backend (frontier or local).
    pub fn backend(&self) -> JevBackend {
        self.backend
    }

    /// Base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// True if paid calls are allowed.
    pub fn allow_paid(&self) -> bool {
        self.allow_paid
    }

    /// Full System One endpoint for the configured base URL.
    /// A base already ending in `/v1` appends only `/systemone`.
    pub fn endpoint(&self) -> String {
        endpoint_for(&self.base_url)
    }

    /// Send one prompt with an explicit sensitivity. The TypeSafe backend
    /// refuses anything above [`DataClass::Public`](crate::data_class::DataClass)
    /// (sensitive maps to Confidential, unknown to Restricted); the local
    /// backend accepts all sensitivities because data never leaves the
    /// device. Refusals never touch the network.
    pub fn complete_with_sensitivity(
        &self,
        s: Sensitivity,
        prompt: &str,
    ) -> Result<String, RouterError> {
        self.complete_with_class(DataClass::from(s), "jev", None, prompt)
    }

    /// Send one prompt with an explicit data class and caller. The TypeSafe
    /// backend refuses content above [`DataClass::Public`](crate::data_class::DataClass)
    /// before any network I/O (logging the decision to `log` when given);
    /// the local backend accepts every class without logging (local calls
    /// are not egress).
    pub fn complete_with_class(
        &self,
        class: DataClass,
        caller: &str,
        log: Option<&mut DecisionLog>,
        prompt: &str,
    ) -> Result<String, RouterError> {
        if self.backend == JevBackend::TypeSafe {
            check_egress(class, Route::Jev, caller, log)?;
        }
        self.chat(prompt)
    }

    /// Send one prompt. TypeSafe calls fail closed without the paid switch or
    /// the API key; the local backend needs neither. Prefer
    /// [`Self::complete_with_sensitivity`] when sensitivity is known.
    pub fn chat(&self, _prompt: &str) -> Result<String, RouterError> {
        self.preflight()?;
        Err(RouterError::Refused(
            "Jev has no freeform chat endpoint; use JevClient::ask (POST /v1/systemone)".into(),
        ))
    }

    /// Gates shared by every call: paid switch + key for TypeSafe; optional
    /// key for local. Returns the Bearer key to send, if any.
    fn preflight(&self) -> Result<Option<String>, RouterError> {
        match self.backend {
            JevBackend::TypeSafe => {
                if !self.allow_paid {
                    return Err(RouterError::PaidCallsDisabled);
                }
                // Key lives in env only; an empty value counts as missing.
                get_env(TYPESAFE_API_KEY_ENV)
                    .map(Some)
                    .ok_or(RouterError::MissingEnv(TYPESAFE_API_KEY_ENV))
            }
            // Local inference is free: no paid gate. Key is optional.
            JevBackend::LocalJev => Ok(get_env(LOCALJEV_API_KEY_ENV)),
        }
    }

    /// Ask structured questions about `state` (System One). TypeSafe refuses
    /// anything above [`DataClass::Public`](crate::data_class::DataClass)
    /// before touching the network. Prefer [`Self::ask_with_class`] when the
    /// class and a decision log are available.
    pub fn ask(
        &self,
        s: Sensitivity,
        state: &str,
        questions: &serde_json::Value,
    ) -> Result<SystemOneReply, RouterError> {
        self.ask_with_class(DataClass::from(s), "jev.ask", None, state, questions)
    }

    /// Ask structured questions about `state` with an explicit data class and
    /// caller. The TypeSafe backend refuses content above
    /// [`DataClass::Public`](crate::data_class::DataClass) before any network
    /// I/O (logging the decision to `log` when given); the local backend
    /// accepts every class without logging.
    pub fn ask_with_class(
        &self,
        class: DataClass,
        caller: &str,
        log: Option<&mut DecisionLog>,
        state: &str,
        questions: &serde_json::Value,
    ) -> Result<SystemOneReply, RouterError> {
        if self.backend == JevBackend::TypeSafe {
            check_egress(class, Route::Jev, caller, log)?;
        }
        let key = self.preflight()?;
        post_systemone(
            &self.endpoint(),
            &self.model,
            key.as_deref(),
            state,
            questions,
        )
    }
}

/// Parsed System One response.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemOneReply {
    /// Model that answered (may differ from the alias requested).
    pub model: String,
    /// Answers keyed by question name.
    pub answers: serde_json::Map<String, serde_json::Value>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl SystemOneReply {
    /// Probability of yes for a `noul` question.
    pub fn noul(&self, name: &str) -> Option<f64> {
        self.answers.get(name)?.get("noul")?.as_f64()
    }
    /// Selected choice for a `choice` question.
    pub fn choice(&self, name: &str) -> Option<&str> {
        self.answers.get(name)?.get("choice")?.as_str()
    }
}

impl ModelClient for JevClient {
    /// Destination of the configured backend.
    fn destination(&self) -> Destination {
        self.backend.destination()
    }

    /// Fail-closed: the class is unknown here, so the TypeSafe backend
    /// refuses and only the local backend proceeds. Dispatch passes the routed
    /// class via the trait's `complete_with_class`.
    fn complete(&self, prompt: &str) -> Result<String, RouterError> {
        JevClient::complete_with_class(self, DataClass::Restricted, "jev.complete", None, prompt)
    }

    /// Complete a prompt, checking sensitivity first.
    fn complete_with_sensitivity(
        &self,
        s: Sensitivity,
        prompt: &str,
    ) -> Result<String, RouterError> {
        JevClient::complete_with_sensitivity(self, s, prompt)
    }

    /// Complete a prompt, checking the data class against the route and logging egress.
    fn complete_with_class(
        &self,
        class: DataClass,
        caller: &str,
        log: Option<&mut DecisionLog>,
        prompt: &str,
    ) -> Result<String, RouterError> {
        JevClient::complete_with_class(self, class, caller, log, prompt)
    }
}

/// Join a base URL to the System One path.
fn endpoint_for(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/systemone")
    } else {
        format!("{trimmed}/v1/systemone")
    }
}

/// Shared Jev System One call used by BOTH backends (TypeSafe and localjev):
/// `POST /v1/systemone` with `{model, state, questions}` and Bearer auth.
/// Shape verified live against TypeSafe on 2026-10-26; the localjev server is
/// assumed to share it (the Luna stack uses one SystemOneClient for both).
fn post_systemone(
    endpoint: &str,
    model: &str,
    api_key: Option<&str>,
    state: &str,
    questions: &serde_json::Value,
) -> Result<SystemOneReply, RouterError> {
    let body = serde_json::json!({"model": model, "state": state, "questions": questions});
    let request = ureq::post(endpoint).timeout(std::time::Duration::from_secs(TIMEOUT_SECS));
    let request = match api_key {
        Some(key) => request.set("Authorization", &format!("Bearer {key}")),
        None => request,
    };
    let raw: serde_json::Value = request
        .send_json(&body)
        .map_err(map_ureq_error)?
        .into_json()
        .map_err(|e| RouterError::Http(e.to_string()))?;
    let answers = raw
        .get("answers")
        .and_then(|a| a.as_object())
        .filter(|a| !a.is_empty())
        .cloned()
        .ok_or_else(|| RouterError::BadResponse("missing answers object".into()))?;
    let usage = |k: &str| {
        raw.get("usage")
            .and_then(|u| u.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    Ok(SystemOneReply {
        model: raw
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(model)
            .to_string(),
        answers,
        input_tokens: usage("input_tokens"),
        output_tokens: usage("output_tokens"),
    })
}

/// Non-empty env var.
fn get_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// True when `ANCIENTOS_ALLOW_PAID=1`.
fn is_paid_allowed() -> bool {
    std::env::var(ALLOW_PAID_ENV)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Map a ureq error to a router error.
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Process-wide env guard: every env-mutating test in the crate shares it.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    /// Set process env vars for a test, restoring previous values on drop.
    /// `None` removes the var for the guard's lifetime.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        /// Test helper: set.
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
        /// Test helper: drop.
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// Captured requests: (Authorization header, body) per hit.
    type SeenRequests = Arc<Mutex<Vec<(Option<String>, String)>>>;

    /// One-shot mock HTTP server on 127.0.0.1: captures the Authorization
    /// header plus the request body, replies once with `status` + `body`,
    /// then exits. No external network is ever used.
    fn mock_server(status: u16, body: &str) -> (String, SeenRequests, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let seen: SeenRequests = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        let body = body.to_string();
        let handle = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut stream = stream;
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout");
            // Read headers line by line until the blank line.
            let mut raw = Vec::new();
            let mut buf = [0u8; 1];
            let mut content_length = 0usize;
            let mut auth: Option<String> = None;
            let mut line = Vec::new();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        raw.push(buf[0]);
                        line.push(buf[0]);
                        if buf[0] == b'\n' {
                            let text = String::from_utf8_lossy(&line);
                            let lower = text.to_lowercase();
                            if lower.starts_with("content-length:") {
                                content_length = lower
                                    .split(':')
                                    .nth(1)
                                    .and_then(|v| v.trim().parse().ok())
                                    .unwrap_or(0);
                            }
                            if lower.starts_with("authorization:") {
                                auth = text.split(':').nth(1).map(|v| v.trim().to_string());
                            }
                            if line == b"\r\n" || line == b"\n" {
                                break;
                            }
                            line.clear();
                        }
                        if raw.len() > 65536 {
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
                .push((auth, String::from_utf8_lossy(&rest).into_owned()));
            let reason = match status {
                200 => "OK",
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

    const OK_BODY: &str = r#"{"model":"jev-1.14.0","answers":{"q":{"type":"noul","noul":0.9}},"usage":{"input_tokens":304,"output_tokens":21}}"#;

    /// Test helper: qs.
    fn qs() -> serde_json::Value {
        serde_json::json!({"q": {"type": "noul", "instructions": "yes?"}})
    }

    /// Test helper: typesafe client.
    fn typesafe_client(base_url: String, allow_paid: bool) -> JevClient {
        JevClient::new(JevBackend::TypeSafe, base_url, "test-jevo", allow_paid)
    }

    /// Test helper: localjev client.
    fn localjev_client(base_url: String) -> JevClient {
        // allow_paid=false on purpose: localjev must never be paid-gated.
        JevClient::new(JevBackend::LocalJev, base_url, "test-localjev", false)
    }

    #[test]
    fn backend_selection_matrix() {
        // Default (unset/empty) is auto.
        for backend in [None, Some(""), Some("  "), Some("auto")] {
            assert_eq!(
                resolve_backend_from(backend, Some("frontier")).unwrap(),
                JevBackend::TypeSafe,
                "backend={backend:?}"
            );
            assert_eq!(
                resolve_backend_from(backend, Some("local")).unwrap(),
                JevBackend::LocalJev,
                "backend={backend:?}"
            );
            // Unset, empty, or other mode under auto fails closed.
            for mode in [None, Some(""), Some("  "), Some("turbo"), Some("FRONTIER")] {
                assert!(
                    resolve_backend_from(backend, mode).is_err(),
                    "backend={backend:?} mode={mode:?} must fail closed"
                );
            }
        }
        // Explicit backend wins over any mode, even a conflicting one.
        assert_eq!(
            resolve_backend_from(Some("typesafe"), Some("local")).unwrap(),
            JevBackend::TypeSafe
        );
        assert_eq!(
            resolve_backend_from(Some("localjev"), Some("frontier")).unwrap(),
            JevBackend::LocalJev
        );
        assert_eq!(
            resolve_backend_from(Some("typesafe"), None).unwrap(),
            JevBackend::TypeSafe
        );
        // Any other backend value fails closed.
        for backend in ["openai", "TYPESAFE", "local", "frontier", "Automatic"] {
            assert!(
                resolve_backend_from(Some(backend), Some("frontier")).is_err(),
                "backend={backend:?} must fail closed"
            );
        }
    }

    #[test]
    fn from_env_selects_backend_with_defaults() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[
            (TYPESAFE_BASE_URL_ENV, None),
            (TYPESAFE_MODEL_ENV, None),
            (LOCALJEV_BASE_URL_ENV, None),
            (LOCALJEV_MODEL_ENV, None),
            (JEV_BACKEND_ENV, None),
            (LUNA_MODEL_MODE_ENV, Some("frontier")),
        ]);
        let c = JevClient::from_env().expect("auto+frontier");
        assert_eq!(c.backend(), JevBackend::TypeSafe);
        assert_eq!(c.base_url(), TYPESAFE_DEFAULT_BASE_URL);
        assert_eq!(c.model(), TYPESAFE_DEFAULT_MODEL);
        assert_eq!(c.destination(), Destination::Jev);

        let _env = EnvGuard::set(&[(LUNA_MODEL_MODE_ENV, Some("local"))]);
        let c = JevClient::from_env().expect("auto+local");
        assert_eq!(c.backend(), JevBackend::LocalJev);
        assert_eq!(c.base_url(), LOCALJEV_DEFAULT_BASE_URL);
        assert_eq!(c.model(), LOCALJEV_DEFAULT_MODEL);
        assert_eq!(c.destination(), Destination::LocalJev);
    }

    #[test]
    fn from_env_invalid_selection_fails_closed() {
        let _guard = env_lock();
        // Unset mode under auto.
        let _env = EnvGuard::set(&[(JEV_BACKEND_ENV, None), (LUNA_MODEL_MODE_ENV, None)]);
        assert!(matches!(
            JevClient::from_env(),
            Err(RouterError::Refused(_))
        ));
        // Other mode under auto.
        let _env = EnvGuard::set(&[(LUNA_MODEL_MODE_ENV, Some("turbo"))]);
        assert!(matches!(
            JevClient::from_env(),
            Err(RouterError::Refused(_))
        ));
        // Other backend value.
        let _env = EnvGuard::set(&[
            (JEV_BACKEND_ENV, Some("openai")),
            (LUNA_MODEL_MODE_ENV, Some("frontier")),
        ]);
        let err = JevClient::from_env().unwrap_err();
        assert!(
            matches!(err, RouterError::Refused(_)),
            "unexpected: {err:?}"
        );
        assert!(err.to_string().contains("JEV_BACKEND"));
    }

    #[test]
    fn from_env_base_url_and_model_override() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[
            (JEV_BACKEND_ENV, Some("typesafe")),
            (TYPESAFE_BASE_URL_ENV, Some("https://typesafe.example.test")),
            (TYPESAFE_MODEL_ENV, Some("jev-test-1")),
        ]);
        let c = JevClient::from_env().expect("typesafe from env");
        assert_eq!(c.base_url(), "https://typesafe.example.test");
        assert_eq!(c.model(), "jev-test-1");
        assert_eq!(c.endpoint(), "https://typesafe.example.test/v1/systemone");

        let _env = EnvGuard::set(&[
            (JEV_BACKEND_ENV, Some("localjev")),
            (LOCALJEV_BASE_URL_ENV, Some("http://127.0.0.1:9999")),
            (LOCALJEV_MODEL_ENV, Some("localjev-test-1")),
        ]);
        let c = JevClient::from_env().expect("localjev from env");
        assert_eq!(c.base_url(), "http://127.0.0.1:9999");
        assert_eq!(c.model(), "localjev-test-1");
    }

    #[test]
    fn typesafe_success_posts_systemone_body_with_bearer() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(TYPESAFE_API_KEY_ENV, Some("test-key"))]);
        let (url, seen, handle) = mock_server(200, OK_BODY);
        let client = typesafe_client(url, true);
        let out = client
            .ask(Sensitivity::Sanitized, "public prompt", &qs())
            .expect("mock success");
        assert_eq!(out.noul("q"), Some(0.9));
        assert_eq!(out.model, "jev-1.14.0");
        assert_eq!((out.input_tokens, out.output_tokens), (304, 21));
        handle.join().expect("server thread");
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        let (auth, body) = &seen[0];
        assert_eq!(auth.as_deref(), Some("Bearer test-key"));
        let sent: serde_json::Value = serde_json::from_str(body).expect("request is json");
        assert_eq!(sent["model"], "test-jevo");
        assert_eq!(sent["state"], "public prompt");
        assert_eq!(sent["questions"], qs());
    }

    #[test]
    fn typesafe_paid_gate_off_errors_before_network() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(TYPESAFE_API_KEY_ENV, Some("test-key"))]);
        // Closed URL proves no network is attempted when the gate is off.
        let err = typesafe_client(closed_url(), false)
            .complete_with_sensitivity(Sensitivity::Sanitized, "public")
            .unwrap_err();
        assert!(
            matches!(err, RouterError::PaidCallsDisabled),
            "unexpected: {err:?}"
        );
        // The env switch defaults to off: from_env without it refuses too.
        let _env = EnvGuard::set(&[(JEV_BACKEND_ENV, Some("typesafe")), (ALLOW_PAID_ENV, None)]);
        let client = JevClient::from_env().expect("selection ok");
        assert!(!client.allow_paid());
        assert!(matches!(
            client.complete_with_sensitivity(Sensitivity::Sanitized, "public"),
            Err(RouterError::PaidCallsDisabled)
        ));
    }

    #[test]
    fn typesafe_missing_key_errors_before_network() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(TYPESAFE_API_KEY_ENV, None)]);
        // Closed URL proves no network is attempted with no key.
        for empty in [false, true] {
            if empty {
                std::env::set_var(TYPESAFE_API_KEY_ENV, "  ");
            }
            let err = typesafe_client(closed_url(), true)
                .complete_with_sensitivity(Sensitivity::Sanitized, "public")
                .unwrap_err();
            assert!(
                matches!(err, RouterError::MissingEnv(TYPESAFE_API_KEY_ENV)),
                "unexpected: {err:?}"
            );
        }
    }

    #[test]
    fn typesafe_refuses_sensitive_unknown_and_bare_complete() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(TYPESAFE_API_KEY_ENV, Some("test-key"))]);
        // Closed URL proves refusals never touch the network: any attempt
        // would surface as Http, so EgressDenied proves zero network use.
        let client = typesafe_client(closed_url(), true);
        for s in [Sensitivity::Sensitive, Sensitivity::Unknown] {
            let err = client
                .complete_with_sensitivity(s, "PRIVATE health note")
                .unwrap_err();
            assert!(
                matches!(err, RouterError::EgressDenied { .. }),
                "unexpected: {err:?}"
            );
            assert!(err.to_string().contains("egress denied"));
        }
        // Bare complete() has unknown class, so it refuses too.
        assert!(matches!(
            client.complete("anything"),
            Err(RouterError::EgressDenied { .. })
        ));
        assert!(matches!(
            ModelClient::complete_with_sensitivity(&client, Sensitivity::Sensitive, "x"),
            Err(RouterError::EgressDenied { .. })
        ));
    }

    #[test]
    fn typesafe_ask_refuses_over_ceiling_with_denied_entry() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(TYPESAFE_API_KEY_ENV, Some("test-key"))]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for class in [
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            // Closed URL: EgressDenied proves the refusal beat the network.
            let client = typesafe_client(closed_url(), true);
            let err = client
                .ask_with_class(class, "test-jev", Some(&mut log), "canary-state", &qs())
                .unwrap_err();
            assert!(
                matches!(err, RouterError::EgressDenied { .. }),
                "unexpected: {err:?}"
            );
        }
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 3);
        for (e, class) in entries
            .iter()
            .zip(["internal", "confidential", "restricted"])
        {
            assert_eq!(e.kind, crate::data_class::EGRESS_DENIED_KIND);
            assert_eq!(e.payload["class"], class);
            assert_eq!(e.payload["route"], "jev");
            assert_eq!(e.payload["caller"], "test-jev");
            let raw = serde_json::to_string(&e.payload).unwrap();
            assert!(!raw.contains("canary"), "{raw}");
        }
    }

    #[test]
    fn typesafe_ask_public_logs_allowed_with_one_request() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(TYPESAFE_API_KEY_ENV, Some("test-key"))]);
        let (url, seen, handle) = mock_server(200, OK_BODY);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        let out = typesafe_client(url, true)
            .ask_with_class(DataClass::Public, "test-jev", Some(&mut log), "hi", &qs())
            .expect("public ask ok");
        handle.join().expect("server thread");
        assert_eq!(out.noul("q"), Some(0.9));
        assert_eq!(seen.lock().expect("seen lock").len(), 1);
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, crate::data_class::EGRESS_ALLOWED_KIND);
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "jev");
    }

    #[test]
    fn localjev_accepts_every_class_without_entries() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(LOCALJEV_API_KEY_ENV, None)]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for class in [
            DataClass::Public,
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            let (url, seen, handle) = mock_server(200, OK_BODY);
            let out = localjev_client(url)
                .ask_with_class(class, "test-jev", Some(&mut log), "local state", &qs())
                .unwrap_or_else(|e| panic!("local {class:?} must be allowed: {e}"));
            handle.join().expect("server thread");
            assert_eq!(out.noul("q"), Some(0.9));
            assert_eq!(seen.lock().expect("seen lock").len(), 1);
        }
        // Local calls are not egress: allowed, but never logged.
        assert_eq!(log.verify_or_empty().unwrap().len(), 0);
    }

    #[test]
    fn localjev_success_allows_sensitive_without_key_or_paid() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(LOCALJEV_API_KEY_ENV, None)]);
        let (url, seen, handle) = mock_server(200, OK_BODY);
        let client = localjev_client(url);
        assert_eq!(client.destination(), Destination::LocalJev);
        // Sensitive content is allowed: data stays on the local server.
        let out = client
            .ask(Sensitivity::Sensitive, "PRIVATE health note", &qs())
            .expect("local sensitive ok");
        assert_eq!(out.noul("q"), Some(0.9));
        // Freeform complete() passes local gates but fails closed (no chat API).
        let (url2, _, handle2) = (closed_url(), (), std::thread::spawn(|| {}));
        assert!(matches!(
            localjev_client(url2).complete("PRIVATE"),
            Err(RouterError::Refused(_))
        ));
        handle.join().expect("server thread");
        handle2.join().expect("server thread");
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        let (auth, body) = &seen[0];
        assert_eq!(auth, &None, "no key configured, no auth header");
        // Same shared request shape as the TypeSafe backend.
        let sent: serde_json::Value = serde_json::from_str(body).expect("request is json");
        assert_eq!(sent["model"], "test-localjev");
        assert_eq!(sent["state"], "PRIVATE health note");
    }

    #[test]
    fn localjev_optional_bearer_sent_when_set() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(LOCALJEV_API_KEY_ENV, Some("local-secret"))]);
        let (url, seen, handle) = mock_server(200, OK_BODY);
        let out = localjev_client(url)
            .ask(Sensitivity::Sanitized, "hi", &qs())
            .expect("mock success");
        assert_eq!(out.noul("q"), Some(0.9));
        handle.join().expect("server thread");
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen[0].0.as_deref(), Some("Bearer local-secret"));
    }

    #[test]
    fn localjev_bad_status_and_malformed_body_fail_closed() {
        let _guard = env_lock();
        let _env = EnvGuard::set(&[(LOCALJEV_API_KEY_ENV, None)]);
        let (url, _, handle) = mock_server(500, r#"{"error":"boom"}"#);
        let err = localjev_client(url)
            .ask(Sensitivity::Sensitive, "x", &qs())
            .unwrap_err();
        assert!(matches!(err, RouterError::Http(_)), "unexpected: {err:?}");
        assert!(err.to_string().contains("500"));
        handle.join().expect("server thread");

        for body in [
            "{}",
            r#"{"answers":{}}"#,
            r#"{"answers":null}"#,
            r#"{"answers":42}"#,
            r#"{"choices":[{"message":{"content":"hi"}}]}"#,
        ] {
            let (url, _, handle) = mock_server(200, body);
            let err = localjev_client(url)
                .ask(Sensitivity::Sensitive, "x", &qs())
                .unwrap_err();
            assert!(
                matches!(err, RouterError::BadResponse(_)),
                "unexpected ok for {body}: {err:?}"
            );
            handle.join().expect("server thread");
        }
    }

    #[test]
    fn endpoint_appends_systemone() {
        let c = typesafe_client("https://api.typesafe.ai".into(), true);
        assert_eq!(c.endpoint(), "https://api.typesafe.ai/v1/systemone");
        let trailing = typesafe_client("https://api.typesafe.ai/".into(), true);
        assert_eq!(trailing.endpoint(), "https://api.typesafe.ai/v1/systemone");
        let with_v1 = localjev_client("http://127.0.0.1:8080/v1".into());
        assert_eq!(with_v1.endpoint(), "http://127.0.0.1:8080/v1/systemone");
    }

    #[test]
    fn debug_output_holds_no_key_material() {
        let dbg = format!("{:?}", typesafe_client(closed_url(), true));
        assert!(!dbg.contains("TYPESAFE_API_KEY"));
        assert!(!dbg.contains("Bearer"));
        assert!(!dbg.contains("PRIVATE"));
    }
}
