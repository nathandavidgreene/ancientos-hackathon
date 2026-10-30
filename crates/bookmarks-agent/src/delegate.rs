//! `delegate` command: expand one post's thread, summarize it with Muse
//! Spark, and append the summary to `inspiration.md`.
//!
//! Paid calls only: fails closed unless `ANCIENTOS_ALLOW_PAID=1`. Exactly
//! one X conversation search and one Muse call per run. Post text and the
//! summary never enter the decision log — only metadata.
use ancientos_core::log::DecisionLog;
use ancientos_router::data_class::{check_egress, max_of, DataClass, Route};
use ancientos_router::muse::MuseUsage;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const DELEGATE_DONE_KIND: &str = "delegate.done";
pub const INSPIRATION_FILE_NAME: &str = "inspiration.md";
pub const DELEGATE_MAX_COMPLETION_TOKENS: u32 = 1500;
pub const DELEGATE_REASONING_EFFORT: &str = "low";

/// Thread fetcher: a post id -> the fetched (possibly root-only) thread.
pub type ThreadFetcher<'a> = dyn Fn(&str) -> Result<crate::x_api::ThreadFetch, String> + 'a;
/// Muse caller: a prompt -> (content, model, usage).
pub type MuseCaller<'a> = dyn Fn(&str) -> Result<(String, String, Option<MuseUsage>), String> + 'a;

#[derive(Debug, Clone)]
pub struct DelegateResult {
    pub output: String,
    pub model: String,
    pub usage: Option<MuseUsage>,
    pub thread_posts: usize,
    pub search_ok: bool,
    pub post_url: String,
    pub inspiration_path: PathBuf,
    /// Idea cards created from "worth borrowing" bullets.
    pub idea_cards: usize,
}

/// Default inspiration path: `$ANCIENTOS_HOME/inspiration.md` (or
/// `~/.ancientos-hackathon/inspiration.md`).
pub fn inspiration_path() -> Result<PathBuf, String> {
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(INSPIRATION_FILE_NAME))
}

/// Public URL for a post, preferring the author's handle when known.
pub fn post_url(post_id: &str, username: Option<&str>) -> String {
    match username.filter(|u| !u.is_empty()) {
        Some(u) => format!("https://x.com/{u}/status/{post_id}"),
        None => format!("https://x.com/i/status/{post_id}"),
    }
}

/// Build the one prompt sent to Muse Spark: a developer-style instruction
/// plus the thread with URLs/@handles stripped (full length, uncapped).
/// Each post is followed by its quoted post's text (also sanitized
/// uncapped, prefixed `quoted:`) when the post quotes another post.
pub fn build_prompt(posts: &[crate::unroll::ThreadPost]) -> String {
    let mut thread = String::new();
    for (i, p) in posts.iter().enumerate() {
        let clean = crate::sanitize::sanitize_uncapped(&p.text);
        thread.push_str(&format!("Post {}/{}:\n{clean}\n", i + 1, posts.len()));
        if let Some((_, quoted_text)) = &p.quoted {
            let clean_quoted = crate::sanitize::sanitize_uncapped(quoted_text);
            thread.push_str(&format!("quoted: {clean_quoted}\n"));
        }
        thread.push('\n');
    }
    format!(
        "Instructions: summarize the public post thread below for AncientOS, a personal agent OS \
         with a scheduler, approval gate, append-only decision log, privacy router, and agents \
         like a bookmarks curator. Reply in markdown with exactly these two sections:\n\
         ## Setups\nList each setup mentioned: name + one line on what it does.\n\
         ## Worth borrowing for AncientOS\n3-6 bullets on ideas worth borrowing for AncientOS.\n\
         \nPublic thread (URLs and @handles stripped):\n\n{thread}"
    )
}

/// Append one `### <date> — <post url>` section to the inspiration file.
/// The directory is 0700, the file 0600, and the write is append-only:
/// existing sections are never rewritten.
pub fn append_inspiration(
    path: &Path,
    date: &str,
    post_url: &str,
    output: &str,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("inspiration dir failed: {e}"))?;
        }
    }
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("inspiration open failed: {e}"))?;
        writeln!(f, "### {date} — {post_url}\n\n{}\n", output.trim_end())
            .map_err(|e| format!("inspiration write failed: {e}"))?;
    }
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("inspiration perms failed: {e}"))?;
    Ok(())
}

/// Production run: real thread fetch plus one real Muse Spark call.
pub fn run(post_id: &str) -> Result<DelegateResult, String> {
    let allow_paid = std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false);
    let client = ancientos_router::muse::MuseClient::from_env()
        .with_max_completion_tokens(DELEGATE_MAX_COMPLETION_TOKENS)
        .with_reasoning_effort(DELEGATE_REASONING_EFFORT);
    let call = |prompt: &str| {
        // Public: run_with classified the thread (refusing non-public ones)
        // before this closure runs.
        client
            .chat_with_class(DataClass::Public, "delegate", None, prompt)
            .map_err(|e| e.to_string())
            .map(|r| {
                let model = if r.model.is_empty() {
                    client.model().to_string()
                } else {
                    r.model
                };
                (r.content, model, r.usage)
            })
    };
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
        &crate::x_api::fetch_thread,
        &call,
        &log_path,
        &inspiration_path()?,
        &crate::brief::current_date(),
        Some(&mut log),
    )
}

/// Same as [`run`] with injected thread fetch, Muse call, and paths.
/// Still fails closed without `allow_paid` and on empty input/output.
/// The prompt only covers Public threads: content from every post (plus
/// inline quoted text) takes the highest class, and anything above Public
/// is refused before the Muse call (logged as `router.egress_denied` when
/// `log` is given; allowed runs log `router.egress_allowed`).
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    post_id: &str,
    allow_paid: bool,
    fetch: &ThreadFetcher<'_>,
    call: &MuseCaller<'_>,
    log_path: &Path,
    insp_path: &Path,
    date: &str,
    log: Option<&mut DecisionLog>,
) -> Result<DelegateResult, String> {
    crate::topic_mute::screen_stored_post(
        post_id,
        None,
        log_path.parent().unwrap_or(Path::new(".")),
    )?;
    if !allow_paid {
        return Err("paid calls are disabled (set ANCIENTOS_ALLOW_PAID=1 to enable)".to_string());
    }
    if post_id.is_empty() {
        return Err("post id must be non-empty".to_string());
    }
    let thread = fetch(post_id)?;
    if thread.posts.is_empty() {
        return Err("thread fetch returned no posts".to_string());
    }
    let mutes = crate::topic_mute::current()?;
    for post in thread.posts.iter() {
        if mutes.matches_any(&[
            &post.text,
            post.quoted
                .as_ref()
                .map(|(_, text)| text.as_str())
                .unwrap_or(""),
        ]) {
            return Err("excluded topic".into());
        }
    }
    let class = {
        let mut classes = Vec::new();
        for p in &thread.posts {
            classes.push(crate::translate::class_of(&p.text));
            if let Some((user, quoted_text)) = &p.quoted {
                classes.push(crate::translate::class_of(user));
                classes.push(crate::translate::class_of(quoted_text));
            }
        }
        max_of(classes)
    };
    check_egress(class, Route::MuseSpark, "delegate", log).map_err(|e| e.to_string())?;
    let prompt = build_prompt(&thread.posts);
    let (output, model, usage) = call(&prompt)?;
    if output.trim().is_empty() {
        return Err("model returned an empty summary".to_string());
    }
    let url = post_url(post_id, thread.username.as_deref());
    append_inspiration(insp_path, date, &url, &output)?;
    let cards_path = insp_path.with_file_name(crate::idea_card::IDEA_CARDS_FILE_NAME);
    let idea_cards = crate::idea_card::capture_from_delegate(
        &cards_path,
        &url,
        &output,
        crate::idea_card::now_secs(),
    )?
    .len();
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
    // Metadata only here: never post text or the summary.
    log.append(
        DELEGATE_DONE_KIND,
        serde_json::json!({
            "post_id": post_id,
            "thread_posts": thread.posts.len(),
            "model": model,
            "prompt_tokens": pt,
            "completion_tokens": ct,
            "total_tokens": tt,
            "search_ok": thread.search_ok,
            "search_note": thread.search_note,
        }),
    )
    .map_err(|e| e.to_string())?;
    if !thread.search_ok {
        eprintln!("note: conversation search failed; summarized the root post only");
    }
    Ok(DelegateResult {
        output,
        model,
        usage,
        thread_posts: thread.posts.len(),
        search_ok: thread.search_ok,
        post_url: url,
        inspiration_path: insp_path.to_path_buf(),
        idea_cards,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unroll::ThreadPost;

    /// Test helper: thread post with fixed author and conversation.
    fn post(id: &str, text: &str) -> ThreadPost {
        ThreadPost {
            id: id.into(),
            author_id: "u1".into(),
            text: text.into(),
            created_at: None,
            conversation_id: Some("p1".into()),
            referenced_tweets: Vec::new(),
            quoted: None,
        }
    }

    /// Test helper: canned thread bundling the posts with a search flag.
    fn fetch(posts: Vec<ThreadPost>, search_ok: bool) -> crate::x_api::ThreadFetch {
        crate::x_api::ThreadFetch {
            root: posts[0].clone(),
            username: Some("syn_author".into()),
            posts,
            search_ok,
            search_note: None,
        }
    }

    #[test]
    fn prompt_strips_handles_and_urls_with_full_length() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let long = format!("see https://t.co/abc @syn_handle {}", "word ".repeat(100));
        let prompt = build_prompt(&[post("p1", &long), post("p2", "synthetic second post")]);
        assert!(prompt.contains("## Setups"), "{prompt}");
        assert!(
            prompt.contains("## Worth borrowing for AncientOS"),
            "{prompt}"
        );
        assert!(prompt.contains("AncientOS"), "{prompt}");
        assert!(prompt.contains("Post 1/2:"), "{prompt}");
        assert!(prompt.contains("synthetic second post"), "{prompt}");
        assert!(!prompt.contains("https://t.co/abc"), "{prompt}");
        assert!(!prompt.contains("@syn_handle"), "{prompt}");
        assert!(
            prompt.chars().count() > 500,
            "thread is not truncated: {} chars",
            prompt.chars().count()
        );
    }

    #[test]
    fn prompt_includes_sanitized_quoted_text_after_each_post() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let mut p1 = post("p1", "synthetic setup 1 https://t.co/root");
        p1.quoted = Some((
            "syn_quoted".into(),
            "synthetic quoted content @syn_handle https://t.co/q".into(),
        ));
        let p2 = post("p2", "synthetic setup 2");
        let prompt = build_prompt(&[p1, p2]);
        assert!(prompt.contains("Post 1/2:"), "{prompt}");
        assert!(prompt.contains("synthetic setup 1"), "{prompt}");
        let quoted_line = prompt
            .lines()
            .find(|l| l.starts_with("quoted:"))
            .unwrap_or_else(|| panic!("quoted line missing: {prompt}"));
        assert!(quoted_line.contains("synthetic quoted content"), "{prompt}");
        assert!(!quoted_line.contains("@syn_handle"), "{prompt}");
        assert!(!quoted_line.contains("https://t.co/q"), "{prompt}");
        // Quoted text follows its post and precedes the next one.
        let post_pos = prompt.find("synthetic setup 1").unwrap();
        let quoted_pos = prompt.find("synthetic quoted content").unwrap();
        let next_pos = prompt.find("Post 2/2:").unwrap();
        assert!(post_pos < quoted_pos && quoted_pos < next_pos, "{prompt}");
        // A post without a quote adds no quoted line.
        assert_eq!(
            prompt.lines().filter(|l| l.starts_with("quoted:")).count(),
            1,
            "{prompt}"
        );
    }

    #[test]
    fn post_url_prefers_username() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        assert_eq!(
            post_url("123", Some("syn_user")),
            "https://x.com/syn_user/status/123"
        );
        assert_eq!(post_url("123", None), "https://x.com/i/status/123");
        assert_eq!(post_url("123", Some("")), "https://x.com/i/status/123");
    }

    #[test]
    fn inspiration_appends_sections_without_rewriting() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(INSPIRATION_FILE_NAME);
        append_inspiration(
            &path,
            "2026-10-26",
            "https://x.com/i/status/1",
            "## Setups\n- synthetic one",
        )
        .unwrap();
        append_inspiration(
            &path,
            "2026-10-27",
            "https://x.com/i/status/2",
            "## Setups\n- synthetic two",
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("### 2026-10-26 — https://x.com/i/status/1"),
            "{body}"
        );
        assert!(body.contains("- synthetic one"), "{body}");
        assert!(
            body.contains("### 2026-10-27 — https://x.com/i/status/2"),
            "{body}"
        );
        assert!(body.contains("- synthetic two"), "{body}");
        assert!(
            body.find("synthetic one").unwrap() < body.find("synthetic two").unwrap(),
            "append order kept: {body}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn inspiration_file_is_0600_and_dir_is_0700() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(INSPIRATION_FILE_NAME);
        append_inspiration(&path, "2026-10-26", "https://x.com/i/status/1", "synthetic").unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file mode is {file_mode:o}");
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "dir mode is {dir_mode:o}");
    }

    #[test]
    fn run_with_writes_inspiration_and_metadata_log() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let insp_path = dir.path().join(INSPIRATION_FILE_NAME);
        let seen = std::cell::RefCell::new(Vec::new());
        let fetch_fn = |id: &str| -> Result<crate::x_api::ThreadFetch, String> {
            assert_eq!(id, "p1");
            Ok(fetch(
                vec![
                    post("p1", "synthetic root https://t.co/x"),
                    post("p2", "synthetic reply @syn_handle"),
                ],
                true,
            ))
        };
        let call_fn = |prompt: &str| -> Result<(String, String, Option<MuseUsage>), String> {
            seen.borrow_mut().push(prompt.to_string());
            assert!(!prompt.contains("https://t.co/x"), "{prompt}");
            assert!(!prompt.contains("@syn_handle"), "{prompt}");
            Ok((
                "## Setups\n- synthetic setup\n## Worth borrowing for AncientOS\n- synthetic idea"
                    .into(),
                "fake-spark".into(),
                Some(MuseUsage {
                    prompt_tokens: Some(100),
                    completion_tokens: Some(50),
                    total_tokens: Some(150),
                }),
            ))
        };
        let res = run_with(
            "p1",
            true,
            &fetch_fn,
            &call_fn,
            &log_path,
            &insp_path,
            "2026-10-26",
            None,
        )
        .unwrap();
        assert_eq!(seen.borrow().len(), 1, "exactly one Muse call");
        assert_eq!(res.thread_posts, 2);
        assert!(res.search_ok);
        assert_eq!(res.model, "fake-spark");
        assert_eq!(res.post_url, "https://x.com/syn_author/status/p1");
        let body = std::fs::read_to_string(&insp_path).unwrap();
        assert!(
            body.contains("### 2026-10-26 — https://x.com/syn_author/status/p1"),
            "{body}"
        );
        assert!(body.contains("synthetic setup"), "{body}");
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, DELEGATE_DONE_KIND);
        assert_eq!(entries[0].payload["post_id"], "p1");
        assert_eq!(entries[0].payload["thread_posts"], 2);
        assert_eq!(entries[0].payload["model"], "fake-spark");
        assert_eq!(entries[0].payload["total_tokens"], 150);
        assert_eq!(entries[0].payload["search_ok"], true);
        let logged = entries[0].payload.to_string();
        assert!(!logged.contains("synthetic root"), "{logged}");
        assert!(!logged.contains("synthetic setup"), "{logged}");
    }

    #[test]
    fn run_with_root_only_fallback_still_succeeds() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let fetch_fn = |_: &str| Ok(fetch(vec![post("p9", "synthetic old root")], false));
        let call_fn = |_: &str| Ok(("## Setups\nnone".into(), "m".into(), None));
        let res = run_with(
            "p9",
            true,
            &fetch_fn,
            &call_fn,
            &dir.path().join("d.jsonl"),
            &dir.path().join(INSPIRATION_FILE_NAME),
            "2026-10-26",
            None,
        )
        .unwrap();
        assert_eq!(res.thread_posts, 1);
        assert!(!res.search_ok);
        let log = DecisionLog::open(dir.path().join("d.jsonl")).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries[0].payload["search_ok"], false);
    }

    #[test]
    fn run_with_logs_search_note_in_delegate_done() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let insp_path = dir.path().join(INSPIRATION_FILE_NAME);
        let root = post("p9", "synthetic old root");
        let fetch_fn = |_: &str| {
            Ok(crate::x_api::ThreadFetch {
                root: root.clone(),
                username: Some("syn_author".into()),
                posts: vec![root.clone()],
                search_ok: false,
                search_note: Some("X API http status 403: synthetic forbidden".into()),
            })
        };
        let call_fn = |_: &str| Ok(("## Setups\nnone".into(), "m".into(), None));
        run_with(
            "p9",
            true,
            &fetch_fn,
            &call_fn,
            &log_path,
            &insp_path,
            "2026-10-26",
            None,
        )
        .unwrap();
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, DELEGATE_DONE_KIND);
        assert_eq!(entries[0].payload["search_ok"], false);
        assert_eq!(
            entries[0].payload["search_note"],
            "X API http status 403: synthetic forbidden"
        );
    }

    #[test]
    fn run_with_fails_closed() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let insp_path = dir.path().join(INSPIRATION_FILE_NAME);
        let fetch_fn = |_: &str| Ok(fetch(vec![post("p1", "synthetic")], true));
        let call_fn = |_: &str| Ok(("synthetic".into(), "m".into(), None));
        // Paid switch off: nothing is fetched or written.
        let err = run_with(
            "p1",
            false,
            &fetch_fn,
            &call_fn,
            &log_path,
            &insp_path,
            "2026-10-26",
            None,
        )
        .unwrap_err();
        assert!(err.contains("paid calls are disabled"), "{err}");
        assert!(!log_path.exists());
        assert!(!insp_path.exists());
        // Empty post id and empty model output also fail.
        assert!(run_with(
            "",
            true,
            &fetch_fn,
            &call_fn,
            &log_path,
            &insp_path,
            "2026-10-26",
            None,
        )
        .is_err());
        let empty_fn = |_: &str| Ok(("  \n ".into(), "m".into(), None));
        assert!(run_with(
            "p1",
            true,
            &fetch_fn,
            &empty_fn,
            &log_path,
            &insp_path,
            "2026-10-26",
            None,
        )
        .is_err());
        let no_posts = |_: &str| -> Result<crate::x_api::ThreadFetch, String> {
            Ok(crate::x_api::ThreadFetch {
                root: post("p1", "synthetic"),
                username: None,
                posts: Vec::new(),
                search_ok: false,
                search_note: None,
            })
        };
        assert!(run_with(
            "p1",
            true,
            &no_posts,
            &call_fn,
            &log_path,
            &insp_path,
            "2026-10-26",
            None,
        )
        .is_err());
    }

    #[test]
    fn run_with_refuses_non_public_thread_with_zero_calls() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let insp_path = dir.path().join(INSPIRATION_FILE_NAME);
        let fetch_fn = |_: &str| {
            Ok(fetch(
                vec![post("p1", "synthetic root mail quokka@example.invalid now")],
                true,
            ))
        };
        let calls = std::cell::RefCell::new(0usize);
        let call_count = |_: &str| {
            *calls.borrow_mut() += 1;
            Ok(("synthetic".into(), "m".into(), None))
        };
        let mut log = DecisionLog::open(&log_path).unwrap();
        let err = run_with(
            "p1",
            true,
            &fetch_fn,
            &call_count,
            &log_path,
            &insp_path,
            "2026-10-26",
            Some(&mut log),
        )
        .unwrap_err();
        assert!(err.contains("egress denied"), "{err}");
        assert_eq!(*calls.borrow(), 0, "no Muse call on refusal");
        assert!(!insp_path.exists());
        // One denied entry: class, route, caller, no content.
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "router.egress_denied");
        assert_eq!(entries[0].payload["class"], "confidential");
        assert_eq!(entries[0].payload["route"], "muse_spark");
        assert_eq!(entries[0].payload["caller"], "delegate");
        let raw = entries[0].payload.to_string();
        assert!(!raw.contains("quokka"), "{raw}");
    }

    #[test]
    fn run_with_logs_allowed_entry_before_done() {
        let _mute = crate::topic_mute::set_terms(crate::topic_mute::MuteList::default());
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let insp_path = dir.path().join(INSPIRATION_FILE_NAME);
        let fetch_fn = |_: &str| Ok(fetch(vec![post("p1", "synthetic root")], true));
        let call_fn = |_: &str| Ok(("synthetic summary".into(), "m".into(), None));
        let mut log = DecisionLog::open(&log_path).unwrap();
        run_with(
            "p1",
            true,
            &fetch_fn,
            &call_fn,
            &log_path,
            &insp_path,
            "2026-10-26",
            Some(&mut log),
        )
        .unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "router.egress_allowed");
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "muse_spark");
        assert_eq!(entries[1].kind, DELEGATE_DONE_KIND);
    }
}
