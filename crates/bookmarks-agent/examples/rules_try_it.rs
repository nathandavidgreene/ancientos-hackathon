//! Score the local rules "Try it" classifier: stdin Bookmark JSONL -> stdout
//! `{"post_id","try_it"}` JSONL. Offline, deterministic.
use bookmarks_agent::{fixture::Bookmark, try_it::is_try_it};
use std::io::BufRead;

/// Example: run the rule-based try-it classifier over sample bookmarks and print the verdicts.
fn main() {
    for line in std::io::stdin().lock().lines() {
        let b: Bookmark = serde_json::from_str(&line.expect("stdin")).expect("bookmark json");
        println!(
            "{}",
            serde_json::json!({"post_id": b.post_id, "try_it": is_try_it(&b)})
        );
    }
}
