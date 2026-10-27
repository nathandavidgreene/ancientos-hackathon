//! Incremental ingest. A bookmark missing from a fetch is NEVER treated as removed:
//! a partial page proves nothing. Records are only added or refreshed.
//!
//! The local store persists as JSONL next to the tokens file so a partial sync
//! can never drop prior rows.
use crate::fixture::Bookmark;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const BOOKMARKS_FILE_NAME: &str = "bookmarks.jsonl";

/// One page of results from the X bookmarks endpoint.
pub struct Page {
    pub items: Vec<Bookmark>,
    pub next_token: Option<String>,
}

pub trait BookmarkSource {
    /// Fetch one page of bookmarks, continuing from `pagination_token`.
    fn fetch(&mut self, pagination_token: Option<&str>) -> Result<Page, String>;
}

#[derive(Default)]
pub struct Store {
    pub by_id: BTreeMap<String, Bookmark>,
}

/// Default store path: `$ANCIENTOS_HOME/bookmarks.jsonl` or
/// `~/.ancientos-hackathon/bookmarks.jsonl`.
pub fn store_path() -> Result<PathBuf, String> {
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(BOOKMARKS_FILE_NAME))
}

impl Store {
    /// Load from an explicit JSONL path. A missing file means an empty store.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("store read failed: {e}")),
            Ok(text) => {
                let rows = crate::fixture::parse_jsonl(&text)
                    .map_err(|e| format!("store parse failed: {e}"))?;
                let mut store = Self::default();
                for b in rows {
                    store.by_id.insert(b.post_id.clone(), b);
                }
                Ok(store)
            }
        }
    }

    /// Load from the default path.
    pub fn load() -> Result<Self, String> {
        Self::load_from(&store_path()?)
    }

    /// Save all rows to an explicit JSONL path, atomically. Never drops rows:
    /// the whole in-memory map (which `sync` only grows) is written.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("store dir failed: {e}"))?;
        }
        let mut data = String::new();
        for b in self.by_id.values() {
            let line = serde_json::to_string(b).map_err(|e| format!("store encode failed: {e}"))?;
            data.push_str(&line);
            data.push('\n');
        }
        crate::token_store::atomic_write_0600(path, data.as_bytes())
            .map_err(|e| format!("store write failed: {e}"))?;
        Ok(())
    }

    /// Save to the default path. Returns the path written.
    pub fn save(&self) -> Result<PathBuf, String> {
        let path = store_path()?;
        self.save_to(&path)?;
        Ok(path)
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct SyncReport {
    pub new: usize,
    pub pages: usize,
}

/// Fetch pages until we reach a known post or `max_pages`. Never deletes.
pub fn sync(
    store: &mut Store,
    src: &mut dyn BookmarkSource,
    max_pages: usize,
) -> Result<SyncReport, String> {
    let mut report = SyncReport::default();
    let mut token: Option<String> = None;
    while report.pages < max_pages {
        let page = src.fetch(token.as_deref())?;
        report.pages += 1;
        let mut hit_known = false;
        for b in page.items {
            if store.by_id.contains_key(&b.post_id) {
                hit_known = true;
            } else {
                report.new += 1;
            }
            store.by_id.insert(b.post_id.clone(), b);
        }
        match page.next_token {
            Some(t) if !hit_known => token = Some(t),
            _ => break,
        }
    }
    Ok(report)
}

/// Network source placeholder; real X API calls are not wired yet (paid, off by default).
pub struct StubXSource;

impl BookmarkSource for StubXSource {
    /// Test helper: fetch.
    fn fetch(&mut self, _t: Option<&str>) -> Result<Page, String> {
        Err("X API source not implemented".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: bm.
    fn bm(id: &str) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: String::new(),
            author_username: None,
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: None,
            urls: Vec::new(),
        }
    }

    struct OnePage(Vec<&'static str>);
    impl BookmarkSource for OnePage {
        /// Test helper: fetch.
        fn fetch(&mut self, _t: Option<&str>) -> Result<Page, String> {
            Ok(Page {
                items: self.0.iter().map(|i| bm(i)).collect(),
                next_token: Some("n".into()),
            })
        }
    }

    #[test]
    fn partial_sync_never_removes() {
        let mut store = Store::default();
        store.by_id.insert("old".into(), bm("old"));
        let r = sync(&mut store, &mut OnePage(vec!["new", "old"]), 5).unwrap();
        assert_eq!(r, SyncReport { new: 1, pages: 1 });
        assert!(store.by_id.contains_key("old"));
        assert_eq!(store.by_id.len(), 2);
    }

    #[test]
    fn save_after_partial_sync_keeps_all_prior_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BOOKMARKS_FILE_NAME);
        // Seed the persisted store with two rows.
        let mut seeded = Store::default();
        seeded.by_id.insert("a".into(), bm("a"));
        seeded.by_id.insert("b".into(), bm("b"));
        seeded.save_to(&path).unwrap();

        // A partial fetch returns only one old row plus one new row.
        let mut store = Store::load_from(&path).unwrap();
        let r = sync(&mut store, &mut OnePage(vec!["b", "c"]), 5).unwrap();
        assert_eq!(r.new, 1);
        store.save_to(&path).unwrap();

        let back = Store::load_from(&path).unwrap();
        assert_eq!(back.by_id.len(), 3);
        assert!(back.by_id.contains_key("a"));
        assert!(back.by_id.contains_key("b"));
        assert!(back.by_id.contains_key("c"));
    }

    #[test]
    fn missing_store_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load_from(&dir.path().join("nope.jsonl")).unwrap();
        assert!(store.by_id.is_empty());
    }
}
