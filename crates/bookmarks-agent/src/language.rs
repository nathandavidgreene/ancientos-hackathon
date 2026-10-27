//! Language detection: decide whether a post is English.
//!
//! Nathan's rule change: non-English posts are translated into English and
//! surfaced, never skipped (see [`crate::translate`]). This module keeps the
//! detection only ([`is_english`]): every surface shows every post, with a
//! `Translated from <lang>` marker plus the original, or an
//! `Untranslated (<lang>)` marker when no translation is available. The store
//! is never modified.
//!
//! Precedence per bookmark:
//! - X's stored `lang` field wins when it is a determined code: `en`
//!   (case-insensitive, `en-US`/`en_US` included) means English, any other
//!   determined code (`es`, `fr`, `de`, `ja`, ...) means not English — even
//!   when the text looks English.
//! - Missing, blank, or undetermined codes (`und`, `zxx`, `qme`, `qht`) fall
//!   back to a local, offline heuristic on the sanitized text.
//!
//! Heuristic (no network, no paid calls, deterministic): strip urls, emails,
//! phones and @handles (via [`crate::sanitize`]), then hashtags and emoji;
//! reject when any remaining letter is a non-Latin script (CJK, Cyrillic,
//! Arabic, ...); otherwise require at least [`MIN_WORDS`] words with at
//! least [`MIN_STOPWORDS`] English stopwords at a stopword ratio of at
//! least [`MIN_RATIO`]. Fail closed: short, emoji-only, empty, or otherwise
//! undetermined text counts as not English (not surfaced).
use crate::fixture::Bookmark;

/// X lang codes meaning "could not determine": fall back to the heuristic.
const UNDETERMINED: &[&str] = &["und", "zxx", "qme", "qht"];
/// Minimum words for the heuristic to pass (shorter fails closed).
pub const MIN_WORDS: usize = 5;
/// Minimum English stopword hits for the heuristic to pass.
pub const MIN_STOPWORDS: usize = 2;
/// Minimum stopword hits / words ratio for the heuristic to pass.
pub const MIN_RATIO: f64 = 0.12;

/// Small English stopword list, matched whole-token on lowercased words.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "then", "than", "so", "as", "of", "at", "by",
    "for", "with", "about", "into", "over", "after", "before", "between", "through", "during",
    "from", "to", "in", "on", "out", "up", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "can", "could", "should", "may",
    "might", "must", "i", "me", "my", "we", "our", "you", "your", "he", "him", "his", "she", "her",
    "it", "its", "they", "them", "their", "this", "that", "these", "those", "there", "here",
    "when", "where", "which", "who", "what", "why", "how", "not", "no", "only", "also", "just",
    "like", "more", "most", "other", "some", "such", "each", "every", "any", "all", "too", "very",
    "now", "because", "while", "until", "without", "within",
];

/// Decide a stored X `lang` value: `Some(true)` for English, `Some(false)`
/// for any other determined code, `None` when missing, blank, or
/// undetermined (fall back to the heuristic).
fn decide_lang_code(lang: Option<&str>) -> Option<bool> {
    let raw = lang?.trim();
    if raw.is_empty() {
        return None;
    }
    let primary = raw
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if primary.is_empty() || UNDETERMINED.contains(&primary.as_str()) {
        return None;
    }
    Some(primary == "en")
}

/// True when `c` is an emoji-ish char (stripped before detection).
fn is_emoji(c: char) -> bool {
    matches!(c,
        '\u{1F000}'..='\u{1FAFF}'
        | '\u{2600}'..='\u{27BF}'
        | '\u{2B00}'..='\u{2BFF}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{200D}')
}

/// True when `c` is a Latin-script letter (ASCII plus Latin-1/Extended).
fn is_latin_letter(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(c, '\u{C0}'..='\u{FF}' | '\u{100}'..='\u{24F}' | '\u{1E00}'..='\u{1EFF}')
}

/// Strip urls/emails/phones/handles (sanitize), then whole hashtag tokens
/// and emoji, for language detection.
fn strip_for_detection(text: &str) -> String {
    let sanitized = crate::sanitize::sanitize_uncapped(text);
    let kept: Vec<&str> = sanitized
        .split_whitespace()
        .filter(|t| {
            let bare = t.trim_start_matches(['(', '[', '{', '"', '\'', '<']);
            !bare.starts_with('#')
        })
        .collect();
    kept.join(" ").chars().filter(|c| !is_emoji(*c)).collect()
}

/// True when any letter in `text` is a non-Latin script.
fn has_non_latin_letters(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_alphabetic() && !is_latin_letter(c))
}

/// Lowercased word tokens: maximal runs of alphanumeric/`'`/`’` chars that
/// contain at least one letter (pure numbers are not words).
fn latin_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, words: &mut Vec<String>| {
        let w = cur.trim_matches(['\'', '\u{2019}']).to_lowercase();
        if !w.is_empty() && w.chars().any(|c| c.is_alphabetic()) {
            words.push(w);
        }
        cur.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() || c == '\'' || c == '\u{2019}' {
            cur.push(c);
        } else if !cur.is_empty() {
            flush(&mut cur, &mut words);
        }
    }
    if !cur.is_empty() {
        flush(&mut cur, &mut words);
    }
    words
}

/// Offline English heuristic on free text: script check plus an English
/// stopword ratio. Fails closed on short/emoji-only/undetermined text.
pub fn is_english_text(text: &str) -> bool {
    let cleaned = strip_for_detection(text);
    if has_non_latin_letters(&cleaned) {
        return false;
    }
    let words = latin_words(&cleaned);
    if words.len() < MIN_WORDS {
        return false;
    }
    let hits = words
        .iter()
        .filter(|w| STOPWORDS.contains(&w.as_str()))
        .count();
    if hits < MIN_STOPWORDS {
        return false;
    }
    hits as f64 / words.len() as f64 >= MIN_RATIO
}

/// True when the bookmark is English: X's stored `lang` wins when
/// determined, else the offline heuristic on the text. Fail closed.
pub fn is_english(b: &Bookmark) -> bool {
    if let Some(decided) = decide_lang_code(b.lang.as_deref()) {
        return decided;
    }
    is_english_text(&b.text)
}

/// Source language code for display (`Translated from <lang>`): the
/// determined X `lang` primary code lowercased (`es`, `en`, ...), or `None`
/// when missing, blank, or undetermined (heuristic case, renders as `und`
/// via [`crate::translate::display_lang`]).
pub fn source_lang(b: &Bookmark) -> Option<String> {
    let raw = b.lang.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let primary = raw
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if primary.is_empty() || UNDETERMINED.contains(&primary.as_str()) {
        return None;
    }
    Some(primary)
}

/// Extract the post id from an X status URL (`.../status/<id>`), if present.
pub fn post_id_from_status_url(url: &str) -> Option<String> {
    let (_, tail) = url.split_once("/status/")?;
    let id = tail.split(['?', '#', '/']).next().unwrap_or("").trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: bm.
    fn bm(id: &str, text: &str, lang: Option<&str>) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: text.into(),
            author_username: Some(format!("syn_{id}")),
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: lang.map(str::to_string),
            urls: Vec::new(),
        }
    }

    const EN: &str =
        "[synthetic] The government presented an economic plan to support small local businesses.";
    const ES: &str =
        "[synthetic] El gobierno presentó un plan económico para apoyar pequeñas empresas locales.";
    const FR: &str =
        "[synthetic] Le gouvernement présente un plan économique pour soutenir les petites entreprises locales.";
    const PT: &str =
        "[synthetic] O governo apresentou um plano econômico para apoiar pequenas empresas locais.";

    #[test]
    fn lang_en_passes() {
        assert!(is_english(&bm("e1", EN, Some("en"))));
        assert!(is_english(&bm("e2", EN, Some("EN"))));
        assert!(is_english(&bm("e3", EN, Some("en-US"))));
        assert!(is_english(&bm("e4", EN, Some("en_US"))));
    }

    #[test]
    fn lang_en_is_trusted_over_text() {
        // X's determined code wins, even when the text looks like another language.
        assert!(is_english(&bm("e5", ES, Some("en"))));
    }

    #[test]
    fn lang_non_english_detected_even_if_text_looks_english() {
        for code in ["es", "fr", "pt", "de", "it", "ja", "zh", "ar", "ru", "ES"] {
            assert!(
                !is_english(&bm("x", EN, Some(code))),
                "lang={code} must detect non-English even for English-looking text"
            );
        }
    }

    #[test]
    fn lang_undetermined_falls_back_to_heuristic() {
        for code in ["und", "zxx", "qme", "qht", "UND", ""] {
            assert!(
                is_english(&bm("h", EN, Some(code))),
                "lang={code} with English text must pass via heuristic"
            );
            assert!(
                !is_english(&bm("h", ES, Some(code))),
                "lang={code} with Spanish text must detect non-English via heuristic"
            );
        }
        assert!(is_english(&bm("h", EN, None)));
        assert!(!is_english(&bm("h", ES, None)));
    }

    #[test]
    fn heuristic_passes_english_and_detects_romance_latin_script() {
        assert!(is_english_text(EN));
        assert!(!is_english_text(ES), "Spanish must detect non-English");
        assert!(!is_english_text(FR), "French must detect non-English");
        assert!(!is_english_text(PT), "Portuguese must detect non-English");
    }

    #[test]
    fn heuristic_detects_non_latin_scripts() {
        assert!(!is_english_text("[synthetic] 这是一个中文测试帖子"));
        assert!(!is_english_text("[synthetic] これは日本語のテスト投稿です"));
        assert!(!is_english_text(
            "[synthetic] 이것은 한국어 테스트 게시물입니다"
        ));
        assert!(!is_english_text(
            "[synthetic] Это тестовый пост на русском языке"
        ));
        assert!(!is_english_text("[synthetic] هذا اختبار باللغة العربية"));
        // One non-Latin word fails the whole post (fail closed).
        assert!(!is_english_text(
            "[synthetic] The government presented an economic plan 测试 to support businesses."
        ));
    }

    #[test]
    fn heuristic_fails_closed_on_short_emoji_only_and_empty() {
        assert!(!is_english_text("Nice!"));
        assert!(!is_english_text("[synthetic] Wow"));
        assert!(!is_english_text("👍🎉🔥"));
        assert!(!is_english_text(""));
        assert!(!is_english_text("   "));
        // Hashtag-only: stripped, then empty.
        assert!(!is_english_text("#love #photo"));
        // Two words with stopwords is still too short to trust.
        assert!(!is_english_text("This is"));
    }

    #[test]
    fn heuristic_strips_noise_before_deciding() {
        // English survives heavy decoration.
        assert!(is_english_text(
            "[synthetic] The government presented https://t.co/abc123 an economic plan @syn_handle to support #economy small local businesses 🎉"
        ));
        // Spanish with an English hashtag still detects non-English (hashtag stripped).
        assert!(!is_english_text(
            "[synthetic] El gobierno presentó un plan económico #economy para apoyar pequeñas empresas 🎉"
        ));
    }

    #[test]
    fn source_lang_reports_determined_code_or_none() {
        assert_eq!(
            source_lang(&bm("e1", EN, Some("en"))),
            Some("en".to_string())
        );
        assert_eq!(
            source_lang(&bm("e2", EN, Some("EN-US"))),
            Some("en".to_string())
        );
        assert_eq!(
            source_lang(&bm("e3", EN, Some("es"))),
            Some("es".to_string())
        );
        assert_eq!(
            source_lang(&bm("e4", EN, Some("PT_br"))),
            Some("pt".to_string())
        );
        assert_eq!(source_lang(&bm("e5", EN, None)), None);
        assert_eq!(source_lang(&bm("e6", EN, Some(""))), None);
        for und in ["und", "zxx", "qme", "qht", "UND"] {
            assert_eq!(source_lang(&bm("u", EN, Some(und))), None, "lang={und}");
        }
    }

    #[test]
    fn status_url_id_extraction() {
        assert_eq!(
            post_id_from_status_url("https://x.com/someone/status/123"),
            Some("123".to_string())
        );
        assert_eq!(
            post_id_from_status_url("https://x.com/i/status/456?t=1"),
            Some("456".to_string())
        );
        assert_eq!(
            post_id_from_status_url("https://example.invalid/a/status/demo-7"),
            Some("demo-7".to_string())
        );
        assert_eq!(post_id_from_status_url("https://x.com/home"), None);
        assert_eq!(post_id_from_status_url("https://x.com/i/status/"), None);
        assert_eq!(post_id_from_status_url("not a url"), None);
    }
}
