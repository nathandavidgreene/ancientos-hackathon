//! Post liveness: confirm a bookmark still exists before surfacing it.
//!
//! Every post shown in the brief is checked against the X API via the
//! existing batched `/tweets?ids=` lookup ([`crate::x_api`], max 100 ids per
//! request). Web-ui reuses the same on-disk cache without fetching: it makes
//! no network calls by design, so the brief (via the X client) writes the
//! cache and web-ui only reads it. Dead means the post was deleted / not
//! found, the author account was suspended or deleted, or the post/author
//! is protected (X returns authorization errors for these).
//!
//! Dead posts are ignored completely: skipped silently, dropped from
//! surfacing (brief, web-ui) and from brief evaluation (candidates, model
//! prompt, approval gate). Only a liveness cache entry is recorded: NO
//! decision-log entry, NO UI mention. The X API is used read-only (batched
//! GET only; never un-bookmark, never write), and stored bookmarks are
//! NEVER deleted — filtering is in-memory only.
//!
//! Fail-closed rules (governance is the product):
//! - Unknown (transient network/HTTP errors, unattributable or partial
//!   responses) is neither marked dead nor surfaced as verified: unknown
//!   posts are skipped, never cached (so the next run retries), and never
//!   logged. This matches the repo's fail-closed conventions (missing key,
//!   paid switch off, or gate denial all stop the action; a tampered log
//!   withholds entries; hydration failures keep stored text rather than
//!   guessing).
//! - Offline/demo mode skips the check deterministically: everything is
//!   treated as live, with no network and no cache writes. Skipping covers
//!   an explicit offline flag (`brief --offline` / `--demo`),
//!   `ANCIENTOS_OFFLINE=1`, paid calls disabled (`ANCIENTOS_ALLOW_PAID !=
//!   "1"`, the default), and synthetic `demo-` fixture ids (which can never
//!   exist on X, so checking them would only burn paid calls).
//!
//! Results are cached 24h on disk keyed by post id with a `checked_at`
//! timestamp; fresh entries are reused without network. Secrets come only
//! from env vars (the X client reads `X_CLIENT_ID` and the token store);
//! tests use a mock lookup only and never touch the network.
use crate::fixture::Bookmark;
use crate::x_api::PostLiveness;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cache time-to-live: 24 hours.
pub const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
/// Max ids per batched X lookup (client limit).
pub const MAX_BATCH: usize = 100;
/// Cache file name next to the decision log.
pub const CACHE_FILE_NAME: &str = "liveness_cache.json";
/// Env override for the cache path (tests, custom data dirs).
pub const CACHE_ENV_VAR: &str = "LIVENESS_CACHE_PATH";
/// Explicit offline switch: `ANCIENTOS_OFFLINE=1` skips the check.
pub const OFFLINE_ENV_VAR: &str = "ANCIENTOS_OFFLINE";

/// Injectable batched lookup: ids -> per-id liveness. `Err` means a
/// transient failure (unknown, never dead). Production uses
/// [`crate::x_api::check_liveness`]; tests use a mock.
pub type Lookup<'a> = dyn Fn(&[String]) -> Result<HashMap<String, PostLiveness>, String> + 'a;

/// One cached outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    /// `live` or `unavailable` (`unknown` is never cached).
    pub status: String,
    /// Unix seconds when the check ran.
    pub checked_at: u64,
    /// `deleted` / `suspended` / `protected` for unavailable, else `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CacheEntry {
    fn for_status(status: PostLiveness, checked_at: u64) -> Self {
        let (s, reason) = match status {
            PostLiveness::Live => ("live", None),
            PostLiveness::Unknown => ("unknown", None),
            d => ("unavailable", d.reason().map(str::to_string)),
        };
        Self {
            status: s.to_string(),
            checked_at,
            reason,
        }
    }

    fn to_liveness(&self) -> PostLiveness {
        match self.status.as_str() {
            "live" => PostLiveness::Live,
            "unavailable" => match self.reason.as_deref() {
                Some("suspended") => PostLiveness::Suspended,
                Some("protected") => PostLiveness::Protected,
                _ => PostLiveness::Deleted,
            },
            _ => PostLiveness::Unknown,
        }
    }
}

/// True when the entry is still inside the 24h window.
pub fn is_fresh(entry: &CacheEntry, now: u64) -> bool {
    now.saturating_sub(entry.checked_at) < CACHE_TTL_SECS
}

/// Synthetic fixture ids are never checked: they can never exist on X, so
/// they are always treated as live with no network and no cache reads or
/// writes. This keeps fixture flows deterministic.
pub fn is_demo_id(id: &str) -> bool {
    id.starts_with("demo-")
}

/// Default cache path: `$LIVENESS_CACHE_PATH` or `$ANCIENTOS_HOME/liveness_cache.json`.
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
/// isolated), falling back to [`default_cache_path`]. Takes the log path
/// only to locate the data dir; liveness never writes log entries.
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

/// Load the disk cache. A missing file means empty; a corrupt file is
/// ignored (fail closed: refetch rather than trust it).
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

/// Save the disk cache atomically (parents created 0700). Errors are
/// returned; callers decide whether a cache write failure fails the run.
pub fn save_cache(path: &Path, cache: &HashMap<String, CacheEntry>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("liveness cache dir failed: {e}"))?;
        }
    }
    let data = serde_json::to_string(cache).map_err(|e| format!("liveness cache encode: {e}"))?;
    crate::token_store::atomic_write_0600(path, data.as_bytes())
        .map_err(|e| format!("liveness cache write failed: {e}"))?;
    Ok(())
}

/// True when the check must be skipped deterministically: an explicit
/// offline/demo flag, `ANCIENTOS_OFFLINE=1`, or paid calls disabled
/// (`ANCIENTOS_ALLOW_PAID != "1"`, the default). Skipping means every post
/// is treated as live with no network and no cache writes.
pub fn should_skip(explicit_offline: bool) -> bool {
    if explicit_offline {
        return true;
    }
    if std::env::var(OFFLINE_ENV_VAR)
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return true;
    }
    !std::env::var("ANCIENTOS_ALLOW_PAID").is_ok_and(|v| v == "1")
}

/// Check `ids` with `lookup`, reusing the 24h disk cache. Batches of at
/// most [`MAX_BATCH`] ids per request. Offline/paid-off skips
/// deterministically (all live, no network, no cache writes). A lookup
/// `Err` marks the whole chunk unknown (never dead). Unknown outcomes are
/// never cached. Writes no decision-log entries; never deletes anything.
pub fn check_with_lookup(
    ids: &[String],
    cache_path: &Path,
    explicit_offline: bool,
    now: u64,
    lookup: &Lookup<'_>,
) -> Result<HashMap<String, PostLiveness>, String> {
    if should_skip(explicit_offline) {
        return Ok(ids
            .iter()
            .map(|id| (id.clone(), PostLiveness::Live))
            .collect());
    }
    let mut cache = load_cache(cache_path);
    let mut out: HashMap<String, PostLiveness> = HashMap::new();
    let mut stale: Vec<String> = Vec::new();
    for id in ids {
        if is_demo_id(id) {
            out.insert(id.clone(), PostLiveness::Live);
            continue;
        }
        match cache.get(id) {
            Some(e) if is_fresh(e, now) => {
                out.insert(id.clone(), e.to_liveness());
            }
            _ => stale.push(id.clone()),
        }
    }
    let mut fresh_results: HashMap<String, PostLiveness> = HashMap::new();
    for chunk in stale.chunks(MAX_BATCH) {
        match lookup(chunk) {
            Ok(map) => {
                for id in chunk {
                    let status = map.get(id).copied().unwrap_or(PostLiveness::Unknown);
                    fresh_results.insert(id.clone(), status);
                }
            }
            Err(_) => {
                for id in chunk {
                    fresh_results.insert(id.clone(), PostLiveness::Unknown);
                }
            }
        }
    }
    // Cache live + unavailable only; unknown is retried next run.
    let mut cache_dirty = false;
    for (id, status) in &fresh_results {
        out.insert(id.clone(), *status);
        match status {
            PostLiveness::Live
            | PostLiveness::Deleted
            | PostLiveness::Suspended
            | PostLiveness::Protected => {
                cache.insert(id.clone(), CacheEntry::for_status(*status, now));
                cache_dirty = true;
            }
            PostLiveness::Unknown => {}
        }
    }
    if cache_dirty {
        save_cache(cache_path, &cache)?;
    }
    Ok(out)
}

/// Keep only positively live bookmarks, preserving order. Dead and unknown
/// are skipped (fail closed). Pure; never touches the store.
pub fn live_only(
    bookmarks: &[Bookmark],
    statuses: &HashMap<String, PostLiveness>,
) -> Vec<Bookmark> {
    bookmarks
        .iter()
        .filter(|b| statuses.get(&b.post_id).is_some_and(|s| s.is_live()))
        .cloned()
        .collect()
}

/// Filter `bookmarks` to live ones via `lookup` (cache as in
/// [`check_with_lookup`]). Offline skips return the input unchanged.
/// Writes no decision-log entries; the store is untouched.
pub fn filter_with_lookup(
    bookmarks: &[Bookmark],
    cache_path: &Path,
    explicit_offline: bool,
    now: u64,
    lookup: &Lookup<'_>,
) -> Result<Vec<Bookmark>, String> {
    if should_skip(explicit_offline) {
        return Ok(bookmarks.to_vec());
    }
    let ids: Vec<String> = bookmarks.iter().map(|b| b.post_id.clone()).collect();
    let statuses = check_with_lookup(ids.as_slice(), cache_path, explicit_offline, now, lookup)?;
    Ok(live_only(bookmarks, &statuses))
}

/// Production filter via the existing X client.
pub fn filter_live(
    bookmarks: &[Bookmark],
    cache_path: &Path,
    explicit_offline: bool,
    now: u64,
) -> Result<Vec<Bookmark>, String> {
    filter_with_lookup(bookmarks, cache_path, explicit_offline, now, &|chunk| {
        crate::x_api::check_liveness(chunk)
    })
}

/// True when `cache` holds a fresh `unavailable` entry for `id`.
/// Synthetic `demo-` ids are never dead. Used by web-ui to drop dead
/// posts from surfaces without fetching.
pub fn is_fresh_unavailable(cache: &HashMap<String, CacheEntry>, id: &str, now: u64) -> bool {
    if is_demo_id(id) {
        return false;
    }
    matches!(cache.get(id), Some(e) if is_fresh(e, now) && e.to_liveness().is_unavailable())
}

/// Filter `bookmarks` using only fresh cache entries: drops posts with a
/// fresh `unavailable` entry and keeps everything else (fresh live,
/// missing, expired, or corrupt cache). No network, no cache writes;
/// synthetic `demo-` ids are always kept. Used by web-ui, which makes no
/// network calls by design: the brief (via the X client) writes the cache
/// and web-ui only reuses it. Stale entries are treated as live here —
/// re-verification is the brief's job.
pub fn filter_cached(bookmarks: &[Bookmark], cache_path: &Path, now: u64) -> Vec<Bookmark> {
    let cache = load_cache(cache_path);
    bookmarks
        .iter()
        .filter(|b| !is_fresh_unavailable(&cache, &b.post_id, now))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn bm(id: &str) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: format!("synthetic text for {id}"),
            author_username: Some(format!("syn_{id}")),
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: None,
            urls: Vec::new(),
        }
    }

    fn mock_lookup(
        map: HashMap<String, PostLiveness>,
        calls: &RefCell<Vec<Vec<String>>>,
    ) -> impl Fn(&[String]) -> Result<HashMap<String, PostLiveness>, String> + '_ {
        move |chunk: &[String]| {
            calls.borrow_mut().push(chunk.to_vec());
            let mut out = HashMap::new();
            for id in chunk {
                out.insert(
                    id.clone(),
                    map.get(id).copied().unwrap_or(PostLiveness::Live),
                );
            }
            Ok(out)
        }
    }

    fn cached_status(cache_path: &Path, id: &str) -> Option<(String, Option<String>, u64)> {
        load_cache(cache_path)
            .get(id)
            .map(|e| (e.status.clone(), e.reason.clone(), e.checked_at))
    }

    #[test]
    fn deleted_post_is_unavailable_and_skipped() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("dead1".to_string(), PostLiveness::Deleted);
        map.insert("live1".to_string(), PostLiveness::Live);
        let rows = vec![bm("dead1"), bm("live1")];
        let out =
            filter_with_lookup(&rows, &cache, false, 1_000_000, &mock_lookup(map, &calls)).unwrap();
        assert_eq!(
            out.iter().map(|b| b.post_id.as_str()).collect::<Vec<_>>(),
            vec!["live1"]
        );
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0].len(), 2);
        assert_eq!(
            cached_status(&cache, "dead1"),
            Some(("unavailable".into(), Some("deleted".into()), 1_000_000))
        );
        assert_eq!(
            cached_status(&cache, "live1"),
            Some(("live".into(), None, 1_000_000))
        );
    }

    #[test]
    fn suspended_account_is_unavailable() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("s1".to_string(), PostLiveness::Suspended);
        let out = filter_with_lookup(
            std::slice::from_ref(&bm("s1")),
            &cache,
            false,
            2_000_000,
            &mock_lookup(map, &calls),
        )
        .unwrap();
        assert!(out.is_empty());
        assert_eq!(
            cached_status(&cache, "s1"),
            Some(("unavailable".into(), Some("suspended".into()), 2_000_000))
        );
    }

    #[test]
    fn protected_post_is_unavailable() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("p1".to_string(), PostLiveness::Protected);
        let out = filter_with_lookup(
            std::slice::from_ref(&bm("p1")),
            &cache,
            false,
            3_000_000,
            &mock_lookup(map, &calls),
        )
        .unwrap();
        assert!(out.is_empty());
        assert_eq!(
            cached_status(&cache, "p1"),
            Some(("unavailable".into(), Some("protected".into()), 3_000_000))
        );
    }

    #[test]
    fn live_post_is_surfaced_and_cached() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("live9".to_string(), PostLiveness::Live);
        let out = filter_with_lookup(
            std::slice::from_ref(&bm("live9")),
            &cache,
            false,
            4_000_000,
            &mock_lookup(map, &calls),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].post_id, "live9");
        assert_eq!(
            cached_status(&cache, "live9"),
            Some(("live".into(), None, 4_000_000))
        );
    }

    #[test]
    fn no_decision_log_entry_written_for_dead_posts() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let log = dir.path().join("decision_log.jsonl");
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("dead1".to_string(), PostLiveness::Deleted);
        map.insert("dead2".to_string(), PostLiveness::Suspended);
        let rows = vec![bm("dead1"), bm("dead2"), bm("live1")];
        let out =
            filter_with_lookup(&rows, &cache, false, 5_000_000, &mock_lookup(map, &calls)).unwrap();
        assert_eq!(out.len(), 1);
        // Only the cache records the outcome; no decision-log file appears.
        assert!(cache.exists());
        assert!(!log.exists(), "dead posts must not write log entries");
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![CACHE_FILE_NAME.to_string()], "{names:?}");
    }

    #[test]
    fn cache_hit_within_24h_reuses_without_network() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("c1".to_string(), PostLiveness::Live);
        map.insert("c2".to_string(), PostLiveness::Deleted);
        let rows = vec![bm("c1"), bm("c2")];
        let out = filter_with_lookup(
            &rows,
            &cache,
            false,
            10_000_000,
            &mock_lookup(map.clone(), &calls),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(calls.borrow().len(), 1);
        assert!(cache.exists());
        let cached = load_cache(&cache);
        assert_eq!(cached.len(), 2);
        assert_eq!(cached["c1"].checked_at, 10_000_000);
        // Second run 1h later: cache hit, no lookup calls.
        calls.borrow_mut().clear();
        let out2 = filter_with_lookup(
            &rows,
            &cache,
            false,
            10_000_000 + 3_600,
            &mock_lookup(map, &calls),
        )
        .unwrap();
        assert_eq!(out2.len(), 1);
        assert!(calls.borrow().is_empty(), "cache hit must not call lookup");
    }

    #[test]
    fn cache_expiry_refetches_after_24h() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("e1".to_string(), PostLiveness::Live);
        let rows = vec![bm("e1")];
        filter_with_lookup(
            &rows,
            &cache,
            false,
            20_000_000,
            &mock_lookup(map.clone(), &calls),
        )
        .unwrap();
        assert_eq!(calls.borrow().len(), 1);
        calls.borrow_mut().clear();
        // Exactly at TTL boundary: expired, refetch.
        filter_with_lookup(
            &rows,
            &cache,
            false,
            20_000_000 + CACHE_TTL_SECS,
            &mock_lookup(map, &calls),
        )
        .unwrap();
        assert_eq!(calls.borrow().len(), 1, "expired cache must refetch");
        let cached = load_cache(&cache);
        assert_eq!(cached["e1"].checked_at, 20_000_000 + CACHE_TTL_SECS);
    }

    #[test]
    fn demo_mode_skip_treats_everything_live_without_network_or_cache() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("d1".to_string(), PostLiveness::Deleted);
        let rows = vec![bm("d1"), bm("d2")];
        let lookup = mock_lookup(map, &calls);
        let out = filter_with_lookup(&rows, &cache, true, 30_000_000, &lookup).unwrap();
        assert_eq!(out.len(), 2, "offline skip keeps everything");
        assert!(calls.borrow().is_empty(), "offline must not call lookup");
        assert!(!cache.exists(), "offline must not write cache");
    }

    #[test]
    fn paid_switch_off_skips_like_demo() {
        let _env = crate::test_support::EnvHold::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let lookup = |chunk: &[String]| -> Result<HashMap<String, PostLiveness>, String> {
            calls.borrow_mut().push(chunk.to_vec());
            panic!("lookup must not run when paid is off");
        };
        let rows = vec![bm("x1")];
        let out = filter_with_lookup(&rows, &cache, false, 40_000_000, &lookup).unwrap();
        assert_eq!(out.len(), 1);
        assert!(!cache.exists());
    }

    #[test]
    fn demo_ids_are_always_live_without_lookup_or_cache() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        // Even a lookup that would mark it dead must never see the demo id.
        map.insert("demo-1001".to_string(), PostLiveness::Deleted);
        map.insert("real1".to_string(), PostLiveness::Live);
        let rows = vec![bm("demo-1001"), bm("real1")];
        let out = filter_with_lookup(&rows, &cache, false, 45_000_000, &mock_lookup(map, &calls))
            .unwrap();
        assert_eq!(out.len(), 2);
        for batch in calls.borrow().iter() {
            assert!(
                !batch.iter().any(|id| id == "demo-1001"),
                "demo ids must not be looked up: {batch:?}"
            );
        }
        let cached = load_cache(&cache);
        assert!(
            !cached.contains_key("demo-1001"),
            "demo ids must not be cached"
        );
        assert!(cached.contains_key("real1"));
    }

    #[test]
    fn transient_errors_are_unknown_skipped_and_not_cached() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let lookup = |chunk: &[String]| -> Result<HashMap<String, PostLiveness>, String> {
            calls.borrow_mut().push(chunk.to_vec());
            Err("X API http status 503: overloaded".to_string())
        };
        let rows = vec![bm("t1"), bm("t2")];
        let out = filter_with_lookup(&rows, &cache, false, 50_000_000, &lookup).unwrap();
        assert!(out.is_empty(), "unknown must not surface as verified");
        assert_eq!(calls.borrow().len(), 1);
        // Unknown is never cached: no cache file for unknown-only runs.
        assert!(!cache.exists(), "unknown must not be cached");
    }

    #[test]
    fn batches_lookups_at_100_ids() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let calls = RefCell::new(Vec::new());
        let rows: Vec<Bookmark> = (0..250).map(|i| bm(&format!("b{i:03}"))).collect();
        let map = HashMap::new();
        let out = filter_with_lookup(&rows, &cache, false, 60_000_000, &mock_lookup(map, &calls))
            .unwrap();
        assert_eq!(out.len(), 250);
        let sizes: Vec<usize> = calls.borrow().iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![100, 100, 50], "{sizes:?}");
    }

    #[test]
    fn nothing_is_deleted_from_inputs_or_store() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let store = dir.path().join("store.jsonl");
        let rows = vec![bm("gone1"), bm("live1")];
        let before: Vec<String> = rows
            .iter()
            .map(|b| serde_json::to_string(b).unwrap())
            .collect();
        std::fs::write(&store, before.join("\n") + "\n").unwrap();
        let before_bytes = std::fs::read(&store).unwrap();
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("gone1".to_string(), PostLiveness::Deleted);
        map.insert("live1".to_string(), PostLiveness::Live);
        let snapshot = rows.clone();
        let out = filter_with_lookup(&rows, &cache, false, 70_000_000, &mock_lookup(map, &calls))
            .unwrap();
        assert_eq!(out.len(), 1);
        // Input slice is unchanged and the store file is byte-identical.
        assert_eq!(rows, snapshot);
        assert_eq!(std::fs::read(&store).unwrap(), before_bytes);
        // Cache holds the dead verdict but the store still has the row.
        let store_text = std::fs::read_to_string(&store).unwrap();
        assert!(store_text.contains("gone1"), "{store_text}");
    }

    #[test]
    fn corrupt_cache_is_ignored_and_refetched() {
        let _env = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        std::fs::write(&cache, "{not valid json").unwrap();
        let calls = RefCell::new(Vec::new());
        let mut map = HashMap::new();
        map.insert("c1".to_string(), PostLiveness::Live);
        let out = filter_with_lookup(
            std::slice::from_ref(&bm("c1")),
            &cache,
            false,
            75_000_000,
            &mock_lookup(map, &calls),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(calls.borrow().len(), 1, "corrupt cache must refetch");
        assert_eq!(
            cached_status(&cache, "c1"),
            Some(("live".into(), None, 75_000_000))
        );
    }

    #[test]
    fn filter_cached_drops_only_fresh_unavailable() {
        // No env needed: cache-only reuse never reads env or network.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let now = 80_000_000;
        let mut entries = HashMap::new();
        entries.insert(
            "fresh-dead".to_string(),
            CacheEntry::for_status(PostLiveness::Deleted, now - 100),
        );
        entries.insert(
            "fresh-live".to_string(),
            CacheEntry::for_status(PostLiveness::Live, now - 100),
        );
        entries.insert(
            "stale-dead".to_string(),
            CacheEntry::for_status(PostLiveness::Suspended, now - CACHE_TTL_SECS - 1),
        );
        entries.insert(
            "demo-9999".to_string(),
            CacheEntry::for_status(PostLiveness::Deleted, now - 100),
        );
        save_cache(&cache, &entries).unwrap();
        let rows = vec![
            bm("fresh-dead"),
            bm("fresh-live"),
            bm("stale-dead"),
            bm("uncached"),
            bm("demo-9999"),
        ];
        let out = filter_cached(&rows, &cache, now);
        let ids: Vec<&str> = out.iter().map(|b| b.post_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["fresh-live", "stale-dead", "uncached", "demo-9999"]
        );
        // Read-only: the cache file is untouched.
        assert_eq!(load_cache(&cache).len(), 4);
    }

    #[test]
    fn filter_cached_with_missing_cache_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE_NAME);
        let rows = vec![bm("a"), bm("b")];
        let out = filter_cached(&rows, &cache, 90_000_000);
        assert_eq!(out.len(), 2);
        assert!(!cache.exists(), "cache-only reuse must not write");
    }
}
