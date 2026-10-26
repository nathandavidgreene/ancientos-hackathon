//! Incremental ingest. A bookmark missing from a fetch is NEVER treated as removed:
//! a partial page proves nothing. Records are only added or refreshed.
use crate::fixture::Bookmark;
use std::collections::BTreeMap;

/// One page of results from the X bookmarks endpoint.
pub struct Page {
    pub items: Vec<Bookmark>,
    pub next_token: Option<String>,
}

pub trait BookmarkSource {
    fn fetch(&mut self, pagination_token: Option<&str>) -> Result<Page, String>;
}

#[derive(Default)]
pub struct Store {
    pub by_id: BTreeMap<String, Bookmark>,
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
    fn fetch(&mut self, _t: Option<&str>) -> Result<Page, String> {
        Err("X API source not implemented".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(id: &str) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: String::new(),
            author_username: None,
            created_at: None,
            folders: None,
            label: None,
        }
    }

    struct OnePage(Vec<&'static str>);
    impl BookmarkSource for OnePage {
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
}
