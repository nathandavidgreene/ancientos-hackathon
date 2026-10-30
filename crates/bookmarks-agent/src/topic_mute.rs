//! Topic mute: a user-configured list of muted terms. A bookmark whose post
//! text, quoted text, or linked text contains a muted term (case-insensitive)
//! never surfaces. Bookmarks with `x_bookmark_present == Some(false)` were
//! removed on X and never surface either (`None` means still bookmarked).
//!
//! The term list is private config: `ANCIENTOS_MUTED_TERMS` (comma or
//! newline separated) and/or a file named by `ANCIENTOS_MUTED_TERMS_FILE`
//! (one term per line, `#` comments). The repo default is empty. Skips are
//! logged as counts only, never post text or terms.
//!
//! This is the single topic filter (P9): the same list also answers try-it
//! stage 0 ([`excluded_topic`]) for try-it, the try-it cascade, the eval
//! binary, and demo-runner. Nothing personal is hardcoded in the repo.

use crate::fixture::Bookmark;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

thread_local! {
    /// Per-thread list set by [`with_terms`]; wins over the env config.
    static OVERRIDE: RefCell<Option<MuteList>> = const { RefCell::new(None) };
}

/// Restores the previous per-thread override when dropped.
#[must_use = "the override ends when the guard drops"]
pub struct TermsGuard(Option<MuteList>);

impl Drop for TermsGuard {
    /// Put the saved override back.
    fn drop(&mut self) {
        let prev = self.0.take();
        OVERRIDE.with(|o| *o.borrow_mut() = prev);
    }
}

/// Make `list` this thread's mute config (instead of the env) until the
/// returned guard drops. For synthetic demos and tests.
pub fn set_terms(list: MuteList) -> TermsGuard {
    TermsGuard(OVERRIDE.with(|o| o.borrow_mut().replace(list)))
}

/// Run `f` with `list` as this thread's mute config instead of the env
/// (restored afterwards, even on panic).
pub fn with_terms<R>(list: MuteList, f: impl FnOnce() -> R) -> R {
    let _guard = set_terms(list);
    f()
}

/// The active mute config: this thread's [`with_terms`] list, else env.
pub fn current() -> Result<MuteList, String> {
    if let Some(l) = OVERRIDE.with(|o| o.borrow().clone()) {
        return Ok(l);
    }
    MuteList::from_env()
}

/// Read-only: the active mute config, or `Ok(None)` when none is
/// configured. A [`with_terms`] override counts as configured. Otherwise
/// configured means `ANCIENTOS_MUTED_TERMS` is set (even to zero terms) or
/// `ANCIENTOS_MUTED_TERMS_FILE` names a file; an unreadable or missing file
/// is `Err`. Unlike [`current`], "not configured" is distinguishable from
/// "configured with zero terms". Changes nothing for existing callers.
pub fn configured() -> Result<Option<MuteList>, String> {
    if let Some(l) = OVERRIDE.with(|o| o.borrow().clone()) {
        return Ok(Some(l));
    }
    let env_set = std::env::var_os(TERMS_ENV_VAR).is_some();
    let file_set = std::env::var(TERMS_FILE_ENV_VAR).is_ok_and(|p| !p.trim().is_empty());
    if !env_set && !file_set {
        return Ok(None);
    }
    MuteList::from_env().map(Some)
}

/// Id returned by [`excluded_topic`] when the config cannot be read.
pub const CONFIG_ERROR_ID: &str = "mute config unreadable";

/// Try-it stage 0: the muted term `text` mentions under the active config,
/// or `None`. Fails closed: an unreadable config returns
/// [`CONFIG_ERROR_ID`], so the post is answered no with no model call.
pub fn excluded_topic(text: &str) -> Option<String> {
    match current() {
        Ok(l) => l.muted_term(text).map(str::to_string),
        Err(_) => Some(CONFIG_ERROR_ID.to_string()),
    }
}

/// Refuse a muted or unavailable stored root before any thread or model fetch.
/// Empty mute lists need no store read. Explicit fixtures stay isolated from live data.
pub fn screen_stored_post(
    post_id: &str,
    fixture: Option<&std::path::Path>,
    dir: &std::path::Path,
) -> Result<(), String> {
    let mutes = current().map_err(|_| "excluded topic: mute config unavailable".to_string())?;
    if mutes.is_empty() {
        return Ok(());
    }
    let rows = if let Some(path) = fixture {
        crate::fixture::load(path)
            .map_err(|_| "excluded topic: post context unavailable".to_string())?
    } else {
        let real = dir.join(crate::ingest::BOOKMARKS_FILE_NAME);
        let path = if real.exists() {
            real
        } else {
            dir.join("store.jsonl")
        };
        crate::ingest::Store::load_from(&path)?
            .by_id
            .into_values()
            .collect()
    };
    let post = rows
        .iter()
        .find(|b| b.post_id == post_id)
        .ok_or("excluded topic: post context unavailable")?;
    let alt: Vec<&str> = post
        .media
        .iter()
        .filter_map(|m| m.alt_text.as_deref())
        .collect();
    if mutes.is_muted(post, &alt) {
        return Err("excluded topic".into());
    }
    Ok(())
}

/// Env var holding an inline muted-term list.
pub const TERMS_ENV_VAR: &str = "ANCIENTOS_MUTED_TERMS";
/// Env var naming a private file with one muted term per line.
pub const TERMS_FILE_ENV_VAR: &str = "ANCIENTOS_MUTED_TERMS_FILE";

/// Lowercased, de-duplicated muted terms. Empty means nothing is muted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuteList {
    terms: Vec<String>,
}

/// Counts of bookmarks kept out of view by [`filter_surfaced`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkipCounts {
    /// Matched a muted term.
    pub muted: usize,
    /// No longer bookmarked on X (`x_bookmark_present == Some(false)`).
    pub removed: usize,
}

impl SkipCounts {
    /// True when nothing was skipped.
    pub fn is_empty(&self) -> bool {
        self.muted == 0 && self.removed == 0
    }
}

impl MuteList {
    /// Build from raw terms: trimmed, lowercased, blanks dropped, deduped.
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>>(terms: I) -> Self {
        let mut out: Vec<String> = Vec::new();
        for t in terms {
            let t = t.as_ref().trim().to_lowercase();
            if !t.is_empty() && !out.contains(&t) {
                out.push(t);
            }
        }
        Self { terms: out }
    }

    /// Parse a comma/newline separated list; `#` starts a comment line.
    pub fn parse(text: &str) -> Self {
        Self::new(
            text.lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .flat_map(|l| l.split(','))
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
    }

    /// Load from the env var and/or file. Fails closed: a configured file
    /// that cannot be read is an error rather than a silent empty list.
    pub fn from_env() -> Result<Self, String> {
        let mut raw = std::env::var(TERMS_ENV_VAR).unwrap_or_default();
        if let Ok(p) = std::env::var(TERMS_FILE_ENV_VAR) {
            if !p.trim().is_empty() {
                let text = std::fs::read_to_string(p.trim())
                    .map_err(|e| format!("muted terms file unreadable: {}", e.kind()))?;
                raw.push('\n');
                raw.push_str(&text);
            }
        }
        Ok(Self::parse(&raw))
    }

    /// Number of configured terms.
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// True when no term is configured.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The first muted term contained in `text` (case-insensitive), or `None`.
    pub fn muted_term(&self, text: &str) -> Option<&str> {
        let lower = text.to_lowercase();
        self.terms
            .iter()
            .find(|t| lower.contains(t.as_str()))
            .map(String::as_str)
    }

    /// True when any text contains a muted term, case-insensitively.
    pub fn matches_any(&self, texts: &[&str]) -> bool {
        if self.terms.is_empty() {
            return false;
        }
        texts.iter().any(|t| {
            let lower = t.to_lowercase();
            self.terms.iter().any(|term| lower.contains(term.as_str()))
        })
    }

    /// True when the post text, quoted text, link URLs, or cached linked
    /// page text (`linked`) contains a muted term.
    pub fn is_muted(&self, b: &Bookmark, linked: &[&str]) -> bool {
        let mut texts: Vec<&str> = vec![b.text.as_str()];
        if let Some(q) = b.quoted_text.as_deref() {
            texts.push(q);
        }
        texts.extend(b.urls.iter().map(String::as_str));
        texts.extend_from_slice(linked);
        self.matches_any(&texts)
    }
}

/// True when X reports the post is no longer bookmarked. `None` (never
/// checked) means still bookmarked.
pub fn is_removed(b: &Bookmark) -> bool {
    b.x_bookmark_present == Some(false)
}

/// Cached linked-page texts for `b`, looked up by URL hash in `cache`.
pub fn linked_texts<'a>(
    b: &Bookmark,
    cache: &'a HashMap<String, crate::link_context::CacheEntry>,
) -> Vec<&'a str> {
    crate::link_context::link_urls(b)
        .iter()
        .filter_map(|u| cache.get(&crate::translate::sha256_hex(u)))
        .map(|e| e.text.as_str())
        .collect()
}

/// Drop removed and muted bookmarks, returning the kept rows and counts.
/// Removed is checked first, so each skipped row counts once.
pub fn filter_surfaced(
    rows: &[Bookmark],
    mute: &MuteList,
    cache: &HashMap<String, crate::link_context::CacheEntry>,
) -> (Vec<Bookmark>, SkipCounts) {
    let mut counts = SkipCounts::default();
    let mut kept = Vec::with_capacity(rows.len());
    for b in rows {
        if is_removed(b) {
            counts.removed += 1;
        } else if mute.is_muted(b, &linked_texts(b, cache)) {
            counts.muted += 1;
        } else {
            kept.push(b.clone());
        }
    }
    (kept, counts)
}

/// [`filter_surfaced`] with the env mute list and the link cache that sits
/// beside `log_path`.
pub fn filter_for_log(
    rows: &[Bookmark],
    log_path: &Path,
) -> Result<(Vec<Bookmark>, SkipCounts), String> {
    let mute = current()?;
    let cache = match crate::link_context::cache_path_for_log(log_path) {
        Ok(p) => crate::link_context::load_cache(&p),
        Err(_) => HashMap::new(),
    };
    Ok(filter_surfaced(rows, &mute, &cache))
}

/// Log payload for a skip: counts only, never text, ids, or terms.
pub fn skip_payload(counts: &SkipCounts) -> serde_json::Value {
    serde_json::json!({"muted": counts.muted, "removed": counts.removed})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic bookmark for mute tests.
    fn bm(id: &str, text: &str) -> Bookmark {
        let mut b: Bookmark =
            serde_json::from_value(serde_json::json!({"post_id": id, "url": "", "text": text}))
                .unwrap_or_else(|e| panic!("bm: {e}"));
        b.post_id = id.to_string();
        b
    }

    #[test]
    fn default_list_is_empty_and_mutes_nothing() {
        let m = MuteList::default();
        assert!(m.is_empty());
        assert!(!m.is_muted(&bm("1", "anything zorblax"), &[]));
    }

    #[test]
    fn parse_trims_lowercases_dedups_and_skips_comments() {
        let m = MuteList::parse("# private\nZorblax, quuxwidget\nzorblax\n , \n");
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn matches_post_text_case_insensitively() {
        let m = MuteList::new(["ZorBlax"]);
        assert!(m.is_muted(&bm("1", "I love zORBLAX today"), &[]));
        assert!(!m.is_muted(&bm("2", "nothing here"), &[]));
    }

    #[test]
    fn matches_quoted_and_linked_text() {
        let m = MuteList::new(["quuxwidget"]);
        let mut q = bm("1", "plain");
        q.quoted_text = Some("Quoting QuuxWidget".to_string());
        assert!(m.is_muted(&q, &[]));
        let mut u = bm("2", "plain");
        u.urls = vec!["https://example.com/quuxwidget-launch".to_string()];
        assert!(m.is_muted(&u, &[]));
        assert!(m.is_muted(&bm("3", "plain"), &["page about QUUXWIDGET"]));
    }

    #[test]
    fn removed_means_false_only() {
        let mut b = bm("1", "x");
        assert!(!is_removed(&b));
        b.x_bookmark_present = Some(true);
        assert!(!is_removed(&b));
        b.x_bookmark_present = Some(false);
        assert!(is_removed(&b));
    }

    #[test]
    fn filter_counts_and_keeps_order() {
        let m = MuteList::new(["zorblax"]);
        let mut gone = bm("3", "zorblax too");
        gone.x_bookmark_present = Some(false);
        let rows = vec![bm("1", "keep"), bm("2", "Zorblax"), gone, bm("4", "keep2")];
        let (kept, c) = filter_surfaced(&rows, &m, &HashMap::new());
        let ids: Vec<&str> = kept.iter().map(|b| b.post_id.as_str()).collect();
        assert_eq!(ids, vec!["1", "4"]);
        assert_eq!(
            c,
            SkipCounts {
                muted: 1,
                removed: 1
            }
        );
    }

    #[test]
    fn linked_cache_text_mutes() {
        let m = MuteList::new(["zorblax"]);
        let mut b = bm("1", "plain");
        b.urls = vec!["https://example.com/a".to_string()];
        let mut cache = HashMap::new();
        cache.insert(
            crate::translate::sha256_hex("https://example.com/a"),
            crate::link_context::CacheEntry {
                text: "All about Zorblax".to_string(),
                fetched_at: 0,
            },
        );
        let (kept, c) = filter_surfaced(&[b], &m, &cache);
        assert!(kept.is_empty());
        assert_eq!(c.muted, 1);
    }

    #[test]
    fn excluded_topic_uses_override_and_restores() {
        let hit = with_terms(MuteList::new(["Zorblax"]), || {
            excluded_topic("a ZORBLAX post")
        });
        assert_eq!(hit.as_deref(), Some("zorblax"));
        let miss = with_terms(MuteList::new(["zorblax"]), || excluded_topic("plain"));
        assert_eq!(miss, None);
        assert!(OVERRIDE.with(|o| o.borrow().is_none()));
    }

    /// `configured` reports an override (even an empty one) as configured.
    #[test]
    fn configured_treats_override_as_configured() {
        let got = with_terms(MuteList::default(), configured);
        assert_eq!(got, Ok(Some(MuteList::default())));
    }

    #[test]
    fn payload_has_counts_only() {
        let p = skip_payload(&SkipCounts {
            muted: 2,
            removed: 1,
        });
        assert_eq!(p, serde_json::json!({"muted": 2, "removed": 1}));
    }

    #[test]
    fn serde_defaults_new_fields_to_none() {
        let b = bm("1", "x");
        assert_eq!(b.x_bookmark_present, None);
        assert_eq!(b.quoted_text, None);
        let back: Bookmark = serde_json::from_str(
            r#"{"post_id":"1","url":"","text":"x","x_bookmark_present":false}"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(is_removed(&back));
    }
}
