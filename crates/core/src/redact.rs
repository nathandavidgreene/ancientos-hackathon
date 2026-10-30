//! Secret redaction for hosted-model inputs and the decision log.
//!
//! [`redact_text`] finds vendor key formats (AWS access key ids and paired
//! secrets, GitHub, Slack, `sk-` style, Google, Stripe, JWTs, Bearer tokens)
//! and structural secrets (URL userinfo passwords, PEM private-key blocks,
//! credential assignments) in free text and replaces each with a short tagged
//! marker (`[redacted:<kind>]`, e.g. `[redacted:github_token]`).
//! [`redact_json`] additionally redacts values behind secret-named object keys
//! (`token`, `secret`, `password`, `api_key`, `credential`, `authorization`)
//! at any depth, and scans every other string for embedded secrets.
//!
//! Obvious placeholders (`<your-key>`, `xxxx`, `changeme`, `example`,
//! `${VAR}`, already-redacted markers) are left untouched so docs and
//! examples pass through. See [`is_placeholder`] for the exact rule.
//!
//! The scanners are plain byte matching written from scratch for this repo
//! (no regex engine, no I/O), so redaction is infallible by construction:
//! there is no error path through which a secret could leak unscanned.
//! [`DecisionLog::append`](crate::log::DecisionLog::append) redacts every
//! payload before hashing and writing, and the bookmarks agent runs
//! [`redact_text`] inside `sanitize`, so every hosted-call input is redacted.
//!
//! Fail closed: ambiguous input is redacted, never passed through. Unknown
//! shapes are not detected, so classification keeps unknown content local.

use serde_json::Value;

/// Kind of secret detected. [`Kind::as_str`] is the tag in `[redacted:<tag>]`
/// and in the decision log's `redacted` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// AWS access key id (`AKIA...` / `ASIA...`, 20 chars).
    AwsAccessKey,
    /// AWS secret access key: `secret_access_key` assignment value, or a
    /// 40-char secret-looking run paired with an access key id in one text.
    AwsSecretKey,
    /// GitHub token (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`).
    GithubToken,
    /// Slack token (`xoxa-`, `xoxb-`, `xoxp-`, `xoxr-`, `xoxs-`).
    SlackToken,
    /// `sk-` style secret key (OpenAI / Anthropic shape).
    SecretKey,
    /// Google API key (`AIza...`, 39 chars).
    GoogleApiKey,
    /// Stripe key (`sk_live_`, `rk_live_`).
    StripeKey,
    /// JSON Web Token (`eyJ...` with three base64url segments).
    Jwt,
    /// Token after the whole word `Bearer` (Authorization headers).
    BearerToken,
    /// Password in URL userinfo (`scheme://user:pass@host`).
    UrlPassword,
    /// PEM `-----BEGIN ... PRIVATE KEY-----` armour block.
    PrivateKey,
    /// Credential assignment (`password=` / `token:` / `*_API_KEY=` ...).
    CredentialAssignment,
    /// Value behind a secret-named JSON object key (see [`is_secret_key`]).
    JsonKey,
}

impl Kind {
    /// Short snake_case tag used in `[redacted:<tag>]` markers.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwsAccessKey => "aws_access_key",
            Self::AwsSecretKey => "aws_secret_key",
            Self::GithubToken => "github_token",
            Self::SlackToken => "slack_token",
            Self::SecretKey => "secret_key",
            Self::GoogleApiKey => "google_api_key",
            Self::StripeKey => "stripe_key",
            Self::Jwt => "jwt",
            Self::BearerToken => "bearer_token",
            Self::UrlPassword => "url_password",
            Self::PrivateKey => "private_key",
            Self::CredentialAssignment => "credential_assignment",
            Self::JsonKey => "json_key",
        }
    }

    /// Placeholder text that replaces a secret of this kind.
    fn replacement(self) -> String {
        format!("[redacted:{}]", self.as_str())
    }
}

impl std::fmt::Display for Kind {
    /// Short name of the secret kind.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Output of [`redact_text`]: the scrubbed text plus one entry per kind of
/// secret found (deduplicated, first-seen order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub hits: Vec<Kind>,
}

/// True when an object key looks secret-bearing: its lowercased alphanumeric
/// form contains secret synonyms including keys, auth, bearer, cookies,
/// sessions, passwords and private material. Matches `access_token`, `api_key`, `apiKey`,
/// `client_secret`, `password`, `credentials`, `authorization`, and nested
/// equivalents. Shared by [`redact_json`] and the approvals queue.
pub fn is_secret_key(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    [
        "token",
        "secret",
        "password",
        "apikey",
        "credential",
        "authorization",
        "key",
        "auth",
        "bearer",
        "cookie",
        "session",
        "passwd",
        "pwd",
        "private",
    ]
    .iter()
    .any(|needle| norm.contains(needle))
}

/// True when a would-be secret value is an obvious placeholder, left
/// untouched: empty, anything with `<...>`, anything with `${...}`, short
/// `x` runs (`xxxx`, up to 8), `*` runs, already-redacted markers
/// (`[REDACTED]`, `[redacted]`, `[redacted:<kind>]`), or exactly (case
/// insensitive) `changeme`, `example`, `your-key` and similar doc words.
/// Longer `x` runs are treated as secrets: scanner-safe fakes are full
/// length, placeholders are short. Anything else fails closed to secret.
pub fn is_placeholder(value: &str) -> bool {
    let t = value.trim();
    if t.is_empty() {
        return true;
    }
    if t.contains('<') || t.contains('>') || t.contains("${") {
        return true;
    }
    let lower = t.to_lowercase();
    if lower == "[redacted]" || (lower.starts_with("[redacted:") && lower.ends_with(']')) {
        return true;
    }
    if t.len() >= 2 && t.len() <= 8 && t.bytes().all(|b| b == b'x' || b == b'X') {
        return true;
    }
    if t.len() >= 2 && t.bytes().all(|b| b == b'*') {
        return true;
    }
    matches!(
        lower.as_str(),
        "changeme"
            | "change-me"
            | "change_me"
            | "example"
            | "examples"
            | "your-key"
            | "yourkey"
            | "your_key"
            | "your-api-key"
            | "your_api_key"
            | "placeholder"
            | "none"
            | "null"
            | "nil"
            | "undefined"
    )
}

/// True for ASCII word bytes.
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True for bytes allowed in a vendor key.
fn is_vendor_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

/// True for bytes allowed in a bearer token.
fn is_bearer_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
}

/// True for base64 bytes.
fn is_b64(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'/' | b'+' | b'=')
}

/// True for base64url bytes.
fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Case-sensitive byte search from `from`.
fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Case-insensitive byte search from `from` (`needle` must be lowercase).
/// Byte-indexed, so matches line up with the original text.
fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Non-overlapping replacement spans; first claim wins (callers scan in
/// priority order: PEM blocks, URL passwords, Bearer, JWT, vendor prefixes,
/// AWS pairs, assignments).
struct Spans(Vec<(usize, usize, Kind)>);

impl Spans {
    /// Claim a byte span for redaction unless it overlaps an earlier claim.
    fn claim(&mut self, start: usize, end: usize, kind: Kind) {
        if end <= start {
            return;
        }
        if self.0.iter().any(|(s, e, _)| start < *e && *s < end) {
            return;
        }
        self.0.push((start, end, kind));
    }
}

/// Find PEM private key blocks.
fn scan_private_keys(text: &str, spans: &mut Spans) {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(i) = find_bytes(bytes, b"-----BEGIN", from) {
        from = i + 10;
        let Some(le) = find_bytes(bytes, b"-----", i + 10) else {
            continue;
        };
        let label = text.get(i + 10..le).unwrap_or("");
        if !label.to_lowercase().contains("private key") {
            continue;
        }
        let marker_end = le + 5;
        let mut end_span = None;
        let mut efrom = marker_end;
        while let Some(ei) = find_bytes(bytes, b"-----END", efrom) {
            let Some(ee) = find_bytes(bytes, b"-----", ei + 8) else {
                break;
            };
            let elabel = text.get(ei + 8..ee).unwrap_or("");
            if elabel.to_lowercase().contains("private key") {
                end_span = Some(ee + 5);
                break;
            }
            efrom = ei + 8;
        }
        match end_span {
            Some(e) => spans.claim(i, e, Kind::PrivateKey),
            None => {
                // Unterminated block: redact the BEGIN line only.
                let mut le2 = marker_end;
                while le2 < bytes.len() && bytes[le2] != b'\n' {
                    le2 += 1;
                }
                spans.claim(i, le2, Kind::PrivateKey);
            }
        }
    }
}

/// Find passwords inside URLs (`scheme://user:pass@host`).
fn scan_url_passwords(input: &str, bytes: &[u8], spans: &mut Spans) {
    let mut from = 0;
    while let Some(i) = find_bytes(bytes, b"://", from) {
        from = i + 3;
        if i == 0 {
            continue;
        }
        let mut s = i;
        while s > 0
            && matches!(
                bytes[s - 1],
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'-' | b'.'
            )
        {
            s -= 1;
        }
        if s == i || !bytes[s].is_ascii_alphabetic() {
            continue;
        }
        let mut a = i + 3;
        while a < bytes.len()
            && !matches!(
                bytes[a],
                b'/' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'<' | b'>' | b'\\'
            )
        {
            a += 1;
        }
        let auth = &bytes[i + 3..a];
        let Some(at) = auth.iter().rposition(|b| *b == b'@') else {
            continue;
        };
        let userinfo = &auth[..at];
        let Some(colon) = userinfo.iter().position(|b| *b == b':') else {
            continue;
        };
        if colon == 0 || colon + 1 >= userinfo.len() {
            continue;
        }
        let ps = i + 3 + colon + 1;
        let pe = i + 3 + at;
        let pass = input.get(ps..pe).unwrap_or("");
        if is_placeholder(pass) {
            continue;
        }
        spans.claim(ps, pe, Kind::UrlPassword);
    }
}

/// Find `Bearer` tokens.
fn scan_bearers(input: &str, bytes: &[u8], spans: &mut Spans) {
    let mut from = 0;
    while let Some(i) = find_ci(bytes, b"bearer", from) {
        from = i + 1;
        if i > 0 && is_word(bytes[i - 1]) {
            continue;
        }
        let after = i + "bearer".len();
        if after < bytes.len() && is_word(bytes[after]) {
            continue;
        }
        let mut s = after;
        while s < bytes.len() && bytes[s].is_ascii_whitespace() {
            s += 1;
        }
        let mut e = s;
        while e < bytes.len() && is_bearer_char(bytes[e]) {
            e += 1;
        }
        while e > s && bytes[e - 1] == b'.' {
            e -= 1;
        }
        if e - s < 8 {
            continue;
        }
        let tok = input.get(s..e).unwrap_or("");
        if is_placeholder(tok) {
            continue;
        }
        spans.claim(s, e, Kind::BearerToken);
    }
}

/// Find JWTs (three base64url parts).
fn scan_jwts(bytes: &[u8], spans: &mut Spans) {
    let mut from = 0;
    while let Some(i) = find_bytes(bytes, b"eyJ", from) {
        from = i + 1;
        if i > 0 && (is_b64url(bytes[i - 1]) || bytes[i - 1] == b'.') {
            continue;
        }
        let mut d1 = i;
        while d1 < bytes.len() && is_b64url(bytes[d1]) {
            d1 += 1;
        }
        if d1 >= bytes.len() || bytes[d1] != b'.' {
            continue;
        }
        let mut d2 = d1 + 1;
        while d2 < bytes.len() && is_b64url(bytes[d2]) {
            d2 += 1;
        }
        if d2 >= bytes.len() || bytes[d2] != b'.' || d2 == d1 + 1 {
            continue;
        }
        let mut d3 = d2 + 1;
        while d3 < bytes.len() && is_b64url(bytes[d3]) {
            d3 += 1;
        }
        if d1 - i < 10 || d2 - (d1 + 1) < 10 || d3 - (d2 + 1) < 8 {
            continue;
        }
        spans.claim(i, d3, Kind::Jwt);
    }
}

/// Find vendor keys that start with one of `prefixes`.
fn scan_prefixed(
    input: &str,
    bytes: &[u8],
    prefixes: &[&str],
    kind: Kind,
    min_rest: usize,
    spans: &mut Spans,
) {
    for p in prefixes {
        let pb = p.as_bytes();
        let mut from = 0;
        while let Some(i) = find_bytes(bytes, pb, from) {
            from = i + 1;
            if i > 0 && is_word(bytes[i - 1]) {
                continue;
            }
            let mut end = i + pb.len();
            while end < bytes.len() && is_vendor_char(bytes[end]) {
                end += 1;
            }
            while end > i + pb.len() && matches!(bytes[end - 1], b'-' | b'_' | b'.') {
                end -= 1;
            }
            let rest = input.get(i + pb.len()..end).unwrap_or("");
            if rest.len() < min_rest || is_placeholder(rest) {
                continue;
            }
            spans.claim(i, end, kind);
        }
    }
}

/// Find AWS access key ids.
fn scan_aws_access_keys(input: &str, bytes: &[u8], spans: &mut Spans) {
    for p in ["AKIA", "ASIA"] {
        let pb = p.as_bytes();
        let mut from = 0;
        while let Some(i) = find_bytes(bytes, pb, from) {
            from = i + 1;
            if i > 0 && is_word(bytes[i - 1]) {
                continue;
            }
            let mut end = i + pb.len();
            while end < bytes.len() && matches!(bytes[end], b'A'..=b'Z' | b'0'..=b'9') {
                end += 1;
            }
            let rest = input.get(i + pb.len()..end).unwrap_or("");
            if rest.len() < 16 || is_placeholder(rest) {
                continue;
            }
            spans.claim(i, end, Kind::AwsAccessKey);
        }
    }
}

/// True if the bytes contain an AWS access key id.
fn has_aws_access_key(bytes: &[u8]) -> bool {
    for p in ["AKIA", "ASIA"] {
        let pb = p.as_bytes();
        let mut from = 0;
        while let Some(i) = find_bytes(bytes, pb, from) {
            from = i + 1;
            if i > 0 && is_word(bytes[i - 1]) {
                continue;
            }
            let mut end = i + pb.len();
            while end < bytes.len() && matches!(bytes[end], b'A'..=b'Z' | b'0'..=b'9') {
                end += 1;
            }
            if end - (i + pb.len()) >= 16 {
                return true;
            }
        }
    }
    false
}

/// A 40-char secret-looking run counts only paired with an AWS access key id
/// in the same text (bare 40-char runs are too often git SHAs or hashes).
fn scan_aws_secret_pair(input: &str, bytes: &[u8], spans: &mut Spans) {
    if !has_aws_access_key(bytes) {
        return;
    }
    let mut i = 0;
    while i < bytes.len() {
        if !is_b64(bytes[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && is_b64(bytes[j]) {
            j += 1;
        }
        if j - i == 40 {
            let run = input.get(i..j).unwrap_or("");
            if !is_placeholder(run) {
                spans.claim(i, j, Kind::AwsSecretKey);
            }
        }
        i = j;
    }
}

/// Credential assignment keys, longest first so `auth_token` wins over
/// `token` on the same text.
const ASSIGN_KEYS: &[&str] = &[
    "aws_secret_access_key",
    "secret_access_key",
    "client_secret",
    "access_token",
    "auth_token",
    "authorization",
    "api_key",
    "apikey",
    "api-key",
    "api key",
    "password",
    "passwd",
    "secret",
    "token",
];

/// Find secret-looking `key=value` assignments.
fn scan_assignments(input: &str, bytes: &[u8], spans: &mut Spans) {
    for key in ASSIGN_KEYS {
        let kb = key.as_bytes();
        let mut from = 0;
        while let Some(i) = find_ci(bytes, kb, from) {
            from = i + 1;
            let ke = i + kb.len();
            // Env-style prefixes (`FOO_PASSWORD=`) match; longer words
            // (`tokens`, `secretary`) do not.
            if ke < bytes.len() && is_word(bytes[ke]) {
                continue;
            }
            let mut p = ke;
            while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                p += 1;
            }
            // Closing quote of a JSON key (`"password": ...`).
            if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
                p += 1;
            }
            while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                p += 1;
            }
            if p >= bytes.len() || (bytes[p] != b':' && bytes[p] != b'=') {
                continue;
            }
            p += 1;
            while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                p += 1;
            }
            if p >= bytes.len() {
                continue;
            }
            let (vs, ve) = if bytes[p] == b'"' || bytes[p] == b'\'' {
                let q = bytes[p];
                let mut q2 = p + 1;
                while q2 < bytes.len() && bytes[q2] != q && bytes[q2] != b'\n' && bytes[q2] != b'\r'
                {
                    q2 += 1;
                }
                if q2 < bytes.len() && bytes[q2] == q {
                    (p + 1, q2)
                } else {
                    let mut e2 = p + 1;
                    while e2 < bytes.len() && !bytes[e2].is_ascii_whitespace() {
                        e2 += 1;
                    }
                    (p + 1, e2)
                }
            } else {
                let mut e2 = p;
                while e2 < bytes.len() && !bytes[e2].is_ascii_whitespace() {
                    e2 += 1;
                }
                while e2 > p
                    && matches!(
                        bytes[e2 - 1],
                        b',' | b'.' | b';' | b':' | b'!' | b'?' | b')' | b']' | b'}' | b'"' | b'\''
                    )
                {
                    e2 -= 1;
                }
                (p, e2)
            };
            if ve <= vs {
                continue;
            }
            let val = input.get(vs..ve).unwrap_or("");
            if val.chars().count() < 4 {
                continue;
            }
            // `Authorization: Bearer <token>` belongs to the Bearer rule.
            if val.eq_ignore_ascii_case("bearer") {
                continue;
            }
            if is_placeholder(val) {
                continue;
            }
            let kind = if *key == "secret_access_key" || *key == "aws_secret_access_key" {
                Kind::AwsSecretKey
            } else {
                Kind::CredentialAssignment
            };
            spans.claim(vs, ve, kind);
        }
    }
}

/// Redact secrets in free text, replacing each with `[redacted:<kind>]`.
/// Returns the scrubbed text and the kinds found (deduplicated).
pub fn redact_text(input: &str) -> Redacted {
    let bytes = input.as_bytes();
    let mut spans = Spans(Vec::new());
    scan_private_keys(input, &mut spans);
    scan_url_passwords(input, bytes, &mut spans);
    scan_bearers(input, bytes, &mut spans);
    scan_jwts(bytes, &mut spans);
    scan_prefixed(
        input,
        bytes,
        &["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"],
        Kind::GithubToken,
        8,
        &mut spans,
    );
    scan_prefixed(
        input,
        bytes,
        &["xoxa-", "xoxb-", "xoxp-", "xoxr-", "xoxs-"],
        Kind::SlackToken,
        8,
        &mut spans,
    );
    scan_prefixed(
        input,
        bytes,
        &["sk_live_", "rk_live_"],
        Kind::StripeKey,
        8,
        &mut spans,
    );
    scan_prefixed(input, bytes, &["AIza"], Kind::GoogleApiKey, 8, &mut spans);
    // `sk-` needs a longer tail: English `sk-` compounds (`flask-based`)
    // are short, real keys are not.
    scan_prefixed(input, bytes, &["sk-"], Kind::SecretKey, 10, &mut spans);
    scan_aws_access_keys(input, bytes, &mut spans);
    scan_aws_secret_pair(input, bytes, &mut spans);
    scan_assignments(input, bytes, &mut spans);

    let mut v = spans.0;
    v.sort_by_key(|(s, _, _)| *s);
    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    let mut hits = Vec::new();
    for (s, e, k) in v {
        out.push_str(&input[pos..s]);
        out.push_str(&k.replacement());
        if !hits.contains(&k) {
            hits.push(k);
        }
        pos = e;
    }
    out.push_str(&input[pos..]);
    Redacted { text: out, hits }
}

/// Redact every string in a JSON value, collecting hit kinds.
fn walk_json(value: &Value, hits: &mut Vec<Kind>) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if is_secret_key(k) {
                        match v {
                            Value::String(s) if !is_placeholder(s) => {
                                hits.push(Kind::JsonKey);
                                (k.clone(), Value::String(Kind::JsonKey.replacement()))
                            }
                            Value::Object(_) | Value::Array(_) => {
                                hits.push(Kind::JsonKey);
                                (k.clone(), Value::String(Kind::JsonKey.replacement()))
                            }
                            // Placeholders, already-redacted markers, and
                            // non-string scalars (numeric telemetry such as
                            // `prompt_tokens`) are not secret content.
                            _ => (k.clone(), v.clone()),
                        }
                    } else {
                        (k.clone(), walk_json(v, hits))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| walk_json(v, hits)).collect()),
        Value::String(s) => {
            let r = redact_text(s);
            hits.extend(r.hits);
            Value::String(r.text)
        }
        scalar => scalar.clone(),
    }
}

/// Redact secrets in a JSON value: secret-named keys (see [`is_secret_key`])
/// at any depth plus embedded secrets in every other string. Returns the
/// scrubbed value and the kinds found (deduplicated, first-seen order).
/// Values that are already clean come back equal and with no hits, and
/// redacting twice is a fixed point.
pub fn redact_json(value: &Value) -> (Value, Vec<Kind>) {
    let mut hits = Vec::new();
    let out = walk_json(value, &mut hits);
    let mut dedup = Vec::new();
    for k in hits {
        if !dedup.contains(&k) {
            dedup.push(k);
        }
    }
    (out, dedup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Scanner-safe fakes, built at runtime (never literal secrets in source).
    /// Test helper: fake AWS access key ID safe for source scanning.
    fn aws_id() -> String {
        format!("AKIA{}", "A".repeat(16))
    }
    /// Test helper: fake 40-character AWS secret key.
    fn aws_secret() -> String {
        "B".repeat(40)
    }
    /// Test helper: fake GitHub token with the given prefix.
    fn ghp(prefix: &str) -> String {
        format!("{prefix}{}", "C".repeat(36))
    }
    /// Test helper: fake Slack token with the given prefix.
    fn slack(prefix: &str) -> String {
        format!("{prefix}{}", "D".repeat(24))
    }
    /// Test helper: fake OpenAI-style secret key.
    fn sk() -> String {
        format!("sk-{}", "E".repeat(32))
    }
    /// Test helper: fake Google API key.
    fn google() -> String {
        format!("AIza{}", "F".repeat(35))
    }
    /// Test helper: fake Stripe key with the given prefix.
    fn stripe(prefix: &str) -> String {
        format!("{prefix}{}", "G".repeat(24))
    }
    /// Test helper: fake JWT-shaped token.
    fn jwt() -> String {
        format!(
            "eyJ{}.{}.{}",
            "h".repeat(10),
            "i".repeat(20),
            "k".repeat(10)
        )
    }
    /// Test helper: synthetic Authorization header for auth-redaction tests.
    fn bearer() -> String {
        format!("Authorization: Bearer {}", "L".repeat(32))
    }
    /// Test helper: fake PEM-encoded RSA private key block.
    fn pem() -> String {
        format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n{}\n-----END RSA PRIVATE KEY-----",
            "M".repeat(64),
            "N".repeat(64),
        )
    }

    /// Test helper: verifies the secret is replaced by its redaction marker.
    fn assert_redacted(input: &str, kind: Kind, secret_part: &str) {
        let r = redact_text(input);
        assert!(
            r.hits.contains(&kind),
            "missing {kind:?} hit in {input:?} -> {:?}",
            r.text
        );
        assert!(!r.text.contains(secret_part), "secret leaked: {:?}", r.text);
        assert!(
            r.text.contains(&format!("[redacted:{}]", kind.as_str())),
            "missing tag: {:?}",
            r.text
        );
    }

    /// Test helper: verifies input passes through redaction unchanged.
    fn assert_clean(input: &str) {
        let r = redact_text(input);
        assert!(r.hits.is_empty(), "false hit on {input:?}: {:?}", r.hits);
        assert_eq!(r.text, input);
    }

    #[test]
    fn structural_secret_synonyms_are_always_redacted() {
        for key in [
            "private_key",
            "Private-Key",
            "bearer",
            "Bearer",
            "cookie",
            "session_cookie",
            "passwd",
            "pwd",
            "key",
            "auth",
            "session",
        ] {
            assert!(is_secret_key(key), "missed structural secret synonym");
            let value = serde_json::json!({key: "opaque confidential value"});
            let redacted = redact_json(&value).0;
            assert!(!serde_json::to_string(&redacted)
                .unwrap()
                .contains("opaque confidential value"));
        }
    }

    #[test]
    fn vendor_formats_are_caught() {
        assert_redacted(&format!("key {}", aws_id()), Kind::AwsAccessKey, "AKIA");
        assert_redacted(
            &format!("{} {}", aws_id(), aws_secret()),
            Kind::AwsSecretKey,
            &aws_secret(),
        );
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
            assert_redacted(
                &format!("leaked {}", ghp(prefix)),
                Kind::GithubToken,
                prefix,
            );
        }
        for prefix in ["xoxa-", "xoxb-", "xoxp-", "xoxr-", "xoxs-"] {
            assert_redacted(&format!("hook {}", slack(prefix)), Kind::SlackToken, prefix);
        }
        assert_redacted(&format!("key {}", sk()), Kind::SecretKey, "sk-");
        assert_redacted(&format!("key {}", google()), Kind::GoogleApiKey, "AIza");
        for prefix in ["sk_live_", "rk_live_"] {
            assert_redacted(
                &format!("charge {}", stripe(prefix)),
                Kind::StripeKey,
                prefix,
            );
        }
        assert_redacted(&format!("auth {}", jwt()), Kind::Jwt, "eyJ");
        assert_redacted(&bearer(), Kind::BearerToken, &"L".repeat(32));
        assert!(redact_text(&bearer())
            .text
            .starts_with("Authorization: Bearer [redacted:"));
    }

    #[test]
    fn structural_patterns_are_caught() {
        let pass = "P".repeat(12);
        let r = redact_text(&format!("db postgres://syn_user:{pass}@example.invalid/db"));
        assert_eq!(r.hits, vec![Kind::UrlPassword]);
        assert!(!r.text.contains(&pass), "{:?}", r.text);
        assert!(
            r.text
                .contains("postgres://syn_user:[redacted:url_password]@example.invalid/db"),
            "{:?}",
            r.text
        );
        let r = redact_text(&format!("block:\n{}\ndone", pem()));
        assert_eq!(r.hits, vec![Kind::PrivateKey]);
        assert!(!r.text.contains(&"M".repeat(64)), "{:?}", r.text);
        assert!(
            r.text.contains("block:\n[redacted:private_key]\ndone"),
            "{:?}",
            r.text
        );
        for (assign, kind) in [
            (
                format!("SYN_PASSWORD={}", "Q".repeat(16)),
                Kind::CredentialAssignment,
            ),
            (
                format!("SYN_SECRET={}", "R".repeat(16)),
                Kind::CredentialAssignment,
            ),
            (
                format!("SYN_TOKEN={}", "S".repeat(16)),
                Kind::CredentialAssignment,
            ),
            (
                format!("SYN_API_KEY={}", "T".repeat(16)),
                Kind::CredentialAssignment,
            ),
            (
                format!("password: {}", "U".repeat(12)),
                Kind::CredentialAssignment,
            ),
            (
                format!("token={}", "V".repeat(12)),
                Kind::CredentialAssignment,
            ),
            (
                format!("\"api_key\": \"{}\"", "W".repeat(12)),
                Kind::CredentialAssignment,
            ),
            (
                format!("token: {}", "synthetic-token-1234"),
                Kind::CredentialAssignment,
            ),
        ] {
            let r = redact_text(&assign);
            assert!(r.hits.contains(&kind), "{assign:?} -> {:?}", r.text);
            assert!(
                r.text.contains(&format!("[redacted:{}]", kind.as_str())),
                "{assign:?} -> {:?}",
                r.text
            );
        }
        let r = redact_text(&format!("aws_secret_access_key={}", aws_secret()));
        assert_eq!(r.hits, vec![Kind::AwsSecretKey]);
    }

    #[test]
    fn placeholders_are_untouched() {
        for text in [
            "API_KEY=<your-key>".to_string(),
            "token=xxxx".to_string(),
            "password=changeme".to_string(),
            "secret=example".to_string(),
            "SYN_TOKEN=${SYN_TOKEN}".to_string(),
            "password=\"<redacted>\"".to_string(),
            "note [REDACTED] kept".to_string(),
            "body [redacted:github_token] kept".to_string(),
            "ghp_xxxx too short".to_string(),
            "sk-9 is not a key".to_string(),
            "token: 5".to_string(),
            "prompt_tokens: 9".to_string(),
            "tokens per brief: ~1.6k".to_string(),
            "TYPESAFE_API_KEY must stay unset here".to_string(),
            "my api_key".to_string(),
            "Bearer [REDACTED] good news".to_string(),
            "Bearer [REDACTED]".to_string(),
            "https://x.com/i/status/100".to_string(),
            "(https://x.com/i/status/1)".to_string(),
            "read this https://t.co/abc123 now".to_string(),
            "flask-based sauces".to_string(),
            "a plain public post".to_string(),
            "saves 15-20 hours monthly".to_string(),
            "post 1973436671111635192 stays".to_string(),
        ] {
            assert_clean(&text);
        }
        // Full-length scanner-safe fakes still count as secrets.
        assert_redacted(
            &format!("AKIA{}", "X".repeat(16)),
            Kind::AwsAccessKey,
            "AKIA",
        );
        assert_redacted(
            &format!("ghp_{}", "x".repeat(36)),
            Kind::GithubToken,
            "ghp_",
        );
    }

    #[test]
    fn near_misses_do_not_match() {
        // Hashes, SHAs and ids are not secrets: no dots, no prefixes, and a
        // bare 40-char run without an AWS id nearby is left alone.
        assert_clean(&"a".repeat(64));
        assert_clean(&"0".repeat(64));
        assert_clean(&"f".repeat(40));
        assert_clean("version 1.2.3 shipped");
        assert_clean("word1.word2word3.word4word5word6x");
        assert_clean("muse-spark model");
        assert_clean("fake-spark model");
        assert_clean("task-key for later");
    }

    #[test]
    fn json_keys_redacted_at_any_depth() {
        let secret = "Z".repeat(24);
        let (v, hits) = redact_json(&json!({
            "draft": "keep me",
            "access_token": secret,
            "nested": {"password": secret, "deep": [{"apiKey": secret}]},
            "authorization": format!("Bearer {secret}"),
        }));
        assert_eq!(hits, vec![Kind::JsonKey]);
        assert_eq!(v["draft"], "keep me");
        for got in [
            &v["access_token"],
            &v["nested"]["password"],
            &v["nested"]["deep"][0]["apiKey"],
            &v["authorization"],
        ] {
            assert_eq!(got, "[redacted:json_key]");
        }
        assert!(!v.to_string().contains(&secret));
    }

    #[test]
    fn json_embedded_and_telemetry() {
        let token = ghp("ghp_");
        let (v, hits) = redact_json(&json!({
            "note": format!("use {token} now"),
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150,
            "nothing": Option::<u64>::None,
            "count": 5,
        }));
        assert_eq!(hits, vec![Kind::GithubToken]);
        assert_eq!(v["prompt_tokens"], 100);
        assert_eq!(v["completion_tokens"], 50);
        assert_eq!(v["total_tokens"], 150);
        assert!(v["nothing"].is_null());
        assert!(!v.to_string().contains(&token));
        // Structured secrets behind secret keys go wholesale.
        let (v, hits) = redact_json(&json!({"credentials": {"user": "u", "x": 1}}));
        assert_eq!(hits, vec![Kind::JsonKey]);
        assert_eq!(v["credentials"], "[redacted:json_key]");
        // Placeholders and markers behind secret keys stay.
        let (v, hits) = redact_json(&json!({"api_key": "changeme", "token": "[REDACTED]"}));
        assert!(hits.is_empty());
        assert_eq!(v["api_key"], "changeme");
        assert_eq!(v["token"], "[REDACTED]");
    }

    #[test]
    fn clean_values_come_back_equal_and_idempotent() {
        for v in [
            json!({"to": "local"}),
            json!({"a": 1, "b": [1, 2], "c": null, "d": true}),
            json!("plain string"),
            json!(42),
        ] {
            let (out, hits) = redact_json(&v);
            assert!(hits.is_empty());
            assert_eq!(out, v);
        }
        let dirty = json!({
            "token": "Z".repeat(16),
            "note": format!("key {}", sk()),
        });
        let (once, hits) = redact_json(&dirty);
        assert!(!hits.is_empty());
        let (twice, hits2) = redact_json(&once);
        assert!(hits2.is_empty());
        assert_eq!(twice, once);
        let r = redact_text(&redact_text(&format!("k {}", sk())).text);
        assert!(r.hits.is_empty());
    }

    #[test]
    fn secret_key_rule_matrix() {
        for key in [
            "token",
            "access_token",
            "refreshToken",
            "secret",
            "client_secret",
            "password",
            "api_key",
            "apiKey",
            "credentials",
            "authorization",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in [
            "draft", "note", "post_id", "model", "redacted", "caller", "route",
        ] {
            assert!(!is_secret_key(key), "{key}");
        }
    }
}
