//! Translation: non-English posts are translated into English and surfaced.
//!
//! Nathan's rule change: never hide a post for language. English detection
//! stays in [`crate::language`]; this module turns a non-English bookmark
//! into a [`Translation`] shown alongside the original:
//! - Translated: English text plus `Translated from <lang>`, original kept
//!   below (collapsible in HTML, details block in markdown).
//! - Untranslated: original text plus `Untranslated (<lang>)`.
//! - English posts are unchanged and never translated (no cache, no calls).
//!
//! Routing (privacy router decides where data goes):
//! - Only [`DataClass::Public`](ancientos_router::data_class::DataClass) text
//!   may reach the hosted Muse client, and only when `ANCIENTOS_ALLOW_PAID=1`;
//!   the sanitized text is sent, never the raw text (see [`crate::sanitize`]).
//! - Anything else goes to the local model only ([`LocalLlmClient`](ancientos_router::local_llm::LocalLlmClient), via
//!   `scripts/local_llm.sh`) or stays [`TranslationStatus::Untranslated`];
//!   it is never sent to a hosted model.
//! - The class comes from [`class_of`]: text whose only stripped parts are
//!   URLs and @handles is Public; emails and phone numbers are Confidential;
//!   detected secrets are Restricted.
//!
//! Cache: translations live on disk in the data dir as JSON
//! ([`CACHE_FILE_NAME`], no secrets), keyed by `post_id` plus the SHA-256 of
//! the translated text. A cache hit makes zero model calls. Any error, paid
//! off, or missing client yields [`TranslationStatus::Untranslated`] — fail
//! open on visibility: the post is still shown.
use crate::fixture::Bookmark;
use ancientos_core::log::DecisionLog;
use ancientos_router::data_class::{check_egress, DataClass, Route};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Translation cache file name in the data dir (JSON, no secrets).
pub const CACHE_FILE_NAME: &str = "translations.json";
/// Env override for the cache path (tests, custom data dirs).
pub const CACHE_ENV_VAR: &str = "TRANSLATIONS_CACHE_PATH";
/// Paid-call switch: translations via Muse need `ANCIENTOS_ALLOW_PAID=1`.
pub const PAID_ENV_VAR: &str = "ANCIENTOS_ALLOW_PAID";
/// Display language when the source language is undetermined.
pub const UNKNOWN_LANG: &str = "und";

/// Whether a translation succeeded. `Untranslated` still surfaces the post.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TranslationStatus {
    Translated,
    Untranslated,
}

impl TranslationStatus {
    /// Stable string form of the translation status.
    pub fn as_str(self) -> &'static str {
        match self {
            TranslationStatus::Translated => "translated",
            TranslationStatus::Untranslated => "untranslated",
        }
    }
}

/// English rendering of one bookmark plus the original text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Translation {
    /// Original post text (always the current text, never cached).
    pub original: String,
    /// English translation when [`TranslationStatus::Translated`].
    pub english: Option<String>,
    /// Source language code (`es`, `fr`, ...) when known.
    pub source_lang: Option<String>,
    pub status: TranslationStatus,
}

impl Translation {
    /// English posts need no translation: original only, no marker.
    fn english(original: String) -> Self {
        Self {
            original,
            english: None,
            source_lang: None,
            status: TranslationStatus::Untranslated,
        }
    }

    /// Non-English post with no translation available: original plus an
    /// `Untranslated (<lang>)` marker (see [`marker_for`]).
    fn untranslated(original: String, source_lang: Option<String>) -> Self {
        Self {
            original,
            english: None,
            source_lang,
            status: TranslationStatus::Untranslated,
        }
    }

    /// Non-English post with a translation: English plus a
    /// `Translated from <lang>` marker and the original below.
    fn translated(original: String, english: String, source_lang: Option<String>) -> Self {
        Self {
            original,
            english: Some(english),
            source_lang,
            status: TranslationStatus::Translated,
        }
    }
}

/// Cached translation: English text plus the source language. Keyed by
/// [`cache_key`] (`post_id` + SHA-256 of the translated text).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    pub english: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_lang: Option<String>,
}

/// SHA-256 hex of `text` (cache key ingredient).
pub fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Cache key: `post_id` plus the SHA-256 of the translated text, so edited
/// text re-translates instead of reusing a stale entry.
pub fn cache_key(post_id: &str, text: &str) -> String {
    format!("{}:{}", post_id, sha256_hex(text))
}

/// Default cache path: `$TRANSLATIONS_CACHE_PATH` or
/// `$ANCIENTOS_HOME/translations.json`.
pub fn default_cache_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var(CACHE_ENV_VAR) {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(CACHE_FILE_NAME))
}

/// Cache path alongside `log_path` (keeps tests and custom data dirs
/// isolated), falling back to [`default_cache_path`].
pub fn cache_path_for_log(log_path: &Path) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var(CACHE_ENV_VAR) {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            return Ok(parent.join(CACHE_FILE_NAME));
        }
    }
    default_cache_path()
}

/// Load the disk cache. A missing or corrupt file means empty (fail open:
/// re-translate rather than trust it).
pub fn load_cache(path: &Path) -> HashMap<String, CacheEntry> {
    match std::fs::read_to_string(path) {
        Err(_) => HashMap::new(),
        Ok(text) => {
            if text.trim().is_empty() {
                return HashMap::new();
            }
            serde_json::from_str(&text).unwrap_or_default()
        }
    }
}

/// Save the disk cache atomically (parents created 0700, file 0600).
pub fn save_cache(path: &Path, cache: &HashMap<String, CacheEntry>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("translations cache dir failed: {e}"))?;
        }
    }
    let data =
        serde_json::to_string(cache).map_err(|e| format!("translations cache encode: {e}"))?;
    crate::token_store::atomic_write_0600(path, data.as_bytes())
        .map_err(|e| format!("translations cache write failed: {e}"))?;
    Ok(())
}

/// Data class of free text: [`DataClass::Restricted`] when it carries a
/// detected secret, [`DataClass::Confidential`] when it carries an email
/// address or a phone number, else [`DataClass::Public`]. Text whose only
/// stripped parts are URLs and @handles is Public — but hosted calls still
/// receive only the sanitized text, never the raw text. Non-public text
/// never reaches a hosted model.
pub fn class_of(text: &str) -> DataClass {
    if crate::sanitize::contains_secret(text) {
        DataClass::Restricted
    } else if crate::sanitize::contains_email(text) || crate::sanitize::contains_phone(text) {
        DataClass::Confidential
    } else {
        DataClass::Public
    }
}

/// True when paid calls are enabled (`ANCIENTOS_ALLOW_PAID=1`).
pub fn is_paid_allowed() -> bool {
    std::env::var(PAID_ENV_VAR).is_ok_and(|v| v == "1")
}

/// Prompt sent to the translator. The input is already sanitized for Muse
/// or original for the local model (see [`translate_one`]).
pub fn translate_prompt(text: &str, source_lang: Option<&str>) -> String {
    match source_lang.map(str::trim).filter(|s| !s.is_empty()) {
        Some(lang) => format!(
            "Translate the following {lang} post to English. Reply with the English translation only, no other text:\n\n{text}"
        ),
        None => format!(
            "Translate the following post to English. Reply with the English translation only, no other text:\n\n{text}"
        ),
    }
}

/// Display language: the source code when known, else [`UNKNOWN_LANG`].
pub fn display_lang(source_lang: Option<&str>) -> &str {
    source_lang
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(UNKNOWN_LANG)
}

/// Marker for a translation, or `None` for English posts (shown unchanged).
/// Non-English posts always get a marker: `Translated from <lang>` when a
/// translation is available, else `Untranslated (<lang>)`.
pub fn marker_for(b: &Bookmark, t: &Translation) -> Option<String> {
    if crate::language::is_english(b) {
        return None;
    }
    let lang = display_lang(t.source_lang.as_deref());
    match t.status {
        TranslationStatus::Translated => Some(format!("Translated from {lang}")),
        TranslationStatus::Untranslated => Some(format!("Untranslated ({lang})")),
    }
}

/// English text to show: the translation when available, else the original.
pub fn english_for<'a>(b: &'a Bookmark, t: &'a Translation) -> &'a str {
    if !crate::language::is_english(b) && t.status == TranslationStatus::Translated {
        if let Some(e) = t.english.as_deref() {
            if !e.trim().is_empty() {
                return e;
            }
        }
    }
    &t.original
}

/// Cached translation only: zero model calls, ever. `Translated` on a cache
/// hit, else `Untranslated` with the source language. Used by surfaces that
/// make no network calls (web-ui, offline eval).
pub fn cached_only(b: &Bookmark, cache: &HashMap<String, CacheEntry>) -> Translation {
    if crate::language::is_english(b) {
        return Translation::english(b.text.clone());
    }
    let source_lang = crate::language::source_lang(b);
    let key = cache_key(&b.post_id, &b.text);
    match cache.get(&key) {
        Some(entry) if !entry.english.trim().is_empty() => Translation::translated(
            b.text.clone(),
            entry.english.clone(),
            entry.source_lang.clone().or(source_lang),
        ),
        _ => Translation::untranslated(b.text.clone(), source_lang),
    }
}

/// Translate one bookmark, consulting `cache` first (a hit makes zero
/// calls) and inserting successful translations (the caller saves).
///
/// - English posts: returned unchanged, no cache, no calls.
/// - Non-public text ([`class_of`]): local client only (original text),
///   never Muse; the refused hosted egress is logged when `log` is given.
///   No local client or any error yields `Untranslated`.
/// - Public text: Muse only (sanitized text, never the raw text) when paid
///   calls are on; paid off, missing Muse client, or any error yields
///   `Untranslated`.
///
/// Empty replies are treated as errors (never cached). The post is always
/// surfaced: failures yield `Untranslated`, never an error.
pub fn translate_one(
    b: &Bookmark,
    cache: &mut HashMap<String, CacheEntry>,
    muse: Option<&dyn ancientos_router::ModelClient>,
    local: Option<&dyn ancientos_router::ModelClient>,
    log: Option<&mut DecisionLog>,
) -> Translation {
    if crate::language::is_english(b) {
        return Translation::english(b.text.clone());
    }
    if b.text.trim().is_empty() {
        return Translation::untranslated(b.text.clone(), crate::language::source_lang(b));
    }
    let source_lang = crate::language::source_lang(b);
    let key = cache_key(&b.post_id, &b.text);
    if let Some(entry) = cache.get(&key) {
        if !entry.english.trim().is_empty() {
            return Translation::translated(
                b.text.clone(),
                entry.english.clone(),
                entry.source_lang.clone().or_else(|| source_lang.clone()),
            );
        }
    }
    let class = class_of(&b.text);
    let english = if class > DataClass::Public {
        // Non-public: local model only, original text (handles/urls kept).
        // The refused hosted egress is logged (best-effort: translation
        // stays fail-open to local either way); no hosted call is attempted.
        let _ = check_egress(class, Route::MuseSpark, "translate", log);
        let Some(local) = local else {
            return Translation::untranslated(b.text.clone(), source_lang);
        };
        match local.complete(&translate_prompt(&b.text, source_lang.as_deref())) {
            Ok(reply) if !reply.trim().is_empty() => Some(reply.trim().to_string()),
            _ => None,
        }
    } else {
        // Public: Muse only, sanitized text (never the raw text), paid-gated.
        // The client enforces the ceiling again and logs the decision.
        if !is_paid_allowed() {
            return Translation::untranslated(b.text.clone(), source_lang);
        }
        let Some(muse) = muse else {
            return Translation::untranslated(b.text.clone(), source_lang);
        };
        let sanitized = crate::sanitize::sanitize_uncapped(&b.text);
        match muse.complete_with_class(
            class,
            "translate",
            log,
            &translate_prompt(&sanitized, source_lang.as_deref()),
        ) {
            Ok(reply) if !reply.trim().is_empty() => Some(reply.trim().to_string()),
            _ => None,
        }
    };
    match english {
        Some(english) => {
            cache.insert(
                key,
                CacheEntry {
                    english: english.clone(),
                    source_lang: source_lang.clone(),
                },
            );
            Translation::translated(b.text.clone(), english, source_lang)
        }
        None => Translation::untranslated(b.text.clone(), source_lang),
    }
}

/// Translate one bookmark with the env-wired clients
/// ([`MuseClient`](ancientos_router::muse::MuseClient) plus
/// [`LocalLlmClient`](ancientos_router::local_llm::LocalLlmClient)), consulting
/// `cache` first. Same fail-open rules as [`translate_one`]; egress
/// decisions are logged when `log` is given.
pub fn translate_with_env(
    b: &Bookmark,
    cache: &mut HashMap<String, CacheEntry>,
    log: Option<&mut DecisionLog>,
) -> Translation {
    if crate::language::is_english(b) {
        return Translation::english(b.text.clone());
    }
    let key = cache_key(&b.post_id, &b.text);
    if let Some(entry) = cache.get(&key) {
        if !entry.english.trim().is_empty() {
            let source_lang = entry
                .source_lang
                .clone()
                .or_else(|| crate::language::source_lang(b));
            return Translation::translated(b.text.clone(), entry.english.clone(), source_lang);
        }
    }
    let muse = ancientos_router::muse::MuseClient::from_env();
    let local = ancientos_router::local_llm::LocalLlmClient::from_env();
    translate_one(b, cache, Some(&muse), Some(&local), log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Test helper: builds a bookmark with the given id, text, and language.
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
    const ES_EN: &str =
        "[synthetic] The government presented an economic plan to support small local businesses.";

    struct Mock {
        dest: ancientos_router::Destination,
        reply: Option<Result<String, String>>,
        seen: RefCell<Vec<String>>,
    }

    impl Mock {
        /// Test helper: builds a mock client returning the given reply.
        fn ok(dest: ancientos_router::Destination, reply: &str) -> Self {
            Self {
                dest,
                reply: Some(Ok(reply.to_string())),
                seen: RefCell::new(Vec::new()),
            }
        }

        /// Test helper: builds a mock client that always fails.
        fn err(dest: ancientos_router::Destination) -> Self {
            Self {
                dest,
                reply: Some(Err("synthetic model failure".to_string())),
                seen: RefCell::new(Vec::new()),
            }
        }

        /// Test helper: returns how many prompts the mock has seen.
        fn calls(&self) -> usize {
            self.seen.borrow().len()
        }
    }

    impl ancientos_router::ModelClient for Mock {
        /// Test helper: returns which route the mock client claims.
        fn destination(&self) -> ancientos_router::Destination {
            self.dest
        }

        /// Test helper: records the prompt and replays the mock reply.
        fn complete(&self, prompt: &str) -> Result<String, ancientos_router::RouterError> {
            self.seen.borrow_mut().push(prompt.to_string());
            match self.reply.as_ref().expect("mock reply") {
                Ok(s) => Ok(s.clone()),
                Err(_) => Err(ancientos_router::RouterError::Http(
                    "synthetic model failure".to_string(),
                )),
            }
        }
    }

    /// Test helper: builds a mock Muse client returning the given reply.
    fn muse_mock(reply: &str) -> Mock {
        Mock::ok(ancientos_router::Destination::MuseSpark, reply)
    }

    /// Test helper: builds a mock local-LLM client returning the given reply.
    fn local_mock(reply: &str) -> Mock {
        Mock::ok(ancientos_router::Destination::LocalLlama, reply)
    }

    #[test]
    fn cache_hit_makes_zero_calls() {
        let _env = crate::test_support::EnvHold::paid_on();
        let b = bm("es1", ES, Some("es"));
        let mut cache = HashMap::new();
        cache.insert(
            cache_key(&b.post_id, &b.text),
            CacheEntry {
                english: ES_EN.to_string(),
                source_lang: Some("es".to_string()),
            },
        );
        let muse = muse_mock("MUST NOT BE CALLED");
        let local = local_mock("MUST NOT BE CALLED");
        let t = translate_one(&b, &mut cache, Some(&muse), Some(&local), None);
        assert_eq!(t.status, TranslationStatus::Translated);
        assert_eq!(t.english.as_deref(), Some(ES_EN));
        assert_eq!(t.original, ES);
        assert_eq!(muse.calls(), 0);
        assert_eq!(local.calls(), 0);
        assert_eq!(marker_for(&b, &t).as_deref(), Some("Translated from es"));
        assert_eq!(english_for(&b, &t), ES_EN);
    }

    #[test]
    fn paid_off_yields_untranslated_with_no_calls() {
        let _env = crate::test_support::EnvHold::paid_off();
        let b = bm("es1", ES, Some("es"));
        let mut cache = HashMap::new();
        let muse = muse_mock("MUST NOT BE CALLED");
        let local = local_mock("MUST NOT BE CALLED");
        let t = translate_one(&b, &mut cache, Some(&muse), Some(&local), None);
        assert_eq!(t.status, TranslationStatus::Untranslated);
        assert_eq!(t.english, None);
        assert_eq!(t.original, ES);
        assert_eq!(muse.calls(), 0);
        assert_eq!(local.calls(), 0, "sanitized paid-off makes no calls at all");
        assert!(cache.is_empty(), "failures are never cached");
        assert_eq!(marker_for(&b, &t).as_deref(), Some("Untranslated (es)"));
        assert_eq!(english_for(&b, &t), ES);
    }

    #[test]
    fn sensitive_text_never_reaches_hosted_mock() {
        let _env = crate::test_support::EnvHold::paid_on();
        // PII (email + phone + handle + url) marks the text sensitive.
        let text = "[synthetic] El gobierno presentó un plan escríbeme a ana@example.com o llama al 555-123-4567 @syn_handle https://t.co/abc123";
        let b = bm("es2", text, Some("es"));
        assert_eq!(class_of(&b.text), DataClass::Confidential);

        // Local succeeds: translated, hosted untouched.
        let mut cache = HashMap::new();
        let muse = muse_mock("MUST NOT BE CALLED");
        let local = local_mock(ES_EN);
        let t = translate_one(&b, &mut cache, Some(&muse), Some(&local), None);
        assert_eq!(t.status, TranslationStatus::Translated);
        assert_eq!(t.english.as_deref(), Some(ES_EN));
        assert_eq!(muse.calls(), 0, "sensitive must never hit hosted");
        assert_eq!(local.calls(), 1);
        // Local gets the original (handles/urls kept for translation).
        assert!(
            local.seen.borrow()[0].contains("ana@example.com"),
            "local keeps original: {}",
            local.seen.borrow()[0]
        );

        // No local client: untranslated, hosted still untouched.
        let mut cache = HashMap::new();
        let muse = muse_mock("MUST NOT BE CALLED");
        let t = translate_one(&b, &mut cache, Some(&muse), None, None);
        assert_eq!(t.status, TranslationStatus::Untranslated);
        assert_eq!(muse.calls(), 0);
    }

    #[test]
    fn muse_error_yields_untranslated_and_still_surfaces() {
        let _env = crate::test_support::EnvHold::paid_on();
        let b = bm("es1", ES, Some("es"));
        let mut cache = HashMap::new();
        let muse = Mock::err(ancientos_router::Destination::MuseSpark);
        let t = translate_one(&b, &mut cache, Some(&muse), None, None);
        assert_eq!(t.status, TranslationStatus::Untranslated);
        assert_eq!(t.original, ES);
        assert_eq!(t.source_lang.as_deref(), Some("es"));
        assert!(cache.is_empty(), "errors are never cached");
        // Still surfaced: original plus the untranslated marker.
        assert_eq!(marker_for(&b, &t).as_deref(), Some("Untranslated (es)"));
        assert_eq!(english_for(&b, &t), ES);
    }

    #[test]
    fn english_posts_are_unchanged_and_never_translated() {
        let _env = crate::test_support::EnvHold::paid_on();
        for b in [
            bm("en1", EN, Some("en")),
            bm("en2", EN, None),
            bm("en3", "[synthetic] short", Some("en")),
        ] {
            let mut cache = HashMap::new();
            let muse = muse_mock("MUST NOT BE CALLED");
            let local = local_mock("MUST NOT BE CALLED");
            let t = translate_one(&b, &mut cache, Some(&muse), Some(&local), None);
            assert_eq!(t.original, b.text);
            assert_eq!(t.english, None);
            assert_eq!(muse.calls(), 0, "English never calls hosted");
            assert_eq!(local.calls(), 0, "English never calls local");
            assert!(cache.is_empty(), "English never touches the cache");
            assert_eq!(marker_for(&b, &t), None, "English shows no marker");
            assert_eq!(english_for(&b, &t), b.text);
            assert!(crate::language::is_english(&b));
        }
    }

    #[test]
    fn muse_receives_sanitized_text_only() {
        let _env = crate::test_support::EnvHold::paid_on();
        // No PII: sanitized, so Muse is allowed.
        let b = bm("es1", ES, Some("es"));
        assert_eq!(class_of(&b.text), DataClass::Public);
        let mut cache = HashMap::new();
        let muse = muse_mock(ES_EN);
        let t = translate_one(&b, &mut cache, Some(&muse), None, None);
        assert_eq!(t.status, TranslationStatus::Translated);
        assert_eq!(muse.calls(), 1);
        let prompt = muse.seen.borrow()[0].clone();
        assert!(prompt.contains("Translate"), "{prompt}");
        assert!(
            prompt.contains(&crate::sanitize::sanitize_uncapped(ES)),
            "sanitized text is sent: {prompt}"
        );
        // Success is cached under post_id + sha256(text).
        let key = cache_key(&b.post_id, &b.text);
        assert_eq!(cache.get(&key).map(|e| e.english.as_str()), Some(ES_EN));
    }

    #[test]
    fn cache_key_changes_with_text() {
        let a = cache_key("p1", ES);
        assert_ne!(a, cache_key("p1", ES_EN), "edited text re-translates");
        assert_ne!(a, cache_key("p2", ES), "post ids do not collide");
        assert!(a.starts_with("p1:"));
        assert_eq!(sha256_hex(ES).len(), 64);
    }

    #[test]
    fn heuristic_non_english_uses_und_display_lang() {
        let _env = crate::test_support::EnvHold::paid_off();
        let b = bm("h1", ES, None);
        assert!(!crate::language::is_english(&b));
        let mut cache = HashMap::new();
        let t = translate_one(&b, &mut cache, None, None, None);
        assert_eq!(t.status, TranslationStatus::Untranslated);
        assert_eq!(t.source_lang, None);
        assert_eq!(marker_for(&b, &t).as_deref(), Some("Untranslated (und)"));
    }

    #[test]
    fn empty_text_never_calls() {
        let _env = crate::test_support::EnvHold::paid_on();
        let b = bm("e1", "   ", Some("es"));
        let mut cache = HashMap::new();
        let muse = muse_mock("MUST NOT BE CALLED");
        let local = local_mock("MUST NOT BE CALLED");
        let t = translate_one(&b, &mut cache, Some(&muse), Some(&local), None);
        assert_eq!(t.status, TranslationStatus::Untranslated);
        assert_eq!(muse.calls(), 0);
        assert_eq!(local.calls(), 0);
    }

    #[test]
    fn class_of_matrix() {
        use ancientos_router::data_class::DataClass;
        // URLs and @handles alone are Public.
        for text in [
            ES,
            "[synthetic] Lee esto https://t.co/abc123 ahora",
            "hola @syn_user, gran publicación https://example.invalid/x",
        ] {
            assert_eq!(class_of(text), DataClass::Public, "{text}");
        }
        // Emails and phones are Confidential.
        for text in [
            "[synthetic] escríbeme a ana@example.com por favor",
            "[synthetic] llámame al 555-123-4567 ahora",
            "[synthetic] hola https://t.co/abc123 o mail ana@example.com",
        ] {
            assert_eq!(class_of(text), DataClass::Confidential, "{text}");
        }
        // Detected secrets are Restricted.
        for text in [
            "[synthetic] mi password: hunter2-hunter aquí",
            "token sk-synthetic-test-key-value-1234 filtrado",
            "clave -----BEGIN RSA PRIVATE KEY----- sintética",
        ] {
            assert_eq!(class_of(text), DataClass::Restricted, "{text}");
        }
    }

    #[test]
    fn public_post_with_tco_link_reaches_hosted_with_sanitized_prompt() {
        use ancientos_router::data_class::DataClass;
        let _env = crate::test_support::EnvHold::paid_on();
        // A public post carrying a t.co link and a @handle: Public, so the
        // hosted stage runs (this used to be local-only: the bug this fixes).
        let text =
            "[synthetic] El gobierno presentó un plan económico https://t.co/abc123 @syn_user";
        let b = bm("es-link", text, Some("es"));
        assert_eq!(class_of(&b.text), DataClass::Public);
        let mut cache = HashMap::new();
        let muse = muse_mock(ES_EN);
        let local = local_mock("MUST NOT BE CALLED");
        let t = translate_one(&b, &mut cache, Some(&muse), Some(&local), None);
        assert_eq!(t.status, TranslationStatus::Translated);
        assert_eq!(muse.calls(), 1, "public posts reach hosted");
        assert_eq!(local.calls(), 0);
        // The hosted prompt carries only sanitized text: no URL, no handle.
        let prompt = muse.seen.borrow()[0].clone();
        assert!(!prompt.contains("https://"), "{prompt}");
        assert!(!prompt.contains("t.co"), "{prompt}");
        assert!(!prompt.contains("@syn_user"), "{prompt}");
        assert!(
            prompt.contains(&crate::sanitize::sanitize_uncapped(text)),
            "sanitized text is sent: {prompt}"
        );
    }

    #[test]
    fn post_with_email_never_reaches_hosted() {
        use ancientos_router::data_class::DataClass;
        let _env = crate::test_support::EnvHold::paid_on();
        let text = "[synthetic] El gobierno presentó un plan escríbeme a ana@example.com";
        let b = bm("es-mail", text, Some("es"));
        assert_eq!(class_of(&b.text), DataClass::Confidential);
        // Local succeeds: translated, hosted untouched, denial logged.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = ancientos_core::log::DecisionLog::open(&path).unwrap();
        let mut cache = HashMap::new();
        let muse = muse_mock("MUST NOT BE CALLED");
        let local = local_mock(ES_EN);
        let t = translate_one(&b, &mut cache, Some(&muse), Some(&local), Some(&mut log));
        assert_eq!(t.status, TranslationStatus::Translated);
        assert_eq!(muse.calls(), 0, "email posts never hit hosted");
        assert_eq!(local.calls(), 1);
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].kind,
            ancientos_router::data_class::EGRESS_DENIED_KIND
        );
        assert_eq!(entries[0].payload["class"], "confidential");
        assert_eq!(entries[0].payload["route"], "muse_spark");
        assert_eq!(entries[0].payload["caller"], "translate");
        // No local client: untranslated, hosted still untouched.
        let mut cache = HashMap::new();
        let muse = muse_mock("MUST NOT BE CALLED");
        let t = translate_one(&b, &mut cache, Some(&muse), None, None);
        assert_eq!(t.status, TranslationStatus::Untranslated);
        assert_eq!(muse.calls(), 0);
    }

    #[test]
    fn secret_post_is_restricted_and_stays_local() {
        use ancientos_router::data_class::DataClass;
        let _env = crate::test_support::EnvHold::paid_on();
        let text = "[synthetic] El plan dice password: hunter2-hunter aquí";
        let b = bm("es-sec", text, Some("es"));
        assert_eq!(class_of(&b.text), DataClass::Restricted);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = ancientos_core::log::DecisionLog::open(&path).unwrap();
        let mut cache = HashMap::new();
        let muse = muse_mock("MUST NOT BE CALLED");
        let local = local_mock(ES_EN);
        let t = translate_one(&b, &mut cache, Some(&muse), Some(&local), Some(&mut log));
        assert_eq!(t.status, TranslationStatus::Translated);
        assert_eq!(muse.calls(), 0);
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].payload["class"], "restricted");
        let raw = serde_json::to_string(&entries[0].payload).unwrap();
        assert!(!raw.contains("hunter2"), "{raw}");
    }

    #[test]
    fn real_hosted_client_sees_zero_requests_for_email_post() {
        use ancientos_router::muse::MuseClient;
        let _env = crate::test_support::EnvHold::paid_on();
        std::env::set_var("META_API_KEY", "test-key");
        // Closed port: any hosted attempt would fail, so a translation via
        // the local mock proves the real hosted client saw zero requests.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let closed = format!("http://{}", probe.local_addr().expect("probe addr"));
        drop(probe);
        let real_muse = MuseClient::new("m", closed, 16, true);
        let text = "[synthetic] El gobierno presentó un plan escríbeme a ana@example.com";
        let b = bm("es-mail2", text, Some("es"));
        let mut cache = HashMap::new();
        let local = local_mock(ES_EN);
        let t = translate_one(&b, &mut cache, Some(&real_muse), Some(&local), None);
        assert_eq!(t.status, TranslationStatus::Translated);
        assert_eq!(t.english.as_deref(), Some(ES_EN));
        assert_eq!(local.calls(), 1);
    }

    #[test]
    fn cache_round_trip_holds_no_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE_NAME);
        let mut cache = HashMap::new();
        cache.insert(
            cache_key("es1", ES),
            CacheEntry {
                english: ES_EN.to_string(),
                source_lang: Some("es".to_string()),
            },
        );
        save_cache(&path, &cache).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("META_API_KEY"), "{text}");
        assert!(!text.contains("Bearer"), "{text}");
        let back = load_cache(&path);
        assert_eq!(back, cache);
        assert!(load_cache(&dir.path().join("missing.json")).is_empty());
    }
}
