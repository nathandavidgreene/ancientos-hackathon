//! `vote` command: record a feedback verdict for one post.
//!
//! The post's category is looked up from the merged store (live bookmarks
//! plus the optional fixture); posts not found there get category
//! `"unknown"` rather than failing. A `delegate` verdict also queues the
//! post for deeper research with a `delegate.queued` log entry. A
//! `never_author` verdict resolves the post's author from the same store
//! and records the username (not the post id); unknown posts or posts
//! with no author fail closed instead of recording.
use crate::feedback::{self, FeedbackRecord, ThresholdPolicy, Verdict};
use ancientos_core::log::DecisionLog;
use std::collections::BTreeSet;
use std::path::Path;

pub const DELEGATE_QUEUED_KIND: &str = "delegate.queued";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoteResult {
    pub post_id: String,
    pub category: String,
    pub verdict: Verdict,
    pub queued_delegate: bool,
    /// Author hidden by a `never_author` vote (as stored), else `None`.
    pub author: Option<String>,
}

/// Category for a post id from the merged store, `"unknown"` when absent.
pub fn category_for(merged: &crate::merge::MergeOutput, post_id: &str) -> String {
    merged
        .rows
        .iter()
        .find(|b| b.post_id == post_id)
        .map(crate::brief::topic_of)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Author for a post id from the merged store, `None` when the post is
/// absent or has no (non-blank) author.
pub fn author_for(merged: &crate::merge::MergeOutput, post_id: &str) -> Option<String> {
    let name = merged
        .rows
        .iter()
        .find(|b| b.post_id == post_id)?
        .author_username
        .as_deref()?;
    let name = name.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn ensure_log_dir(log_path: &Path) -> Result<(), String> {
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    Ok(())
}

/// Record one verdict in the decision log. `cli_fixture` overrides
/// `ANCIENTOS_FIXTURE` for the category lookup, exactly like `brief`.
/// `never_author` stores the resolved username as the record id; it fails
/// closed when the author cannot be resolved.
pub fn run_vote(
    post_id: &str,
    verdict: Verdict,
    tags: &[String],
    cli_fixture: Option<&str>,
    log_path: &Path,
) -> Result<VoteResult, String> {
    if post_id.is_empty() {
        return Err("post id must be non-empty".to_string());
    }
    let merged = crate::merge::load_merged(cli_fixture)?;
    let category = category_for(&merged, post_id);
    let author = if verdict == Verdict::NeverAuthor {
        Some(author_for(&merged, post_id).ok_or_else(|| {
            format!("cannot hide author: no author for post {post_id} in the store")
        })?)
    } else {
        None
    };
    ensure_log_dir(log_path)?;
    let mut log = DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    // For never_author the record id is the username (see hidden_authors).
    let pick_id = author.clone().unwrap_or_else(|| post_id.to_string());
    feedback::record(
        &mut log,
        FeedbackRecord {
            pick_id,
            category: category.clone(),
            verdict,
            tags: tags.to_vec(),
        },
        ThresholdPolicy::default(),
    )
    .map_err(|e| e.to_string())?;
    let queued_delegate = verdict == Verdict::Delegate;
    if queued_delegate {
        log.append(
            DELEGATE_QUEUED_KIND,
            serde_json::json!({"post_id": post_id, "category": category}),
        )
        .map_err(|e| e.to_string())?;
        // A delegate verdict also queues a wiki ingest (logged, same handle).
        crate::wiki::append_queued(
            &mut log,
            crate::wiki::QUEUE_REASON_DELEGATE,
            post_id,
            post_id,
            crate::idea_card::now_secs(),
        )?;
    }
    Ok(VoteResult {
        post_id: post_id.to_string(),
        category,
        verdict,
        queued_delegate,
        author,
    })
}

/// Undo an author hide: append an `unhide_author` record for `username`.
/// Returns the normalized username. Empty names fail closed; unhiding an
/// author that was never hidden is a harmless no-op entry.
pub fn run_unhide_author(username: &str, log_path: &Path) -> Result<String, String> {
    let normalized = crate::feedback::normalize_author(username);
    if normalized.is_empty() {
        return Err("username must be non-empty".to_string());
    }
    ensure_log_dir(log_path)?;
    let mut log = DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    feedback::record(
        &mut log,
        FeedbackRecord {
            pick_id: username.trim().to_string(),
            category: "unknown".to_string(),
            verdict: Verdict::UnhideAuthor,
            tags: Vec::new(),
        },
        ThresholdPolicy::default(),
    )
    .map_err(|e| e.to_string())?;
    Ok(normalized)
}

/// Currently hidden authors from the decision log (normalized, sorted).
/// A missing log means none; a tampered log is an error (fail closed).
pub fn list_hidden_authors(log_path: &Path) -> Result<BTreeSet<String>, String> {
    let log = DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    let records = feedback::records_from_log(&log).map_err(|e| e.to_string())?;
    Ok(feedback::hidden_authors(&records))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_with_labeled_post(dir: &Path) -> String {
        let path = dir.join("fix.jsonl");
        std::fs::write(
            &path,
            "{\"post_id\":\"p1\",\"url\":\"https://x.com/i/status/p1\",\
             \"text\":\"synthetic labeled post\",\
             \"label\":{\"parent_label\":\"Rust\",\"confidence\":0.9}}\n",
        )
        .unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn records_verdict_with_category_and_tags() {
        let dir = tempfile::tempdir().unwrap();
        let fix = fixture_with_labeled_post(dir.path());
        let log_path = dir.path().join("sub").join("d.jsonl");
        let res = run_vote(
            "p1",
            Verdict::Approve,
            &["syn-tag".to_string()],
            Some(&fix),
            &log_path,
        )
        .unwrap();
        assert_eq!(res.category, "Rust");
        assert!(!res.queued_delegate);
        assert_eq!(res.author, None);
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "feedback");
        assert_eq!(entries[0].payload["pick_id"], "p1");
        assert_eq!(entries[0].payload["category"], "Rust");
        assert_eq!(entries[0].payload["verdict"], "approve");
        assert_eq!(entries[0].payload["tags"], serde_json::json!(["syn-tag"]));
    }

    #[test]
    fn unknown_post_gets_unknown_category() {
        let dir = tempfile::tempdir().unwrap();
        let fix = fixture_with_labeled_post(dir.path());
        let log_path = dir.path().join("d.jsonl");
        let res = run_vote("missing", Verdict::Skip, &[], Some(&fix), &log_path).unwrap();
        assert_eq!(res.category, "unknown");
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries[0].payload["category"], "unknown");
    }

    #[test]
    fn delegate_verdict_queues_delegation() {
        let dir = tempfile::tempdir().unwrap();
        let fix = fixture_with_labeled_post(dir.path());
        let log_path = dir.path().join("d.jsonl");
        let res = run_vote("p1", Verdict::Delegate, &[], Some(&fix), &log_path).unwrap();
        assert!(res.queued_delegate);
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, "feedback");
        assert_eq!(entries[0].payload["verdict"], "delegate");
        assert_eq!(entries[1].kind, DELEGATE_QUEUED_KIND);
        assert_eq!(entries[1].payload["post_id"], "p1");
        assert_eq!(entries[1].payload["category"], "Rust");
        // A delegate verdict also queues a wiki ingest (logged).
        assert_eq!(entries[2].kind, crate::wiki::WIKI_QUEUED_KIND);
        assert_eq!(entries[2].payload["post_id"], "p1");
        assert_eq!(
            entries[2].payload["reason"],
            crate::wiki::QUEUE_REASON_DELEGATE
        );
    }

    #[test]
    fn empty_post_id_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            run_vote("", Verdict::Approve, &[], None, &dir.path().join("d.jsonl")).unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
    }

    fn fixture_with_authors(dir: &Path) -> String {
        let path = dir.join("authors.jsonl");
        std::fs::write(
            &path,
            "{\"post_id\":\"p1\",\"url\":\"https://x.com/i/status/p1\",\
             \"text\":\"synthetic post one\",\"author_username\":\"Syn_User\",\
             \"label\":{\"parent_label\":\"Rust\",\"confidence\":0.9}}\n\
             {\"post_id\":\"p2\",\"url\":\"https://x.com/i/status/p2\",\
             \"text\":\"synthetic post two\",\"author_username\":\"syn_other\"}\n\
             {\"post_id\":\"p3\",\"url\":\"https://x.com/i/status/p3\",\
             \"text\":\"synthetic post without author\"}\n",
        )
        .unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn never_author_stores_resolved_username() {
        let dir = tempfile::tempdir().unwrap();
        let fix = fixture_with_authors(dir.path());
        let log_path = dir.path().join("d.jsonl");
        let res = run_vote("p1", Verdict::NeverAuthor, &[], Some(&fix), &log_path).unwrap();
        assert_eq!(res.category, "Rust");
        assert_eq!(res.author.as_deref(), Some("Syn_User"));
        assert!(!res.queued_delegate);
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].payload["verdict"], "never_author");
        assert_eq!(entries[0].payload["pick_id"], "Syn_User");
        assert_eq!(
            list_hidden_authors(&log_path).unwrap(),
            BTreeSet::from(["syn_user".to_string()])
        );
    }

    #[test]
    fn never_author_fails_closed_without_author_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let fix = fixture_with_authors(dir.path());
        let log_path = dir.path().join("d.jsonl");
        for post_id in ["missing", "p3"] {
            let err =
                run_vote(post_id, Verdict::NeverAuthor, &[], Some(&fix), &log_path).unwrap_err();
            assert!(err.contains("no author"), "{post_id}: {err}");
        }
        assert!(!log_path.exists(), "failed votes must not create a log");
    }

    #[test]
    fn unhide_author_round_trip_with_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let fix = fixture_with_authors(dir.path());
        let log_path = dir.path().join("d.jsonl");
        run_vote("p1", Verdict::NeverAuthor, &[], Some(&fix), &log_path).unwrap();
        run_vote("p2", Verdict::NeverAuthor, &[], Some(&fix), &log_path).unwrap();
        assert_eq!(
            list_hidden_authors(&log_path).unwrap(),
            BTreeSet::from(["syn_other".to_string(), "syn_user".to_string()])
        );
        // Different case/@ spelling still unhides; the log stays append-only.
        let unhidden = run_unhide_author("@SYN_USER", &log_path).unwrap();
        assert_eq!(unhidden, "syn_user");
        assert_eq!(
            list_hidden_authors(&log_path).unwrap(),
            BTreeSet::from(["syn_other".to_string()])
        );
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.kind == "feedback"));
        assert_eq!(entries[2].payload["verdict"], "unhide_author");
    }

    #[test]
    fn unhide_author_rejects_empty_and_lists_empty_when_no_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        for bad in ["", "   ", "@"] {
            assert!(run_unhide_author(bad, &log_path).is_err());
        }
        assert!(!log_path.exists());
        assert!(list_hidden_authors(&log_path).unwrap().is_empty());
        // Unhiding a never-hidden author is a harmless no-op entry.
        run_unhide_author("syn_ghost", &log_path).unwrap();
        assert!(list_hidden_authors(&log_path).unwrap().is_empty());
    }
}
