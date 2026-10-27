//! Sanitize bookmark text before it may leave the device.
//!
//! Secret-shaped values are redacted first (see
//! [`ancientos_core::redact`], `[redacted:<kind>]` markers), then URLs,
//! @handles, emails, and phone-like numbers are removed, whitespace is
//! collapsed, and the result is truncated to [`MAX_CHARS`] characters. Only
//! sanitized text is ever sent to a hosted model.
pub const MAX_CHARS: usize = 200;

fn is_url_token(token: &str) -> bool {
    let lower = token.to_lowercase();
    lower.contains("://")
        || lower
            .trim_start_matches(|c: char| {
                c == '(' || c == '[' || c == '{' || c == '"' || c == '\'' || c == '<'
            })
            .starts_with("www.")
}

fn looks_like_email(token: &str) -> bool {
    let t = token.trim_matches(|c: char| {
        c.is_ascii_punctuation() && c != '@' && c != '.' && c != '_' && c != '-' && c != '+'
    });
    let Some(at) = t.find('@') else {
        return false;
    };
    let (local, rest) = t.split_at(at);
    let domain = &rest[1..];
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    if !local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-'))
    {
        return false;
    }
    if !domain.contains('.') {
        return false;
    }
    domain
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && domain
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Remove whole whitespace-separated tokens that are URLs or emails.
fn strip_url_and_email_tokens(text: &str) -> String {
    text.split_whitespace()
        .filter(|t| !is_url_token(t) && !looks_like_email(t))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_handle_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Remove `@handle` substrings. Callers must strip emails first so the `@`
/// in an address is never treated as a handle.
fn strip_handles(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let boundary = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        if c == '@' && boundary && i + 1 < chars.len() && is_handle_char(chars[i + 1]) {
            i += 1;
            while i < chars.len() && is_handle_char(chars[i]) {
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn is_phone_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, ' ' | '-' | '.' | '(' | ')' | '+')
}

/// End index (exclusive) of a phone-like run starting at `i`, or `None`:
/// 7-15 digits (with common separators) bounded by non-alphanumerics. Short
/// numbers (years, counts) and very long digit strings (post ids) never match.
fn phone_match_end(chars: &[char], i: usize) -> Option<usize> {
    let c = chars[i];
    if !(c.is_ascii_digit() || c == '+' || c == '(') {
        return None;
    }
    let mut j = i;
    while j < chars.len() && is_phone_char(chars[j]) && j - i < 32 {
        j += 1;
    }
    // Trim separators from both ends of the candidate span.
    let mut start = i;
    let mut end = j;
    while start < end && !chars[start].is_ascii_digit() {
        start += 1;
    }
    while end > start && !chars[end - 1].is_ascii_digit() {
        end -= 1;
    }
    let digits = chars[start..end]
        .iter()
        .filter(|c| c.is_ascii_digit())
        .count();
    let left_ok = start == 0 || !chars[start - 1].is_ascii_alphanumeric();
    let right_ok = end >= chars.len() || !chars[end].is_ascii_alphanumeric();
    if (7..=15).contains(&digits) && left_ok && right_ok {
        Some(end)
    } else {
        None
    }
}

/// Remove phone-like runs (see [`phone_match_end`]).
fn strip_phones(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match phone_match_end(&chars, i) {
            Some(end) => i = end,
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    let out: String = text.chars().take(max).collect();
    out.trim_end().to_string()
}

/// Sanitize free text for hosted models, without truncating.
/// Same pipeline as [`sanitize`] (redact secrets, strip URLs, emails,
/// @handles, phone-like numbers, collapse whitespace) but keeps the full
/// length: for thread summaries where every post matters.
pub fn sanitize_uncapped(text: &str) -> String {
    let redacted = ancientos_core::redact::redact_text(text).text;
    let no_urls = strip_url_and_email_tokens(&redacted);
    let no_handles = strip_handles(&no_urls);
    let no_phones = strip_phones(&no_handles);
    collapse_whitespace(&no_phones)
}

/// Sanitize free text for hosted models.
pub fn sanitize(text: &str) -> String {
    truncate_chars(&sanitize_uncapped(text), MAX_CHARS)
}

/// True when text contains an email address (exactly the rule [`sanitize`]
/// strips: same token test, so detection and stripping never disagree).
pub fn contains_email(text: &str) -> bool {
    text.split_whitespace().any(looks_like_email)
}

/// True when text contains a phone-like run (exactly the rule [`sanitize`]
/// strips: same span test, so detection and stripping never disagree).
pub fn contains_phone(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).any(|i| phone_match_end(&chars, i).is_some())
}

/// Credential-bearing key names flagged as `key: value` / `key=value`.
/// Matched case-insensitively as whole words (letters, digits, and `_`
/// on either side block the match, so `tokens` and `TYPESAFE_API_KEY`
/// never match `token` / `api_key`).
const SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "api key",
    "client_secret",
    "access_token",
    "auth_token",
    "secret",
    "password",
    "passwd",
    "token",
];

/// Known high-entropy token prefixes (fixed case). A whitespace-separated
/// token starting with one of these (plus a value of 8+ chars) is a secret.
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "AKIA",
];

/// True for a word character (blocks a secret-key boundary).
fn is_secret_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// True when `text` contains a `key: value` / `key=value` credential
/// assignment for `key` (case-insensitive, whole-word): optional whitespace
/// around `:` or `=`, then a value of 4+ non-whitespace chars (counts and
/// labels like `token: 5` are not secrets).
fn has_secret_assignment(lower: &str, key: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(key) {
        let abs = from + pos;
        let before_ok = abs == 0 || !is_secret_word_char(bytes[abs - 1] as char);
        let after = abs + key.len();
        let boundary_ok = after >= bytes.len() || !is_secret_word_char(bytes[after] as char);
        if before_ok && boundary_ok {
            let rest = &lower[after..];
            let rest = rest.trim_start();
            if let Some(eq) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('=')) {
                let value = eq.trim_start();
                let value_len = value.chars().take_while(|c| !c.is_whitespace()).count();
                if value_len >= 4 {
                    return true;
                }
            }
        }
        // Advance past the key (ASCII, so this stays a char boundary).
        from = abs + key.len().max(1);
        if from >= lower.len() {
            break;
        }
    }
    false
}

/// True when `text` carries a Bearer token: the whole word `bearer`
/// (case-insensitive) plus a value of 8+ token chars.
fn has_bearer_token(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("bearer") {
        let abs = from + pos;
        let before_ok = abs == 0 || !is_secret_word_char(bytes[abs - 1] as char);
        let after = abs + "bearer".len();
        let boundary_ok = after >= bytes.len() || !(bytes[after] as char).is_ascii_alphanumeric();
        if before_ok && boundary_ok {
            let value: String = lower[after..]
                .chars()
                .skip_while(|c| c.is_whitespace())
                .take_while(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '-' | '.' | '_' | '~' | '+' | '/' | '=')
                })
                .collect();
            if value.len() >= 8 {
                return true;
            }
        }
        // Advance past the keyword (ASCII, so this stays a char boundary).
        from = abs + "bearer".len();
        if from >= lower.len() {
            break;
        }
    }
    false
}

/// True when text looks like it carries a secret: a private-key marker, a
/// known high-entropy token prefix, a `key: value` credential assignment, or
/// a Bearer token. Heuristic and fail-closed on matches only: anything it
/// flags classifies as Restricted and stays on the device. [`sanitize`]
/// additionally redacts secret-shaped values, so hosted inputs carry
/// `[redacted:<kind>]` markers even when a shape slips past this heuristic.
pub fn contains_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower.contains("private key") {
        return true;
    }
    for token in text.split_whitespace() {
        let trimmed =
            token.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        for prefix in SECRET_PREFIXES {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                if rest.len() >= 8 {
                    return true;
                }
            }
        }
    }
    if SECRET_KEYS.iter().any(|k| has_secret_assignment(&lower, k)) {
        return true;
    }
    has_bearer_token(&lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_urls() {
        assert_eq!(
            sanitize("read this https://t.co/abc123 now"),
            "read this now"
        );
        assert_eq!(
            sanitize("see http://example.com/a?b=c and www.example.org/x"),
            "see and"
        );
        assert_eq!(sanitize("(https://x.com/i/status/1)"), "");
    }

    #[test]
    fn strips_handles_but_keeps_words() {
        assert_eq!(sanitize("hi @syn_user, great post"), "hi , great post");
        assert_eq!(sanitize("@syn_user leading"), "leading");
        assert_eq!(
            sanitize("email a@b kept, handle @x dropped"),
            "email a@b kept, handle dropped"
        );
    }

    #[test]
    fn strips_emails() {
        assert_eq!(
            sanitize("write to syn.user+tag@example.com please"),
            "write to please"
        );
        assert_eq!(sanitize("a@b is not an email"), "a@b is not an email");
    }

    #[test]
    fn strips_phone_like_numbers_but_keeps_short_ones() {
        assert_eq!(sanitize("call +1 (555) 123-4567 now"), "call now");
        assert_eq!(sanitize("call 555-123-4567 now"), "call now");
        assert_eq!(
            sanitize("saves 15-20 hours monthly"),
            "saves 15-20 hours monthly"
        );
        assert_eq!(sanitize("top 0.1% in 2026"), "top 0.1% in 2026");
        assert_eq!(
            sanitize("post 1973436671111635192 stays"),
            "post 1973436671111635192 stays"
        );
    }

    #[test]
    fn collapses_whitespace_and_truncates() {
        assert_eq!(sanitize("a\n\t  b   c"), "a b c");
        let long = "ab".repeat(150);
        assert_eq!(sanitize(&long).chars().count(), MAX_CHARS);
        let words = "w ".repeat(150);
        assert!(sanitize(&words).chars().count() <= MAX_CHARS);
    }

    #[test]
    fn combined_pii_is_removed() {
        let raw = "Hi @syn_user, see https://t.co/x or mail me@example.org or call 555-123-4567!";
        assert_eq!(sanitize(raw), "Hi , see or mail or call !");
    }

    #[test]
    fn detectors_agree_with_stripping() {
        // Emails and phones: detected exactly when stripping changes the text.
        for text in [
            "write to syn.user+tag@example.com please",
            "call +1 (555) 123-4567 now",
            "call 555-123-4567 now",
        ] {
            assert_ne!(sanitize_uncapped(text), text, "{text}");
            assert!(
                contains_email(text) || contains_phone(text),
                "must detect: {text}"
            );
        }
        // URLs and @handles strip but are NOT email/phone detections.
        for text in [
            "read this https://t.co/abc123 now",
            "hi @syn_user, great post",
            "see http://example.com/a and www.example.org/x",
        ] {
            assert_ne!(sanitize_uncapped(text), text, "{text}");
            assert!(!contains_email(text), "{text}");
            assert!(!contains_phone(text), "{text}");
        }
        // Clean text: nothing detected, nothing stripped.
        for text in [
            "a plain public post",
            "saves 15-20 hours monthly",
            "top 0.1% in 2026",
            "post 1973436671111635192 stays",
            "a@b is not an email",
        ] {
            assert!(!contains_email(text), "{text}");
            assert!(!contains_phone(text), "{text}");
        }
    }

    #[test]
    fn secrets_detected_and_never_confused_with_prose() {
        // Private keys, token prefixes, assignments, Bearer tokens.
        for text in [
            "key block -----BEGIN RSA PRIVATE KEY----- synthetic",
            "my private key is below, rotate it",
            "token sk-synthetic-test-key-value-1234 leaked",
            "ghp_syntheticgithubtokenvalue1234567890 in the post",
            "AKIAIOSFODNN7EXAMPLE was pasted",
            "xoxb-synthetic-slack-token-value here",
            "password: hunter2-hunter",
            "api_key=synthetic-key-value",
            "API KEY: synthetic value here",
            "client_secret : synthetic-secret",
            "token: synthetic-token-1234",
            "Authorization: Bearer synthetic-bearer-token-value",
        ] {
            assert!(contains_secret(text), "must detect: {text}");
        }
        // Prose that merely mentions the words is not a secret.
        for text in [
            "a plain public post",
            "use a password manager for safety",
            "the secret-handle @syn_user posted",
            "synthetic-secret-marker-zzz",
            "tokens per brief: ~1.6k",
            "token: 5",
            "prompt_tokens: 9",
            "TYPESAFE_API_KEY must stay unset here",
            "my api_key",
            "bearer of good news",
            "sk-9 is not a key",
            "read https://t.co/abc123 now @syn_user",
        ] {
            assert!(!contains_secret(text), "must not detect: {text}");
        }
    }

    #[test]
    fn uncapped_strips_without_truncating() {
        let raw = format!("Hi @syn_user, see https://t.co/x {}", "word ".repeat(100));
        let out = sanitize_uncapped(&raw);
        assert!(!out.contains("@syn_user"), "{out}");
        assert!(!out.contains("https://t.co/x"), "{out}");
        assert!(out.contains("Hi , see"), "{out}");
        assert!(
            out.chars().count() > MAX_CHARS,
            "uncapped keeps full length: {} chars",
            out.chars().count()
        );
        // Capped is exactly the truncated uncapped form.
        assert_eq!(
            sanitize(&raw),
            out.chars().take(MAX_CHARS).collect::<String>().trim_end()
        );
    }

    #[test]
    fn sanitize_redacts_secrets_for_hosted_calls() {
        // Scanner-safe fakes, built at runtime.
        let token = format!("ghp_{}", "C".repeat(36));
        let out = sanitize(&format!("rotate {token} now"));
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("[redacted:github_token]"), "{out}");
        let secret = "U".repeat(12);
        let out = sanitize_uncapped(&format!("db password: {secret}"));
        assert!(!out.contains(&secret), "{out}");
        assert_eq!(out, "db password: [redacted:credential_assignment]");
        // Redaction composes with PII stripping: secrets, handles, URLs go.
        let key = format!("sk-{}", "E".repeat(32));
        let out = sanitize(&format!("hi @syn_user, key {key} https://t.co/x"));
        assert!(!out.contains(&key), "{out}");
        assert!(!out.contains("@syn_user"), "{out}");
        assert!(!out.contains("https://"), "{out}");
        assert_eq!(out, "hi , key [redacted:secret_key]");
        // Placeholders still pass through untouched.
        assert_eq!(sanitize("API_KEY=<your-key>"), "API_KEY=<your-key>");
        assert_eq!(sanitize("token=xxxx"), "token=xxxx");
    }
}
