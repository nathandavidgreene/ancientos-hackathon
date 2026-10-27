//! Author hiding: never surface a post by a hidden author anywhere.
//!
//! Nathan's rule for authors, mirroring [`crate::language`] and
//! [`crate::liveness`]. Posts by hidden authors are skipped silently:
//! dropped in memory only (brief candidates, web-ui pages,
//! digest/try-it nudges), with no message, no placeholder, and no
//! decision-log entry. The bookmark store is never modified.
//!
//! The hidden set comes from
//! [`crate::feedback::hidden_authors`]; every helper here is pure.
//! Matching is case-insensitive and `@`-insensitive. Bookmarks with no
//! `author_username` (or a blank one) are always kept: hiding applies only
//! on positive evidence of a hidden author (fail closed the other way would
//! hide posts we know nothing about).
use crate::fixture::Bookmark;
use std::collections::BTreeSet;

/// Canonical author key: trimmed, leading `@` stripped, lowercased.
/// Same normalization as [`crate::feedback::normalize_author`].
pub fn normalize(username: &str) -> String {
    crate::feedback::normalize_author(username)
}

/// True when the bookmark's author is in the hidden set. Bookmarks with
/// no author (or a blank one) are never hidden.
pub fn is_hidden(bookmark: &Bookmark, hidden: &BTreeSet<String>) -> bool {
    match bookmark.author_username.as_deref() {
        None => false,
        Some(u) => {
            let name = normalize(u);
            !name.is_empty() && hidden.contains(&name)
        }
    }
}

/// Keep only bookmarks whose author is not hidden, preserving order.
/// Pure; never touches the store.
pub fn filter_visible(bookmarks: &[Bookmark], hidden: &BTreeSet<String>) -> Vec<Bookmark> {
    bookmarks
        .iter()
        .filter(|b| !is_hidden(b, hidden))
        .cloned()
        .collect()
}

/// Keep ids that are not by a hidden author. Ids missing from `bookmarks`
/// (unknown author) are kept: id-only surfaces show no post text for them.
pub fn retain_not_hidden(
    ids: &[String],
    bookmarks: &[Bookmark],
    hidden: &BTreeSet<String>,
) -> Vec<String> {
    ids.iter()
        .filter(|id| {
            bookmarks
                .iter()
                .find(|b| &b.post_id == *id)
                .is_none_or(|b| !is_hidden(b, hidden))
        })
        .cloned()
        .collect()
}

/// Hidden state of an idea-card source post: `Some(true/false)` when the
/// source URL resolves to a bookmark in `bookmarks`, `None` when the
/// source is unknown (unparseable URL or id not in the store).
pub fn source_post_is_hidden(
    source_url: &str,
    bookmarks: &[Bookmark],
    hidden: &BTreeSet<String>,
) -> Option<bool> {
    let id = crate::language::post_id_from_status_url(source_url)?;
    bookmarks
        .iter()
        .find(|b| b.post_id == id)
        .map(|b| is_hidden(b, hidden))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: synthetic bookmark with the given author.
    fn bm(id: &str, author: Option<&str>) -> Bookmark {
        Bookmark {
            quoted_text: None,
            x_bookmark_present: None,
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: format!("synthetic text for {id}"),
            author_username: author.map(str::to_string),
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: Some("en".to_string()),
            urls: Vec::new(),
        }
    }

    /// Test helper: normalized hidden-author set from the given names.
    fn hidden_of(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| normalize(n)).collect()
    }

    #[test]
    fn normalize_strips_at_and_lowercases() {
        assert_eq!(normalize("@Syn_User"), "syn_user");
        assert_eq!(normalize("@@syn_user"), "syn_user");
        assert_eq!(normalize("  SYN_USER  "), "syn_user");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("  @ "), "");
    }

    #[test]
    fn matching_is_case_and_at_insensitive() {
        let hidden = hidden_of(&["syn_user"]);
        assert!(is_hidden(&bm("a", Some("syn_user")), &hidden));
        assert!(is_hidden(&bm("b", Some("SYN_USER")), &hidden));
        assert!(is_hidden(&bm("c", Some("@syn_user")), &hidden));
        assert!(is_hidden(&bm("d", Some(" @Syn_User ")), &hidden));
        assert!(!is_hidden(&bm("e", Some("syn_other")), &hidden));
        assert!(!is_hidden(&bm("f", Some("syn_user2")), &hidden));
    }

    #[test]
    fn bookmarks_without_author_are_kept() {
        let hidden = hidden_of(&["syn_user"]);
        assert!(!is_hidden(&bm("a", None), &hidden));
        assert!(!is_hidden(&bm("b", Some("")), &hidden));
        assert!(!is_hidden(&bm("c", Some("  @ ")), &hidden));
        let rows = vec![bm("a", None), bm("b", Some("syn_user"))];
        let out = filter_visible(&rows, &hidden);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].post_id, "a");
    }

    #[test]
    fn filter_visible_preserves_order_and_drops_silently() {
        let hidden = hidden_of(&["@Syn_Bad"]);
        let rows = vec![
            bm("p1", Some("syn_good")),
            bm("p2", Some("syn_bad")),
            bm("p3", None),
            bm("p4", Some("SYN_BAD")),
        ];
        let out = filter_visible(&rows, &hidden);
        let ids: Vec<&str> = out.iter().map(|b| b.post_id.as_str()).collect();
        assert_eq!(ids, vec!["p1", "p3"]);
    }

    #[test]
    fn unknown_ids_and_sources_are_kept() {
        let rows = vec![bm("p1", Some("syn_good")), bm("p2", Some("syn_bad"))];
        let hidden = hidden_of(&["syn_bad"]);
        let ids = ["p1", "p2", "ghost"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            retain_not_hidden(&ids, &rows, &hidden),
            vec!["p1".to_string(), "ghost".to_string()]
        );
        assert_eq!(
            source_post_is_hidden("https://x.com/a/status/p1", &rows, &hidden),
            Some(false)
        );
        assert_eq!(
            source_post_is_hidden("https://x.com/a/status/p2", &rows, &hidden),
            Some(true)
        );
        assert_eq!(
            source_post_is_hidden("https://x.com/a/status/ghost", &rows, &hidden),
            None
        );
        assert_eq!(source_post_is_hidden("not a url", &rows, &hidden), None);
    }
}
