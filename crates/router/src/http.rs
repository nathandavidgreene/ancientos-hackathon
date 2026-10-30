//! Single HTTP transport for the workspace.
//!
//! This is the only module allowed to touch `ureq` (or `reqwest`): a CI
//! grep (`scripts/ci_local.sh`) fails the build when either crate is used in
//! any other `.rs` file. Centralizing transport here keeps hosted model
//! calls auditable: they reach the network only after their client's
//! [`check_egress`](crate::data_class::check_egress) data-class check and
//! the `ANCIENTOS_ALLOW_PAID` paid gate.
//!
//! The helpers are deliberately dumb transport: no URLs, headers, or policy
//! decisions live here. Callers keep their own validation, auth headers,
//! timeouts, and error text; this module only performs the call and reports
//! a [`HttpError`] that carries everything callers need to reproduce their
//! fail-closed messages (status code plus a capped body snippet, transport
//! kind plus the original message, or the body-read failure).
use std::time::Duration;

/// Body-snippet cap for [`HttpError::Status`]: the first 500 chars, matching
/// what the router clients reported before transport was centralized.
/// Callers needing a shorter snippet re-truncate (same result).
pub const STATUS_SNIPPET_CHARS: usize = 500;

/// Why a request never got a response. Carries no URLs, hostnames, or bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// The deadline expired before the response completed.
    Timeout,
    /// Redirects exceeded the caller's limit.
    TooManyRedirects,
    /// The hostname did not resolve.
    Dns,
    /// The TCP/TLS connection failed.
    ConnectionFailed,
    /// The URL was malformed or used an unknown scheme.
    InvalidUrl,
    /// Any other transport failure; the message is on [`HttpError::Transport`].
    Other,
}

/// How an HTTP call failed. Never carries request URLs or headers; the
/// status snippet is response-body text capped at [`STATUS_SNIPPET_CHARS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The server answered with a non-2xx status.
    Status {
        /// HTTP status code.
        code: u16,
        /// First [`STATUS_SNIPPET_CHARS`] chars of the response body.
        snippet: String,
    },
    /// The request never got a response.
    Transport {
        /// Classified failure kind.
        kind: TransportKind,
        /// Original transport error message (no URLs or headers).
        message: String,
    },
    /// The response arrived but its body could not be read or parsed as
    /// asked; carries the underlying message.
    Body(String),
}

/// Bytes fetched by [`get_bytes`]: the capped body plus the response facts
/// callers need for their own policy checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedBytes {
    /// Final URL after redirects (callers re-validate it before trusting it).
    pub final_url: String,
    /// Raw `content-type` response header, if any.
    pub content_type: Option<String>,
    /// Response body, truncated to the caller's byte limit.
    pub bytes: Vec<u8>,
}

/// POST a JSON body and parse the JSON reply. Used for model chat/completion
/// endpoints (hosted and local) and the local embedding endpoint.
pub fn post_json(
    url: &str,
    timeout: Duration,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
) -> Result<serde_json::Value, HttpError> {
    let mut request = agent(timeout, None).post(url);
    for (name, value) in headers {
        request = request.set(name, value);
    }
    read_json(request.send_json(body.clone()).map_err(map_err)?)
}

/// GET and parse the JSON reply. Used for JSON APIs with header auth.
pub fn get_json(
    url: &str,
    timeout: Duration,
    headers: &[(&str, &str)],
) -> Result<serde_json::Value, HttpError> {
    let mut request = agent(timeout, None).get(url);
    for (name, value) in headers {
        request = request.set(name, value);
    }
    read_json(request.call().map_err(map_err)?)
}

/// POST a form and parse the JSON reply. Used for the OAuth token endpoint.
pub fn post_form_json(
    url: &str,
    timeout: Duration,
    form: &[(&str, &str)],
) -> Result<serde_json::Value, HttpError> {
    let response = agent(timeout, None)
        .post(url)
        .send_form(form)
        .map_err(map_err)?;
    read_json(response)
}

/// Plain GET succeeding only on 2xx; the body is discarded. Used for health
/// preflights, which send no prompt and need only the outcome.
pub fn get_status(url: &str, timeout: Duration) -> Result<(), HttpError> {
    agent(timeout, None).get(url).call().map_err(map_err)?;
    Ok(())
}

/// Plain GET with an optional redirect limit, reading at most `max_bytes` of
/// the body. Used for page/media downloads whose callers enforce their own
/// host allowlists, size caps, and final-URL re-checks.
pub fn get_bytes(
    url: &str,
    timeout: Duration,
    max_redirects: Option<u32>,
    max_bytes: u64,
) -> Result<FetchedBytes, HttpError> {
    let response = agent(timeout, max_redirects)
        .get(url)
        .call()
        .map_err(map_err)?;
    let final_url = response.get_url().to_string();
    let content_type = response.header("content-type").map(str::to_string);
    let mut bytes = Vec::new();
    use std::io::Read;
    response
        .into_reader()
        .take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|e| HttpError::Body(e.to_string()))?;
    Ok(FetchedBytes {
        final_url,
        content_type,
        bytes,
    })
}

/// One-request agent: the caller's timeout plus its redirect limit, if any.
/// No limit means the HTTP client's default policy.
fn agent(timeout: Duration, max_redirects: Option<u32>) -> ureq::Agent {
    let builder = ureq::AgentBuilder::new().timeout(timeout);
    match max_redirects {
        Some(n) => builder.redirects(n).build(),
        None => builder.build(),
    }
}

/// Parse a response body as JSON.
fn read_json(response: ureq::Response) -> Result<serde_json::Value, HttpError> {
    response
        .into_json()
        .map_err(|e| HttpError::Body(e.to_string()))
}

/// Map a client error to an [`HttpError`]. Status responses contribute their
/// code and a capped body snippet; transport failures contribute their kind
/// and the exact original message.
fn map_err(e: ureq::Error) -> HttpError {
    match e {
        ureq::Error::Status(code, response) => HttpError::Status {
            code,
            snippet: snippet(response.into_string().unwrap_or_default()),
        },
        ureq::Error::Transport(t) => {
            let kind = classify(&t);
            // Re-wrap so the message is exactly the original error text.
            let message = ureq::Error::Transport(t).to_string();
            HttpError::Transport { kind, message }
        }
    }
}

/// First [`STATUS_SNIPPET_CHARS`] chars of a response body.
fn snippet(body: String) -> String {
    body.chars().take(STATUS_SNIPPET_CHARS).collect()
}

/// Classify a transport failure. Timeouts need [`is_timeout`]: the client
/// wraps the deadline expiry as an `io::Error` of kind `TimedOut` in the
/// source chain, which no error kind names.
fn classify(t: &ureq::Transport) -> TransportKind {
    if is_timeout(t) {
        return TransportKind::Timeout;
    }
    match t.kind() {
        ureq::ErrorKind::TooManyRedirects => TransportKind::TooManyRedirects,
        ureq::ErrorKind::Dns => TransportKind::Dns,
        ureq::ErrorKind::ConnectionFailed => TransportKind::ConnectionFailed,
        ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => TransportKind::InvalidUrl,
        _ => TransportKind::Other,
    }
}

/// True when a transport error was caused by a timeout: the client wraps the
/// deadline expiry as an `io::Error` of kind `TimedOut` in the source chain
/// (the message alone does not name it).
fn is_timeout(t: &ureq::Transport) -> bool {
    use std::error::Error;
    let mut next = t.source();
    while let Some(e) = next {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::TimedOut {
                return true;
            }
        }
        next = e.source();
    }
    t.message().is_some_and(|m| {
        let m = m.to_ascii_lowercase();
        m.contains("timed out") || m.contains("timeout")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Short bodies pass through; long ones are cut at exactly the cap.
    #[test]
    fn status_snippet_caps_at_500_chars() {
        assert_eq!(snippet(String::new()), "");
        assert_eq!(snippet("boom".to_string()), "boom");
        let long = "é".repeat(STATUS_SNIPPET_CHARS + 10);
        let cut = snippet(long);
        assert_eq!(cut.chars().count(), STATUS_SNIPPET_CHARS);
        // Callers needing less re-truncate with the same result.
        assert_eq!(cut.chars().take(300).count(), 300);
    }

    /// The error type carries code/snippet, kind/message, or body text, and
    /// nothing else (no URLs, headers, or request bodies).
    #[test]
    fn http_error_variants_carry_only_response_facts() {
        let status = HttpError::Status {
            code: 503,
            snippet: "busy".to_string(),
        };
        assert_eq!(
            status,
            HttpError::Status {
                code: 503,
                snippet: "busy".to_string()
            }
        );
        let transport = HttpError::Transport {
            kind: TransportKind::Dns,
            message: "dns failed".to_string(),
        };
        assert!(matches!(
            transport,
            HttpError::Transport {
                kind: TransportKind::Dns,
                ..
            }
        ));
        assert_eq!(
            HttpError::Body("bad json".to_string()),
            HttpError::Body("bad json".to_string())
        );
    }
}
