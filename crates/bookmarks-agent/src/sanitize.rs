//! Sanitize bookmark text before it may leave the device.
//!
//! Removes URLs, @handles, emails, and phone-like numbers, then collapses
//! whitespace and truncates to [`MAX_CHARS`] characters. Only sanitized text
//! is ever sent to a hosted model.
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

/// Remove phone-like runs: 7-15 digits (with common separators) bounded by
/// non-alphanumerics. Short numbers (years, counts) and very long digit
/// strings (post ids) are kept.
fn strip_phones(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let starts = c.is_ascii_digit() || c == '+' || c == '(';
        if !starts {
            out.push(c);
            i += 1;
            continue;
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
            i = end;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    let out: String = text.chars().take(max).collect();
    out.trim_end().to_string()
}

/// Sanitize free text for hosted models.
pub fn sanitize(text: &str) -> String {
    let no_urls = strip_url_and_email_tokens(text);
    let no_handles = strip_handles(&no_urls);
    let no_phones = strip_phones(&no_handles);
    truncate_chars(&collapse_whitespace(&no_phones), MAX_CHARS)
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
}
