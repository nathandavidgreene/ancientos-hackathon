//! Loopback OAuth2 callback listener on 127.0.0.1:8080.
//!
//! Accepts one `GET /callback?code=..&state=..` request, verifies the state
//! matches (fail closed), and replies with a small "close this tab" page.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

pub const CALLBACK_ADDR: &str = "127.0.0.1:8080";
pub const CALLBACK_PATH: &str = "/callback";
/// How long `wait_for_code` blocks for the browser redirect.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub const CLOSE_TAB_HTML: &str =
    "<html><body><h1>Login complete</h1><p>You can close this tab.</p></body></html>";

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out waiting for the browser callback")]
    Timeout,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("state mismatch (possible CSRF); denying login")]
    StateMismatch,
    #[error("authorization server error: {0}")]
    ServerError(String),
    #[error("callback is missing the code parameter")]
    MissingCode,
}

/// Percent-decode a URL query value; fails closed on malformed escapes.
fn percent_decode(s: &str) -> Result<String, CallbackError> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(CallbackError::BadRequest("truncated percent escape".into()));
                }
                let hex = |b: u8| match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                };
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(h), Some(l)) => out.push(h * 16 + l),
                    _ => {
                        return Err(CallbackError::BadRequest("invalid percent escape".into()));
                    }
                }
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| CallbackError::BadRequest("query is not valid UTF-8".into()))
}

/// Parse a raw HTTP callback request. Returns the authorization code when the
/// path, state, and code all check out. Pure and unit-tested.
pub fn parse_callback_request(
    request: &str,
    expected_state: &str,
) -> Result<String, CallbackError> {
    if expected_state.is_empty() {
        return Err(CallbackError::StateMismatch);
    }
    let line = request.lines().next().unwrap_or_default();
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" {
        return Err(CallbackError::BadRequest("only GET is accepted".into()));
    }
    let query = target
        .strip_prefix(CALLBACK_PATH)
        .and_then(|rest| rest.strip_prefix('?'))
        .ok_or_else(|| CallbackError::BadRequest("path must be /callback?..".into()))?;

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "error" => {
                let desc = format!("error={}", percent_decode(v)?);
                return Err(CallbackError::ServerError(desc));
            }
            "code" => code = Some(percent_decode(v)?),
            "state" => state = Some(percent_decode(v)?),
            _ => {}
        }
    }
    match state {
        Some(s) if s == expected_state => {}
        _ => return Err(CallbackError::StateMismatch),
    }
    match code {
        Some(c) if !c.is_empty() => Ok(c),
        _ => Err(CallbackError::MissingCode),
    }
}

/// Build a minimal HTTP/1.1 response string with the given status and body.
fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serve one OAuth callback connection and return the code if `state` matches.
fn handle_connection(
    stream: std::net::TcpStream,
    expected_state: &str,
) -> Result<String, CallbackError> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut raw = String::new();
    // Read the request line plus headers (bounded); the query is on line one.
    for _ in 0..64 {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        raw.push_str(&line);
        if n == 0 || line == "\r\n" || line == "\n" || raw.len() > 16 * 1024 {
            break;
        }
    }
    let result = parse_callback_request(&raw, expected_state);
    let mut stream = reader.into_inner();
    let reply = match &result {
        Ok(_) => http_response("200 OK", CLOSE_TAB_HTML),
        Err(e) => http_response(
            "400 Bad Request",
            &format!("<html><body><h1>Login failed</h1><p>{e}</p><p>You can close this tab.</p></body></html>"),
        ),
    };
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
    result
}

/// Bind the loopback listener and wait for one callback. Fails closed on
/// timeout, state mismatch, or any protocol error.
pub fn wait_for_code_with_timeout(
    expected_state: &str,
    timeout: Duration,
) -> Result<String, CallbackError> {
    if expected_state.is_empty() {
        return Err(CallbackError::StateMismatch);
    }
    let listener = TcpListener::bind(CALLBACK_ADDR)?;
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                return handle_connection(stream, expected_state);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(CallbackError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(CallbackError::Io(e)),
        }
    }
}

/// Wait up to [`CALLBACK_TIMEOUT`] for the browser callback.
pub fn wait_for_code(expected_state: &str) -> Result<String, CallbackError> {
    wait_for_code_with_timeout(expected_state, CALLBACK_TIMEOUT)
}

/// Drain helper for tests: read a full reply until the server closes.
#[cfg(test)]
fn read_all(stream: &std::net::TcpStream) -> String {
    use std::io::Read;
    let mut s = String::new();
    let mut reader = BufReader::new(stream);
    let _ = reader.read_to_string(&mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: req.
    fn req(target: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
    }

    #[test]
    fn parses_valid_callback() {
        let code = parse_callback_request(&req("/callback?code=abc123&state=xyz"), "xyz").unwrap();
        assert_eq!(code, "abc123");
    }

    #[test]
    fn percent_decoding_applies_to_code_and_state() {
        let code =
            parse_callback_request(&req("/callback?code=a%2Fb%3Dc&state=x%20y"), "x y").unwrap();
        assert_eq!(code, "a/b=c");
    }

    #[test]
    fn state_mismatch_fails_closed() {
        let r = parse_callback_request(&req("/callback?code=c&state=wrong"), "right");
        assert!(matches!(r, Err(CallbackError::StateMismatch)));
        let missing = parse_callback_request(&req("/callback?code=c"), "right");
        assert!(matches!(missing, Err(CallbackError::StateMismatch)));
        let empty_expected = parse_callback_request(&req("/callback?code=c&state=s"), "");
        assert!(matches!(empty_expected, Err(CallbackError::StateMismatch)));
    }

    #[test]
    fn error_param_fails_closed() {
        let r = parse_callback_request(&req("/callback?error=access_denied&state=s"), "s");
        assert!(matches!(r, Err(CallbackError::ServerError(_))));
    }

    #[test]
    fn missing_code_fails_closed() {
        let r = parse_callback_request(&req("/callback?state=s"), "s");
        assert!(matches!(r, Err(CallbackError::MissingCode)));
    }

    #[test]
    fn wrong_path_or_method_fails_closed() {
        assert!(matches!(
            parse_callback_request(&req("/other?code=c&state=s"), "s"),
            Err(CallbackError::BadRequest(_))
        ));
        assert!(matches!(
            parse_callback_request("POST /callback?code=c&state=s HTTP/1.1\r\n\r\n", "s"),
            Err(CallbackError::BadRequest(_))
        ));
        assert!(matches!(
            parse_callback_request("", "s"),
            Err(CallbackError::BadRequest(_))
        ));
    }

    #[test]
    fn reply_page_mentions_closing_the_tab() {
        assert!(CLOSE_TAB_HTML.contains("You can close this tab"));
        let ok = http_response("200 OK", CLOSE_TAB_HTML);
        assert!(ok.starts_with("HTTP/1.1 200 OK"));
        assert!(ok.contains("You can close this tab"));
    }

    #[test]
    fn listener_roundtrip_accepts_one_callback() {
        // Binds the real loopback port; other tests here are pure.
        let state = "roundtrip-state";
        let handle =
            std::thread::spawn(|| wait_for_code_with_timeout(state, Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(200));
        let mut stream = std::net::TcpStream::connect(CALLBACK_ADDR).unwrap();
        stream
            .write_all(req("/callback?code=the-code&state=roundtrip-state").as_bytes())
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let body = read_all(&stream);
        assert!(body.contains("You can close this tab"), "reply: {body}");
        assert_eq!(handle.join().unwrap().unwrap(), "the-code");
    }
}
