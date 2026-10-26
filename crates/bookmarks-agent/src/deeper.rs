//! `deeper` command ("go deeper"): expand one post's thread plus its quoted
//! and linked posts, then write a step-by-step walkthrough to
//! `deeper/<post_id>.md`.
//!
//! Read-only on the network side: the thread comes from the existing thread
//! fetch and quoted/linked posts are looked up one by one through an injected
//! fetcher. Any fetch error fails closed (no file, no log entry). Offline
//! mode builds the same context from a fixture JSONL with no network. At
//! most one Muse Spark call (paid switch on and online only); otherwise a
//! deterministic local fallback walkthrough. Only metadata enters the
//! decision log — never post text or walkthrough output.
use crate::delegate::{MuseCaller, ThreadFetcher};
use crate::fixture::Bookmark;
use crate::unroll::{ReferencedTweet, ThreadPost};
use ancientos_core::log::DecisionLog;
use ancientos_router::muse::MuseUsage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DEEPER_DONE_KIND: &str = "deeper.write";
pub const DEEPER_DIR_NAME: &str = "deeper";
pub const DEEPER_MAX_COMPLETION_TOKENS: u32 = 1000;
pub const DEEPER_REASONING_EFFORT: &str = "low";

/// One-post lookup: a post id -> the fetched post. Read-only; any error
/// fails the whole run closed.
pub type PostFetcher<'a> = dyn Fn(&str) -> Result<ThreadPost, String> + 'a;

#[derive(Debug, Clone)]
pub struct DeeperResult {
    pub output: String,
    pub model: String,
    pub usage: Option<MuseUsage>,
    /// "paid" (one Muse call) or "local" (deterministic fallback).
    pub mode: String,
    pub thread_posts: usize,
    pub quoted_posts: usize,
    pub linked_posts: usize,
    pub path: PathBuf,
}

/// Output directory: `$ANCIENTOS_HOME/deeper` (or
/// `~/.ancientos-hackathon/deeper`).
pub fn deeper_dir() -> Result<PathBuf, String> {
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(DEEPER_DIR_NAME))
}

/// Fail closed unless the id is all digits (or a synthetic `demo-<digits>`
/// fixture id): no empty ids, no separators, no traversal — the id becomes a
/// file name below. Live X fetches still validate digits-only in `x_api`.
pub fn validate_post_id(id: &str) -> Result<(), String> {
    let digits = id.strip_prefix("demo-").unwrap_or(id);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        Ok(())
    } else {
        Err("invalid post id (digits only, or demo-<digits>)".to_string())
    }
}

/// Post ids linked from post text: `x.com`/`twitter.com` URLs with a
/// `/status/<id>` path. Order-preserving, deduplicated. Pure, for tests.
pub fn extract_linked_ids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(pos) = text[start..].find("/status/") {
        let abs = start + pos;
        let before = &text[..abs];
        let token_start = before
            .rfind(|c: char| c.is_whitespace())
            .map(|p| p + 1)
            .unwrap_or(0);
        let token = &before[token_start..];
        if token.contains("x.com") || token.contains("twitter.com") {
            let after = &text[abs + "/status/".len()..];
            let id: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if !id.is_empty() && !out.contains(&id) {
                out.push(id);
            }
        }
        start = abs + 1;
    }
    out
}

/// Quoted ids referenced by thread posts (kind `"quoted"`), excluding ids
/// already in the thread. Order-preserving, deduplicated. Pure, for tests.
pub fn quoted_ids(thread: &[ThreadPost]) -> Vec<String> {
    let in_thread = |id: &str| thread.iter().any(|p| p.id == id);
    let mut out = Vec::new();
    for p in thread {
        for r in &p.referenced_tweets {
            if r.kind == "quoted" && !r.id.is_empty() && !in_thread(&r.id) && !out.contains(&r.id) {
                out.push(r.id.clone());
            }
        }
    }
    out
}

/// Linked ids from all thread post texts, excluding thread ids and quoted
/// ids. Order-preserving, deduplicated. Pure, for tests.
pub fn linked_ids(thread: &[ThreadPost], quoted: &[String]) -> Vec<String> {
    let in_thread = |id: &str| thread.iter().any(|p| p.id == id);
    let mut out = Vec::new();
    for p in thread {
        for id in extract_linked_ids(&p.text) {
            if !in_thread(&id) && !quoted.contains(&id) && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// Build the one prompt sent to Muse Spark: a sanitized walkthrough request
/// plus the thread with its quoted/linked context (full length, uncapped).
pub fn build_paid_prompt(
    thread: &[ThreadPost],
    quoted: &[ThreadPost],
    linked: &[ThreadPost],
) -> String {
    let mut body = String::new();
    for (i, p) in thread.iter().enumerate() {
        let clean = crate::sanitize::sanitize_uncapped(&p.text);
        body.push_str(&format!("Post {} (id {}):\n{clean}\n", i + 1, p.id));
        if let Some((user, text)) = &p.quoted {
            let clean_quoted = crate::sanitize::sanitize_uncapped(text);
            let clean_user = crate::sanitize::sanitize_uncapped(user);
            body.push_str(&format!("Quoted by @{clean_user}: {clean_quoted}\n"));
        }
        body.push('\n');
    }
    if !quoted.is_empty() {
        body.push_str("Quoted posts:\n");
        for q in quoted {
            let clean = crate::sanitize::sanitize_uncapped(&q.text);
            body.push_str(&format!("- (id {}): {clean}\n", q.id));
        }
        body.push('\n');
    }
    if !linked.is_empty() {
        body.push_str("Linked posts:\n");
        for l in linked {
            let clean = crate::sanitize::sanitize_uncapped(&l.text);
            body.push_str(&format!("- (id {}): {clean}\n", l.id));
        }
        body.push('\n');
    }
    format!(
        "Instructions: walk through the public post thread below step by step for AncientOS, a personal agent OS \
         with a scheduler, approval gate, append-only decision log, privacy router, and agents \
         like a bookmarks curator. Reply in markdown with a numbered step-by-step walkthrough: \
         one numbered step per post in order, then one step per quoted/linked post explaining \
         how it connects. Keep each step to two sentences.\n\
         \nPublic thread (URLs and @handles stripped):\n\n{body}"
    )
}

/// Try-it appendix for both paid and local output: extracted practice steps
/// plus rendered media, only when the classifier fires. Empty otherwise.
fn try_it_appendix(joined: &str, media: &[crate::media::Media], is_try_it: bool) -> String {
    if !is_try_it {
        return String::new();
    }
    let mut out = String::from("\n## Steps\n");
    let steps = crate::try_it::extract_steps(joined);
    if steps.is_empty() {
        out.push_str("_No explicit steps found._\n");
    } else {
        for (i, step) in steps.iter().enumerate() {
            out.push_str(&format!("{}. {step}\n", i + 1));
        }
    }
    let media_md = crate::media::render_markdown(media);
    if !media_md.is_empty() {
        out.push('\n');
        out.push_str(&media_md);
    }
    out
}

/// Deterministic local fallback walkthrough with fixed sections.
#[allow(clippy::too_many_arguments)]
fn local_walkthrough(
    post_id: &str,
    username: Option<&str>,
    thread: &[ThreadPost],
    quoted: &[ThreadPost],
    linked: &[ThreadPost],
    joined: &str,
    media: &[crate::media::Media],
    is_try_it: bool,
) -> String {
    let url = crate::delegate::post_url(post_id, username);
    let mut out = format!("# Go deeper — {url}\n\n## Source\n- Post: {url}\n");
    out.push_str(&format!(
        "- Thread posts: {} · Quoted: {} · Linked: {}\n\n",
        thread.len(),
        quoted.len() + thread.iter().filter(|p| p.quoted.is_some()).count(),
        linked.len()
    ));
    out.push_str("## Thread summary per post\n");
    for (i, p) in thread.iter().enumerate() {
        let clean = crate::sanitize::sanitize_uncapped(&p.text);
        out.push_str(&format!("### {}. {}\n{clean}\n", i + 1, p.id));
        if let Some((user, text)) = &p.quoted {
            let clean_user = crate::sanitize::sanitize_uncapped(user);
            let clean_quoted = crate::sanitize::sanitize_uncapped(text);
            out.push_str(&format!("Quoted by @{clean_user}: {clean_quoted}\n"));
        }
        out.push('\n');
    }
    out.push_str("## Quoted/linked posts\n");
    if quoted.is_empty() && linked.is_empty() {
        out.push_str("(none)\n\n");
    } else {
        for q in quoted {
            let clean = crate::sanitize::sanitize_uncapped(&q.text);
            out.push_str(&format!("### Quoted {}\n{clean}\n\n", q.id));
        }
        for l in linked {
            let clean = crate::sanitize::sanitize_uncapped(&l.text);
            out.push_str(&format!("### Linked {}\n{clean}\n\n", l.id));
        }
    }
    if is_try_it {
        out.push_str(&try_it_appendix(joined, media, true));
    } else {
        out.push_str("## Steps\n");
        for (i, p) in thread.iter().enumerate() {
            out.push_str(&format!(
                "{}. Re-read post {} and write down its one key move.\n",
                i + 1,
                p.id
            ));
        }
        for (j, q) in quoted.iter().chain(linked.iter()).enumerate() {
            out.push_str(&format!(
                "{}. Check post {} for context on the step above.\n",
                thread.len() + j + 1,
                q.id
            ));
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Gathered context shared by online and offline runs: the thread, the root
/// author's username when known, quoted posts, linked posts, and any media
/// attached to those posts (fixture rows only; the thread API carries none).
type Context = (
    Vec<ThreadPost>,
    Option<String>,
    Vec<ThreadPost>,
    Vec<ThreadPost>,
    Vec<crate::media::Media>,
);

/// Online context: existing thread fetch, then one read-only lookup per
/// quoted/linked id. Any fetch error aborts with nothing written.
fn online_context(
    post_id: &str,
    fetch_thread: &ThreadFetcher<'_>,
    fetch_post: &PostFetcher<'_>,
) -> Result<Context, String> {
    let fetched = fetch_thread(post_id)?;
    if fetched.posts.is_empty() {
        return Err("thread fetch returned no posts".to_string());
    }
    let quoted = quoted_ids(&fetched.posts);
    let linked = linked_ids(&fetched.posts, &quoted);
    let mut quoted_posts = Vec::new();
    for id in &quoted {
        quoted_posts.push(fetch_post(id)?);
    }
    let mut linked_posts = Vec::new();
    for id in &linked {
        linked_posts.push(fetch_post(id)?);
    }
    Ok((
        fetched.posts,
        fetched.username,
        quoted_posts,
        linked_posts,
        Vec::new(),
    ))
}

/// One fixture row as a thread post. `referenced_tweets` come from a raw
/// array when present; `in_reply_to_post_id` becomes a `replied_to` entry.
fn thread_post_from_row(b: &Bookmark, v: &serde_json::Value) -> Result<ThreadPost, String> {
    let author = b
        .author_username
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("offline fixture row {} has no author", b.post_id))?;
    let mut refs = Vec::new();
    if let Some(arr) = v.get("referenced_tweets").and_then(|r| r.as_array()) {
        for r in arr {
            let kind = r.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let id = r.get("id").and_then(|x| x.as_str()).unwrap_or("");
            if !kind.is_empty() && !id.is_empty() {
                refs.push(ReferencedTweet {
                    kind: kind.to_string(),
                    id: id.to_string(),
                });
            }
        }
    }
    if let Some(reply) = v
        .get("in_reply_to_post_id")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
    {
        if !refs.iter().any(|r| r.kind == "replied_to" && r.id == reply) {
            refs.push(ReferencedTweet {
                kind: "replied_to".to_string(),
                id: reply.to_string(),
            });
        }
    }
    Ok(ThreadPost {
        id: b.post_id.clone(),
        author_id: author,
        text: b.text.clone(),
        created_at: b.created_at.clone(),
        conversation_id: v
            .get("conversation_id")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        referenced_tweets: refs,
        quoted: None,
    })
}

/// Offline context from a fixture JSONL (Bookmark rows): the root row by
/// post id, the thread as rows sharing its `conversation_id` (when present)
/// in `created_at` order, and quoted/linked rows by post id. A referenced
/// id missing from the fixture fails closed. No network.
fn offline_context(fixture_path: &Path, post_id: &str) -> Result<Context, String> {
    let text = std::fs::read_to_string(fixture_path).map_err(|e| {
        format!(
            "offline fixture read failed for {}: {e}",
            fixture_path.display()
        )
    })?;
    let mut rows: Vec<(Bookmark, serde_json::Value)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("offline fixture line {}: {e}", n + 1))?;
        let bookmark: Bookmark = serde_json::from_value(value.clone())
            .map_err(|e| format!("offline fixture line {}: {e}", n + 1))?;
        rows.push((bookmark, value));
    }
    let root_idx = rows
        .iter()
        .position(|(b, _)| b.post_id == post_id)
        .ok_or_else(|| format!("offline fixture has no post {post_id}"))?;
    let conv = rows[root_idx]
        .1
        .get("conversation_id")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut picked: Vec<usize> = match &conv {
        Some(c) => rows
            .iter()
            .enumerate()
            .filter(|(_, (_, v))| {
                v.get("conversation_id").and_then(|x| x.as_str()) == Some(c.as_str())
            })
            .map(|(i, _)| i)
            .collect(),
        None => vec![root_idx],
    };
    if !picked.contains(&root_idx) {
        picked.push(root_idx);
    }
    picked.sort_by(|&a, &b| {
        (
            rows[a].0.created_at.is_none(),
            rows[a].0.created_at.clone(),
            rows[a].0.post_id.clone(),
        )
            .cmp(&(
                rows[b].0.created_at.is_none(),
                rows[b].0.created_at.clone(),
                rows[b].0.post_id.clone(),
            ))
    });
    let by_id: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, (b, _))| (b.post_id.as_str(), i))
        .collect();
    let mut thread = Vec::new();
    let mut media = Vec::new();
    for i in &picked {
        let (b, v) = &rows[*i];
        thread.push(thread_post_from_row(b, v)?);
        media.extend(b.media.iter().cloned());
    }
    let quoted = quoted_ids(&thread);
    let linked = linked_ids(&thread, &quoted);
    let mut quoted_posts = Vec::new();
    for id in &quoted {
        let i = by_id
            .get(id.as_str())
            .ok_or_else(|| format!("offline fixture has no quoted post {id}"))?;
        let (b, v) = &rows[*i];
        quoted_posts.push(thread_post_from_row(b, v)?);
        media.extend(b.media.iter().cloned());
    }
    let mut linked_posts = Vec::new();
    for id in &linked {
        let i = by_id
            .get(id.as_str())
            .ok_or_else(|| format!("offline fixture has no linked post {id}"))?;
        let (b, v) = &rows[*i];
        linked_posts.push(thread_post_from_row(b, v)?);
        media.extend(b.media.iter().cloned());
    }
    let username = rows[root_idx].0.author_username.clone();
    Ok((thread, username, quoted_posts, linked_posts, media))
}

/// Same as [`run`] with injected thread/post fetchers, Muse caller, and
/// paths, so tests need no network or home directory. `offline_fixture`
/// selects fixture mode (always the local fallback); otherwise the thread
/// and quoted/linked posts are fetched read-only and one Muse call is made
/// when `allow_paid` is set. Any fetch error or empty model output fails
/// closed with nothing written.
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    post_id: &str,
    allow_paid: bool,
    offline_fixture: Option<&Path>,
    fetch_thread: &ThreadFetcher<'_>,
    fetch_post: &PostFetcher<'_>,
    call: &MuseCaller<'_>,
    log_path: &Path,
    out_dir: &Path,
) -> Result<DeeperResult, String> {
    validate_post_id(post_id)?;
    let (thread, username, quoted, linked, media) = match offline_fixture {
        Some(path) => offline_context(path, post_id)?,
        None => online_context(post_id, fetch_thread, fetch_post)?,
    };
    if thread.is_empty() {
        return Err("thread is empty".to_string());
    }
    let mut joined = String::new();
    for p in thread.iter().chain(quoted.iter()).chain(linked.iter()) {
        if !joined.is_empty() {
            joined.push_str("\n\n");
        }
        joined.push_str(&p.text);
    }
    // Probe the hands-on classifier with the full context text.
    let probe = Bookmark {
        post_id: post_id.to_string(),
        url: crate::delegate::post_url(post_id, username.as_deref()),
        text: joined.clone(),
        author_username: username.clone(),
        created_at: None,
        folders: None,
        label: None,
        had_note_tweet: false,
        media: media.clone(),
    };
    let is_try_it = crate::try_it::classify(&probe).is_some();
    let (output, model, usage, mode) = if allow_paid && offline_fixture.is_none() {
        let prompt = build_paid_prompt(&thread, &quoted, &linked);
        let (content, model, usage) = call(&prompt)?;
        if content.trim().is_empty() {
            return Err("model returned an empty walkthrough".to_string());
        }
        let mut out = format!("{}\n", content.trim_end());
        out.push_str(&try_it_appendix(&joined, &media, is_try_it));
        (out, model, usage, "paid".to_string())
    } else {
        let out = local_walkthrough(
            post_id,
            username.as_deref(),
            &thread,
            &quoted,
            &linked,
            &joined,
            &media,
            is_try_it,
        );
        (out, "local".to_string(), None, "local".to_string())
    };
    crate::token_store::ensure_dir_0700(out_dir).map_err(|e| format!("deeper dir failed: {e}"))?;
    let path = out_dir.join(format!("{post_id}.md"));
    crate::token_store::atomic_write_0600(&path, output.as_bytes())
        .map_err(|e| format!("deeper write failed: {e}"))?;
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    let (pt, ct, tt) = match &usage {
        Some(u) => (u.prompt_tokens, u.completion_tokens, u.total_tokens),
        None => (None, None, None),
    };
    // Metadata only here: never post text or the walkthrough.
    log.append(
        DEEPER_DONE_KIND,
        serde_json::json!({
            "post_id": post_id,
            "mode": mode,
            "path": path.to_string_lossy(),
            "thread_posts": thread.len(),
            "quoted_posts": quoted.len() + thread.iter().filter(|p| p.quoted.is_some()).count(),
            "linked_posts": linked.len(),
            "model": model,
            "prompt_tokens": pt,
            "completion_tokens": ct,
            "total_tokens": tt,
        }),
    )
    .map_err(|e| e.to_string())?;
    Ok(DeeperResult {
        output,
        model,
        usage,
        mode,
        thread_posts: thread.len(),
        quoted_posts: quoted.len(),
        linked_posts: linked.len(),
        path,
    })
}

/// Production run: real thread/post fetches plus one real Muse Spark call
/// (paid switch on, online only), paths from the data directory.
/// `offline` selects fixture mode with the local fallback.
pub fn run(post_id: &str, offline: Option<&str>) -> Result<DeeperResult, String> {
    let allow_paid = std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false);
    let client = ancientos_router::muse::MuseClient::from_env()
        .with_max_completion_tokens(DEEPER_MAX_COMPLETION_TOKENS)
        .with_reasoning_effort(DEEPER_REASONING_EFFORT);
    let call = |prompt: &str| {
        client.chat(prompt).map_err(|e| e.to_string()).map(|r| {
            let model = if r.model.is_empty() {
                client.model().to_string()
            } else {
                r.model
            };
            (r.content, model, r.usage)
        })
    };
    run_with(
        post_id,
        allow_paid,
        offline.map(Path::new),
        &crate::x_api::fetch_thread,
        &crate::x_api::fetch_post,
        &call,
        &crate::brief::decision_log_path()?,
        &deeper_dir()?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(id: &str, text: &str) -> ThreadPost {
        ThreadPost {
            id: id.into(),
            author_id: "u1".into(),
            text: text.into(),
            created_at: None,
            conversation_id: Some("100".into()),
            referenced_tweets: Vec::new(),
            quoted: None,
        }
    }

    fn quoted_post(id: &str, text: &str, quote_id: &str) -> ThreadPost {
        let mut p = post(id, text);
        p.referenced_tweets.push(ReferencedTweet {
            kind: "quoted".into(),
            id: quote_id.into(),
        });
        p
    }

    fn fetch(posts: Vec<ThreadPost>) -> crate::x_api::ThreadFetch {
        crate::x_api::ThreadFetch {
            root: posts[0].clone(),
            username: Some("syn_author".into()),
            posts,
            search_ok: true,
            search_note: None,
        }
    }

    fn write_fixture(dir: &tempfile::TempDir, name: &str, lines: &[serde_json::Value]) -> PathBuf {
        let path = dir.path().join(name);
        let body = lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, body).unwrap();
        path
    }

    fn row(
        id: &str,
        text: &str,
        author: &str,
        created: &str,
        conv: &str,
        extra: serde_json::Value,
    ) -> serde_json::Value {
        let mut v = serde_json::json!({
            "post_id": id,
            "url": format!("https://example.invalid/{author}/status/{id}"),
            "text": text,
            "author_username": author,
            "created_at": created,
            "conversation_id": conv,
        });
        if let Some(obj) = extra.as_object() {
            for (k, val) in obj {
                v[k] = val.clone();
            }
        }
        v
    }

    fn no_thread_fetch(_: &str) -> Result<crate::x_api::ThreadFetch, String> {
        panic!("network thread fetch used in offline mode")
    }

    fn no_post_fetch(_: &str) -> Result<ThreadPost, String> {
        panic!("network post fetch used in offline mode")
    }

    fn no_call(_: &str) -> Result<(String, String, Option<MuseUsage>), String> {
        panic!("model call used in local mode")
    }

    fn thread_fixture() -> Vec<serde_json::Value> {
        vec![
            row(
                "100",
                "[synthetic] start, see https://x.com/syn_other/status/102 for background",
                "syn_author",
                "2026-10-20T14:00:00+00:00",
                "100",
                serde_json::json!({"referenced_tweets": [{"type": "quoted", "id": "101"}]}),
            ),
            row(
                "104",
                "[synthetic] reply one",
                "syn_author",
                "2026-10-20T14:01:00+00:00",
                "100",
                serde_json::json!({"in_reply_to_post_id": "100"}),
            ),
            row(
                "101",
                "[synthetic] quoted context",
                "syn_quoted",
                "2026-10-19T10:00:00+00:00",
                "101",
                serde_json::json!({}),
            ),
            row(
                "102",
                "[synthetic] linked background",
                "syn_linked",
                "2026-10-19T11:00:00+00:00",
                "102",
                serde_json::json!({}),
            ),
        ]
    }

    #[test]
    fn linked_ids_find_status_urls() {
        assert!(extract_linked_ids("[synthetic] no links here").is_empty());
        assert_eq!(
            extract_linked_ids("[synthetic] see https://x.com/syn/status/123 and https://twitter.com/syn/status/456"),
            vec!["123".to_string(), "456".to_string()]
        );
        // Own example.invalid fixture URLs never match; duplicates collapse.
        assert_eq!(
            extract_linked_ids("https://x.com/syn/status/7, https://x.com/syn/status/7"),
            vec!["7".to_string()]
        );
        assert!(extract_linked_ids("https://example.invalid/syn/status/9").is_empty());
    }

    #[test]
    fn local_fallback_is_deterministic_with_all_sections() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir, "t.jsonl", &thread_fixture());
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let run_once = || {
            run_with(
                "100",
                false,
                Some(&fixture),
                &no_thread_fetch,
                &no_post_fetch,
                &no_call,
                &log,
                &out_dir,
            )
            .unwrap()
        };
        let first = run_once();
        let second = run_once();
        assert_eq!(first.output, second.output, "fallback is deterministic");
        for section in [
            "## Source",
            "## Thread summary per post",
            "## Quoted/linked posts",
            "## Steps",
        ] {
            assert!(first.output.contains(section), "{section} missing");
        }
        assert_eq!(first.mode, "local");
        assert_eq!(first.model, "local");
    }

    #[test]
    fn offline_collects_thread_quoted_and_linked() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir, "t.jsonl", &thread_fixture());
        let res = run_with(
            "100",
            false,
            Some(&fixture),
            &no_thread_fetch,
            &no_post_fetch,
            &no_call,
            &dir.path().join("d.jsonl"),
            &dir.path().join("deeper"),
        )
        .unwrap();
        assert_eq!(res.thread_posts, 2);
        assert_eq!(res.quoted_posts, 1);
        assert_eq!(res.linked_posts, 1);
        assert!(res.output.contains("[synthetic] start"), "{}", res.output);
        assert!(
            res.output.contains("[synthetic] reply one"),
            "{}",
            res.output
        );
        assert!(
            res.output.contains("[synthetic] quoted context"),
            "{}",
            res.output
        );
        assert!(
            res.output.contains("[synthetic] linked background"),
            "{}",
            res.output
        );
        let body = std::fs::read_to_string(&res.path).unwrap();
        assert_eq!(body, res.output);
    }

    #[test]
    fn try_it_includes_steps_and_media() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(
            &dir,
            "t.jsonl",
            &[row(
                "200",
                "[synthetic] Desk stretch: Try it now. 1) roll shoulders 10x 2) neck tilt 20s each side 3) seated twist 30s each side.",
                "syn_mobility",
                "2026-10-21T15:00:00+00:00",
                "200",
                serde_json::json!({"media": [{"type": "photo", "url": "https://pbs.twimg.com/media/synthetic_stretch.jpg", "alt_text": "[synthetic] seated twist"}]}),
            )],
        );
        let res = run_with(
            "200",
            false,
            Some(&fixture),
            &no_thread_fetch,
            &no_post_fetch,
            &no_call,
            &dir.path().join("d.jsonl"),
            &dir.path().join("deeper"),
        )
        .unwrap();
        assert!(res.output.contains("## Steps"), "{}", res.output);
        assert!(res.output.contains("roll shoulders"), "{}", res.output);
        assert!(res.output.contains("**Media**"), "{}", res.output);
        assert!(
            res.output
                .contains("https://pbs.twimg.com/media/synthetic_stretch.jpg"),
            "{}",
            res.output
        );
    }

    #[test]
    fn bad_id_rejected_before_any_write() {
        for bad in ["", "abc", "12 3", "../100", "100/", "10\n0", "0x12"] {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("d.jsonl");
            let out_dir = dir.path().join("deeper");
            let missing = dir.path().join("missing.jsonl");
            let err = run_with(
                bad,
                false,
                Some(missing.as_path()),
                &no_thread_fetch,
                &no_post_fetch,
                &no_call,
                &log,
                &out_dir,
            )
            .unwrap_err();
            assert!(err.contains("digits only"), "{bad}: {err}");
            assert!(!log.exists(), "{bad}: log written");
            assert!(!out_dir.exists(), "{bad}: output dir created");
        }
    }

    #[test]
    fn fetch_error_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_fail = |_: &str| -> Result<crate::x_api::ThreadFetch, String> {
            Err("synthetic fetch boom".to_string())
        };
        let err = run_with(
            "100",
            false,
            None,
            &fetch_fail,
            &no_post_fetch,
            &no_call,
            &log,
            &out_dir,
        )
        .unwrap_err();
        assert!(err.contains("synthetic fetch boom"), "{err}");
        assert!(!log.exists(), "log written on fetch error");
        assert!(
            !out_dir.join("100.md").exists(),
            "file written on fetch error"
        );
    }

    #[test]
    fn quoted_fetch_error_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| Ok(fetch(vec![quoted_post("100", "[synthetic] root", "101")]));
        let post_fail =
            |_: &str| -> Result<ThreadPost, String> { Err("synthetic quoted boom".to_string()) };
        let err = run_with(
            "100", false, None, &fetch_ok, &post_fail, &no_call, &log, &out_dir,
        )
        .unwrap_err();
        assert!(err.contains("synthetic quoted boom"), "{err}");
        assert!(!log.exists(), "log written on quoted fetch error");
        assert!(
            !out_dir.join("100.md").exists(),
            "file written on quoted error"
        );
    }

    #[test]
    fn paid_path_uses_caller_and_logs_usage() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| {
            Ok(fetch(vec![
                quoted_post(
                    "300",
                    "[synthetic] Try it now: breathe in 4s, hold 4s, exhale 4s.",
                    "301",
                ),
                post("302", "[synthetic] follow-up step"),
            ]))
        };
        let post_ok = |id: &str| -> Result<ThreadPost, String> {
            assert_eq!(id, "301");
            Ok(post("301", "[synthetic] quoted routine"))
        };
        let seen = std::cell::RefCell::new(Vec::new());
        let call_ok = |prompt: &str| -> Result<(String, String, Option<MuseUsage>), String> {
            seen.borrow_mut().push(prompt.to_string());
            assert!(prompt.contains("step-by-step"), "{prompt}");
            assert!(prompt.contains("[synthetic] Try it now"), "{prompt}");
            assert!(!prompt.contains("@syn"), "{prompt}");
            Ok((
                "## Walkthrough\n1. [synthetic] paid step one".into(),
                "fake-spark".into(),
                Some(MuseUsage {
                    prompt_tokens: Some(40),
                    completion_tokens: Some(20),
                    total_tokens: Some(60),
                }),
            ))
        };
        let res = run_with(
            "300", true, None, &fetch_ok, &post_ok, &call_ok, &log, &out_dir,
        )
        .unwrap();
        assert_eq!(seen.borrow().len(), 1, "exactly one Muse call");
        assert_eq!(res.mode, "paid");
        assert_eq!(res.model, "fake-spark");
        assert!(res.output.contains("paid step one"), "{}", res.output);
        assert!(res.output.contains("## Steps"), "{}", res.output);
        assert_eq!(res.thread_posts, 2);
        assert_eq!(res.quoted_posts, 1);
        let log_file = DecisionLog::open(&log).unwrap();
        let entries = log_file.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, DEEPER_DONE_KIND);
        assert_eq!(entries[0].payload["post_id"], "300");
        assert_eq!(entries[0].payload["mode"], "paid");
        assert_eq!(entries[0].payload["total_tokens"], 60);
        assert_eq!(entries[0].payload["thread_posts"], 2);
        assert_eq!(entries[0].payload["quoted_posts"], 1);
        let logged = entries[0].payload.to_string();
        assert!(!logged.contains("[synthetic]"), "{logged}");
        assert!(!logged.contains("paid step one"), "{logged}");
    }

    #[test]
    fn empty_model_output_errors_with_nothing_written() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| Ok(fetch(vec![post("400", "[synthetic] root")]));
        let call_empty = |_: &str| Ok(("   \n ".to_string(), "m".to_string(), None));
        let err = run_with(
            "400",
            true,
            None,
            &fetch_ok,
            &no_post_fetch,
            &call_empty,
            &log,
            &out_dir,
        )
        .unwrap_err();
        assert!(err.contains("empty walkthrough"), "{err}");
        assert!(!log.exists(), "log written on empty model output");
        assert!(
            !out_dir.join("400.md").exists(),
            "file written on empty output"
        );
    }

    #[test]
    fn decision_log_entry_appended_for_local_run() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sub").join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fixture = write_fixture(&dir, "t.jsonl", &thread_fixture());
        let res = run_with(
            "100",
            false,
            Some(&fixture),
            &no_thread_fetch,
            &no_post_fetch,
            &no_call,
            &log,
            &out_dir,
        )
        .unwrap();
        let log_file = DecisionLog::open(&log).unwrap();
        let entries = log_file.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, DEEPER_DONE_KIND);
        let p = &entries[0].payload;
        assert_eq!(p["post_id"], "100");
        assert_eq!(p["mode"], "local");
        assert_eq!(p["thread_posts"], 2);
        assert_eq!(p["quoted_posts"], 1);
        assert_eq!(p["linked_posts"], 1);
        assert_eq!(p["model"], "local");
        assert_eq!(p["prompt_tokens"], serde_json::Value::Null);
        assert_eq!(
            p["path"],
            serde_json::Value::String(res.path.to_string_lossy().into_owned())
        );
        assert!(res.path.ends_with("100.md"));
    }

    #[cfg(unix)]
    #[test]
    fn output_file_is_0600_and_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir, "t.jsonl", &thread_fixture());
        let out_dir = dir.path().join("nested").join(DEEPER_DIR_NAME);
        let res = run_with(
            "100",
            false,
            Some(&fixture),
            &no_thread_fetch,
            &no_post_fetch,
            &no_call,
            &dir.path().join("d.jsonl"),
            &out_dir,
        )
        .unwrap();
        let file_mode = std::fs::metadata(&res.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file mode is {file_mode:o}");
        let dir_mode = std::fs::metadata(&out_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "dir mode is {dir_mode:o}");
    }
}
