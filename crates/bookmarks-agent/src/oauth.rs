//! OAuth2 PKCE (public client, no secret) for read-only X access.
//!
//! Native App flow: random verifier/state, loopback redirect, token exchange
//! and refresh. There is deliberately no client secret anywhere here.
use sha2::{Digest, Sha256};

pub const AUTHORIZE_URL: &str = "https://x.com/i/oauth2/authorize";
pub const TOKEN_URL: &str = "https://api.x.com/2/oauth2/token";
pub const REDIRECT_URI: &str = "http://127.0.0.1:8080/callback";
pub const SCOPES: &[&str] = &[
    "tweet.read",
    "users.read",
    "bookmark.read",
    "offline.access",
];

/// RFC 7636 verifier length bounds (characters).
pub const VERIFIER_MIN_LEN: usize = 43;
pub const VERIFIER_MAX_LEN: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("random source failed: {0}")]
    Random(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("bad token response: {0}")]
    BadResponse(String),
}

/// Token endpoint response. Never printed: callers must not log this.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

/// base64url without padding.
fn b64url(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = c
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..=c.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Random base64url string from `nbytes` of OS randomness.
fn random_b64url(nbytes: usize) -> Result<String, OAuthError> {
    let mut buf = vec![0u8; nbytes];
    getrandom::getrandom(&mut buf).map_err(|e| OAuthError::Random(e.to_string()))?;
    Ok(b64url(&buf))
}

/// Random PKCE verifier: 43-128 chars from the system RNG.
pub fn generate_verifier() -> Result<String, OAuthError> {
    // 32 bytes -> 43 chars, the RFC 7636 minimum.
    random_b64url(32)
}

/// Random OAuth state parameter from the system RNG.
pub fn generate_state() -> Result<String, OAuthError> {
    random_b64url(32)
}

/// S256 code challenge for a verifier (RFC 7636).
pub fn code_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

/// Percent-encode a string for use in a URL or form body.
pub(crate) fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Build the authorize URL for a Native App (public client, S256).
pub fn authorize_url(client_id: &str, verifier: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        url_encode(client_id),
        url_encode(REDIRECT_URI),
        url_encode(&SCOPES.join(" ")),
        url_encode(state),
        code_challenge(verifier)
    )
}

/// Form fields for the authorization_code exchange (pure, for tests).
pub fn exchange_form<'a>(
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    verifier: &'a str,
) -> [(&'static str, &'a str); 5] {
    [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ]
}

/// Form fields for a refresh_token grant (pure, for tests).
pub fn refresh_form<'a>(
    refresh_token: &'a str,
    client_id: &'a str,
) -> [(&'static str, &'a str); 3] {
    [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ]
}

/// POST a form to the X token endpoint and parse the token response.
fn post_token_form(form: &[(&str, &str)]) -> Result<TokenResponse, OAuthError> {
    let parsed: TokenResponse = ureq::post(TOKEN_URL)
        .timeout(std::time::Duration::from_secs(30))
        .send_form(form)
        .map_err(map_ureq_error)?
        .into_json()
        .map_err(|e| OAuthError::BadResponse(e.to_string()))?;
    if parsed.access_token.is_empty() {
        return Err(OAuthError::BadResponse("missing access_token".into()));
    }
    Ok(parsed)
}

/// Exchange an authorization code for tokens (public client, no secret).
/// Never logs the code, verifier, or tokens.
pub fn exchange_code(
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, OAuthError> {
    if client_id.is_empty() || code.is_empty() || verifier.is_empty() {
        return Err(OAuthError::BadResponse(
            "client_id, code, and verifier must all be non-empty".into(),
        ));
    }
    post_token_form(&exchange_form(code, redirect_uri, client_id, verifier))
}

/// Refresh an expired access token (public client, no secret).
pub fn refresh_tokens(client_id: &str, refresh_token: &str) -> Result<TokenResponse, OAuthError> {
    if client_id.is_empty() || refresh_token.is_empty() {
        return Err(OAuthError::BadResponse(
            "client_id and refresh_token must both be non-empty".into(),
        ));
    }
    post_token_form(&refresh_form(refresh_token, client_id))
}

/// Map a ureq error to an OAuth error without leaking secrets.
fn map_ureq_error(e: ureq::Error) -> OAuthError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            OAuthError::Http(format!("token endpoint http status {code}: {snippet}"))
        }
        other => OAuthError::Http(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: whether a char is valid in a PKCE verifier.
    fn is_verifier_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~')
    }

    #[test]
    fn rfc7636_challenge() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn scopes_are_read_only() {
        let u = authorize_url("cid", "v", "s");
        assert!(u.contains("bookmark.read") && !u.contains("write"));
        assert!(u.contains("127.0.0.1%3A8080%2Fcallback"));
    }

    #[test]
    fn verifier_has_valid_length_and_charset() {
        for _ in 0..8 {
            let v = generate_verifier().unwrap();
            assert!(
                (VERIFIER_MIN_LEN..=VERIFIER_MAX_LEN).contains(&v.len()),
                "bad length {}",
                v.len()
            );
            assert!(v.chars().all(is_verifier_char), "bad charset: {v}");
        }
    }

    #[test]
    fn verifier_and_state_are_random() {
        assert_ne!(generate_verifier().unwrap(), generate_verifier().unwrap());
        let s = generate_state().unwrap();
        assert!(!s.is_empty());
        assert!(s.chars().all(is_verifier_char));
        assert_ne!(s, generate_state().unwrap());
    }

    #[test]
    fn exchange_form_has_pkce_fields() {
        let form = exchange_form("code1", "redir", "cid", "verif");
        let get = |k: &str| form.iter().find(|(key, _)| *key == k).unwrap().1;
        assert_eq!(get("grant_type"), "authorization_code");
        assert_eq!(get("code"), "code1");
        assert_eq!(get("redirect_uri"), "redir");
        assert_eq!(get("client_id"), "cid");
        assert_eq!(get("code_verifier"), "verif");
        assert!(!form.iter().any(|(k, _)| *k == "client_secret"));
    }

    #[test]
    fn refresh_form_has_no_secret() {
        let form = refresh_form("rt", "cid");
        let get = |k: &str| form.iter().find(|(key, _)| *key == k).unwrap().1;
        assert_eq!(get("grant_type"), "refresh_token");
        assert_eq!(get("refresh_token"), "rt");
        assert_eq!(get("client_id"), "cid");
        assert!(!form.iter().any(|(k, _)| *k == "client_secret"));
    }

    #[test]
    fn token_response_parses_sample() {
        let raw = serde_json::json!({
            "token_type": "bearer",
            "expires_in": 7200,
            "access_token": "a",
            "refresh_token": "r",
            "scope": "tweet.read users.read bookmark.read offline.access"
        });
        let parsed: TokenResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.access_token, "a");
        assert_eq!(parsed.refresh_token.as_deref(), Some("r"));
        assert_eq!(parsed.expires_in, Some(7200));
    }

    #[test]
    fn empty_exchange_inputs_fail_closed() {
        assert!(exchange_code("", "c", "v", REDIRECT_URI).is_err());
        assert!(exchange_code("cid", "", "v", REDIRECT_URI).is_err());
        assert!(refresh_tokens("cid", "").is_err());
    }
}
