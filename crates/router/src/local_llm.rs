//! Local LLM client for sensitive data (Ollama / llama.cpp server).
//!
//! OpenAI-compatible chat client: POSTs
//! `{model, messages: [{role: "user", content}], stream: false}` to
//! `$ANCIENTOS_LOCAL_LLM_URL/v1/chat/completions`. Both Ollama
//! (`http://localhost:11434`) and llama.cpp server (`--port 8080`) serve
//! this endpoint. Model defaults to `ANCIENTOS_LOCAL_LLM_MODEL`, else
//! [`DEFAULT_MODEL`]. No paid switch: local inference is free.
//!
//! Fails closed: unset URL, unreachable host, non-2xx status, or a
//! malformed response all return [`RouterError`]. Sensitive content must
//! never fall back to Muse on any of these errors — dispatch routes to
//! this client for the local slot and propagates the error.
use crate::{Destination, ModelClient, RouterError};

/// Env var carrying the local server base URL (e.g. `http://localhost:11434`).
pub const URL_ENV: &str = "ANCIENTOS_LOCAL_LLM_URL";
/// Env var overriding the local chat model.
pub const MODEL_ENV: &str = "ANCIENTOS_LOCAL_LLM_MODEL";
/// Default local chat model (Ollama tag).
pub const DEFAULT_MODEL: &str = "llama3.2:1b";
/// Request timeout: local models on CPU can be slow.
const TIMEOUT_SECS: u64 = 120;

/// Local LLM client. Holds no secrets, so `Debug` is safe to print.
#[derive(Debug, Clone)]
pub struct LocalLlmClient {
    base_url: Option<String>,
    model: String,
}

impl LocalLlmClient {
    /// Build from env: [`URL_ENV`], [`MODEL_ENV`].
    pub fn from_env() -> Self {
        Self {
            base_url: get_env(URL_ENV),
            model: get_env(MODEL_ENV).unwrap_or_else(|| DEFAULT_MODEL.into()),
        }
    }

    /// Build with explicit settings (tests, callers that already read env).
    pub fn new(base_url: Option<String>, model: impl Into<String>) -> Self {
        let base_url = base_url.filter(|u| !u.trim().is_empty());
        Self {
            base_url,
            model: model.into(),
        }
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Full chat-completions endpoint for the configured base URL.
    /// A base already ending in `/v1` appends only `/chat/completions`.
    pub fn endpoint(&self) -> Result<String, RouterError> {
        let base = self
            .base_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or(RouterError::MissingEnv(URL_ENV))?;
        let trimmed = base.trim_end_matches('/');
        if trimmed.ends_with("/v1") {
            Ok(format!("{trimmed}/chat/completions"))
        } else {
            Ok(format!("{trimmed}/v1/chat/completions"))
        }
    }

    /// Request body for a single user prompt.
    pub fn request_body(&self, prompt: &str) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
        })
    }

    /// Parse an OpenAI-compatible chat completion value.
    /// Reads `choices[0].message.content` as a string; anything else is a
    /// `BadResponse` (fail closed, never an empty guess).
    pub fn parse_chat_response(value: &serde_json::Value) -> Result<String, RouterError> {
        let choice = value
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| RouterError::BadResponse("missing choices[0]".into()))?;
        choice
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                RouterError::BadResponse("missing choices[0].message.content string".into())
            })
    }
}

impl ModelClient for LocalLlmClient {
    fn destination(&self) -> Destination {
        Destination::LocalLlama
    }

    fn complete(&self, prompt: &str) -> Result<String, RouterError> {
        let endpoint = self.endpoint()?;
        let body = self.request_body(prompt);
        let raw: serde_json::Value = ureq::post(&endpoint)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .send_json(&body)
            .map_err(map_ureq_error)?
            .into_json()
            .map_err(|e| RouterError::Http(e.to_string()))?;
        Self::parse_chat_response(&raw)
    }
}

fn get_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// One-shot mock HTTP server: captures the request body, replies once
    /// with `status` + `body`, then exits. Returns the base URL, the
    /// captured request bodies, and the server thread handle.
    fn mock_server(
        status: u16,
        body: &str,
    ) -> (String, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
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
            let mut line = Vec::new();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        raw.push(buf[0]);
                        line.push(buf[0]);
                        if buf[0] == b'\n' {
                            let text = String::from_utf8_lossy(&line).to_lowercase();
                            if text.starts_with("content-length:") {
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
                .push(String::from_utf8_lossy(&rest).into_owned());
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

    /// A base URL with nothing listening (connection refused, fast).
    fn closed_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let url = format!("http://{}", listener.local_addr().expect("probe addr"));
        drop(listener);
        url
    }

    const OK_BODY: &str =
        r#"{"choices":[{"message":{"role":"assistant","content":"hello local"}}]}"#;

    #[test]
    fn success_posts_openai_body_and_returns_content() {
        let (url, seen, handle) = mock_server(200, OK_BODY);
        let client = LocalLlmClient::new(Some(url), "test-model");
        assert_eq!(client.destination(), Destination::LocalLlama);
        let out = client.complete("private prompt").expect("mock success");
        assert_eq!(out, "hello local");
        handle.join().expect("server thread");
        let bodies = seen.lock().expect("seen lock");
        assert_eq!(bodies.len(), 1);
        let sent: serde_json::Value = serde_json::from_str(&bodies[0]).expect("request is json");
        assert_eq!(sent["model"], "test-model");
        assert_eq!(sent["messages"][0]["role"], "user");
        assert_eq!(sent["messages"][0]["content"], "private prompt");
        assert_eq!(sent["stream"], false);
    }

    #[test]
    fn unreachable_host_fails_closed() {
        let client = LocalLlmClient::new(Some(closed_url()), DEFAULT_MODEL);
        let err = client.complete("private").unwrap_err();
        assert!(matches!(err, RouterError::Http(_)), "unexpected: {err:?}");
    }

    #[test]
    fn http_500_fails_closed() {
        let (url, seen, handle) = mock_server(500, r#"{"error":"boom"}"#);
        let client = LocalLlmClient::new(Some(url), DEFAULT_MODEL);
        let err = client.complete("private").unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, RouterError::Http(_)), "unexpected: {msg}");
        assert!(msg.contains("500"), "status in message: {msg}");
        handle.join().expect("server thread");
        assert_eq!(seen.lock().expect("seen lock").len(), 1);
    }

    #[test]
    fn malformed_response_fails_closed() {
        for body in [
            "{}",
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{}}]}"#,
            r#"{"choices":[{"message":{"content":42}}]}"#,
            r#"{"choices":[{"message":{"content":null}}]}"#,
        ] {
            let err = LocalLlmClient::parse_chat_response(
                &serde_json::from_str(body).expect("fixture json"),
            )
            .unwrap_err();
            assert!(
                matches!(err, RouterError::BadResponse(_)),
                "unexpected ok for {body}: {err:?}"
            );
        }
        // Malformed over the wire also errors (not an empty guess).
        let (url, _, handle) = mock_server(200, r#"{"choices":[]}"#);
        let client = LocalLlmClient::new(Some(url), DEFAULT_MODEL);
        assert!(matches!(
            client.complete("private"),
            Err(RouterError::BadResponse(_))
        ));
        handle.join().expect("server thread");
    }

    #[test]
    fn missing_url_fails_before_network() {
        let client = LocalLlmClient::new(None, DEFAULT_MODEL);
        assert!(matches!(
            client.complete("private"),
            Err(RouterError::MissingEnv(URL_ENV))
        ));
        let blank = LocalLlmClient::new(Some("   ".into()), DEFAULT_MODEL);
        assert!(matches!(
            blank.complete("private"),
            Err(RouterError::MissingEnv(URL_ENV))
        ));
    }

    #[test]
    fn dispatch_sensitive_with_local_down_never_calls_muse() {
        use crate::dispatch::{dispatch, Backends};
        use crate::Sensitivity;
        use std::cell::RefCell;

        struct Recorder {
            dest: Destination,
            seen: RefCell<Vec<String>>,
        }
        impl ModelClient for Recorder {
            fn destination(&self) -> Destination {
                self.dest
            }
            fn complete(&self, p: &str) -> Result<String, RouterError> {
                self.seen.borrow_mut().push(p.into());
                Ok("muse reply".into())
            }
        }

        let local = LocalLlmClient::new(Some(closed_url()), DEFAULT_MODEL);
        let spark = Recorder {
            dest: Destination::MuseSpark,
            seen: RefCell::new(Vec::new()),
        };
        let wonder = Recorder {
            dest: Destination::MuseWonder,
            seen: RefCell::new(Vec::new()),
        };
        let b = Backends {
            local: &local,
            muse_spark: &spark,
            muse_wonder: &wonder,
        };
        // Sensitive and unknown both route local; local down is an error,
        // never a silent fallback to Muse.
        for s in [Sensitivity::Sensitive, Sensitivity::Unknown] {
            let err = dispatch(&b, s, false, "PRIVATE health note").unwrap_err();
            assert!(
                matches!(err, RouterError::Http(_) | RouterError::MissingEnv(_)),
                "unexpected: {err:?}"
            );
        }
        assert!(spark.seen.borrow().is_empty(), "muse spark called");
        assert!(wonder.seen.borrow().is_empty(), "muse wonder called");
    }

    #[test]
    fn endpoint_appends_chat_completions() {
        let c = LocalLlmClient::new(Some("http://localhost:11434".into()), DEFAULT_MODEL);
        assert_eq!(
            c.endpoint().unwrap(),
            "http://localhost:11434/v1/chat/completions"
        );
        let trailing = LocalLlmClient::new(Some("http://localhost:8080/".into()), DEFAULT_MODEL);
        assert_eq!(
            trailing.endpoint().unwrap(),
            "http://localhost:8080/v1/chat/completions"
        );
        let with_v1 = LocalLlmClient::new(Some("http://localhost:11434/v1".into()), DEFAULT_MODEL);
        assert_eq!(
            with_v1.endpoint().unwrap(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn default_model_is_llama() {
        assert_eq!(DEFAULT_MODEL, "llama3.2:1b");
        let c = LocalLlmClient::new(Some("http://localhost:11434".into()), DEFAULT_MODEL);
        assert_eq!(c.request_body("hi")["model"], "llama3.2:1b");
        assert_eq!(c.request_body("hi")["stream"], false);
    }

    #[test]
    fn debug_output_holds_no_prompt_material() {
        let dbg = format!(
            "{:?}",
            LocalLlmClient::new(Some("http://localhost:11434".into()), DEFAULT_MODEL)
        );
        assert!(!dbg.contains("PRIVATE"));
    }
}
