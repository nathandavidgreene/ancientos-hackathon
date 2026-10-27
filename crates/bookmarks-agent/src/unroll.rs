//! Thread reconstruction from X API tweet objects.
//!
//! Pure helpers shared by `delegate` and brief display: parse tweet JSON
//! into [`ThreadPost`] and unroll one author's consecutive self-replies
//! into reading order with [`unroll`].
//!
//! Fail-closed rules: a candidate joins the chain only when its `author_id`
//! matches the root's and its `referenced_tweets` contains a `replied_to`
//! entry for the previous kept post. Other people's replies, branches, and
//! posts with missing ids are skipped, never guessed.
use std::collections::BTreeMap;

/// One `referenced_tweets` entry: `{ "type": "replied_to", "id": "..." }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencedTweet {
    pub kind: String,
    pub id: String,
}

/// One tweet as needed for thread reconstruction. `text` is the full post
/// text with `note_tweet.text` preferred when present. `quoted` holds the
/// quoted post's `(author_username, text)` when this post quotes another
/// post; it is attached metadata only and never counts as a thread post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPost {
    pub id: String,
    pub author_id: String,
    pub text: String,
    pub created_at: Option<String>,
    pub conversation_id: Option<String>,
    pub referenced_tweets: Vec<ReferencedTweet>,
    pub quoted: Option<(String, String)>,
}

/// Full text of a tweet, preferring the long note_tweet text.
fn full_text_of(tweet: &serde_json::Value) -> String {
    if let Some(s) = tweet
        .get("note_tweet")
        .and_then(|n| n.get("text"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return s.to_string();
    }
    tweet
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Non-empty string field `key` of a JSON object.
fn non_empty_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse one X API tweet object. Returns `None` when `id` or `author_id`
/// is missing: without authorship the post can never join a chain.
pub fn parse_post(value: &serde_json::Value) -> Option<ThreadPost> {
    let id = non_empty_str(value, "id")?;
    let author_id = non_empty_str(value, "author_id")?;
    let mut referenced_tweets = Vec::new();
    if let Some(refs) = value.get("referenced_tweets").and_then(|r| r.as_array()) {
        for r in refs {
            if let (Some(kind), Some(rid)) = (
                r.get("type")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty()),
                r.get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty()),
            ) {
                referenced_tweets.push(ReferencedTweet {
                    kind: kind.to_string(),
                    id: rid.to_string(),
                });
            }
        }
    }
    Some(ThreadPost {
        id,
        author_id,
        text: full_text_of(value),
        created_at: non_empty_str(value, "created_at"),
        conversation_id: non_empty_str(value, "conversation_id"),
        referenced_tweets,
        // Quoted posts resolve from `includes` in x_api; a lone tweet
        // object never carries enough to guess one.
        quoted: None,
    })
}

/// True if `post` is a reply to `parent_id`.
fn replies_to(post: &ThreadPost, parent_id: &str) -> bool {
    post.referenced_tweets
        .iter()
        .any(|r| r.kind == "replied_to" && r.id == parent_id)
}

/// Reconstruct the author's own thread starting at `root`.
///
/// Keeps only consecutive self-replies: same `author_id` as the root, each
/// replying to the previous kept post. Branches resolve deterministically
/// to the earliest `created_at` (posts without a timestamp sort last, ties
/// break by id). The result starts with `root` and is in time order.
pub fn unroll(root: &ThreadPost, candidates: &[ThreadPost]) -> Vec<ThreadPost> {
    // Index by id, first occurrence wins; drop other authors and the root.
    let mut by_id: BTreeMap<&str, &ThreadPost> = BTreeMap::new();
    for c in candidates {
        if c.author_id == root.author_id && c.id != root.id && !c.id.is_empty() {
            by_id.entry(c.id.as_str()).or_insert(c);
        }
    }
    let mut chain = vec![root.clone()];
    loop {
        let parent = chain.last().map(|p| p.id.clone()).unwrap_or_default();
        let next = by_id
            .values()
            .filter(|c| replies_to(c, &parent))
            .min_by(|a, b| {
                (a.created_at.is_none(), &a.created_at, &a.id).cmp(&(
                    b.created_at.is_none(),
                    &b.created_at,
                    &b.id,
                ))
            })
            .map(|c| c.id.clone());
        match next {
            Some(id) => {
                let post = by_id.remove(id.as_str()).expect("indexed above");
                chain.push(post.clone());
            }
            None => break,
        }
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Test helper: thread post with reply linkage and optional timestamp.
    fn post(id: &str, author: &str, reply_to: Option<&str>, created: Option<&str>) -> ThreadPost {
        ThreadPost {
            id: id.into(),
            author_id: author.into(),
            text: format!("synthetic text {id}"),
            created_at: created.map(str::to_string),
            conversation_id: Some("root".into()),
            referenced_tweets: reply_to
                .map(|p| {
                    vec![ReferencedTweet {
                        kind: "replied_to".into(),
                        id: p.into(),
                    }]
                })
                .unwrap_or_default(),
            quoted: None,
        }
    }

    /// Test helper: post ids in chain order for compact assertions.
    fn ids(chain: &[ThreadPost]) -> Vec<&str> {
        chain.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn keeps_consecutive_self_replies_in_time_order() {
        let root = post("root", "u1", None, Some("2026-01-01T00:00:00Z"));
        // Shuffled input, including other people's replies and a branch.
        let candidates = vec![
            post("other", "u2", Some("root"), Some("2026-01-01T00:01:00Z")),
            post("r3", "u1", Some("r2"), Some("2026-01-01T00:03:00Z")),
            post("branch", "u1", Some("root"), Some("2026-01-01T00:05:00Z")),
            post("r1", "u1", Some("root"), Some("2026-01-01T00:01:00Z")),
            post("r2", "u1", Some("r1"), Some("2026-01-01T00:02:00Z")),
            post("retweet", "u1", None, Some("2026-01-01T00:04:00Z")),
        ];
        let chain = unroll(&root, &candidates);
        assert_eq!(ids(&chain), vec!["root", "r1", "r2", "r3"]);
    }

    #[test]
    fn stops_at_gap_and_ignores_wrong_author() {
        let root = post("root", "u1", None, None);
        let candidates = vec![
            // Same author but replies to an unknown post: not consecutive.
            post("gap", "u1", Some("elsewhere"), None),
            // Replies to root but by someone else.
            post("stranger", "u9", Some("root"), None),
        ];
        assert_eq!(ids(&unroll(&root, &candidates)), vec!["root"]);
    }

    #[test]
    fn branch_picks_earliest_then_lowest_id() {
        let root = post("root", "u1", None, None);
        let a = post("b2", "u1", Some("root"), Some("2026-01-01T00:02:00Z"));
        let b = post("b1", "u1", Some("root"), Some("2026-01-01T00:02:00Z"));
        let c = post("b0", "u1", Some("root"), None);
        assert_eq!(ids(&unroll(&root, &[a, b, c])), vec!["root", "b1"]);
        // Continuation follows the kept branch only.
        let next = post("b1-next", "u1", Some("b1"), None);
        let other = post("b2-next", "u1", Some("b2"), None);
        let root2 = post("root", "u1", None, None);
        assert_eq!(
            ids(&unroll(
                &root2,
                &[
                    post("b2", "u1", Some("root"), Some("2026-01-01T00:02:00Z")),
                    post("b1", "u1", Some("root"), Some("2026-01-01T00:02:00Z")),
                    next,
                    other,
                ]
            )),
            vec!["root", "b1", "b1-next"]
        );
    }

    #[test]
    fn cycles_and_self_replies_terminate() {
        let root = post("root", "u1", None, None);
        // Synthetic cycle: a replies to root and (impossibly) to b.
        let mut a = post("a", "u1", Some("root"), None);
        a.referenced_tweets.push(ReferencedTweet {
            kind: "replied_to".into(),
            id: "b".into(),
        });
        let b = post("b", "u1", Some("a"), None);
        // A post replying to itself can never re-enter the chain either.
        let mut selfish = post("z", "u1", Some("root"), None);
        selfish.referenced_tweets.push(ReferencedTweet {
            kind: "replied_to".into(),
            id: "z".into(),
        });
        assert_eq!(
            ids(&unroll(&root, &[a, b, selfish])),
            vec!["root", "a", "b"]
        );
    }

    #[test]
    fn parse_prefers_note_tweet_and_requires_ids() {
        let v = json!({
            "id": "p1",
            "author_id": "u1",
            "text": "truncated… https://t.co/x",
            "note_tweet": {"text": "synthetic full thread post"},
            "created_at": "2026-01-01T00:00:00Z",
            "conversation_id": "root",
            "referenced_tweets": [
                {"type": "replied_to", "id": "root"},
                {"type": "", "id": "drop"},
                {"type": "quoted", "id": ""},
            ],
        });
        let p = parse_post(&v).unwrap();
        assert_eq!(p.text, "synthetic full thread post");
        assert_eq!(p.created_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(p.conversation_id.as_deref(), Some("root"));
        assert_eq!(p.referenced_tweets.len(), 1);
        assert_eq!(p.referenced_tweets[0].kind, "replied_to");
        // Missing id or author_id can never join a chain.
        assert!(parse_post(&json!({"author_id": "u1", "text": "x"})).is_none());
        assert!(parse_post(&json!({"id": "p1", "text": "x"})).is_none());
        assert!(parse_post(&json!({"id": "", "author_id": "u1"})).is_none());
        // Empty note_tweet falls back to text.
        let p = parse_post(&json!({
            "id": "p2", "author_id": "u1", "text": "synthetic short",
            "note_tweet": {"text": ""},
        }))
        .unwrap();
        assert_eq!(p.text, "synthetic short");
    }

    #[test]
    fn parse_leaves_quoted_empty_for_lone_tweet() {
        let p = parse_post(&json!({
            "id": "p1", "author_id": "u1", "text": "synthetic with quote ref",
            "referenced_tweets": [{"type": "quoted", "id": "q1"}],
        }))
        .unwrap();
        assert_eq!(p.quoted, None, "includes resolve quoted posts later");
    }

    #[test]
    fn unroll_carries_quoted_without_counting_it() {
        let mut root = post("root", "u1", None, Some("2026-01-01T00:00:00Z"));
        root.quoted = Some(("syn_quoted".into(), "synthetic quoted real content".into()));
        let mut r1 = post("r1", "u1", Some("root"), Some("2026-01-01T00:01:00Z"));
        r1.quoted = Some(("syn_other".into(), "synthetic second quoted post".into()));
        let chain = unroll(&root, &[r1]);
        assert_eq!(ids(&chain), vec!["root", "r1"]);
        assert_eq!(
            chain[0].quoted,
            Some(("syn_quoted".into(), "synthetic quoted real content".into()))
        );
        assert_eq!(
            chain[1].quoted,
            Some(("syn_other".into(), "synthetic second quoted post".into()))
        );
    }
}
