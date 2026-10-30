//! `deeper` command ("go deeper"): expand one post's thread plus its quoted
//! and linked posts, then write a step-by-step walkthrough to
//! `deeper/<post_id>.md`.
//!
//! Read-only on the network side: the thread comes from the existing thread
//! fetch and quoted/linked posts are looked up one by one through an injected
//! fetcher. Any fetch error fails closed (no file, no log entry). Offline
//! mode builds the same context from a fixture JSONL with no network. At
//! most two Muse Spark calls (first try plus one continuation on
//! `finish_reason` "length", paid switch on and online only); otherwise a
//! deterministic local fallback walkthrough. Only metadata enters the
//! decision log — never post text or walkthrough output.
use crate::delegate::ThreadFetcher;
use crate::fixture::Bookmark;
use crate::unroll::{ReferencedTweet, ThreadPost};
use ancientos_core::log::{DecisionLog, Entry};
use ancientos_router::data_class::{check_egress, max_of, DataClass, Route};
use ancientos_router::muse::MuseUsage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DEEPER_DONE_KIND: &str = "deeper.write";
/// Log kind appended by [`queue_later`]: `{post_id, at, queued_at}`.
pub const DEEPER_QUEUED_KIND: &str = "deeper.queued";
/// Log kind appended when a queued job finishes: `{post_id, at, mode, done_at}`.
pub const DEEPER_QUEUE_DONE_KIND: &str = "deeper.done";
/// Log kind appended when a queued paid job is proposed for approval:
/// `{post_id, at, approval_id, proposed_at}`.
pub const DEEPER_PROPOSED_KIND: &str = "deeper.proposed";
pub const DEEPER_DIR_NAME: &str = "deeper";
pub const DEEPER_MAX_COMPLETION_TOKENS: u32 = 3000;
pub const DEEPER_REASONING_EFFORT: &str = "low";
/// User instruction for the single continuation request on truncation.
pub const DEEPER_CONTINUE_PROMPT: &str = "Continue exactly where you stopped.";
/// Marker appended when output is still truncated after the continuation.
pub const DEEPER_TRUNCATED_MARKER: &str = "[truncated]";

/// One-post lookup: a post id -> the fetched post. Read-only; any error
/// fails the whole run closed.
pub type PostFetcher<'a> = dyn Fn(&str) -> Result<ThreadPost, String> + 'a;

/// Deeper Muse caller: `(prompt, prior_content)` -> `(content, model, usage,
/// finish_reason)`. `prior_content` is `None` for the first call and
/// `Some(prior)` for the single continuation on `finish_reason` "length".
/// Production sends the continuation as user prompt + assistant prior +
/// user [`DEEPER_CONTINUE_PROMPT`]; tests inject canned responses.
pub type DeeperMuseCaller<'a> = dyn Fn(&str, Option<&str>) -> Result<(String, String, Option<MuseUsage>, Option<String>), String>
    + 'a;

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

/// Queue state of one go-deeper job, folded from the decision log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueStatus {
    /// Queued, not yet proposed or done.
    Queued,
    /// A paid approval was proposed for this job (still runs only after
    /// the human approves and the daemon releases it).
    Proposed,
    /// Finished (local fallback ran, or a released paid walkthrough ran).
    Done,
}

impl QueueStatus {
    /// Stable string form of the queue status, as written to the log.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Proposed => "proposed",
            Self::Done => "done",
        }
    }
}

/// One queued go-deeper job: `post_id` to walk through at unix-secs `at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJob {
    pub post_id: String,
    pub at: u64,
    pub queued_at: u64,
    pub status: QueueStatus,
    pub approval_id: Option<String>,
}

/// Queue a go-deeper walkthrough for later: validates the post id and
/// appends [`DEEPER_QUEUED_KIND`] (`{post_id, at, queued_at}`, metadata
/// only). Times in the past (`at < now`) are refused; callers parse
/// RFC 3339 with [`crate::idea_card::parse_rfc3339`] and refuse
/// unparseable input before calling here.
pub fn queue_later(
    log: &mut DecisionLog,
    post_id: &str,
    at: u64,
    now: u64,
) -> Result<Entry, String> {
    validate_post_id(post_id)?;
    if at < now {
        return Err("cannot queue a go-deeper job in the past".to_string());
    }
    log.append(
        DEEPER_QUEUED_KIND,
        serde_json::json!({"post_id": post_id, "at": at, "queued_at": now}),
    )
    .map_err(|e| e.to_string())
}

/// Queue a job in the decision log at `log_path` (creating parent
/// directories), for the `deeper --later` CLI command and the web UI.
pub fn queue_later_at_path(
    post_id: &str,
    at: u64,
    now: u64,
    log_path: &Path,
) -> Result<Entry, String> {
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = DecisionLog::open_with_key_env(log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    queue_later(&mut log, post_id, at, now)
}

/// Dedup key for a queued deeper request: (post id, queued-at time).
fn queued_key(post_id: &str, at: u64) -> (String, u64) {
    (post_id.to_string(), at)
}

/// Fold [`DEEPER_QUEUED_KIND`] / [`DEEPER_PROPOSED_KIND`] /
/// [`DEEPER_QUEUE_DONE_KIND`] entries into jobs keyed by `(post_id, at)`,
/// sorted by `(at, post_id)`. First valid entry wins per key transition;
/// malformed entries and proposed/done entries for unknown jobs are
/// ignored (fail closed: they prove no queue state).
pub fn list_queue(entries: &[Entry]) -> Vec<QueuedJob> {
    let mut jobs: std::collections::BTreeMap<(String, u64), QueuedJob> =
        std::collections::BTreeMap::new();
    for e in entries {
        match e.kind.as_str() {
            DEEPER_QUEUED_KIND => {
                let (Some(post_id), Some(at), Some(queued_at)) = (
                    e.payload.get("post_id").and_then(|v| v.as_str()),
                    e.payload.get("at").and_then(|v| v.as_u64()),
                    e.payload.get("queued_at").and_then(|v| v.as_u64()),
                ) else {
                    continue;
                };
                if post_id.is_empty() || validate_post_id(post_id).is_err() {
                    continue;
                }
                jobs.entry(queued_key(post_id, at)).or_insert(QueuedJob {
                    post_id: post_id.to_string(),
                    at,
                    queued_at,
                    status: QueueStatus::Queued,
                    approval_id: None,
                });
            }
            DEEPER_PROPOSED_KIND => {
                let (Some(post_id), Some(at), Some(approval_id)) = (
                    e.payload.get("post_id").and_then(|v| v.as_str()),
                    e.payload.get("at").and_then(|v| v.as_u64()),
                    e.payload.get("approval_id").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                if let Some(job) = jobs.get_mut(&queued_key(post_id, at)) {
                    if job.status == QueueStatus::Queued && !approval_id.is_empty() {
                        job.status = QueueStatus::Proposed;
                        job.approval_id = Some(approval_id.to_string());
                    }
                }
            }
            DEEPER_QUEUE_DONE_KIND => {
                let (Some(post_id), Some(at)) = (
                    e.payload.get("post_id").and_then(|v| v.as_str()),
                    e.payload.get("at").and_then(|v| v.as_u64()),
                ) else {
                    continue;
                };
                if let Some(job) = jobs.get_mut(&queued_key(post_id, at)) {
                    job.status = QueueStatus::Done;
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<QueuedJob> = jobs.into_values().collect();
    out.sort_by(|a, b| (a.at, &a.post_id).cmp(&(b.at, &b.post_id)));
    out
}

/// Jobs whose time has come (`at <= now`) and are not done, sorted by
/// `(at, post_id)`. Proposed jobs are included: they still need their
/// approval claimed, released and executed.
pub fn due_jobs(entries: &[Entry], now: u64) -> Vec<QueuedJob> {
    list_queue(entries)
        .into_iter()
        .filter(|j| j.status != QueueStatus::Done && j.at <= now)
        .collect()
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
         numbered imperative steps (\"1. Install X\", \"2. Create a page for Y\"), one per post in order, \
         then one per quoted/linked post explaining how it connects. Each step must be a single \
         imperative sentence grounded in the post text, no meta lines (no \"Here is...\", no summaries), \
         no URLs. State each step's action in your own words: do not repeat setup titles or headings \
         as steps. Do not use the word AncientOS inside the steps; after the steps add a short \
         \"## Mapping\" section (one or two lines) mapping the thread to AncientOS modules. \
         Keep each step to two sentences.\n\
         \nPublic thread (URLs and @handles stripped):\n\n{body}"
    )
}

/// Max chars for the derived `# title` line.
const MAX_TITLE_CHARS: usize = 60;

/// Short title from the first post's first non-empty line: sanitized,
/// truncated at a word boundary with `…`. Falls back to `Post <id>` when
/// the first post has no text. Pure.
pub fn short_title(thread: &[ThreadPost]) -> String {
    let first = thread.first();
    let raw_line = first
        .map(|p| {
            p.text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
        })
        .unwrap_or("");
    let clean = crate::sanitize::sanitize_uncapped(raw_line);
    let clean = clean.trim();
    if clean.is_empty() {
        return first
            .map(|p| format!("Post {}", p.id))
            .unwrap_or_else(|| "Untitled".to_string());
    }
    if clean.chars().count() <= MAX_TITLE_CHARS {
        return clean.to_string();
    }
    let mut out: String = clean.chars().take(MAX_TITLE_CHARS).collect();
    if let Some(pos) = out.rfind(' ') {
        out.truncate(pos);
    }
    out.push('…');
    out
}

/// Remove `https?://t.co/\S+` matches (case-sensitive, `/` plus at least
/// one non-whitespace char required, like the regex). Pure.
fn strip_tco_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let prefix_len = if rest.starts_with("https://t.co/") {
            Some("https://t.co/".len())
        } else if rest.starts_with("http://t.co/") {
            Some("http://t.co/".len())
        } else {
            None
        };
        if let Some(n) = prefix_len {
            let after = i + n;
            let next_is_content = s[after..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace());
            if next_is_content {
                let mut j = after;
                while j < s.len() {
                    let c = s[j..].chars().next().expect("char boundary");
                    if c.is_whitespace() {
                        break;
                    }
                    j += c.len_utf8();
                }
                i = j;
                continue;
            }
        }
        let c = s[i..].chars().next().expect("char boundary");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Convert `[text](url)` and `![alt](url)` to their inner text, dropping the
/// URL. Unmatched brackets pass through. Pure.
fn strip_markdown_links(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let bang = chars[i] == '!' && i + 1 < chars.len() && chars[i + 1] == '[';
        let bracket = chars[i] == '[';
        if bang || bracket {
            let open = if bang { i + 1 } else { i };
            let close = chars[open + 1..]
                .iter()
                .position(|&c| c == ']')
                .map(|p| open + 1 + p);
            if let Some(close) = close {
                if close + 1 < chars.len() && chars[close + 1] == '(' {
                    let paren = chars[close + 2..]
                        .iter()
                        .position(|&c| c == ')')
                        .map(|p| close + 2 + p);
                    if let Some(end) = paren {
                        out.extend(chars[open + 1..close].iter());
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Strip leading `>` blockquote markers (repeated) plus surrounding space.
/// Only the line start is affected; mid-line `>` is kept. Pure.
fn strip_quote_markers(s: &str) -> String {
    let mut t = s.trim_start();
    while let Some(rest) = t.strip_prefix('>') {
        t = rest.trim_start();
    }
    t.to_string()
}

/// Clean one step line for the Steps section: reduce markdown
/// links/images to their text first (so link text survives), then drop
/// standalone t.co links, strip leading quote markers, and collapse
/// whitespace. Pure.
fn clean_step_text(s: &str) -> String {
    let no_md = strip_markdown_links(s);
    let no_tco = strip_tco_links(&no_md);
    let no_quote = strip_quote_markers(&no_tco);
    no_quote.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip all `http(s)://…` and `www.…` tokens. Pure.
fn strip_all_urls(s: &str) -> String {
    s.split_whitespace()
        .filter(|t| {
            let lower = t.to_lowercase();
            let trimmed = lower.trim_start_matches(|c: char| {
                c == '(' || c == '[' || c == '{' || c == '"' || c == '\'' || c == '<'
            });
            !(trimmed.starts_with("http://")
                || trimmed.starts_with("https://")
                || trimmed.starts_with("www."))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sequence words stripped when converting to imperative form.
const IMPERATIVE_STRIP_LEADERS: &[&str] = &[
    "first", "then", "next", "finally", "lastly", "also", "just", "now", "please",
];
/// Pronouns stripped with an optional following modal ("I can …", "you should …").
const IMPERATIVE_STRIP_PRONOUNS: &[&str] = &["i", "we", "you", "he", "she", "they", "it"];
const IMPERATIVE_STRIP_MODALS: &[&str] = &[
    "can", "could", "should", "will", "would", "may", "might", "must", "shall",
];

/// Trim leading separators left after stripping a word.
fn trim_seps(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ':')
        .trim_start()
}

/// Convert a step candidate to imperative form: strip a leading
/// `[synthetic]` marker, sequence words ("then", "next", …), pronouns with
/// an optional modal ("I can", "you should"), and "please"; capitalize the
/// verb. Pure.
fn to_imperative_form(s: &str) -> String {
    let mut rest = s.trim();
    if rest.len() >= 11 && rest[..11].eq_ignore_ascii_case("[synthetic]") {
        rest = rest[11..].trim_start();
    }
    loop {
        if rest.is_empty() {
            break;
        }
        let lower = rest.to_lowercase();
        if lower.starts_with("after that")
            && rest["after that".len()..]
                .starts_with(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ':')
        {
            rest = trim_seps(&rest["after that".len()..]);
            continue;
        }
        let first_end = rest
            .find(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ':')
            .unwrap_or(rest.len());
        let key = rest[..first_end]
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if IMPERATIVE_STRIP_LEADERS.contains(&key.as_str()) {
            rest = trim_seps(&rest[first_end..]);
            continue;
        }
        if IMPERATIVE_STRIP_PRONOUNS.contains(&key.as_str()) {
            let mut tail = trim_seps(&rest[first_end..]);
            let m_end = tail
                .find(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ':')
                .unwrap_or(tail.len());
            let m_key = tail[..m_end]
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            if IMPERATIVE_STRIP_MODALS.contains(&m_key.as_str()) {
                tail = trim_seps(&tail[m_end..]);
            }
            rest = tail;
            continue;
        }
        break;
    }
    let mut chars = rest.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// First words that never start an imperative step (pronouns, articles,
/// demonstratives, meta openers). Compared lowercased.
const NON_IMPERATIVE_STARTERS: &[&str] = &[
    "i", "we", "you", "he", "she", "it", "they", "this", "that", "these", "those", "the", "a",
    "an", "there", "here", "my", "our", "your", "his", "her", "its", "their", "in", "on", "at",
    "for", "with", "from", "about", "as", "if", "when", "while", "because", "overall", "summary",
    "note", "below", "above",
];

/// True when the cleaned step reads as an imperative how-to action: at
/// least two words, first word capitalized and not a pronoun/article/meta
/// starter, no URLs. Pure.
fn is_imperative_step(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    let lower_all = t.to_lowercase();
    if lower_all.contains("http://") || lower_all.contains("https://") || lower_all.contains("www.")
    {
        return false;
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.len() < 2 {
        return false;
    }
    let first = words[0].trim_matches(|c: char| !c.is_alphanumeric());
    if first.is_empty() || !first.chars().next().is_some_and(|c| c.is_uppercase()) {
        return false;
    }
    if NON_IMPERATIVE_STARTERS.contains(&first.to_lowercase().as_str()) {
        return false;
    }
    if lower_all.starts_with("here is")
        || lower_all.starts_with("here are")
        || lower_all.starts_with("in summary")
    {
        return false;
    }
    true
}

/// Clean, convert to imperative, and filter step candidates: drops empty,
/// URL-only, non-imperative, and duplicate (case-insensitive) lines.
/// Returns imperative-form steps in order. Pure.
fn filter_steps(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in raw {
        let cleaned = strip_all_urls(&clean_step_text(r));
        if cleaned.trim().is_empty() {
            continue;
        }
        let imp = to_imperative_form(&cleaned);
        if !is_imperative_step(&imp) {
            continue;
        }
        if seen.insert(imp.to_lowercase()) {
            out.push(imp);
        }
    }
    out
}

/// Filter numbered step lines in paid model output: drops non-imperative /
/// empty / URL-only / duplicate steps and renumbers survivors. Non-step
/// lines pass through unchanged. Pure.
fn filter_model_steps(content: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Per-line plan: None = pass through, Some(None) = dropped step,
    // Some(Some(idx)) = kept step with index into `kept`.
    let mut plan: Vec<Option<Option<usize>>> = Vec::new();
    for line in content.lines() {
        if let Some(body) = strip_step_marker(line).or_else(|| strip_bullet(line)) {
            let cleaned = strip_all_urls(&clean_step_text(body));
            let imp = to_imperative_form(&cleaned);
            if cleaned.trim().is_empty() || !is_imperative_step(&imp) {
                plan.push(Some(None));
                continue;
            }
            if seen.insert(imp.to_lowercase()) {
                kept.push(imp);
                plan.push(Some(Some(kept.len() - 1)));
            } else {
                plan.push(Some(None));
            }
        } else {
            plan.push(None);
        }
    }
    let mut out = String::new();
    let mut n = 0usize;
    for (line, p) in content.lines().zip(plan) {
        match p {
            None => {
                out.push_str(line);
                out.push('\n');
            }
            Some(None) => {}
            Some(Some(idx)) => {
                n += 1;
                out.push_str(&format!("{n}. {}\n", kept[idx]));
            }
        }
    }
    if content.ends_with('\n') || out.is_empty() {
        out
    } else {
        out.trim_end().to_string() + "\n"
    }
}

/// Try-it appendix for both paid and local output: extracted practice steps
/// (thread text only; quoted/linked posts never become steps) plus rendered
/// media, only when the classifier fires. Empty otherwise. Steps are plain
/// numbered imperative lines with no URLs, markdown links, or quote markers.
fn try_it_appendix(thread_joined: &str, media: &[crate::media::Media], is_try_it: bool) -> String {
    if !is_try_it {
        return String::new();
    }
    let mut out = String::from("\n## Steps\n");
    let candidates: Vec<String> = crate::try_it::extract_steps(thread_joined);
    let steps = filter_steps(&candidates);
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

/// Max items per fallback section; thread order wins, deterministic.
const MAX_FALLBACK_ITEMS: usize = 8;
/// Max characters per emitted fallback item (suffix-truncated).
const MAX_FALLBACK_ITEM_CHARS: usize = 280;

/// Collapse whitespace; truncate suffix-only to [`MAX_FALLBACK_ITEM_CHARS`].
/// Pure, deterministic.
fn truncate_item(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_FALLBACK_ITEM_CHARS {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(MAX_FALLBACK_ITEM_CHARS).collect();
    out.push('…');
    out
}

/// Split text into sentences on `.`/`!`/`?`/`;`/newlines. Empty fragments
/// are dropped. Pure.
fn fallback_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        for part in line.split(['.', '!', '?', ';']) {
            let collapsed = part.split_whitespace().collect::<Vec<_>>().join(" ");
            if collapsed.chars().count() >= 2 {
                out.push(collapsed);
            }
        }
    }
    out
}

/// True when lowercase `word` appears with non-alphanumeric boundaries in
/// lowercase `hay`. Pure.
fn contains_word(hay: &str, word: &str) -> bool {
    let h = hay.as_bytes();
    let p = word.as_bytes();
    if p.is_empty() || h.len() < p.len() {
        return false;
    }
    for i in 0..=(h.len() - p.len()) {
        if h[i..i + p.len()] == p[..] {
            let left_ok = i == 0 || !h[i - 1].is_ascii_alphanumeric();
            let right_ok = i + p.len() == h.len() || !h[i + p.len()].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

/// Assertion words marking a sentence as a claim (any digit also counts).
/// Whole-word, case-insensitive. Pure.
const CLAIM_WORDS: &[&str] = &[
    "is",
    "are",
    "was",
    "were",
    "has",
    "have",
    "had",
    "makes",
    "make",
    "made",
    "means",
    "mean",
    "shows",
    "show",
    "proves",
    "cuts",
    "cut",
    "reduces",
    "improves",
    "increases",
    "takes",
    "take",
    "needs",
    "need",
    "requires",
    "keeps",
    "keep",
    "drains",
    "covers",
    "will",
    "can",
    "should",
    "never",
    "always",
    "faster",
    "slower",
    "better",
    "half",
    "every",
];

/// True for sentences carrying an assertion or a number: at least four
/// words plus a digit or a whole-word assertion cue. Pure.
fn is_claim_sentence(sentence: &str) -> bool {
    if sentence.split_whitespace().count() < 4 {
        return false;
    }
    if sentence.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    let lower = sentence.to_lowercase();
    CLAIM_WORDS.iter().any(|w| contains_word(&lower, w))
}

/// `(post_id, sentence)` claims in thread order: assertion/number
/// sentences first, then one representative sentence per uncovered post so
/// every post is quoted in its own words. Deterministic, capped. Pure.
fn fallback_claims(thread: &[ThreadPost]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in thread {
        let clean = crate::sanitize::sanitize_uncapped(&p.text);
        for s in fallback_sentences(&clean) {
            if is_claim_sentence(&s) && seen.insert(s.clone()) {
                out.push((p.id.clone(), truncate_item(&s)));
                if out.len() >= MAX_FALLBACK_ITEMS {
                    return out;
                }
            }
        }
    }
    if out.len() < MAX_FALLBACK_ITEMS {
        let covered: std::collections::HashSet<String> =
            out.iter().map(|(id, _)| id.clone()).collect();
        for p in thread {
            if covered.contains(&p.id) {
                continue;
            }
            let clean = crate::sanitize::sanitize_uncapped(&p.text);
            if let Some(first) = fallback_sentences(&clean).into_iter().next() {
                if seen.insert(first.clone()) {
                    out.push((p.id.clone(), truncate_item(&first)));
                }
            }
            if out.len() >= MAX_FALLBACK_ITEMS {
                break;
            }
        }
    }
    out
}

/// Strip a leading `1) `/`1. `/`1: `/`(1) `/`Step 1: ` marker. `1/3`
/// counters, decimals, and clock times are not steps. Pure.
fn strip_step_marker(line: &str) -> Option<&str> {
    let mut rest = line.trim();
    if rest.len() >= 5 && rest[..5].eq_ignore_ascii_case("step ") {
        rest = rest[5..].trim_start();
    }
    if let Some(after) = rest.strip_prefix('(') {
        let close = after.find(')')?;
        if after[..close].is_empty() || !after[..close].chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        rest = after[close + 1..].trim_start();
        return if rest.is_empty() { None } else { Some(rest) };
    }
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits == rest.len() {
        return None;
    }
    let marker = rest.as_bytes()[digits];
    if !matches!(marker, b')' | b'.' | b':') {
        return None;
    }
    let tail = &rest[digits + 1..];
    if tail.starts_with(|c: char| c.is_ascii_digit()) {
        return None; // decimal (`v2.0`) or clock time (`12:30`)
    }
    let body = tail.trim_start();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

/// Strip a leading `- `/`* `/`• ` bullet. Pure.
fn strip_bullet(line: &str) -> Option<&str> {
    let t = line.trim_start();
    for marker in ["- ", "* ", "• "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return if rest.trim().is_empty() {
                None
            } else {
                Some(rest.trim())
            };
        }
    }
    None
}

/// Sequence words marking a sentence as a procedure step. Pure.
const ORDER_MARKERS: &[&str] = &["first", "then", "next", "finally", "after that", "lastly"];

/// True if lowercased text contains one of `ORDER_MARKERS` as a word.
fn has_order_marker(lower: &str) -> bool {
    ORDER_MARKERS.iter().any(|m| contains_word(lower, m))
}

/// `(post_id, step)` in thread order: numbered/bulleted lines first, then
/// imperative or first/then/next sentences. Thread posts only; quoted and
/// linked posts never become steps. Each step is cleaned plain imperative
/// text (no URLs, markdown links, or quote markers); non-imperative,
/// empty, URL-only, and duplicate candidates are dropped. Empty when
/// nothing actionable is found. Deterministic, capped. Pure.
fn fallback_steps(thread: &[ThreadPost]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |id: &str, raw: &str| {
        let cleaned = strip_all_urls(&clean_step_text(&truncate_item(raw)));
        if cleaned.trim().is_empty() {
            return;
        }
        let imp = to_imperative_form(&cleaned);
        if !is_imperative_step(&imp) {
            return;
        }
        if seen.insert(imp.to_lowercase()) && out.len() < MAX_FALLBACK_ITEMS {
            out.push((id.to_string(), imp));
        }
    };
    for p in thread {
        // Raw lines: sanitizing collapses newlines, so markers are found
        // first and each extracted item is sanitized on its own. Markdown
        // links reduce to their text before sanitizing so the text survives.
        for line in p.text.lines() {
            if let Some(body) = strip_step_marker(line).or_else(|| strip_bullet(line)) {
                let no_md = strip_markdown_links(body);
                push(&p.id, &crate::sanitize::sanitize_uncapped(&no_md));
            }
        }
    }
    for p in thread {
        let no_md = strip_markdown_links(&p.text);
        let clean = crate::sanitize::sanitize_uncapped(&no_md);
        for s in fallback_sentences(&clean) {
            let lower = s.to_lowercase();
            if crate::try_it::is_imperative_sentence(&s) || has_order_marker(&lower) {
                push(&p.id, &s);
            }
        }
    }
    out
}

/// `## Quoted & linked` section for the paid path: ids + truncated text.
fn quoted_linked_section(quoted: &[ThreadPost], linked: &[ThreadPost]) -> String {
    let mut out = String::from("\n## Quoted & linked\n");
    if quoted.is_empty() && linked.is_empty() {
        out.push_str("(none)\n");
    }
    for (kind, list) in [("quoted", quoted), ("linked", linked)] {
        for q in list {
            let clean = crate::sanitize::sanitize_uncapped(&q.text);
            out.push_str(&format!("- ({kind} {}) {}\n", q.id, truncate_item(&clean)));
        }
    }
    out
}

/// Deterministic local fallback walkthrough with fixed sections.
/// `thread_joined` is thread text only for the Steps section; quoted and
/// linked text appears only under Quoted sections, never as steps.
#[allow(clippy::too_many_arguments)]
fn local_walkthrough(
    post_id: &str,
    username: Option<&str>,
    thread: &[ThreadPost],
    quoted: &[ThreadPost],
    linked: &[ThreadPost],
    thread_joined: &str,
    media: &[crate::media::Media],
    is_try_it: bool,
) -> String {
    let url = crate::delegate::post_url(post_id, username);
    let title = short_title(thread);
    let mut out = format!("# {title}\n\n## Source\n- Post: {url}\n");
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
    if is_try_it {
        out.push_str("## Quoted & linked\n");
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
        out.push_str(&try_it_appendix(thread_joined, media, true));
    } else {
        // Non-Try-it threads: quote real thread content (claims, steps,
        // quoted/linked posts) with the post each item came from. No
        // generic placeholder lines. Steps are plain numbered lines.
        out.push_str("## Key claims\n");
        for (id, claim) in fallback_claims(thread) {
            out.push_str(&format!("- ({id}) {claim}\n"));
        }
        out.push('\n');
        let steps = fallback_steps(thread);
        if !steps.is_empty() {
            out.push_str("## Steps\n");
            for (i, (_, step)) in steps.iter().enumerate() {
                out.push_str(&format!("{}. {step}\n", i + 1));
            }
            out.push('\n');
        }
        out.push_str("## Quoted & linked\n");
        if quoted.is_empty() && linked.is_empty() {
            out.push_str("(none)\n\n");
        } else {
            for q in quoted {
                let clean = crate::sanitize::sanitize_uncapped(&q.text);
                out.push_str(&format!("- (quoted {}) {}\n", q.id, truncate_item(&clean)));
            }
            for l in linked {
                let clean = crate::sanitize::sanitize_uncapped(&l.text);
                out.push_str(&format!("- (linked {}) {}\n", l.id, truncate_item(&clean)));
            }
            out.push('\n');
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

/// Add two optional token counts; `None` + `None` stays `None`. Pure.
fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.saturating_add(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

/// Sum two usages field-wise for the first try plus its continuation.
/// Pure.
fn combine_usage(a: Option<MuseUsage>, b: Option<MuseUsage>) -> Option<MuseUsage> {
    match (a, b) {
        (None, None) => None,
        (Some(u), None) | (None, Some(u)) => Some(u),
        (Some(x), Some(y)) => Some(MuseUsage {
            prompt_tokens: add_opt(x.prompt_tokens, y.prompt_tokens),
            completion_tokens: add_opt(x.completion_tokens, y.completion_tokens),
            total_tokens: add_opt(x.total_tokens, y.total_tokens),
        }),
    }
}

/// Same as [`run`] with injected thread/post fetchers, Muse caller, and
/// paths, so tests need no network or home directory. `offline_fixture`
/// selects fixture mode (always the local fallback); otherwise the thread
/// and quoted/linked posts are fetched read-only and up to two Muse calls
/// are made when `allow_paid` is set (first try plus one continuation on
/// `finish_reason` "length"). Any fetch error, model error, or empty model
/// output fails closed with nothing written.
///
/// The paid path only runs for Public threads: content from the thread plus
/// quoted/linked posts takes the highest class, and anything above Public
/// is refused before any model call (logged as `router.egress_denied` when
/// `log` is given; allowed paid runs log `router.egress_allowed`). The local
/// fallback handles every class.
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    post_id: &str,
    allow_paid: bool,
    offline_fixture: Option<&Path>,
    fetch_thread: &ThreadFetcher<'_>,
    fetch_post: &PostFetcher<'_>,
    call: &DeeperMuseCaller<'_>,
    log_path: &Path,
    out_dir: &Path,
    log: Option<&mut DecisionLog>,
) -> Result<DeeperResult, String> {
    validate_post_id(post_id)?;
    let (thread, username, quoted, linked, media) = match offline_fixture {
        Some(path) => offline_context(path, post_id)?,
        None => online_context(post_id, fetch_thread, fetch_post)?,
    };
    if thread.is_empty() {
        return Err("thread is empty".to_string());
    }
    // Highest class across the thread plus quoted/linked posts (and inline
    // quoted text): the paid prompt below carries all of it.
    let class = {
        let mut classes = Vec::new();
        for p in thread.iter().chain(quoted.iter()).chain(linked.iter()) {
            classes.push(crate::translate::class_of(&p.text));
            if let Some((user, quoted_text)) = &p.quoted {
                classes.push(crate::translate::class_of(user));
                classes.push(crate::translate::class_of(quoted_text));
            }
        }
        max_of(classes)
    };
    let mut thread_joined = String::new();
    for p in thread.iter() {
        if !thread_joined.is_empty() {
            thread_joined.push_str("\n\n");
        }
        thread_joined.push_str(&p.text);
    }
    let mut full_joined = String::new();
    for p in thread.iter().chain(quoted.iter()).chain(linked.iter()) {
        if !full_joined.is_empty() {
            full_joined.push_str("\n\n");
        }
        full_joined.push_str(&p.text);
    }
    // Probe the hands-on classifier with the full context text. Steps use
    // thread text only so quoted/linked posts never become steps.
    let probe = Bookmark {
        quoted_text: None,
        x_bookmark_present: None,
        post_id: post_id.to_string(),
        url: crate::delegate::post_url(post_id, username.as_deref()),
        text: full_joined,
        author_username: username.clone(),
        created_at: None,
        folders: None,
        label: None,
        had_note_tweet: false,
        media: media.clone(),
        lang: None,
        urls: Vec::new(),
    };
    let is_try_it = crate::try_it::classify(&probe).is_some();
    let (output, model, usage, mode) = if allow_paid && offline_fixture.is_none() {
        check_egress(class, Route::MuseSpark, "deeper", log).map_err(|e| e.to_string())?;
        let prompt = build_paid_prompt(&thread, &quoted, &linked);
        let (first, model_first, usage_first, reason_first) = call(&prompt, None)?;
        if first.trim().is_empty() && reason_first.as_deref() != Some("length") {
            return Err("model returned an empty walkthrough".to_string());
        }
        let (content, model, usage) = if reason_first.as_deref() == Some("length") {
            let (second, model_second, usage_second, reason_second) = call(&prompt, Some(&first))?;
            let mut combined = format!("{first}{second}");
            if combined.trim().is_empty() {
                return Err("model returned an empty walkthrough".to_string());
            }
            if reason_second.as_deref() == Some("length") {
                combined = format!("{}\n\n{DEEPER_TRUNCATED_MARKER}", combined.trim_end());
            }
            let merged_model = if model_first.is_empty() {
                model_second
            } else {
                model_first
            };
            (
                combined,
                merged_model,
                combine_usage(usage_first, usage_second),
            )
        } else {
            (first, model_first, usage_first)
        };
        if content.trim().is_empty() {
            return Err("model returned an empty walkthrough".to_string());
        }
        let filtered = filter_model_steps(content.trim_end());
        if filtered.trim().is_empty() {
            return Err("model returned no usable steps after filtering".to_string());
        }
        let title = short_title(&thread);
        let mut out = format!("# {title}\n\n## Steps\n{}\n", filtered.trim_end());
        out.push_str(&quoted_linked_section(&quoted, &linked));
        // Model already wrote the Steps; the Try-it appendix adds practice
        // steps + media under its own heading (no duplicate Steps).
        out.push_str(
            &try_it_appendix(&thread_joined, &media, is_try_it).replacen(
                "\n## Steps\n",
                "\n## Try it\n",
                1,
            ),
        );
        (out, model, usage, "paid".to_string())
    } else {
        let out = local_walkthrough(
            post_id,
            username.as_deref(),
            &thread,
            &quoted,
            &linked,
            &thread_joined,
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
    let mut log = DecisionLog::open_with_key_env(log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
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

/// Build the production Muse caller for [`run_with`] from a configured
/// client: one user prompt first, then a single continuation (user prompt
/// plus assistant prior plus [`DEEPER_CONTINUE_PROMPT`]) when the first
/// reply truncates with `finish_reason` "length". The caller passes
/// [`DataClass::Public`]: only use it for prompts [`run_with`] already
/// classified (it refuses non-public threads before calling).
#[allow(clippy::type_complexity)]
pub fn production_muse_caller(
    client: &ancientos_router::muse::MuseClient,
) -> impl Fn(&str, Option<&str>) -> Result<(String, String, Option<MuseUsage>, Option<String>), String>
       + '_ {
    move |prompt: &str, prior: Option<&str>| {
        let resp = match prior {
            None => client.chat_with_class(DataClass::Public, "deeper", None, prompt),
            Some(prior_content) => {
                let messages = serde_json::json!([
                    {"role": "user", "content": prompt},
                    {"role": "assistant", "content": prior_content},
                    {"role": "user", "content": DEEPER_CONTINUE_PROMPT},
                ]);
                client.chat_messages_with_class(DataClass::Public, "deeper", None, messages)
            }
        }
        .map_err(|e| e.to_string())?;
        let model = if resp.model.is_empty() {
            client.model().to_string()
        } else {
            resp.model
        };
        Ok((resp.content, model, resp.usage, resp.finish_reason))
    }
}

/// Production run: real thread/post fetches plus up to two real Muse Spark
/// calls (first try plus one continuation on truncation, paid switch on,
/// online only), paths from the data directory. `offline` selects fixture
/// mode with the local fallback.
pub fn run(post_id: &str, offline: Option<&str>) -> Result<DeeperResult, String> {
    let allow_paid = std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false);
    let client = ancientos_router::muse::MuseClient::from_env()
        .with_max_completion_tokens(DEEPER_MAX_COMPLETION_TOKENS)
        .with_reasoning_effort(DEEPER_REASONING_EFFORT);
    let call = production_muse_caller(&client);
    let log_path = crate::brief::decision_log_path()?;
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = DecisionLog::open_with_key_env(&log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    run_with(
        post_id,
        allow_paid,
        offline.map(Path::new),
        &crate::x_api::fetch_thread,
        &crate::x_api::fetch_post,
        &call,
        &log_path,
        &deeper_dir()?,
        Some(&mut log),
    )
}

/// Online paid run with explicit paths for the daemon: fails closed unless
/// `ANCIENTOS_ALLOW_PAID=1`, fetches the thread read-only over the network
/// and makes up to two Muse Spark calls (first try plus one continuation).
/// The daemon calls this only after claiming and releasing a human approval
/// for the job's exact payload.
pub fn run_paid_with_paths(
    post_id: &str,
    log_path: &Path,
    out_dir: &Path,
) -> Result<DeeperResult, String> {
    let allow_paid = std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false);
    if !allow_paid {
        return Err("paid calls are disabled (set ANCIENTOS_ALLOW_PAID=1 to enable)".to_string());
    }
    let client = ancientos_router::muse::MuseClient::from_env()
        .with_max_completion_tokens(DEEPER_MAX_COMPLETION_TOKENS)
        .with_reasoning_effort(DEEPER_REASONING_EFFORT);
    let call = production_muse_caller(&client);
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = DecisionLog::open_with_key_env(log_path, &|key| std::env::var(key).ok())
        .map_err(|e| e.to_string())?;
    run_with(
        post_id,
        true,
        None,
        &crate::x_api::fetch_thread,
        &crate::x_api::fetch_post,
        &call,
        log_path,
        out_dir,
        Some(&mut log),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: builds a minimal thread post with the given id and text.
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

    /// Test helper: builds a thread post quoting another post.
    fn quoted_post(id: &str, text: &str, quote_id: &str) -> ThreadPost {
        let mut p = post(id, text);
        p.referenced_tweets.push(ReferencedTweet {
            kind: "quoted".into(),
            id: quote_id.into(),
        });
        p
    }

    /// Test helper: wraps posts as a successful synthetic thread result.
    fn fetch(posts: Vec<ThreadPost>) -> crate::x_api::ThreadFetch {
        crate::x_api::ThreadFetch {
            root: posts[0].clone(),
            username: Some("syn_author".into()),
            posts,
            search_ok: true,
            search_note: None,
        }
    }

    /// Test helper: writes JSON lines to a temp file and returns its path.
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

    /// Test helper: builds bookmark fixture JSON with merged extra fields.
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

    /// Test helper: panics if offline code attempts a network thread fetch.
    fn no_thread_fetch(_: &str) -> Result<crate::x_api::ThreadFetch, String> {
        panic!("network thread fetch used in offline mode")
    }

    /// Test helper: panics if offline code attempts a network post fetch.
    fn no_post_fetch(_: &str) -> Result<ThreadPost, String> {
        panic!("network post fetch used in offline mode")
    }

    /// Test helper: panics if local-mode code attempts a model call.
    fn no_call(
        _: &str,
        _: Option<&str>,
    ) -> Result<(String, String, Option<MuseUsage>, Option<String>), String> {
        panic!("model call used in local mode")
    }

    /// Test helper: returns quoted-conversation JSON rows for deeper tests.
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
                None,
            )
            .unwrap()
        };
        let first = run_once();
        let second = run_once();
        assert_eq!(first.output, second.output, "fallback is deterministic");
        for section in [
            "## Source",
            "## Thread summary per post",
            "## Key claims",
            "## Quoted & linked",
        ] {
            assert!(first.output.contains(section), "{section} missing");
        }
        // The plain thread fixture has no numbered/bulleted/imperative or
        // first/then/next steps, so the Steps section is omitted.
        assert!(
            !first.output.contains("## Steps"),
            "Steps must be omitted when none found:\n{}",
            first.output
        );
        for generic in [
            "write down its one key move",
            "for context on the step above",
        ] {
            assert!(
                !first.output.contains(generic),
                "generic line leaked: {generic}"
            );
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
            None,
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

    /// Rich non-Try-it thread: claims with numbers, numbered + sequenced
    /// steps, one quoted and one linked post.
    fn rich_thread_fixture() -> Vec<serde_json::Value> {
        vec![
            row(
                "demo-3001",
                "[synthetic] Kanban review: our queue drains 40% faster when agents pick cards in batches of 5.\n1) Batch size is 5 cards per run\n2) Friday audits cover 30 cards, so the log is the source of truth.\nFirst, the agent lists due cards.\nThen it writes results back as a comment.\nSee background https://x.com/demo_archivist/status/demo-3004 for the earlier measurement.",
                "demo_threadsmith",
                "2026-10-20T14:00:00+00:00",
                "demo-3001",
                serde_json::json!({"referenced_tweets": [{"type": "quoted", "id": "demo-3003"}]}),
            ),
            row(
                "demo-3002",
                "[synthetic] Friday follow-up: audits take time across 30 cards, so the log stays the source of truth.",
                "demo_threadsmith",
                "2026-10-20T14:01:00+00:00",
                "demo-3001",
                serde_json::json!({"in_reply_to_post_id": "demo-3001"}),
            ),
            row(
                "demo-3003",
                "[synthetic] Quoted context: shared logs cut review time by half across 12 teams.",
                "demo_archivist",
                "2026-10-19T10:00:00+00:00",
                "demo-3003",
                serde_json::json!({}),
            ),
            row(
                "demo-3004",
                "[synthetic] Linked background: batching in groups of 5 keeps latency under the weekly budget.",
                "demo_archivist",
                "2026-10-19T11:00:00+00:00",
                "demo-3004",
                serde_json::json!({}),
            ),
        ]
    }

    #[test]
    fn non_try_it_fallback_quotes_real_text_without_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir, "rich.jsonl", &rich_thread_fixture());
        let run_once = || {
            run_with(
                "demo-3001",
                false,
                Some(&fixture),
                &no_thread_fetch,
                &no_post_fetch,
                &no_call,
                &dir.path().join("d.jsonl"),
                &dir.path().join("deeper"),
                None,
            )
            .unwrap()
        };
        let res = run_once();
        assert_eq!(res.mode, "local");
        // Deterministic across runs.
        assert_eq!(res.output, run_once().output);
        for section in ["## Key claims", "## Steps", "## Quoted & linked"] {
            assert!(
                res.output.contains(section),
                "{section} missing:\n{}",
                res.output
            );
        }
        // Extracted thread content appears with the post it came from.
        for want in [
            "40% faster",
            "Batch size is 5 cards per run",
            "Friday audits cover 30 cards",
            "First, the agent lists due cards",
            "Then it writes results back as a comment",
            "demo-3001",
            "demo-3002",
        ] {
            assert!(res.output.contains(want), "{want} missing:\n{}", res.output);
        }
        // Quoted and linked posts appear with their ids.
        for want in [
            "(quoted demo-3003)",
            "cut review time by half across 12 teams",
            "(linked demo-3004)",
            "batching in groups of 5",
        ] {
            assert!(res.output.contains(want), "{want} missing:\n{}", res.output);
        }
        // No generic placeholder lines.
        for generic in [
            "Re-read post",
            "write down its one key move",
            "Check post",
            "for context on the step above",
        ] {
            assert!(
                !res.output.contains(generic),
                "generic line leaked: {generic}\n{}",
                res.output
            );
        }
    }

    #[test]
    fn non_try_it_fallback_lists_claims_per_post() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir, "rich.jsonl", &rich_thread_fixture());
        let res = run_with(
            "demo-3001",
            false,
            Some(&fixture),
            &no_thread_fetch,
            &no_post_fetch,
            &no_call,
            &dir.path().join("d.jsonl"),
            &dir.path().join("deeper"),
            None,
        )
        .unwrap();
        // Both thread posts contribute claim lines naming their own id.
        assert!(
            res.output.contains("- (demo-3001)"),
            "demo-3001 claim missing:\n{}",
            res.output
        );
        assert!(
            res.output.contains("- (demo-3002)"),
            "demo-3002 claim missing:\n{}",
            res.output
        );
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
            None,
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
                None,
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
            None,
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
            "100", false, None, &fetch_ok, &post_fail, &no_call, &log, &out_dir, None,
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
        let call_ok =
            |prompt: &str,
             prior: Option<&str>|
             -> Result<(String, String, Option<MuseUsage>, Option<String>), String> {
                assert!(prior.is_none(), "no continuation expected");
                seen.borrow_mut().push(prompt.to_string());
                assert!(prompt.contains("step-by-step"), "{prompt}");
                assert!(prompt.contains("imperative"), "{prompt}");
                assert!(prompt.contains("grounded"), "{prompt}");
                assert!(prompt.contains("no meta"), "{prompt}");
                assert!(prompt.contains("no URLs"), "{prompt}");
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
                    Some("stop".to_string()),
                ))
            };
        let res = run_with(
            "300", true, None, &fetch_ok, &post_ok, &call_ok, &log, &out_dir, None,
        )
        .unwrap();
        assert_eq!(seen.borrow().len(), 1, "exactly one Muse call");
        assert_eq!(res.mode, "paid");
        assert_eq!(res.model, "fake-spark");
        assert!(res.output.contains("Paid step one"), "{}", res.output);
        assert!(res.output.contains("## Steps"), "{}", res.output);
        assert!(
            res.output.starts_with("# [synthetic] Try it now"),
            "paid output starts with title:\n{}",
            res.output
        );
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
        assert!(!logged.contains("Paid step one"), "{logged}");
    }

    #[test]
    fn empty_model_output_errors_with_nothing_written() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| Ok(fetch(vec![post("400", "[synthetic] root")]));
        let call_empty =
            |_: &str, _: Option<&str>| Ok(("   \n ".to_string(), "m".to_string(), None, None));
        let err = run_with(
            "400",
            true,
            None,
            &fetch_ok,
            &no_post_fetch,
            &call_empty,
            &log,
            &out_dir,
            None,
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
    fn paid_path_refuses_non_public_thread_with_zero_calls() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| {
            Ok(fetch(vec![post(
                "400",
                "[synthetic] root mail quokka@example.invalid now",
            )]))
        };
        let calls = std::cell::RefCell::new(0usize);
        let call_count = |_: &str, _: Option<&str>| {
            *calls.borrow_mut() += 1;
            Ok(("1. Do the thing".to_string(), "m".to_string(), None, None))
        };
        let mut log = DecisionLog::open(&log_path).unwrap();
        let err = run_with(
            "400",
            true,
            None,
            &fetch_ok,
            &no_post_fetch,
            &call_count,
            &log_path,
            &out_dir,
            Some(&mut log),
        )
        .unwrap_err();
        assert!(err.contains("egress denied"), "{err}");
        assert_eq!(*calls.borrow(), 0, "no model call on refusal");
        assert!(!out_dir.join("400.md").exists());
        // One denied entry: class, route, caller, no content.
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "router.egress_denied");
        assert_eq!(entries[0].payload["class"], "confidential");
        assert_eq!(entries[0].payload["route"], "muse_spark");
        assert_eq!(entries[0].payload["caller"], "deeper");
        let raw = entries[0].payload.to_string();
        assert!(!raw.contains("quokka"), "{raw}");
    }

    #[test]
    fn paid_path_logs_allowed_entry_before_done() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| {
            Ok(fetch(vec![post(
                "300",
                "[synthetic] Try it now: breathe in 4s.",
            )]))
        };
        let call_ok = |_: &str, _: Option<&str>| {
            Ok((
                "1. [synthetic] paid step one".to_string(),
                "fake-spark".to_string(),
                None,
                Some("stop".to_string()),
            ))
        };
        let mut log = DecisionLog::open(&log_path).unwrap();
        let res = run_with(
            "300",
            true,
            None,
            &fetch_ok,
            &no_post_fetch,
            &call_ok,
            &log_path,
            &out_dir,
            Some(&mut log),
        )
        .unwrap();
        assert_eq!(res.mode, "paid");
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "router.egress_allowed");
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "muse_spark");
        assert_eq!(entries[1].kind, DEEPER_DONE_KIND);
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
            None,
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
            None,
        )
        .unwrap();
        let file_mode = std::fs::metadata(&res.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file mode is {file_mode:o}");
        let dir_mode = std::fs::metadata(&out_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "dir mode is {dir_mode:o}");
    }

    #[test]
    fn cap_constant_is_3000_with_continue_prompt() {
        assert_eq!(DEEPER_MAX_COMPLETION_TOKENS, 3000);
        assert_eq!(
            DEEPER_CONTINUE_PROMPT,
            "Continue exactly where you stopped."
        );
        assert_eq!(DEEPER_TRUNCATED_MARKER, "[truncated]");
    }

    #[test]
    fn continuation_on_length_concatenates_once() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| {
            Ok(fetch(vec![post(
                "500",
                "[synthetic] root for continuation",
            )]))
        };
        let calls = std::cell::RefCell::new(Vec::new());
        let call = |prompt: &str, prior: Option<&str>| {
            calls
                .borrow_mut()
                .push((prompt.to_string(), prior.map(str::to_string)));
            match prior {
                None => Ok((
                    "part one ".to_string(),
                    "fake-spark".to_string(),
                    Some(MuseUsage {
                        prompt_tokens: Some(10),
                        completion_tokens: Some(5),
                        total_tokens: Some(15),
                    }),
                    Some("length".to_string()),
                )),
                Some(p) => {
                    assert_eq!(p, "part one ", "continuation gets prior content");
                    assert!(prompt.contains("step-by-step"), "{prompt}");
                    Ok((
                        "part two".to_string(),
                        "fake-spark".to_string(),
                        Some(MuseUsage {
                            prompt_tokens: Some(12),
                            completion_tokens: Some(7),
                            total_tokens: Some(19),
                        }),
                        Some("stop".to_string()),
                    ))
                }
            }
        };
        let res = run_with(
            "500",
            true,
            None,
            &fetch_ok,
            &no_post_fetch,
            &call,
            &log,
            &out_dir,
            None,
        )
        .unwrap();
        assert_eq!(calls.borrow().len(), 2, "exactly one continuation");
        assert!(calls.borrow()[0].1.is_none());
        assert!(calls.borrow()[1].1.is_some());
        assert!(
            res.output.contains("part one part two"),
            "concatenated:\n{}",
            res.output
        );
        assert!(
            !res.output.contains(DEEPER_TRUNCATED_MARKER),
            "no marker when continuation finishes:\n{}",
            res.output
        );
        let usage = res.usage.expect("usage logged");
        assert_eq!(usage.prompt_tokens, Some(22));
        assert_eq!(usage.completion_tokens, Some(12));
        assert_eq!(usage.total_tokens, Some(34));
        let entries = DecisionLog::open(&log).unwrap().verify().unwrap();
        assert_eq!(entries[0].payload["total_tokens"], 34);
    }

    #[test]
    fn still_truncated_after_continuation_appends_marker() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| Ok(fetch(vec![post("501", "[synthetic] root still long")]));
        let calls = std::cell::Cell::new(0);
        let call = |_: &str, prior: Option<&str>| {
            calls.set(calls.get() + 1);
            match prior {
                None => Ok((
                    "aaa ".to_string(),
                    "m".to_string(),
                    None,
                    Some("length".to_string()),
                )),
                Some(_) => Ok((
                    "bbb".to_string(),
                    "m".to_string(),
                    None,
                    Some("length".to_string()),
                )),
            }
        };
        let res = run_with(
            "501",
            true,
            None,
            &fetch_ok,
            &no_post_fetch,
            &call,
            &log,
            &out_dir,
            None,
        )
        .unwrap();
        assert_eq!(calls.get(), 2, "only one continuation, never more");
        assert!(res.output.contains("aaa bbb"), "kept text:\n{}", res.output);
        assert!(
            res.output.contains(DEEPER_TRUNCATED_MARKER),
            "marker appended:\n{}",
            res.output
        );
    }

    #[test]
    fn continuation_error_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("d.jsonl");
        let out_dir = dir.path().join("deeper");
        let fetch_ok = |_: &str| Ok(fetch(vec![post("502", "[synthetic] root boom")]));
        let call = |_: &str, prior: Option<&str>| {
            if prior.is_none() {
                Ok((
                    "part one ".to_string(),
                    "m".to_string(),
                    None,
                    Some("length".to_string()),
                ))
            } else {
                Err("synthetic continuation boom".to_string())
            }
        };
        let err = run_with(
            "502",
            true,
            None,
            &fetch_ok,
            &no_post_fetch,
            &call,
            &log,
            &out_dir,
            None,
        )
        .unwrap_err();
        assert!(err.contains("synthetic continuation boom"), "{err}");
        assert!(!log.exists(), "log written on continuation error");
        assert!(
            !out_dir.join("502.md").exists(),
            "file written on continuation error"
        );
    }

    #[test]
    fn strips_tco_links_from_steps() {
        assert_eq!(strip_tco_links("see https://t.co/abc123 now"), "see  now");
        assert_eq!(clean_step_text("see https://t.co/abc123 now"), "see now");
        assert_eq!(
            clean_step_text("a http://t.co/x b https://t.co/y c"),
            "a b c"
        );
        // `/` plus content required, like the regex.
        assert_eq!(
            strip_tco_links("see https://t.co/ now"),
            "see https://t.co/ now"
        );
        // Case-sensitive: uppercase passes through.
        assert!(strip_tco_links("see HTTPS://T.CO/abc now").contains("HTTPS://T.CO/abc"));
        // Other hosts are not t.co links.
        assert_eq!(
            clean_step_text("see https://example.com/x now"),
            "see https://example.com/x now"
        );
    }

    #[test]
    fn steps_are_plain_without_markdown_or_quotes() {
        assert_eq!(
            clean_step_text("[guide](https://example.com) helps"),
            "guide helps"
        );
        assert_eq!(
            clean_step_text("![alt text](https://example.com/i.jpg)"),
            "alt text"
        );
        assert_eq!(clean_step_text("> quoted line"), "quoted line");
        assert_eq!(
            clean_step_text(">> nested > quote kept mid-line"),
            "nested > quote kept mid-line"
        );
        assert_eq!(
            clean_step_text("  >  [step](https://t.co/x) https://t.co/abc do this "),
            "step do this"
        );
        assert_eq!(clean_step_text("  spaced   out  "), "spaced out");
    }

    #[test]
    fn quoted_text_never_becomes_steps() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(
            &dir,
            "q.jsonl",
            &[
                row(
                    "600",
                    "[synthetic] Try it now: 1) thread roll shoulders 10x 2) thread neck tilt 20s",
                    "syn_author",
                    "2026-10-20T14:00:00+00:00",
                    "600",
                    serde_json::json!({"referenced_tweets": [{"type": "quoted", "id": "601"}]}),
                ),
                row(
                    "601",
                    "[synthetic] 1) QUOTED_ONLY_MARKER quoted routine 2) quoted second step",
                    "syn_quoted",
                    "2026-10-19T10:00:00+00:00",
                    "601",
                    serde_json::json!({}),
                ),
            ],
        );
        let res = run_with(
            "600",
            false,
            Some(&fixture),
            &no_thread_fetch,
            &no_post_fetch,
            &no_call,
            &dir.path().join("d.jsonl"),
            &dir.path().join("deeper"),
            None,
        )
        .unwrap();
        let steps_pos = res
            .output
            .find("## Steps")
            .expect("try-it output has Steps");
        let steps_text = &res.output[steps_pos..];
        assert!(
            steps_text.contains("Thread roll shoulders 10x"),
            "thread step kept:\n{steps_text}"
        );
        assert!(
            !steps_text.contains("QUOTED_ONLY_MARKER"),
            "quoted leaked into steps:\n{steps_text}"
        );
        assert!(
            res.output.contains("QUOTED_ONLY_MARKER"),
            "quoted kept under Quoted section:\n{}",
            res.output
        );
        let quoted_pos = res.output.find("QUOTED_ONLY_MARKER").unwrap();
        assert!(
            quoted_pos < steps_pos,
            "quoted marker must sit in the Quoted section before Steps"
        );
        assert!(steps_text.contains("1. "), "numbered:\n{steps_text}");
        assert!(!steps_text.contains("t.co"), "{steps_text}");
        assert!(!steps_text.contains("]("), "{steps_text}");
        for line in steps_text.lines().filter(|l| {
            let t = l.trim();
            !t.is_empty() && t.chars().next().is_some_and(|c| c.is_ascii_digit())
        }) {
            assert!(
                !line.trim_start().starts_with('>'),
                "quote marker in steps: {line}"
            );
        }
    }

    #[test]
    fn try_it_steps_strip_tco_links_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(
            &dir,
            "tco.jsonl",
            &[row(
                "700",
                "[synthetic] Try it now: 1) roll shoulders 10x see https://t.co/abc123 2) neck tilt 20s https://t.co/xyz789 done",
                "syn_author",
                "2026-10-20T14:00:00+00:00",
                "700",
                serde_json::json!({}),
            )],
        );
        let res = run_with(
            "700",
            false,
            Some(&fixture),
            &no_thread_fetch,
            &no_post_fetch,
            &no_call,
            &dir.path().join("d.jsonl"),
            &dir.path().join("deeper"),
            None,
        )
        .unwrap();
        let steps_pos = res.output.find("## Steps").expect("Steps present");
        let steps_text = &res.output[steps_pos..];
        assert!(steps_text.contains("Roll shoulders"), "{steps_text}");
        assert!(!steps_text.contains("t.co"), "{steps_text}");
        assert!(!steps_text.contains("abc123"), "{steps_text}");
    }

    #[test]
    fn non_try_it_steps_are_plain_numbered_lines() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir, "rich.jsonl", &rich_thread_fixture());
        let res = run_with(
            "demo-3001",
            false,
            Some(&fixture),
            &no_thread_fetch,
            &no_post_fetch,
            &no_call,
            &dir.path().join("d.jsonl"),
            &dir.path().join("deeper"),
            None,
        )
        .unwrap();
        let steps_pos = res.output.find("## Steps").expect("Steps present");
        let after = &res.output[steps_pos..];
        let end = after.find("## Quoted & linked").unwrap_or(after.len());
        let steps_text = &after[..end];
        assert!(steps_text.contains("1. "), "{steps_text}");
        assert!(!steps_text.contains("- ("), "{steps_text}");
        assert!(!steps_text.contains("t.co"), "{steps_text}");
        assert!(!steps_text.contains("]("), "{steps_text}");
        assert!(
            !steps_text.lines().any(|l| l.trim_start().starts_with('>')),
            "{steps_text}"
        );
    }

    #[test]
    fn title_comes_from_first_post_line() {
        let t = short_title(&[post("1", "[synthetic] Install WordCraft and open a page")]);
        assert_eq!(t, "[synthetic] Install WordCraft and open a page");
        // First non-empty line wins; handles and URLs are stripped.
        let t = short_title(&[post(
            "2",
            "\nHello @syn_user see https://example.com/x now\nsecond line",
        )]);
        assert_eq!(t, "Hello see now");
        // Long titles truncate at a word boundary with an ellipsis.
        let long = "word ".repeat(30);
        let t = short_title(&[post("3", &long)]);
        assert!(t.ends_with('…'), "{t}");
        assert!(t.chars().count() <= MAX_TITLE_CHARS + 1, "{t}");
        // Empty text falls back to the post id.
        assert_eq!(short_title(&[post("9", "   ")]), "Post 9");
        assert_eq!(short_title(&[]), "Untitled");
    }

    #[test]
    fn saved_file_starts_with_title_and_quoted_ampersand_heading() {
        for (id, fixture_fn) in [
            ("100", thread_fixture()),
            ("demo-3001", rich_thread_fixture()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let fixture = write_fixture(&dir, "t.jsonl", &fixture_fn);
            let res = run_with(
                id,
                false,
                Some(&fixture),
                &no_thread_fetch,
                &no_post_fetch,
                &no_call,
                &dir.path().join("d.jsonl"),
                &dir.path().join("deeper"),
                None,
            )
            .unwrap();
            let first_line = res.output.lines().next().unwrap_or("");
            assert!(
                first_line.starts_with("# ") && first_line.len() > 2,
                "title line missing: {first_line:?}"
            );
            assert!(
                !res.output.contains("# Go deeper"),
                "old title leaked:\n{}",
                res.output
            );
            assert!(
                res.output.contains("## Quoted & linked"),
                "heading missing:\n{}",
                res.output
            );
            assert!(
                !res.output.contains("## Quoted/linked"),
                "old heading leaked:\n{}",
                res.output
            );
        }
        // Try-it output uses the same heading.
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(
            &dir,
            "t.jsonl",
            &[row(
                "200",
                "[synthetic] Desk stretch: Try it now. 1) roll shoulders 10x 2) neck tilt 20s each side.",
                "syn_mobility",
                "2026-10-21T15:00:00+00:00",
                "200",
                serde_json::json!({}),
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
            None,
        )
        .unwrap();
        assert!(
            res.output.lines().next().unwrap_or("").starts_with("# "),
            "title missing:\n{}",
            res.output
        );
        assert!(
            res.output.contains("## Quoted & linked"),
            "try-it heading missing:\n{}",
            res.output
        );
    }

    #[test]
    fn imperative_conversion_strips_leaders_and_capitalizes() {
        assert_eq!(
            to_imperative_form("Install WordCraft now"),
            "Install WordCraft now"
        );
        assert_eq!(
            to_imperative_form("I can install WordCraft now"),
            "Install WordCraft now"
        );
        assert_eq!(
            to_imperative_form("you should create a page for notes"),
            "Create a page for notes"
        );
        assert_eq!(
            to_imperative_form("Then, install the agent"),
            "Install the agent"
        );
        assert_eq!(to_imperative_form("Next create a page"), "Create a page");
        assert_eq!(
            to_imperative_form("First, you can open the board"),
            "Open the board"
        );
        assert_eq!(
            to_imperative_form("[synthetic] roll shoulders 10x"),
            "Roll shoulders 10x"
        );
        assert_eq!(
            to_imperative_form("please hold for 20 seconds"),
            "Hold for 20 seconds"
        );
    }

    #[test]
    fn step_filter_drops_non_imperative_empty_url_and_duplicates() {
        let raw = vec![
            "Install WordCraft now".to_string(),
            "install wordcraft now".to_string(), // duplicate (case-insensitive)
            "This is a very interesting post".to_string(), // non-imperative
            "The agent lists due cards here".to_string(), // article starter
            "Here is my summary of things".to_string(), // meta line
            "https://example.com/only-a-link".to_string(), // URL-only
            "   ".to_string(),                   // empty
            "Create a page for notes".to_string(),
            "Then create a page for notes".to_string(), // duplicate after conversion
            "Run".to_string(),                          // single word
        ];
        assert_eq!(
            filter_steps(&raw),
            vec![
                "Install WordCraft now".to_string(),
                "Create a page for notes".to_string(),
            ]
        );
    }

    #[test]
    fn model_steps_filter_and_renumber() {
        let content = "## Walkthrough\n1. Install WordCraft now\n2. This is a very interesting post\n3. https://example.com/x\n4. Create a page for notes\n5. create a page for notes\n";
        let out = filter_model_steps(content);
        assert_eq!(
            out,
            "## Walkthrough\n1. Install WordCraft now\n2. Create a page for notes\n"
        );
        // Non-step lines pass through; bullets are normalized to numbered steps.
        let content = "Intro line\n- Install WordCraft now\nOutro\n";
        assert_eq!(
            filter_model_steps(content),
            "Intro line\n1. Install WordCraft now\nOutro\n"
        );
    }

    #[test]
    fn paid_section_lists_quoted_and_linked() {
        let out = quoted_linked_section(&[post("9", "quoted text")], &[]);
        assert!(
            out.contains("## Quoted & linked") && out.contains("(quoted 9) quoted text"),
            "{out}"
        );
        assert!(quoted_linked_section(&[], &[]).contains("(none)"));
    }

    #[test]
    fn paid_prompt_requires_imperative_grounded_steps() {
        let prompt = build_paid_prompt(
            &[post("1", "[synthetic] Install WordCraft and open a page")],
            &[],
            &[],
        );
        for want in [
            "step-by-step",
            "imperative",
            "grounded",
            "no meta",
            "no URLs",
            "Install X",
            "Create a page",
        ] {
            assert!(prompt.contains(want), "{want} missing:\n{prompt}");
        }
    }

    #[test]
    fn paid_prompt_keeps_titles_and_ancientos_out_of_steps() {
        let prompt = build_paid_prompt(
            &[post("1", "[synthetic] Install WordCraft and open a page")],
            &[],
            &[],
        );
        for want in [
            "do not repeat setup titles",
            "Do not use the word AncientOS inside the steps",
            "## Mapping",
        ] {
            assert!(prompt.contains(want), "{want} missing:\n{prompt}");
        }
    }

    #[test]
    fn local_fallback_steps_stay_post_text_only() {
        // The local fallback derives steps from post text, so it never
        // forces "AncientOS" into steps; pin that no section mentions it.
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
            None,
        )
        .unwrap();
        assert!(
            !res.output.contains("AncientOS"),
            "fallback leaked mapping word:\n{}",
            res.output
        );
    }

    /// Test helper: opens a fresh decision log for queue tests.
    fn queue_log(dir: &tempfile::TempDir) -> (DecisionLog, PathBuf) {
        let path = dir.path().join("q.jsonl");
        (DecisionLog::open(&path).unwrap(), path)
    }

    /// Test helper: reads back every record stored at a log path.
    fn queue_entries(path: &Path) -> Vec<Entry> {
        DecisionLog::open(path).unwrap().verify_or_empty().unwrap()
    }

    #[test]
    fn queue_later_appends_and_folds_to_due() {
        let dir = tempfile::tempdir().unwrap();
        let (mut log, path) = queue_log(&dir);
        let e = queue_later(&mut log, "100", 2_000, 1_000).unwrap();
        assert_eq!(e.kind, DEEPER_QUEUED_KIND);
        assert_eq!(
            e.payload,
            serde_json::json!({"post_id": "100", "at": 2_000, "queued_at": 1_000})
        );
        queue_later(&mut log, "demo-7", 1_500, 1_000).unwrap();
        let entries = queue_entries(&path);
        // Sorted by (at, post_id), none due before their time.
        let jobs = list_queue(&entries);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].post_id, "demo-7");
        assert_eq!(jobs[0].status, QueueStatus::Queued);
        assert!(due_jobs(&entries, 1_499).is_empty());
        let due = due_jobs(&entries, 1_500);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].post_id, "demo-7");
        // Boundary inclusive: at == now is due.
        assert_eq!(due_jobs(&entries, 2_000).len(), 2);
    }

    #[test]
    fn queue_later_refuses_past_bad_id_and_keeps_first() {
        let dir = tempfile::tempdir().unwrap();
        let (mut log, path) = queue_log(&dir);
        assert_eq!(
            queue_later(&mut log, "100", 999, 1_000).unwrap_err(),
            "cannot queue a go-deeper job in the past"
        );
        assert!(queue_later(&mut log, "../x", 2_000, 1_000).is_err());
        assert!(queue_later(&mut log, "", 2_000, 1_000).is_err());
        assert!(queue_entries(&path).is_empty(), "refusals append nothing");
        // Same post+time twice: first queued_at wins, still one job.
        queue_later(&mut log, "100", 2_000, 1_000).unwrap();
        queue_later(&mut log, "100", 2_000, 1_100).unwrap();
        let jobs = list_queue(&queue_entries(&path));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].queued_at, 1_000);
    }

    #[test]
    fn queue_fold_tracks_proposed_then_done_and_ignores_strays() {
        let dir = tempfile::tempdir().unwrap();
        let (mut log, path) = queue_log(&dir);
        queue_later(&mut log, "100", 2_000, 1_000).unwrap();
        // Proposed/done for unknown jobs are ignored.
        log.append(
            DEEPER_PROPOSED_KIND,
            serde_json::json!({"post_id": "999", "at": 2_000, "approval_id": "x"}),
        )
        .unwrap();
        log.append(
            DEEPER_QUEUE_DONE_KIND,
            serde_json::json!({"post_id": "999", "at": 2_000}),
        )
        .unwrap();
        // Malformed queue entries are ignored.
        log.append(DEEPER_QUEUED_KIND, serde_json::json!({"post_id": "100"}))
            .unwrap();
        log.append(
            DEEPER_QUEUED_KIND,
            serde_json::json!({"post_id": "nope!", "at": 3_000, "queued_at": 1_000}),
        )
        .unwrap();
        let mut jobs = list_queue(&queue_entries(&path));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, QueueStatus::Queued);
        log.append(
            DEEPER_PROPOSED_KIND,
            serde_json::json!({"post_id": "100", "at": 2_000, "approval_id": "abc"}),
        )
        .unwrap();
        jobs = list_queue(&queue_entries(&path));
        assert_eq!(jobs[0].status, QueueStatus::Proposed);
        assert_eq!(jobs[0].approval_id.as_deref(), Some("abc"));
        // Proposed jobs are still due (they need claim+release+execute).
        assert_eq!(due_jobs(&queue_entries(&path), 2_000).len(), 1);
        log.append(
            DEEPER_QUEUE_DONE_KIND,
            serde_json::json!({"post_id": "100", "at": 2_000, "mode": "local"}),
        )
        .unwrap();
        jobs = list_queue(&queue_entries(&path));
        assert_eq!(jobs[0].status, QueueStatus::Done);
        assert!(due_jobs(&queue_entries(&path), 9_999).is_empty());
    }

    #[test]
    fn paid_with_paths_needs_the_switch_and_touches_no_network() {
        let _env = crate::test_support::EnvHold::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let err = run_paid_with_paths(
            "100",
            &dir.path().join("d.jsonl"),
            &dir.path().join("deeper"),
        )
        .unwrap_err();
        assert!(err.contains("ANCIENTOS_ALLOW_PAID=1"), "{err}");
        assert!(!dir.path().join("d.jsonl").exists());
        assert!(!dir.path().join("deeper").exists());
    }
}
