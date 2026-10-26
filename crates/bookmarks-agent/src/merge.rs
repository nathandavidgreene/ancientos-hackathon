//! Merge the live bookmark store with a fixture JSONL file.
//!
//! The merge is a union by `post_id` and never deletes: rows missing from
//! either side are kept, and a null `still_bookmarked` never means removed
//! (the field is ignored entirely). For an id present in both, the live
//! text/metadata wins, folders are unioned, and the best label wins:
//! a `jev` or foldered label beats any other label, which beats none.
use crate::fixture::{Bookmark, Label};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Confidence at or above this counts as high confidence.
pub const HIGH_CONFIDENCE_THRESHOLD: f64 = 0.8;

/// Env var holding the fixture path when no CLI flag is given.
pub const FIXTURE_ENV_VAR: &str = "ANCIENTOS_FIXTURE";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeCounts {
    pub live: usize,
    pub fixture: usize,
    pub overlap: usize,
    pub total: usize,
    pub high_confidence: usize,
}

#[derive(Debug, Clone, Default)]
pub struct MergeOutput {
    /// Union of live and fixture rows, sorted by `post_id`.
    pub rows: Vec<Bookmark>,
    /// Ids that came from the live store (used to prefer live rows later).
    pub live_ids: BTreeSet<String>,
    pub counts: MergeCounts,
}

/// Rank a label: jev-method or foldered (2) > any other label (1) > none (0).
fn label_rank(label: Option<&Label>) -> u8 {
    match label {
        None => 0,
        Some(l) => {
            let jev = l
                .method
                .as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case("jev"));
            if jev || l.is_foldered == Some(true) {
                2
            } else {
                1
            }
        }
    }
}

fn confidence_of(label: Option<&Label>) -> f64 {
    label.and_then(|l| l.confidence).unwrap_or(-1.0)
}

/// Pick the better of two labels. Higher rank wins; ties break by higher
/// confidence, then by preferring the live label.
fn better_label(live: Option<&Label>, fixture: Option<&Label>) -> Option<Label> {
    let rank = (label_rank(live), label_rank(fixture));
    match rank {
        (0, 0) => None,
        (r_live, r_fix) if r_live > r_fix => live.cloned(),
        (r_live, r_fix) if r_fix > r_live => fixture.cloned(),
        _ => {
            // Same rank: higher confidence wins, live wins ties.
            if confidence_of(fixture) > confidence_of(live) {
                fixture.cloned()
            } else {
                live.cloned().or_else(|| fixture.cloned())
            }
        }
    }
}

fn union_folders(a: Option<&Vec<String>>, b: Option<&Vec<String>>) -> Option<Vec<String>> {
    let mut set = BTreeSet::new();
    for list in [a, b].into_iter().flatten() {
        set.extend(list.iter().cloned());
    }
    if set.is_empty() {
        None
    } else {
        Some(set.into_iter().collect())
    }
}

fn merge_pair(live: &Bookmark, fixture: &Bookmark) -> Bookmark {
    let mut merged = live.clone();
    merged.label = better_label(live.label.as_ref(), fixture.label.as_ref());
    merged.folders = union_folders(live.folders.as_ref(), fixture.folders.as_ref());
    merged
}

/// Merge live and fixture rows by `post_id`. Never deletes: the result is
/// the union of both inputs.
pub fn merge(live: &[Bookmark], fixture: &[Bookmark]) -> MergeOutput {
    let mut live_by_id: BTreeMap<&str, &Bookmark> = BTreeMap::new();
    for b in live {
        live_by_id.insert(b.post_id.as_str(), b);
    }
    let mut fixture_by_id: BTreeMap<&str, &Bookmark> = BTreeMap::new();
    for b in fixture {
        fixture_by_id.insert(b.post_id.as_str(), b);
    }

    let mut ids = BTreeSet::new();
    ids.extend(live_by_id.keys().map(|s| (*s).to_string()));
    ids.extend(fixture_by_id.keys().map(|s| (*s).to_string()));

    let mut rows = Vec::with_capacity(ids.len());
    let mut overlap = 0;
    for id in &ids {
        match (live_by_id.get(id.as_str()), fixture_by_id.get(id.as_str())) {
            (Some(l), Some(f)) => {
                overlap += 1;
                rows.push(merge_pair(l, f));
            }
            (Some(l), None) => rows.push((*l).clone()),
            (None, Some(f)) => rows.push((*f).clone()),
            (None, None) => {}
        }
    }

    let high_confidence = rows
        .iter()
        .filter(|b| {
            b.label
                .as_ref()
                .and_then(|l| l.confidence)
                .is_some_and(|c| c >= HIGH_CONFIDENCE_THRESHOLD)
        })
        .count();

    let live_ids: BTreeSet<String> = live_by_id.keys().map(|s| (*s).to_string()).collect();
    MergeOutput {
        rows,
        live_ids,
        counts: MergeCounts {
            live: live.len(),
            fixture: fixture.len(),
            overlap,
            total: ids.len(),
            high_confidence,
        },
    }
}

/// Resolve the fixture path: explicit CLI flag wins, then `ANCIENTOS_FIXTURE`.
/// Empty values count as unset. `Ok(None)` means no fixture was configured.
pub fn resolve_fixture_path(cli_flag: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = cli_flag.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var(FIXTURE_ENV_VAR)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Load the live store plus the configured fixture and merge them.
/// A missing live store means zero live rows; an explicitly configured
/// fixture path that cannot be read is an error (fail closed).
pub fn load_merged(cli_fixture: Option<&str>) -> Result<MergeOutput, String> {
    load_merged_from(cli_fixture, None)
}

fn load_merged_from(
    cli_fixture: Option<&str>,
    live_path_override: Option<&Path>,
) -> Result<MergeOutput, String> {
    let store = match live_path_override {
        Some(p) => crate::ingest::Store::load_from(p)?,
        None => crate::ingest::Store::load()?,
    };
    let live_rows: Vec<Bookmark> = store.by_id.into_values().collect();
    let fixture_rows = match resolve_fixture_path(cli_fixture) {
        None => Vec::new(),
        Some(path) => crate::fixture::load(&path).map_err(|e| e.to_string())?,
    };
    Ok(merge(&live_rows, &fixture_rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(id: &str, text: &str) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: text.into(),
            author_username: Some(format!("syn_{id}")),
            created_at: Some("2026-01-01T00:00:00+00:00".into()),
            folders: None,
            label: None,
            had_note_tweet: false,
        }
    }

    fn labeled(
        mut b: Bookmark,
        method: Option<&str>,
        foldered: Option<bool>,
        conf: f64,
    ) -> Bookmark {
        b.label = Some(Label {
            parent_label: Some("Synthetic Topic".into()),
            confidence: Some(conf),
            method: method.map(str::to_string),
            is_foldered: foldered,
        });
        b
    }

    #[test]
    fn union_counts_without_overlap() {
        let out = merge(&[bm("a", "live a")], &[bm("b", "fix b")]);
        assert_eq!(
            out.counts,
            MergeCounts {
                live: 1,
                fixture: 1,
                overlap: 0,
                total: 2,
                high_confidence: 0,
            }
        );
        assert_eq!(out.live_ids.len(), 1);
        assert!(out.live_ids.contains("a"));
    }

    #[test]
    fn shared_id_prefers_live_text_and_unions_folders() {
        let mut live = bm("s", "live text");
        live.folders = Some(vec!["B".into(), "A".into()]);
        let mut fix = bm("s", "fixture text");
        fix.folders = Some(vec!["C".into(), "A".into()]);
        let out = merge(std::slice::from_ref(&live), std::slice::from_ref(&fix));
        assert_eq!(out.counts.overlap, 1);
        assert_eq!(out.counts.total, 1);
        assert_eq!(out.rows[0].text, "live text");
        assert_eq!(
            out.rows[0].folders,
            Some(vec!["A".into(), "B".into(), "C".into()])
        );
    }

    #[test]
    fn jev_label_beats_plain_label() {
        let live = labeled(bm("s", "live"), None, None, 0.99);
        let fix = labeled(bm("s", "fix"), Some("jev"), None, 0.10);
        let out = merge(std::slice::from_ref(&live), std::slice::from_ref(&fix));
        assert_eq!(
            out.rows[0].label.as_ref().and_then(|l| l.method.clone()),
            Some("jev".into())
        );
    }

    #[test]
    fn foldered_label_beats_plain_label() {
        let live = labeled(bm("s", "live"), None, None, 0.99);
        let fix = labeled(bm("s", "fix"), Some("other"), Some(true), 0.10);
        let out = merge(std::slice::from_ref(&live), std::slice::from_ref(&fix));
        assert_eq!(
            out.rows[0].label.as_ref().and_then(|l| l.is_foldered),
            Some(true)
        );
    }

    #[test]
    fn any_label_beats_none_and_ties_prefer_live() {
        let live = labeled(bm("s", "live"), None, None, 0.5);
        let plain = bm("s", "fix");
        let out = merge(std::slice::from_ref(&live), std::slice::from_ref(&plain));
        assert!(out.rows[0].label.is_some());

        let no_label = bm("t", "live");
        let fix = labeled(bm("t", "fix"), None, None, 0.5);
        let out = merge(std::slice::from_ref(&no_label), std::slice::from_ref(&fix));
        assert_eq!(
            out.rows[0].label.as_ref().and_then(|l| l.confidence),
            Some(0.5)
        );

        // Same rank and confidence: live label wins.
        let mut live2 = labeled(bm("u", "live"), None, None, 0.5);
        live2.label.as_mut().unwrap().parent_label = Some("Live".into());
        let mut fix2 = labeled(bm("u", "fix"), None, None, 0.5);
        fix2.label.as_mut().unwrap().parent_label = Some("Fix".into());
        let out = merge(std::slice::from_ref(&live2), std::slice::from_ref(&fix2));
        assert_eq!(
            out.rows[0]
                .label
                .as_ref()
                .and_then(|l| l.parent_label.clone()),
            Some("Live".into())
        );
    }

    #[test]
    fn higher_confidence_wins_within_same_rank() {
        let live = labeled(bm("s", "live"), None, None, 0.2);
        let fix = labeled(bm("s", "fix"), None, None, 0.7);
        let out = merge(std::slice::from_ref(&live), std::slice::from_ref(&fix));
        assert_eq!(
            out.rows[0].label.as_ref().and_then(|l| l.confidence),
            Some(0.7)
        );
    }

    #[test]
    fn null_still_bookmarked_never_removes() {
        let line = r#"{"post_id":"k1","url":"https://x.com/i/status/k1","text":"synthetic keep","still_bookmarked":null}"#;
        let fixture = crate::fixture::parse_jsonl(line).unwrap();
        let out = merge(&[bm("live-only", "live")], &fixture);
        assert_eq!(out.counts.total, 2);
        assert!(out.rows.iter().any(|b| b.post_id == "k1"));
    }

    #[test]
    fn high_confidence_threshold_counts_merged_labels() {
        let rows = vec![
            labeled(bm("h1", "a"), None, None, 0.8),
            labeled(bm("h2", "b"), None, None, 0.95),
            labeled(bm("l1", "c"), None, None, 0.79),
            bm("n1", "d"),
        ];
        let out = merge(&rows, &[]);
        assert_eq!(out.counts.high_confidence, 2);
    }

    #[test]
    fn fixture_flag_beats_env() {
        let _guard = env_lock();
        let saved = std::env::var(FIXTURE_ENV_VAR).ok();
        std::env::set_var(FIXTURE_ENV_VAR, "/tmp/from-env.jsonl");
        assert_eq!(
            resolve_fixture_path(Some("/tmp/from-flag.jsonl")),
            Some(PathBuf::from("/tmp/from-flag.jsonl"))
        );
        assert_eq!(
            resolve_fixture_path(None),
            Some(PathBuf::from("/tmp/from-env.jsonl"))
        );
        std::env::set_var(FIXTURE_ENV_VAR, "");
        assert_eq!(resolve_fixture_path(None), None);
        assert_eq!(resolve_fixture_path(Some("")), None);
        match saved {
            Some(v) => std::env::set_var(FIXTURE_ENV_VAR, v),
            None => std::env::remove_var(FIXTURE_ENV_VAR),
        }
    }

    #[test]
    fn load_merged_reads_live_and_fixture_files() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let live_path = dir.path().join("bookmarks.jsonl");
        let fix_path = dir.path().join("fix.jsonl");
        std::fs::write(
            &live_path,
            "{\"post_id\":\"s\",\"url\":\"u\",\"text\":\"live text\"}\n\
             {\"post_id\":\"only-live\",\"url\":\"u\",\"text\":\"live\"}\n",
        )
        .unwrap();
        std::fs::write(
            &fix_path,
            "{\"post_id\":\"s\",\"url\":\"u\",\"text\":\"fix text\",\"label\":{\"parent_label\":\"T\",\"confidence\":0.9,\"method\":\"jev\"}}\n",
        )
        .unwrap();
        let saved = std::env::var(FIXTURE_ENV_VAR).ok();
        std::env::remove_var(FIXTURE_ENV_VAR);
        let store = crate::ingest::Store::load_from(&live_path).unwrap();
        let live_rows: Vec<Bookmark> = store.by_id.into_values().collect();
        let fix_rows = crate::fixture::load(&fix_path).unwrap();
        let out = merge(&live_rows, &fix_rows);
        assert_eq!(out.counts.total, 2);
        assert_eq!(out.counts.overlap, 1);
        let shared = out.rows.iter().find(|b| b.post_id == "s").unwrap();
        assert_eq!(shared.text, "live text");
        assert_eq!(
            shared.label.as_ref().and_then(|l| l.method.clone()),
            Some("jev".into())
        );
        // load_merged_from with an explicit live path exercises the same join.
        let out2 = load_merged_from(Some(fix_path.to_str().unwrap()), Some(&live_path)).unwrap();
        assert_eq!(out2.counts, out.counts);
        match saved {
            Some(v) => std::env::set_var(FIXTURE_ENV_VAR, v),
            None => std::env::remove_var(FIXTURE_ENV_VAR),
        }
    }

    #[test]
    fn missing_live_store_loads_empty_without_fixture() {
        let _guard = env_lock();
        let saved = std::env::var(FIXTURE_ENV_VAR).ok();
        std::env::remove_var(FIXTURE_ENV_VAR);
        let dir = tempfile::tempdir().unwrap();
        let out = load_merged_from(None, Some(&dir.path().join("absent.jsonl"))).unwrap();
        assert_eq!(out.counts.total, 0);
        match saved {
            Some(v) => std::env::set_var(FIXTURE_ENV_VAR, v),
            None => std::env::remove_var(FIXTURE_ENV_VAR),
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }
}
