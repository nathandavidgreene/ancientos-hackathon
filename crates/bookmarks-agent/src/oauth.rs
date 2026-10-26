//! OAuth2 PKCE (public client, no secret) for read-only X access.
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

/// S256 code challenge for a verifier (RFC 7636).
pub fn code_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Build the authorize URL. Verifier/state generation and the token exchange are TODO.
pub fn authorize_url(client_id: &str, verifier: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        enc(client_id),
        enc(REDIRECT_URI),
        enc(&SCOPES.join(" ")),
        enc(state),
        code_challenge(verifier)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
