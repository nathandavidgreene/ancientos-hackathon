//! Loopback-only demo web UI: server-rendered HTML, no JS, std only.
//!
//! [`handle`] is a pure-ish `(Request, &State) -> Response` handler; `main.rs`
//! owns sockets and HTTP/1.1 parsing. Request handling makes no network or
//! paid calls: votes reuse `bookmarks_agent::vote`, triage reuses `idea_card`,
//! try-it follow-ups reuse `try_it`, approval decisions reuse
//! `ancientos_core::approvals` (recorded only, nothing executes),
//! translations reuse the on-disk cache
//! only (never live-translate), and everything lands in files under the
//! data dir plus the append-only decision log. Liveness is checked once at
//! startup via [`State::open_with_liveness`] (batched X lookup, 24h cache,
//! `--demo`/paid-off skips with no network); dead posts are filtered in
//! memory only and the store is never modified. Non-English posts are kept
//! and shown with their cached English translation plus the original (or an
//! `Untranslated` marker when the cache misses); they are never hidden for
//! language. Posts by a hidden author (`never_author` verdict, undone from
//! `/hidden-authors`) are filtered per request the same silent in-memory
//! way, so a vote takes effect immediately without a restart.

pub mod labels;
pub mod phone;

use bookmarks_agent::fixture::Bookmark;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Bodies larger than this are rejected with 413 (fail closed).
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Cap for request headers (fail closed well below any real browser's).
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
pub const DEFAULT_PORT: u16 = 8787;

/// Parsed request handed to [`handle`].
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub body: Vec<u8>,
    /// Raw `Cookie` header, if the client sent one.
    pub cookie: Option<String>,
}

impl Request {
    /// Test helper: `GET` for a target like `/?i=1`.
    pub fn get(target: &str) -> Self {
        let (path, query) = split_target(target);
        Self {
            method: "GET".to_string(),
            path,
            query,
            body: Vec::new(),
            cookie: None,
        }
    }

    /// Test helper: `POST` with a raw form-urlencoded body.
    pub fn post(path: &str, body: &str) -> Self {
        Self {
            method: "POST".to_string(),
            path: path.to_string(),
            query: BTreeMap::new(),
            body: body.as_bytes().to_vec(),
            cookie: None,
        }
    }

    /// Test helper: the same request carrying a raw `Cookie` header.
    pub fn with_cookie(mut self, cookie: &str) -> Self {
        self.cookie = Some(cookie.to_string());
        self
    }
}

/// Handler output; `main.rs` serializes it to HTTP/1.1 bytes.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub location: Option<String>,
    /// Value for one `Set-Cookie` header, if any.
    pub set_cookie: Option<String>,
    pub body: String,
    /// Raw bytes served instead of `body` when present (thumbnails).
    pub raw_body: Option<Vec<u8>>,
    /// Content-Type override; `None` means HTML.
    pub content_type: Option<String>,
}

impl Response {
    /// 200 HTML response.
    fn ok(body: String) -> Self {
        Self {
            status: 200,
            location: None,
            set_cookie: None,
            body,
            raw_body: None,
            content_type: None,
        }
    }

    /// 303 redirect response.
    fn redirect(to: String) -> Self {
        Self {
            status: 303,
            location: Some(to),
            set_cookie: None,
            body: String::new(),
            raw_body: None,
            content_type: None,
        }
    }

    /// Build an error response (used by the socket layer too).
    pub fn status(status: u16, body: String) -> Self {
        Self {
            status,
            location: None,
            set_cookie: None,
            body,
            raw_body: None,
            content_type: None,
        }
    }

    /// 200 binary response (thumbnails) with an explicit content type.
    fn bytes(content_type: &str, bytes: Vec<u8>) -> Self {
        Self {
            status: 200,
            location: None,
            set_cookie: None,
            body: String::new(),
            raw_body: Some(bytes),
            content_type: Some(content_type.to_string()),
        }
    }
}

/// Failed login state shared between workers.
type LoginAttempts =
    std::sync::Arc<std::sync::Mutex<BTreeMap<std::net::IpAddr, (std::time::Instant, u32)>>>;

/// Serving state: the data dir plus the bookmark store loaded once at
/// startup (votes never change the store, so no reload is needed).
#[derive(Debug, Clone)]
pub struct State {
    pub data_dir: PathBuf,
    /// Surfaced posts: live only (non-English kept with translations, see
    /// [`State::open_with_liveness`]).
    pub bookmarks: Vec<Bookmark>,
    /// Every post loaded from the store, before liveness filtering.
    /// Used only to resolve a pending id or idea-card source to its stored
    /// post for filtering; never rendered directly.
    all_bookmarks: Vec<Bookmark>,
    /// Muse Code run-log path for the `/stats` "Built with Meta" section
    /// (defaults from env, overridable per state for tests).
    pub muse_log_path: PathBuf,
    /// Shared secret for the phone approvals page (`/m`); `None` turns it
    /// off (403, fail closed). See [`phone::token_from`].
    pub phone_token: Option<String>,
    /// Resolved labels credential; the binary applies the legacy fallback once.
    pub labels_token: Option<String>,
    /// Failed-login buckets shared by the server workers.
    login_attempts: LoginAttempts,
    /// Eval-private dir for the labels page (`/labels`); `None` turns
    /// labels off (404). Set by the binary, per state in tests.
    pub labels_dir: Option<PathBuf>,
    /// Explicit `remove_bookmark` binary for labels deletes; `None` falls
    /// back to a `PATH` lookup at request time. Set by the binary from
    /// the env var, per state in tests.
    pub labels_remove_bin: Option<PathBuf>,
}

/// `store.jsonl` (demo/tests) if present, else the real synced
/// `bookmarks.jsonl` (bookmarks-agent's data dir), else `store.jsonl`.
fn resolve_store(dir: &Path) -> PathBuf {
    let store = dir.join("store.jsonl");
    let real = dir.join(bookmarks_agent::ingest::BOOKMARKS_FILE_NAME);
    if !store.exists() && real.exists() {
        real
    } else {
        store
    }
}

impl State {
    /// Open offline (no liveness network): used by tests and offline flows.
    /// Production servers use [`open_with_liveness`](Self::open_with_liveness).
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_with_liveness(dir, true)
    }

    /// Open and confirm posts still exist before surfacing them, unless
    /// `offline` (or paid-off) skips the check deterministically. Dead and
    /// transient-unknown posts are filtered in memory only; the store file
    /// is never modified. Outcomes are recorded only in the 24h disk cache
    /// in this data dir — never the decision log, never the store.
    /// Non-English posts are kept (translated for display from the on-disk
    /// cache, never hidden for language). The Muse Code run-log path
    /// defaults from `stats::muse_log_path_from_env` (a missing file later
    /// renders as `unavailable`, never an error).
    pub fn open_with_liveness(dir: impl AsRef<Path>, offline: bool) -> Result<Self, String> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| format!("data dir failed: {e}"))?;
        let mut bookmarks = match std::fs::read_to_string(resolve_store(&dir)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("store read failed: {e}")),
            Ok(text) => bookmarks_agent::fixture::parse_jsonl(&text)
                .map_err(|e| format!("store parse failed: {e}"))?,
        };
        bookmarks.sort_by(|a, b| a.post_id.cmp(&b.post_id));
        // Muted and removed-on-X bookmarks never surface in the UI. Reads
        // stay side-effect free, so no log entry is written here.
        let cache = bookmarks_agent::link_context::load_cache(
            &dir.join(bookmarks_agent::link_context::CACHE_FILE_NAME),
        );
        let mute = bookmarks_agent::topic_mute::current()?;
        let (visible, _skips) =
            bookmarks_agent::topic_mute::filter_surfaced(&bookmarks, &mute, &cache);
        let state = Self {
            data_dir: dir.clone(),
            bookmarks: visible,
            all_bookmarks: bookmarks,
            muse_log_path: bookmarks_agent::stats::muse_log_path_from_env(),
            phone_token: None,
            labels_token: None,
            login_attempts: Default::default(),
            labels_dir: None,
            labels_remove_bin: None,
        };
        let live = bookmarks_agent::liveness::filter_live(
            &state.bookmarks,
            &state.liveness_cache_path(),
            offline,
            now_secs(),
        )?;
        Ok(Self {
            data_dir: dir,
            bookmarks: live,
            all_bookmarks: state.all_bookmarks,
            muse_log_path: state.muse_log_path,
            phone_token: None,
            labels_token: None,
            login_attempts: Default::default(),
            labels_dir: None,
            labels_remove_bin: None,
        })
    }

    /// Bookmark store path.
    pub fn store_path(&self) -> PathBuf {
        resolve_store(&self.data_dir)
    }

    /// Decision log path.
    pub fn log_path(&self) -> PathBuf {
        self.data_dir.join("decision_log.jsonl")
    }

    /// Idea cards path.
    pub fn cards_path(&self) -> PathBuf {
        self.data_dir.join("idea_cards.jsonl")
    }

    /// Path for the demo backlog file (inside the temp data dir).
    pub fn backlog_path(&self) -> PathBuf {
        self.data_dir.join("backlog.md")
    }

    /// File holding cached liveness-check results in the data dir.
    pub fn liveness_cache_path(&self) -> PathBuf {
        self.data_dir
            .join(bookmarks_agent::liveness::CACHE_FILE_NAME)
    }

    /// Wiki root: `wiki/` in this data dir (read-only for the web UI;
    /// ingest and lint run in the CLI or daemon).
    pub fn wiki_dir(&self) -> PathBuf {
        self.data_dir.join(bookmarks_agent::wiki::WIKI_DIR_NAME)
    }

    /// Translation cache path: `$TRANSLATIONS_CACHE_PATH` when set, else
    /// `translations.json` in this data dir. Pages read it only; the brief
    /// writes it. Request handling never live-translates (no network).
    pub fn translations_cache_path(&self) -> PathBuf {
        if let Ok(p) = std::env::var(bookmarks_agent::translate::CACHE_ENV_VAR) {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        self.data_dir
            .join(bookmarks_agent::translate::CACHE_FILE_NAME)
    }
}

/// Loopback-only bind address: `localhost` maps to 127.0.0.1, IP literals
/// must be loopback (`127.x`, `::1`); anything else is refused (fail closed).
pub fn loopback_addr(host: &str, port: u16) -> Result<std::net::SocketAddr, String> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
    }
    let ip: std::net::IpAddr = host
        .parse()
        .map_err(|_| format!("refusing non-loopback bind host: {host}"))?;
    if ip.is_loopback() {
        Ok(std::net::SocketAddr::new(ip, port))
    } else {
        Err(format!("refusing non-loopback bind host: {host}"))
    }
}

/// Escape `&<>"'` for HTML text and attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Value of a hex digit.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Form-style decoding (`+` is a space, `%XX` escapes; invalid escapes and
/// non-UTF-8 stay literal via lossy conversion, never an error).
pub fn percent_decode(s: &str) -> String {
    let raw = s.as_bytes();
    let mut bytes: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'+' {
            bytes.push(b' ');
            i += 1;
        } else if raw[i] == b'%' && i + 2 < raw.len() {
            if let (Some(h), Some(l)) = (hex_val(raw[i + 1]), hex_val(raw[i + 2])) {
                bytes.push(h * 16 + l);
                i += 3;
            } else {
                bytes.push(b'%');
                i += 1;
            }
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parse `k=v&k=v` pairs; pairs without `=` are skipped (fail closed: the
/// caller treats missing keys as missing input).
pub fn parse_pairs(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(percent_decode(k), percent_decode(v));
        }
    }
    out
}

/// Split a request target into path and query map.
fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    match target.split_once('?') {
        Some((p, q)) => (percent_decode(p), parse_pairs(q)),
        None => (percent_decode(target), BTreeMap::new()),
    }
}

/// Parse a form-encoded body.
fn parse_body(body: &[u8]) -> BTreeMap<String, String> {
    parse_pairs(&String::from_utf8_lossy(body))
}

const CSS: &str = ":root{color-scheme:light}\
     body{font-family:system-ui,-apple-system,\"Segoe UI\",Roboto,Helvetica,Arial,sans-serif;\
     max-width:44rem;margin:0 auto;padding:1.5rem 1rem 4rem;line-height:1.6;\
     color:#1a1a1a;background:#fff}\
     nav{display:flex;gap:1.25rem;flex-wrap:wrap;margin-bottom:.5rem}\
     nav a{color:#0b57d0;text-decoration:none;font-weight:600}\
     nav a:hover{text-decoration:underline}\
     hr{border:0;border-top:1px solid #e5e5e5;margin:1.25rem 0}\
     h1{font-size:1.5rem;line-height:1.25;margin:0 0 1rem}\
     h2{font-size:1.15rem;margin:1.75rem 0 .5rem}\
     h3{font-size:1rem;margin:1.25rem 0 .4rem}\
     p{margin:.6em 0}ol,ul{padding-left:1.5rem}li{margin:.35em 0}\
     a{color:#0b57d0}\
     .card{border:1px solid #ddd;border-radius:8px;padding:1rem 1.25rem;margin:1rem 0}\
     .badge-ok{color:#fff;background:#137333;padding:.2em .6em;border-radius:4px}\
     .badge-bad{color:#fff;background:#b3261e;padding:.2em .6em;border-radius:4px}\
     blockquote{border-left:3px solid #ddd;margin:1em 0;padding:.2em 1em;color:#333}\
     img{max-width:100%;height:auto;border-radius:6px}\
     .media img{max-height:260px;width:auto;border:1px solid #e5e5e5}\
     button{font:inherit;font-weight:600;color:#fff;background:#111;\
     border:1px solid #111;border-radius:6px;padding:.45em 1em;cursor:pointer}\
     button:hover{background:#333}label{margin-right:.75em}input[type=radio]{margin-right:.25em}\
     table{border-collapse:collapse;width:100%}th{background:#f5f5f5}\
     td,th{border:1px solid #ddd;padding:.35em .6em;text-align:left;vertical-align:top}";

/// Current unix time.
fn now_secs() -> u64 {
    bookmarks_agent::idea_card::now_secs()
}

/// Pending approvals for the nav badge. Best-effort like
/// `hidden_authors_of`: a missing or unreadable log reads as zero
/// (tampering is surfaced on `/approvals` and `/log` instead).
fn pending_count(state: &State) -> String {
    let Ok(log) = ancientos_core::log::DecisionLog::open_with_key_env(state.log_path(), &|key| {
        std::env::var(key).ok()
    }) else {
        return "unavailable".into();
    };
    ancientos_core::approvals::pending(&log, now_secs())
        .map(|items| items.len().to_string())
        .unwrap_or_else(|_| "unavailable".into())
}

/// Wrap a page `body` in the shared HTML shell: doctype, inline CSS,
/// nav (Brief, Ideas, Try it, Wiki, Hidden, Approvals with pending badge,
/// Log, Stats, Accuracy, Labels) and the escaped `title`. Args: the page
/// title, the rendered body HTML, and the serving state (for the approvals
/// badge); returns the full page; never fails.
fn layout(title: &str, body: &str, state: &State) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>{CSS}</style></head><body>\
         <nav><a href=\"/\">Brief</a><a href=\"/ideas\">Ideas</a>\
         <a href=\"/try\">Try it</a><a href=\"/wiki\">Wiki</a>\
         <a href=\"/hidden-authors\">Hidden</a>\
         <a href=\"/approvals\">Approvals ({})</a>\
         <a href=\"/log\">Log</a><a href=\"/stats\">Stats</a>\
         <a href=\"/accuracy\">Accuracy</a>\
         <a href=\"/labels\">Labels</a></nav><hr>{body}</body></html>",
        escape(title),
        pending_count(state),
    )
}

/// Titled error message wrapped in the site layout.
fn error_page(title: &str, msg: &str, state: &State) -> String {
    layout(
        title,
        &format!("<h1>{}</h1><p>{}</p>", escape(title), escape(msg)),
        state,
    )
}

/// 404 page.
fn not_found(state: &State) -> Response {
    Response::status(404, error_page("Not found", "Unknown route.", state))
}

/// One bookmark as a thread post (same shape `unroll` consumes for X data).
fn thread_post_of(b: &Bookmark) -> bookmarks_agent::unroll::ThreadPost {
    bookmarks_agent::unroll::ThreadPost {
        id: b.post_id.clone(),
        author_id: b
            .author_username
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| b.post_id.clone()),
        text: b.text.clone(),
        created_at: b.created_at.clone(),
        conversation_id: None,
        referenced_tweets: Vec::new(),
        quoted: None,
    }
}

/// Render a thread as HTML.
fn thread_html(chain: &[bookmarks_agent::unroll::ThreadPost]) -> String {
    let mut out = format!("<h2>Thread ({} post{})</h2><ol>", chain.len(), {
        if chain.len() == 1 {
            ""
        } else {
            "s"
        }
    });
    for p in chain {
        out.push_str(&format!(
            "<li><p><strong>{}</strong></p><blockquote>{}</blockquote></li>",
            escape(&p.id),
            escape(&p.text),
        ));
    }
    out.push_str("</ol>");
    out
}

/// `<img>`/`<a>` thumbnails; every URL is re-checked with `safe_media_url`
/// (fail closed) and escaped. Photos render inline; videos show the poster
/// plus a watch link.
fn media_html(media: &[bookmarks_agent::media::Media]) -> String {
    use bookmarks_agent::media::safe_media_url;
    let mut items = String::new();
    for m in media {
        let kind = escape(&m.kind);
        let alt = match m.alt_text.as_deref().map(str::trim) {
            Some(a) if !a.is_empty() => escape(a),
            _ => kind.clone(),
        };
        let url = m
            .url
            .as_deref()
            .and_then(safe_media_url)
            .map(|u| escape(&u));
        let preview = m
            .preview_url
            .as_deref()
            .and_then(safe_media_url)
            .map(|u| escape(&u));
        let video = m
            .video_url
            .as_deref()
            .and_then(safe_media_url)
            .map(|u| escape(&u));
        match (url, preview, video) {
            (Some(u), _, None) => {
                items.push_str(&format!(
                    "<p>{kind}: <a href=\"{u}\"><img src=\"{u}\" alt=\"{alt}\"></a></p>"
                ));
            }
            (_, Some(p), Some(v)) => {
                items.push_str(&format!(
                    "<p>{kind} (video): <a href=\"{v}\"><img src=\"{p}\" alt=\"{alt}\"></a> \
                     <a href=\"{v}\">Watch video</a></p>"
                ));
            }
            (_, None, Some(v)) => {
                items.push_str(&format!(
                    "<p>{kind} (video): <a href=\"{v}\">Watch video</a></p>"
                ));
            }
            (_, Some(p), None) => {
                items.push_str(&format!(
                    "<p>{kind}: <a href=\"{p}\"><img src=\"{p}\" alt=\"{alt}\"></a></p>"
                ));
            }
            _ => {}
        }
    }
    if items.is_empty() {
        String::new()
    } else {
        format!("<h2>Media</h2><div class=\"media\">{items}</div>")
    }
}

/// Post text with translation: English posts render unchanged; non-English
/// posts render the cached English plus `Translated from <lang>` with the
/// original in a collapsible, or the original plus `Untranslated (<lang>)`
/// on a cache miss. Reads the on-disk cache only; never live-translates.
fn post_text_html(
    b: &Bookmark,
    cache: &std::collections::HashMap<String, bookmarks_agent::translate::CacheEntry>,
) -> String {
    let t = bookmarks_agent::translate::cached_only(b, cache);
    let Some(marker) = bookmarks_agent::translate::marker_for(b, &t) else {
        return format!("<blockquote>{}</blockquote>", escape(&b.text));
    };
    if t.status == bookmarks_agent::translate::TranslationStatus::Translated {
        if let Some(english) = t.english.as_deref().filter(|e| !e.trim().is_empty()) {
            return format!(
                "<blockquote>{}</blockquote><p><em>{}</em></p>\
                 <details><summary>Original</summary><blockquote>{}</blockquote></details>",
                escape(english),
                escape(&marker),
                escape(&t.original),
            );
        }
    }
    format!(
        "<blockquote>{}</blockquote><p><em>{}</em></p>",
        escape(&t.original),
        escape(&marker),
    )
}

/// Small badge for a tagged post: the deciding stage plus the reason as
/// visible text and on hover (`title`). Empty when untagged. Every
/// dynamic string is escaped. Args: the bookmark and its English text
/// (the translation when available); returns the badge HTML; never fails.
fn try_it_badge(b: &Bookmark, text: &str) -> String {
    let has_video = b
        .media
        .iter()
        .any(|m| m.kind == "video" || m.kind == "animated_gif");
    let topic = bookmarks_agent::brief::topic_of(b);
    let Some(tag) = bookmarks_agent::try_it::classify_with_media(&topic, text, has_video) else {
        return String::new();
    };
    format!(
        "<span class=\"badge-ok\" title=\"{}\">{}</span> {}",
        escape(&tag.reason),
        escape(tag.explanation.stage.as_str()),
        escape(&tag.reason),
    )
}

/// `[TRY IT]` section via the existing classifier and step extractor,
/// scoring `text` (the English translation when available) with the media
/// cue from `b`. Tagged posts show the stage badge plus steps.
fn try_it_html(b: &Bookmark, text: &str) -> String {
    let badge = try_it_badge(b, text);
    if badge.is_empty() {
        return String::new();
    }
    let mut out = format!("<h2>[TRY IT] {badge}</h2>");
    let steps = bookmarks_agent::try_it::extract_steps(text);
    if steps.is_empty() {
        out.push_str("<p>No explicit steps found.</p>");
    } else {
        out.push_str("<ol>");
        for s in &steps {
            out.push_str(&format!("<li>{}</li>", escape(s)));
        }
        out.push_str("</ol>");
    }
    out
}

/// "Why this is here" for one post: the personal ranker's top reasons,
/// trained on this data dir's verdict history. Best-effort like
/// `hidden_authors_of`: a missing or unreadable log, or too few labels,
/// renders no section at all.
fn rank_reasons_html(state: &State, bm: &Bookmark) -> Option<String> {
    let log = ancientos_core::log::DecisionLog::open_with_key_env(state.log_path(), &|key| {
        std::env::var(key).ok()
    })
    .ok()?;
    let ranker =
        bookmarks_agent::ranker::Ranker::train_from_log(&log, &state.all_bookmarks).ok()?;
    if !ranker.is_active() {
        return None;
    }
    let reasons = ranker.score(bm).top_reasons;
    if reasons.is_empty() {
        return None;
    }
    let items: String = reasons
        .iter()
        .map(|r| format!("<li>{}</li>", escape(r)))
        .collect();
    Some(format!("<h2>Why this is here</h2><ul>{items}</ul>"))
}

/// Currently hidden authors; unreadable governance state is an error.
fn hidden_authors_of(state: &State) -> Result<BTreeSet<String>, String> {
    let log = ancientos_core::log::DecisionLog::open_with_key_env(state.log_path(), &|key| {
        std::env::var(key).ok()
    })
    .map_err(|_| "log unreadable, hides not applied".to_string())?;
    let records = bookmarks_agent::feedback::records_from_log(&log)
        .map_err(|_| "log unreadable, hides not applied".to_string())?;
    Ok(bookmarks_agent::feedback::hidden_authors(&records))
}

/// Surfaced posts minus verified hidden authors.
fn visible_bookmarks(state: &State) -> Result<Vec<Bookmark>, String> {
    Ok(bookmarks_agent::author_filter::filter_visible(
        &state.bookmarks,
        &hidden_authors_of(state)?,
    ))
}

/// Explicit outage response: no content is surfaced without verified hides.
fn log_unreadable(state: &State) -> Response {
    Response::status(
        500,
        error_page(
            "Content withheld",
            "log unreadable, hides not applied; content withheld",
            state,
        ),
    )
}

/// Mark a tagged pick as shown once; failures are propagated to the page.
fn mark_shown(state: &State, post_id: &str) -> Result<(), String> {
    let mut log = ancientos_core::log::DecisionLog::open_with_key_env(state.log_path(), &|key| {
        std::env::var(key).ok()
    })
    .map_err(|_| "log unreadable".to_string())?;
    let shown = log
        .verify_or_empty()
        .map_err(|_| "log unreadable".to_string())?
        .iter()
        .any(|e| {
            e.kind == bookmarks_agent::try_it::SHOWN_KIND
                && e.payload["post_id"].as_str() == Some(post_id)
        });
    if !shown {
        log.append(
            bookmarks_agent::try_it::SHOWN_KIND,
            serde_json::json!({"post_id":post_id}),
        )
        .map_err(|_| "shown decision could not be recorded".to_string())?;
    }
    Ok(())
}

/// `/brief` page.
fn page_brief(state: &State, query: &BTreeMap<String, String>) -> Response {
    let raw = query.get("i").map(String::as_str).unwrap_or("0");
    let idx: usize = match raw.parse() {
        Ok(n) => n,
        Err(_) => {
            return Response::status(
                400,
                error_page("Bad index", "Query ?i=N needs a number.", state),
            )
        }
    };
    if state.bookmarks.is_empty() {
        return Response::ok(layout(
            "Brief",
            "<h1>Brief</h1><p>No bookmarks synced into this data dir yet.</p>",
            state,
        ));
    }
    let visible = match visible_bookmarks(state) {
        Ok(items) => items,
        Err(_) => return log_unreadable(state),
    };
    if visible.is_empty() {
        return Response::ok(layout(
            "Brief",
            "<h1>Brief</h1><p>Every post is hidden by an author filter. \
             <a href=\"/hidden-authors\">Manage hidden authors</a>.</p>",
            state,
        ));
    }
    let Some(bm) = visible.get(idx) else {
        return Response::status(
            404,
            error_page("Not found", "No post at that index.", state),
        );
    };
    let root = thread_post_of(bm);
    let candidates: Vec<_> = visible
        .iter()
        .filter(|b| b.post_id != bm.post_id)
        .map(thread_post_of)
        .collect();
    let chain = bookmarks_agent::unroll::unroll(&root, &candidates);
    // Translations come from the on-disk cache only; a miss renders the
    // original with an Untranslated marker (never hidden, never a call).
    let tcache = bookmarks_agent::translate::load_cache(&state.translations_cache_path());
    let translation = bookmarks_agent::translate::cached_only(bm, &tcache);
    let english = bookmarks_agent::translate::english_for(bm, &translation).to_string();
    let try_section = try_it_html(bm, &english);
    if !try_section.is_empty() {
        // Best-effort: a broken log must not break the brief (see /log).
        if mark_shown(state, &bm.post_id).is_err() {
            return log_unreadable(state);
        }
    }
    let mut body = format!(
        "<h1>Brief — post {} of {}</h1>\
         <p><strong>{}</strong> {}</p>{}",
        idx + 1,
        visible.len(),
        escape(&bm.post_id),
        bm.author_username
            .as_deref()
            .map(|u| format!("by {}", escape(u)))
            .unwrap_or_default(),
        post_text_html(bm, &tcache),
    );
    body.push_str(&thread_html(&chain));
    body.push_str(&media_html(&bm.media));
    body.push_str(&try_section);
    if let Some(why) = rank_reasons_html(state, bm) {
        body.push_str(&why);
    }
    body.push_str(&format!(
        "<h2>Vote</h2><form method=\"post\" action=\"/vote\">\
         <input type=\"hidden\" name=\"post_id\" value=\"{}\">\
         <input type=\"hidden\" name=\"i\" value=\"{idx}\">\
         <label><input type=\"radio\" name=\"verdict\" value=\"approve\" checked> approve</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"skip\"> skip</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"never_post\"> never_post</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"never_author\"> never_author</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"never_category\"> never_category</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"delegate\"> delegate</label> \
         <button type=\"submit\">Vote</button></form>",
        escape(&bm.post_id),
    ));
    body.push_str(&format!(
        "<h2>Go deeper</h2><form method=\"post\" action=\"/deeper/{}\">\
         <button type=\"submit\">Go deeper</button></form>\
         <form method=\"post\" action=\"/deeper/{}\">\
         <input type=\"hidden\" name=\"later\" value=\"1\">\
         <label>Later at <input type=\"datetime-local\" name=\"at\"></label> \
         <button type=\"submit\">Later</button></form>",
        escape(&bm.post_id),
        escape(&bm.post_id),
    ));
    if idx + 1 < visible.len() {
        body.push_str(&format!("<p><a href=\"/?i={}\">Next post</a></p>", idx + 1));
    }
    Response::ok(layout("Brief", &body, state))
}

/// POST vote handler.
fn do_vote(state: &State, body: &[u8]) -> Response {
    let form = parse_body(body);
    let post_id = form.get("post_id").map(String::as_str).unwrap_or("");
    if post_id.trim().is_empty() {
        return Response::status(400, error_page("Bad vote", "Missing post_id.", state));
    }
    let verdict: bookmarks_agent::feedback::Verdict = match form
        .get("verdict")
        .map(String::as_str)
        .unwrap_or("")
        .parse()
    {
        Ok(v) => v,
        Err(e) => return Response::status(400, error_page("Bad vote", &e, state)),
    };
    let idx: usize = form.get("i").and_then(|s| s.parse().ok()).unwrap_or(0);
    let store = state.store_path();
    let Some(store_str) = store.to_str() else {
        return Response::status(
            500,
            error_page("Vote failed", "Data dir path is not UTF-8.", state),
        );
    };
    if let Err(e) =
        bookmarks_agent::vote::run_vote(post_id, verdict, &[], Some(store_str), &state.log_path())
    {
        return Response::status(500, error_page("Vote failed", &e, state));
    }
    // Redirect within the still-visible posts (a never_author vote may have
    // just shrunk the list); when nothing is visible, offer the undo page.
    let visible = match visible_bookmarks(state) {
        Ok(items) => items.len(),
        Err(_) => return log_unreadable(state),
    };
    if visible == 0 {
        return Response::redirect("/hidden-authors".to_string());
    }
    Response::redirect(format!("/?i={}", (idx + 1) % visible))
}

/// `/ideas` page.
fn page_ideas(state: &State) -> Response {
    use bookmarks_agent::idea_card::list_due_for_triage;
    let now = now_secs();
    let cards = match list_due_for_triage(&state.cards_path(), now) {
        Ok(c) => c,
        Err(e) => return Response::status(500, error_page("Ideas failed", &e, state)),
    };
    // Cards by a hidden author get no UI mention. Cards are never hidden
    // for language: their idea text is locally generated English.
    let hidden = match hidden_authors_of(state) {
        Ok(hidden) => hidden,
        Err(_) => return log_unreadable(state),
    };
    let cards: Vec<_> = cards
        .into_iter()
        .filter(|c| {
            bookmarks_agent::author_filter::source_post_is_hidden(
                &c.source_url,
                &state.all_bookmarks,
                &hidden,
            ) != Some(true)
        })
        .collect();
    let mut body = String::from("<h1>Ideas due for triage</h1>");
    if cards.is_empty() {
        body.push_str("<p>No ideas due for triage.</p>");
    }
    for c in &cards {
        body.push_str(&format!(
            "<div class=\"card\"><p><strong>{}</strong> (effort {:?}; maps to: {})</p>\
             <blockquote>{}</blockquote><p>Source: {}</p>\
             <form method=\"post\" action=\"/ideas/triage\">\
             <input type=\"hidden\" name=\"id\" value=\"{}\">\
             <label><input type=\"radio\" name=\"decision\" value=\"adopt\" checked> adopt</label> \
             <label><input type=\"radio\" name=\"decision\" value=\"park\"> park</label> \
             <label><input type=\"radio\" name=\"decision\" value=\"drop\"> drop</label> \
             <button type=\"submit\">Triage</button></form></div>",
            escape(&c.id),
            c.effort,
            escape(&c.ancientos_mapping.join(", ")),
            escape(&c.idea),
            escape(&c.source_url),
            escape(&c.id),
        ));
    }
    Response::ok(layout("Ideas", &body, state))
}

/// POST triage handler.
fn do_triage(state: &State, body: &[u8]) -> Response {
    use bookmarks_agent::idea_card::Triage;
    let form = parse_body(body);
    let id = form.get("id").map(String::as_str).unwrap_or("");
    let decision = match form.get("decision").map(String::as_str).unwrap_or("") {
        "adopt" => Triage::Adopt,
        "park" => Triage::Park,
        "drop" => Triage::Drop,
        _ => {
            return Response::status(
                400,
                error_page("Bad triage", "Decision must be adopt, park or drop.", state),
            )
        }
    };
    if id.trim().is_empty() {
        return Response::status(400, error_page("Bad triage", "Missing id.", state));
    }
    let now = now_secs();
    match bookmarks_agent::idea_card::triage(
        &state.cards_path(),
        &state.backlog_path(),
        id,
        decision,
        now,
    ) {
        Ok(card) => {
            // An adopt queues a wiki ingest (logged); the adoption itself
            // already landed, so a queue failure is reported honestly.
            if decision == Triage::Adopt {
                let post_id = bookmarks_agent::wiki::post_id_from_url(&card.source_url)
                    .unwrap_or_else(|| card.id.clone());
                if let Err(e) = bookmarks_agent::wiki::queue_ingest(
                    &state.log_path(),
                    bookmarks_agent::wiki::QUEUE_REASON_ADOPT,
                    &card.id,
                    &post_id,
                    now,
                ) {
                    return Response::status(
                        500,
                        error_page(
                            "Triage queued poorly",
                            &format!("Adopted but failed to queue wiki ingest: {e}"),
                            state,
                        ),
                    );
                }
            }
            Response::redirect("/ideas".to_string())
        }
        Err(e) if e.starts_with("no idea card") => {
            Response::status(400, error_page("Bad triage", &e, state))
        }
        Err(e) => Response::status(500, error_page("Triage failed", &e, state)),
    }
}

/// `/try` page.
fn page_try(state: &State) -> Response {
    let pending: Vec<String> =
        match ancientos_core::log::DecisionLog::open_with_key_env(state.log_path(), &|key| {
            std::env::var(key).ok()
        }) {
            Ok(log) => bookmarks_agent::try_it::pending_followups(&log),
            Err(_) => return log_unreadable(state),
        };
    // Dead posts get no UI mention: skip pending ids with a fresh
    // `unavailable` liveness-cache entry (read-only reuse, no network).
    // Unknown ids are kept (they show "(not in store)"). Hidden authors the
    // same way. Pending posts are never hidden for language: non-English
    // posts show their cached translation plus the original.
    let cache = bookmarks_agent::liveness::load_cache(&state.liveness_cache_path());
    let now = now_secs();
    let hidden = match hidden_authors_of(state) {
        Ok(hidden) => hidden,
        Err(_) => return log_unreadable(state),
    };
    let pending: Vec<&String> = pending
        .iter()
        .filter(|id| !bookmarks_agent::liveness::is_fresh_unavailable(&cache, id, now))
        .filter(|id| {
            state
                .all_bookmarks
                .iter()
                .find(|b| &b.post_id == *id)
                .is_none_or(|b| !bookmarks_agent::author_filter::is_hidden(b, &hidden))
        })
        .collect();
    let tcache = bookmarks_agent::translate::load_cache(&state.translations_cache_path());
    let mut body = String::from("<h1>Try it — pending check-ins</h1>");
    if pending.is_empty() {
        body.push_str("<p>No pending try-it follow-ups.</p>");
    }
    for id in pending {
        let bm = state.bookmarks.iter().find(|b| &b.post_id == id);
        let text = bm
            .map(|b| post_text_html(b, &tcache))
            .unwrap_or_else(|| "<blockquote>(not in store)</blockquote>".to_string());
        // The stage badge re-scores the stored post (English translation
        // when available); posts missing from the store show no badge.
        let badge = bm
            .map(|b| {
                let t = bookmarks_agent::translate::cached_only(b, &tcache);
                let english = bookmarks_agent::translate::english_for(b, &t).to_string();
                try_it_badge(b, &english)
            })
            .unwrap_or_default();
        let badge = if badge.is_empty() {
            String::new()
        } else {
            format!(" {badge}")
        };
        body.push_str(&format!(
            "<div class=\"card\"><p><strong>{}</strong>{}</p>{}\
             <form method=\"post\" action=\"/try\">\
             <input type=\"hidden\" name=\"post_id\" value=\"{}\">\
             <label><input type=\"radio\" name=\"tried\" value=\"yes\" checked> yes</label> \
             <label><input type=\"radio\" name=\"tried\" value=\"no\"> no</label> \
             <label><input type=\"radio\" name=\"tried\" value=\"later\"> later</label> \
             <button type=\"submit\">Record</button></form></div>",
            escape(id),
            badge,
            text,
            escape(id),
        ));
    }
    Response::ok(layout("Try it", &body, state))
}

/// POST try handler.
fn do_try(state: &State, body: &[u8]) -> Response {
    let form = parse_body(body);
    let post_id = form.get("post_id").map(String::as_str).unwrap_or("");
    if post_id.trim().is_empty() {
        return Response::status(400, error_page("Bad follow-up", "Missing post_id.", state));
    }
    let tried: bookmarks_agent::try_it::Tried =
        match form.get("tried").map(String::as_str).unwrap_or("").parse() {
            Ok(t) => t,
            Err(e) => return Response::status(400, error_page("Bad follow-up", &e, state)),
        };
    if let Err(e) = bookmarks_agent::try_it::run_tried(post_id, tried, None, &state.log_path()) {
        return Response::status(500, error_page("Follow-up failed", &e, state));
    }
    Response::redirect("/try".to_string())
}

/// `/hidden-authors` page.
fn page_hidden_authors(state: &State) -> Response {
    use ancientos_core::log::{DecisionLog, LogError};
    let hidden = match DecisionLog::open_with_key_env(state.log_path(), &|key| {
        std::env::var(key).ok()
    }) {
        Err(LogError::Chain(n)) => {
            return Response::ok(layout(
                "Hidden authors",
                &format!(
                    "<h1>Hidden authors</h1><p><span class=\"badge-bad\">TAMPERED at row {n}</span></p>\
                     <p>The hash chain failed verification; the list is withheld (fail closed).</p>"
                ),
                state,
            ));
        }
        Err(e) => {
            return Response::status(
                500,
                error_page("Hidden authors failed", &e.to_string(), state),
            )
        }
        Ok(log) => match bookmarks_agent::feedback::records_from_log(&log) {
            Err(e) => {
                return Response::status(
                    500,
                    error_page("Hidden authors failed", &e.to_string(), state),
                );
            }
            Ok(records) => bookmarks_agent::feedback::hidden_authors(&records),
        },
    };
    let mut body = String::from("<h1>Hidden authors</h1>");
    if hidden.is_empty() {
        body.push_str("<p>No hidden authors. Voting never_author on a post hides its author.</p>");
    } else {
        body.push_str("<ul>");
        for author in &hidden {
            body.push_str(&format!(
                "<li>{} \
                 <form method=\"post\" action=\"/unhide-author\" style=\"display:inline\">\
                 <input type=\"hidden\" name=\"username\" value=\"{}\">\
                 <button type=\"submit\">Unhide</button></form></li>",
                escape(author),
                escape(author),
            ));
        }
        body.push_str("</ul>");
    }
    Response::ok(layout("Hidden authors", &body, state))
}

/// POST unhide-author handler.
fn do_unhide_author(state: &State, body: &[u8]) -> Response {
    let form = parse_body(body);
    let username = form.get("username").map(String::as_str).unwrap_or("");
    if username.trim().is_empty() {
        return Response::status(400, error_page("Bad unhide", "Missing username.", state));
    }
    if let Err(e) = bookmarks_agent::vote::run_unhide_author(username, &state.log_path()) {
        return Response::status(500, error_page("Unhide failed", &e, state));
    }
    Response::redirect("/hidden-authors".to_string())
}

/// Page shown when the log fails its hash check at `row`.
fn tampered_page(row: usize, state: &State) -> Response {
    Response::ok(layout(
        "Decision log",
        &format!(
            "<h1>Decision log</h1><p><span class=\"badge-bad\">TAMPERED at row {row}</span></p>\
             <p>The hash chain failed verification; entries are withheld (fail closed).</p>"
        ),
        state,
    ))
}

/// `/log` page.
fn page_log(state: &State) -> Response {
    use ancientos_core::log::{DecisionLog, LogError};
    let log = match DecisionLog::open_with_key_env(state.log_path(), &|key| std::env::var(key).ok())
    {
        Err(LogError::Chain(n)) => return tampered_page(n, state),
        Err(e) => {
            return Response::status(500, error_page("Log failed", &e.to_string(), state));
        }
        Ok(log) => log,
    };
    let entries = match log.verify_or_empty() {
        Err(LogError::Chain(n)) => return tampered_page(n, state),
        Err(e) => {
            return Response::status(500, error_page("Log failed", &e.to_string(), state));
        }
        Ok(entries) => entries,
    };
    let mut body = format!(
        "<h1>Decision log</h1><p><span class=\"badge-ok\">chain verified</span> {} entries</p>",
        entries.len()
    );
    if !entries.is_empty() {
        body.push_str("<table><tr><th>seq</th><th>kind</th><th>payload</th></tr>");
        for e in &entries {
            body.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                e.seq,
                escape(&e.kind),
                escape(&e.payload.to_string()),
            ));
        }
        body.push_str("</table>");
    }
    Response::ok(layout("Decision log", &body, state))
}

/// Fail-closed id check for `/deeper/<id>`: non-empty ASCII word chars
/// only, so the id is safe as a file name (no separators, no dot
/// segments). The deeper flow further requires digits/`demo-<digits>`.
fn valid_deeper_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Minimal markdown renderer for walkthrough files (`#`/`##`/`###`
/// headings, `- ` bullets, `N. ` steps, paragraphs). Every line is
/// HTML-escaped before wrapping, so output is safe by construction.
fn markdown_html(md: &str) -> String {
    /// Text after a leading list number, if any.
    fn numbered(line: &str) -> Option<&str> {
        let (n, rest) = line.split_once('.')?;
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        rest.strip_prefix(' ')
    }
    let mut out = String::new();
    let mut list: Option<&str> = None; // "ol" or "ul"
    let close_list = |out: &mut String, list: &mut Option<&str>| {
        if let Some(tag) = list.take() {
            out.push_str(&format!("</{tag}>"));
        }
    };
    for raw in md.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            close_list(&mut out, &mut list);
            continue;
        }
        if let Some(body) = line.strip_prefix("### ") {
            close_list(&mut out, &mut list);
            out.push_str(&format!("<h3>{}</h3>", escape(body)));
        } else if let Some(body) = line.strip_prefix("## ") {
            close_list(&mut out, &mut list);
            out.push_str(&format!("<h2>{}</h2>", escape(body)));
        } else if let Some(body) = line.strip_prefix("# ") {
            close_list(&mut out, &mut list);
            out.push_str(&format!("<h1>{}</h1>", escape(body)));
        } else if let Some(body) = numbered(line) {
            if list != Some("ol") {
                close_list(&mut out, &mut list);
                out.push_str("<ol>");
                list = Some("ol");
            }
            out.push_str(&format!("<li>{}</li>", escape(body)));
        } else if let Some(body) = line.strip_prefix("- ") {
            if list != Some("ul") {
                close_list(&mut out, &mut list);
                out.push_str("<ul>");
                list = Some("ul");
            }
            out.push_str(&format!("<li>{}</li>", escape(body)));
        } else {
            close_list(&mut out, &mut list);
            out.push_str(&format!("<p>{}</p>", escape(line)));
        }
    }
    close_list(&mut out, &mut list);
    out
}

/// `/deeper` page for one post.
fn page_deeper(state: &State, post_id: &str) -> Response {
    if !valid_deeper_id(post_id) {
        return Response::status(400, error_page("Bad id", "Invalid post id.", state));
    }
    let path = state
        .data_dir
        .join(bookmarks_agent::deeper::DEEPER_DIR_NAME)
        .join(format!("{post_id}.md"));
    let md = match std::fs::read_to_string(&path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::status(
                404,
                error_page(
                    "Not found",
                    "No walkthrough yet — use Go deeper first.",
                    state,
                ),
            )
        }
        Err(e) => {
            return Response::status(500, error_page("Walkthrough failed", &e.to_string(), state))
        }
    };
    let mut body = format!(
        "<p><a href=\"/\">Back to brief</a></p><h1>Go deeper — {}</h1>",
        escape(post_id)
    );
    body.push_str(&markdown_html(&md));
    Response::ok(layout(&format!("Go deeper — {post_id}"), &body, state))
}

/// Minimal wiki markdown renderer: `#`/`##`/`###` headings, `- ` bullets,
/// paragraphs. Every line is HTML-escaped, then `[[slug]]` targets become
/// links to `/wiki/<slug>` (invalid slugs stay plain text), so output is
/// safe by construction.
fn wiki_markdown_html(md: &str) -> String {
    /// Turn URLs in escaped text into links.
    fn linkify(escaped: &str) -> String {
        let mut out = String::new();
        let mut rest = escaped;
        while let Some(start) = rest.find("[[") {
            out.push_str(&rest[..start]);
            rest = &rest[start + 2..];
            match rest.find("]]") {
                Some(end) => {
                    let target = rest[..end].trim();
                    if bookmarks_agent::wiki::valid_slug(target) {
                        out.push_str(&format!(
                            "<a href=\"/wiki/{}\">[[{}]]</a>",
                            escape(target),
                            escape(target)
                        ));
                    } else {
                        out.push_str("[[");
                        out.push_str(&rest[..end]);
                        out.push_str("]]");
                    }
                    rest = &rest[end + 2..];
                }
                None => {
                    out.push_str("[[");
                    out.push_str(rest);
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }
    let mut out = String::new();
    let mut in_list = false;
    let close_list = |out: &mut String, in_list: &mut bool| {
        if *in_list {
            out.push_str("</ul>");
            *in_list = false;
        }
    };
    for raw in md.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            close_list(&mut out, &mut in_list);
            continue;
        }
        if let Some(body) = line.strip_prefix("### ") {
            close_list(&mut out, &mut in_list);
            out.push_str(&format!("<h3>{}</h3>", linkify(&escape(body))));
        } else if let Some(body) = line.strip_prefix("## ") {
            close_list(&mut out, &mut in_list);
            out.push_str(&format!("<h2>{}</h2>", linkify(&escape(body))));
        } else if let Some(body) = line.strip_prefix("# ") {
            close_list(&mut out, &mut in_list);
            out.push_str(&format!("<h1>{}</h1>", linkify(&escape(body))));
        } else if let Some(body) = line.strip_prefix("- ") {
            if !in_list {
                out.push_str("<ul>");
                in_list = true;
            }
            out.push_str(&format!("<li>{}</li>", linkify(&escape(body))));
        } else {
            close_list(&mut out, &mut in_list);
            out.push_str(&format!("<p>{}</p>", linkify(&escape(line))));
        }
    }
    close_list(&mut out, &mut in_list);
    out
}

/// `/wiki`: the page catalog (one link per page plus its summary), or an
/// empty-state message when nothing has been ingested yet.
fn page_wiki_index(state: &State) -> Response {
    let pages = bookmarks_agent::wiki::list_pages(&state.wiki_dir());
    let mut body = String::from("<h1>Wiki</h1>");
    if pages.is_empty() {
        body.push_str("<p>No wiki pages yet — ingest a post first.</p>");
        return Response::ok(layout("Wiki", &body, state));
    }
    body.push_str("<ul>");
    for p in &pages {
        body.push_str(&format!(
            "<li><a href=\"/wiki/{}\">[[{}]]</a> — {} (sources: {})</li>",
            escape(&p.slug),
            escape(&p.slug),
            escape(&p.summary),
            p.sources.len()
        ));
    }
    body.push_str("</ul>");
    Response::ok(layout("Wiki", &body, state))
}

/// `/wiki/<slug>`: one page, escaped with `[[links]]` linkified. Unknown
/// slugs and unsafe names fail closed.
fn page_wiki_show(state: &State, slug: &str) -> Response {
    if !bookmarks_agent::wiki::valid_slug(slug) {
        return Response::status(400, error_page("Bad slug", "Invalid wiki slug.", state));
    }
    let md = match bookmarks_agent::wiki::read_page(&state.wiki_dir(), slug) {
        Ok(md) => md,
        Err(_) => {
            return Response::status(
                404,
                error_page("Not found", "No wiki page for that slug.", state),
            )
        }
    };
    let mut body = format!(
        "<p><a href=\"/wiki\">Back to wiki</a></p><h1>{}</h1>",
        escape(slug)
    );
    body.push_str(&wiki_markdown_html(&md));
    Response::ok(layout(&format!("Wiki — {slug}"), &body, state))
}

/// Parse the Later form's `at` value: an HTML `datetime-local`
/// (`YYYY-MM-DDTHH:MM`, optional `:SS`, no zone, assumed UTC) or a full
/// RFC 3339 timestamp. Past or unparseable values are refused.
fn parse_later_at(raw: &str, now: u64) -> Result<u64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("Missing time.".to_string());
    }
    let normalized = if s.len() == 16 {
        format!("{s}:00Z")
    } else if s.len() == 19 {
        format!("{s}Z")
    } else {
        s.to_string()
    };
    let at = bookmarks_agent::idea_card::parse_rfc3339(&normalized)
        .ok_or_else(|| "Unparseable time (want YYYY-MM-DDTHH:MM).".to_string())?;
    if at < now {
        return Err("Time is in the past.".to_string());
    }
    Ok(at)
}

/// Run the existing deeper flow offline against this data dir's store
/// (local fallback, no network; paid switch on still stays offline
/// because a fixture is given) and redirect to the walkthrough page.
/// With `later=1` plus an `at` time, queue the walkthrough for later
/// instead of running it and redirect to the brief page.
/// Any failure closes without redirecting.
fn do_deeper(state: &State, post_id: &str, body: &[u8]) -> Response {
    if !valid_deeper_id(post_id) {
        return Response::status(400, error_page("Bad id", "Invalid post id.", state));
    }
    if !state.bookmarks.iter().any(|b| b.post_id == post_id) {
        return Response::status(404, error_page("Not found", "No post with that id.", state));
    }
    if let Err(e) = bookmarks_agent::deeper::validate_post_id(post_id) {
        return Response::status(400, error_page("Bad id", &e, state));
    }
    let form = parse_body(body);
    if let Some(later) = form.get("later") {
        if later != "1" {
            return Response::status(400, error_page("Bad queue", "Invalid later value.", state));
        }
        let raw = form.get("at").map(String::as_str).unwrap_or("");
        let now = now_secs();
        let at = match parse_later_at(raw, now) {
            Ok(t) => t,
            Err(e) => return Response::status(400, error_page("Bad time", &e, state)),
        };
        return match bookmarks_agent::deeper::queue_later_at_path(
            post_id,
            at,
            now,
            &state.log_path(),
        ) {
            Ok(_) => Response::redirect("/".to_string()),
            Err(e) => Response::status(500, error_page("Queue failed", &e, state)),
        };
    }
    if form.contains_key("at") {
        return Response::status(400, error_page("Bad queue", "Time without later=1.", state));
    }
    // Offline mode never calls these (no thread lookups, no model call);
    // they fail closed if that ever changes.
    let fetch_thread = |_id: &str| -> Result<bookmarks_agent::x_api::ThreadFetch, String> {
        Err("offline mode: thread fetch disabled".to_string())
    };
    let fetch_post = |_id: &str| -> Result<bookmarks_agent::unroll::ThreadPost, String> {
        Err("offline mode: post fetch disabled".to_string())
    };
    let store = state.store_path();
    let out_dir = state
        .data_dir
        .join(bookmarks_agent::deeper::DEEPER_DIR_NAME);
    let allow_paid = std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false);
    let err_model =
        |_prompt: &str, _prior: Option<&str>| Err("offline mode: model call disabled".to_string());
    match bookmarks_agent::deeper::run_with(
        post_id,
        allow_paid,
        Some(&store),
        &fetch_thread,
        &fetch_post,
        &err_model,
        &state.log_path(),
        &out_dir,
        None,
    ) {
        Ok(_) => Response::redirect(format!("/deeper/{post_id}")),
        Err(e) => Response::status(500, error_page("Go deeper failed", &e, state)),
    }
}

/// Decider recorded for web-UI approvals: the human at the browser. There is
/// no login, so the channel is the identity (mirroring the CLI's `cli`).
const WEB_DECIDER: &str = "web-ui";

/// Approvals page shown when the log fails its hash check at `row`.
fn tampered_approvals_page(row: usize, state: &State) -> Response {
    Response::ok(layout(
        "Approvals",
        &format!(
            "<h1>Approvals</h1><p><span class=\"badge-bad\">TAMPERED at row {row}</span></p>\
             <p>The hash chain failed verification; requests are withheld (fail closed).</p>"
        ),
        state,
    ))
}

/// `/approvals` page.
fn page_approvals(state: &State) -> Response {
    use ancientos_core::approvals::{decided, pending, ApprovalError};
    use ancientos_core::log::{DecisionLog, LogError};
    let log = match DecisionLog::open_with_key_env(state.log_path(), &|key| std::env::var(key).ok())
    {
        Err(LogError::Chain(n)) => return tampered_approvals_page(n, state),
        Err(e) => {
            return Response::status(500, error_page("Approvals failed", &e.to_string(), state));
        }
        Ok(log) => log,
    };
    let now = now_secs();
    let pend = match pending(&log, now) {
        Err(ApprovalError::Log(LogError::Chain(n))) => {
            return tampered_approvals_page(n, state);
        }
        Err(e) => {
            return Response::status(500, error_page("Approvals failed", &e.to_string(), state));
        }
        Ok(v) => v,
    };
    let done = match decided(&log, now) {
        Err(ApprovalError::Log(LogError::Chain(n))) => {
            return tampered_approvals_page(n, state);
        }
        Err(e) => {
            return Response::status(500, error_page("Approvals failed", &e.to_string(), state));
        }
        Ok(v) => v,
    };
    let mut body = format!("<h1>Approvals</h1><h2>Pending ({})</h2>", pend.len());
    if pend.is_empty() {
        body.push_str("<p>No pending approvals.</p>");
    }
    for a in &pend {
        let r = &a.request;
        body.push_str(&format!(
            "<div class=\"card\"><p><strong>{}</strong> {} · {} · expires {}</p>\
             <blockquote>{}</blockquote>\
             <form method=\"post\" action=\"/approvals/approve\">\
             <input type=\"hidden\" name=\"id\" value=\"{}\">\
             <input type=\"hidden\" name=\"payload_hash\" value=\"{}\">\
             <button type=\"submit\">Approve</button></form> \
             <form method=\"post\" action=\"/approvals/deny\">\
             <input type=\"hidden\" name=\"id\" value=\"{}\">\
             <input type=\"hidden\" name=\"payload_hash\" value=\"{}\">\
             <input name=\"reason\" placeholder=\"reason (optional)\"> \
             <button type=\"submit\">Deny</button></form></div>",
            escape(&r.id),
            escape(&r.agent),
            escape(&r.action),
            r.expires_at,
            escape(&r.summary),
            escape(&r.id),
            escape(&r.payload_hash),
            escape(&r.id),
            escape(&r.payload_hash),
        ));
    }
    body.push_str("<h2>Recently decided</h2>");
    if done.is_empty() {
        body.push_str("<p>No decisions yet.</p>");
    } else {
        body.push_str("<ul>");
        for a in &done {
            let status = a.status_at(now).as_str().to_string();
            let (by, reason) = match a.decision_at(now) {
                Some(d) => (d.decided_by, d.reason.unwrap_or_default()),
                None => ("-".to_string(), String::new()),
            };
            let reason = if reason.is_empty() {
                String::new()
            } else {
                format!(" — {}", escape(&reason))
            };
            body.push_str(&format!(
                "<li>{} — {} by {}{}</li>",
                escape(&a.request.id),
                escape(&status),
                escape(&by),
                reason,
            ));
        }
        body.push_str("</ul>");
    }
    Response::ok(layout("Approvals", &body, state))
}

/// Shared tail of the approve/deny handlers: the id and payload hash come
/// from the form's hidden fields, so a stale form (hash mismatch) fails with
/// an error page and no state change.
fn decide_approval(state: &State, body: &[u8], approve_it: bool) -> Response {
    use ancientos_core::approvals::{approve, deny, ApprovalError};
    use ancientos_core::log::{DecisionLog, LogError};
    let form = parse_body(body);
    let id = form.get("id").map(String::as_str).unwrap_or("");
    if id.trim().is_empty() {
        return Response::status(400, error_page("Bad approval", "Missing id.", state));
    }
    let hash = form.get("payload_hash").map(String::as_str).unwrap_or("");
    if hash.trim().is_empty() {
        return Response::status(
            400,
            error_page("Bad approval", "Missing payload_hash.", state),
        );
    }
    let mut log = match DecisionLog::open_with_key_env(state.log_path(), &|key| {
        std::env::var(key).ok()
    }) {
        Err(LogError::Chain(n)) => return tampered_approvals_page(n, state),
        Err(e) => {
            return Response::status(500, error_page("Approvals failed", &e.to_string(), state));
        }
        Ok(log) => log,
    };
    let now = now_secs();
    let outcome: Result<(), ApprovalError> = if approve_it {
        // The release token is single-process by design: the web UI records
        // the decision only. Nothing executes.
        approve(&mut log, id, hash, WEB_DECIDER, now).map(|_| ())
    } else {
        let reason = form.get("reason").map(String::as_str).unwrap_or("");
        deny(&mut log, id, hash, WEB_DECIDER, reason, now)
    };
    match outcome {
        Ok(()) => Response::redirect("/approvals".to_string()),
        Err(ApprovalError::Log(e)) => {
            Response::status(500, error_page("Approvals failed", &e.to_string(), state))
        }
        Err(e @ ApprovalError::HashMismatch(_)) => Response::status(
            400,
            error_page(
                "Stale approval",
                &format!("{e}; nothing was decided — reload and retry."),
                state,
            ),
        ),
        Err(e) => Response::status(400, error_page("Approval failed", &e.to_string(), state)),
    }
}

/// Accuracy panel: eval metrics computed live from the committed synthetic
/// data compiled into the binary (never eval-private, never this data dir).
/// Shows try-it classification and brief-ranking P@5 for today's order, the
/// vote-learned ranker, ranking by meaning, and the blend. An eval error
/// renders a 500 with the reason (fail closed: no numbers shown).
fn page_accuracy(state: &State) -> Response {
    let (report, cmp) = match ancientos_eval::harness::synthetic_panel() {
        Ok(v) => v,
        Err(e) => return Response::status(500, error_page("Accuracy unavailable", &e, state)),
    };
    let f3 = |x: f64| format!("{x:.3}");
    let opt = |x: Option<f64>| x.map_or_else(|| "n/a".to_string(), f3);
    let t = &report.try_it;
    let r = &report.ranking;
    let mut body = String::from(
        "<h1>Accuracy</h1><p>Measured on the committed synthetic eval set only \
         (fixtures/eval), offline, with cached local embeddings. No private data.</p>",
    );
    body.push_str(&format!(
        "<h2>Try-it classification</h2><p>{} labeled posts.</p>",
        t.n()
    ));
    body.push_str(&kv_table(
        "Metric",
        "Value",
        &[
            ("Accuracy".to_string(), f3(t.accuracy())),
            ("Precision".to_string(), f3(t.precision())),
            ("Recall".to_string(), f3(t.recall())),
            ("F1".to_string(), f3(t.f1())),
        ],
    ));
    body.push_str(&format!(
        "<h2>Brief ranking</h2><p>All {} posts, today's order: P@5 {}, brief-worthy base rate {}.</p>",
        report.n_matched,
        f3(r.precision_at_5),
        f3(r.base_rate),
    ));
    body.push_str(&format!(
        "<h2>Personal ranking</h2><p>The earlier {} votes train; the later {} posts \
         ({} brief-worthy) are scored.</p>",
        cmp.n_train, cmp.n_test, cmp.n_test_worthy
    ));
    body.push_str(&kv_table(
        "Picks",
        "P@5",
        &[
            ("Today's order".to_string(), f3(cmp.baseline_precision_at_5)),
            (
                "Votes (personal ranker)".to_string(),
                f3(cmp.ranked_precision_at_5),
            ),
            (
                "Meaning (local embeddings)".to_string(),
                opt(cmp.semantic_precision_at_5),
            ),
            (
                "Votes + meaning (blend)".to_string(),
                opt(cmp.blended_precision_at_5),
            ),
        ],
    ));
    Response::ok(layout("Accuracy", &body, state))
}

/// Render `rows` as a two-column HTML table with escaped cells and a
/// header row (`head0`, `head1`). Args: the header labels and the
/// `(name, count)` rows; returns the table HTML; never fails.
fn kv_table(head0: &str, head1: &str, rows: &[(String, String)]) -> String {
    let mut out = format!(
        "<table><tr><th>{}</th><th>{}</th></tr>",
        escape(head0),
        escape(head1),
    );
    for (k, v) in rows {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td></tr>",
            escape(k),
            escape(v),
        ));
    }
    out.push_str("</table>");
    out
}

/// Render one withheld `/stats` section for a broken chain: the heading
/// plus `withheld (fail closed)`. Args: the section title; returns the
/// HTML; never fails.
fn withheld_section(title: &str) -> String {
    format!("<h2>{}</h2><p>withheld (fail closed)</p>", escape(title))
}

/// Render verdict counts by category as an inline-SVG horizontal bar chart
/// (no JS, no external assets): one row per category with an escaped label,
/// a bar scaled to the top count, and the count. Args: `(category, count)`
/// pairs in display order; returns the `<svg>` HTML, or an empty string
/// when there is nothing to chart; never fails.
fn verdict_svg(items: &[(String, u64)]) -> String {
    if items.is_empty() {
        return String::new();
    }
    const LABEL_W: u32 = 150;
    const BAR_MAX: u32 = 280;
    const ROW_H: u32 = 24;
    const PAD: u32 = 8;
    let max = items.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1);
    let height = items.len() as u32 * ROW_H + PAD;
    let width = LABEL_W + BAR_MAX + 64;
    let mut out = format!(
        "<svg role=\"img\" aria-label=\"verdicts by category\" width=\"{width}\" height=\"{height}\" \
         viewBox=\"0 0 {width} {height}\" xmlns=\"http://www.w3.org/2000/svg\">",
    );
    for (i, (name, n)) in items.iter().enumerate() {
        let y = i as u32 * ROW_H + PAD / 2;
        let bar = (*n * u64::from(BAR_MAX) / max).max(u64::from(*n > 0)) as u32;
        let label: String = name.chars().take(24).collect();
        out.push_str(&format!(
            "<text x=\"0\" y=\"{}\" font-size=\"12\">{}</text>\
             <rect x=\"{LABEL_W}\" y=\"{}\" width=\"{bar}\" height=\"14\" fill=\"#111\"></rect>\
             <text x=\"{}\" y=\"{}\" font-size=\"12\">{n}</text>",
            y + 12,
            escape(&label),
            y,
            LABEL_W + bar + 6,
            y + 12,
        ));
    }
    out.push_str("</svg>");
    out
}

/// Render the `/stats` dashboard: decision-log chain status plus entry
/// counts, approvals, verdicts with an inline-SVG chart, try-it, cascade
/// stages, daemon ticks and Muse Code runs. Every number comes from
/// `stats::collect` over local files (no network, no paid calls) and every
/// dynamic string is HTML-escaped. A broken chain shows `broken at line N`
/// with every log-derived section withheld (fail closed); a missing run
/// log shows `unavailable`. Args: the serving state; returns the 200
/// response; never fails.
fn page_stats(state: &State) -> Response {
    let stats =
        bookmarks_agent::stats::collect(&state.log_path(), &state.muse_log_path, now_secs());
    let mut body = String::from("<h1>Stats</h1>");
    if !stats.chain_verified {
        match stats.broken_at {
            Some(n) => body.push_str(&format!(
                "<h2>Decision log</h2><p><span class=\"badge-bad\">broken at line {n}</span></p>\
                 <p>The hash chain failed verification; every log-derived number below is \
                 withheld (fail closed).</p>"
            )),
            None => body.push_str(
                "<h2>Decision log</h2><p><span class=\"badge-bad\">unreadable</span></p>\
                 <p>The decision log cannot be read; every log-derived number below is \
                 withheld (fail closed).</p>",
            ),
        }
        for title in [
            "Approvals",
            "Verdicts",
            "Try it",
            "Try-it cascade",
            "Daemon",
        ] {
            body.push_str(&withheld_section(title));
        }
    } else {
        body.push_str(&format!(
            "<h2>Decision log</h2><p><span class=\"badge-ok\">chain verified</span> {} entries</p>",
            stats.entry_count
        ));
        if stats.kinds_top.is_empty() {
            body.push_str("<p>No entries yet.</p>");
        } else {
            let rows: Vec<(String, String)> = stats
                .kinds_top
                .iter()
                .map(|(k, n)| (k.clone(), n.to_string()))
                .collect();
            body.push_str(&kv_table("kind", "entries", &rows));
        }
        let a = &stats.approvals;
        body.push_str("<h2>Approvals</h2>");
        body.push_str(&kv_table(
            "status",
            "count",
            &[
                ("pending".to_string(), a.pending.to_string()),
                ("approved".to_string(), a.approved.to_string()),
                ("denied".to_string(), a.denied.to_string()),
                ("expired".to_string(), a.expired.to_string()),
                ("released".to_string(), a.released.to_string()),
            ],
        ));
        body.push_str("<h2>Verdicts</h2>");
        if stats.verdicts_by_verdict.is_empty() {
            body.push_str("<p>No verdicts yet.</p>");
        } else {
            let rows: Vec<(String, String)> = stats
                .verdicts_by_verdict
                .iter()
                .map(|(k, n)| (k.clone(), n.to_string()))
                .collect();
            body.push_str(&kv_table("verdict", "count", &rows));
        }
        if stats.verdicts_by_category.is_empty() {
            body.push_str("<p>No verdicts by category yet.</p>");
        } else {
            let rows: Vec<(String, String)> = stats
                .verdicts_by_category
                .iter()
                .map(|(k, n)| (k.clone(), n.to_string()))
                .collect();
            body.push_str(&kv_table("category", "count", &rows));
            body.push_str(&verdict_svg(&stats.verdicts_by_category));
        }
        if stats.auto_approved.is_empty() {
            body.push_str("<p>Auto-approved categories: none.</p>");
        } else {
            body.push_str(&format!(
                "<p>Auto-approved categories: {}</p>",
                stats
                    .auto_approved
                    .iter()
                    .map(|c| escape(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let t = &stats.try_it;
        body.push_str("<h2>Try it</h2>");
        body.push_str(&kv_table(
            "metric",
            "count",
            &[
                ("shown".to_string(), t.shown.to_string()),
                ("yes".to_string(), t.yes.to_string()),
                ("no".to_string(), t.no.to_string()),
                ("later".to_string(), t.later.to_string()),
                ("pending".to_string(), t.pending.to_string()),
            ],
        ));
        body.push_str("<h2>Try-it cascade</h2>");
        match &stats.cascade {
            Some(c) => {
                body.push_str(
                    "<p>Decisions that failed closed fall back to the rules verdict; \
                     the Jev and Muse stages only run when paid and the privacy gates allow.</p>",
                );
                let rows: Vec<(String, String)> = bookmarks_agent::stats::CASCADE_STAGES
                    .iter()
                    .map(|s| (s.to_string(), c.by_stage(s).to_string()))
                    .collect();
                body.push_str(&kv_table("stage", "decisions", &rows));
            }
            None => body.push_str("<p>no cascade decisions logged</p>"),
        }
        body.push_str("<h2>Daemon</h2>");
        if stats.ticks.is_empty() {
            body.push_str("<p>No daemon ticks logged.</p>");
        } else {
            let mut ticks = String::from("<table><tr><th>agent</th><th>last tick</th></tr>");
            for tick in &stats.ticks {
                let status = if tick.ok {
                    "ok".to_string()
                } else if let Some(error) = &tick.error {
                    format!("error: {error}")
                } else {
                    "error".to_string()
                };
                ticks.push_str(&format!(
                    "<tr><td>{}</td><td>{} at {}</td></tr>",
                    escape(&tick.agent),
                    escape(&status),
                    tick.at,
                ));
            }
            ticks.push_str("</table>");
            body.push_str(&ticks);
        }
        if stats.deeper_outstanding.is_empty() {
            body.push_str(&format!(
                "<p>Queued go-deeper jobs: none outstanding ({} done).</p>",
                stats.deeper_done
            ));
        } else {
            body.push_str(&format!(
                "<p>Queued go-deeper jobs: {} outstanding ({} done).</p>",
                stats.deeper_outstanding.len(),
                stats.deeper_done
            ));
            let rows: Vec<(String, String)> = stats
                .deeper_outstanding
                .iter()
                .map(|j| {
                    (
                        j.post_id.clone(),
                        format!("at {} ({})", j.at, j.status.as_str()),
                    )
                })
                .collect();
            body.push_str(&kv_table("post", "scheduled", &rows));
        }
    }
    body.push_str("<h2>Built with Meta</h2>");
    match &stats.muse_log {
        Some(m) => body.push_str(&format!(
            "<p>Muse Code runs: {} (est. cost {})</p>",
            m.runs,
            escape(&m.formatted_cost())
        )),
        None => body.push_str("<p>Muse Code runs: unavailable</p>"),
    }
    Response::ok(layout("Stats", &body, state))
}

/// Authenticate decision-log access with the phone credential and scoped cookie.
fn authenticated_log(req: &Request, state: &State) -> Response {
    if let Some(token) = req.query.get("k") {
        if phone::token_valid(state, token) {
            let mut response = Response::redirect("/log".into());
            response.set_cookie = Some(phone::login_cookie_scoped(token, "/log"));
            return response;
        }
    }
    if !phone::cookie_authorized(state, req) {
        return Response::status(403, "<h1>Log authentication required</h1>".into());
    }
    page_log(state)
}

/// Route one request. Pure over `(Request, &State)` apart from the file
/// appends that votes, triage, follow-ups and approval decisions require.
/// `GET /stats` renders the local stats dashboard (read-only).
pub fn handle(req: &Request, state: &State) -> Response {
    if matches!(req.path.as_str(), "/" | "/ideas" | "/try") && hidden_authors_of(state).is_err() {
        return log_unreadable(state);
    }

    if req.body.len() > MAX_BODY_BYTES {
        return Response::status(
            413,
            error_page("Too large", "Request body exceeds 64KB.", state),
        );
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => page_brief(state, &req.query),
        ("POST", "/vote") => do_vote(state, &req.body),
        ("GET", "/ideas") => page_ideas(state),
        ("POST", "/ideas/triage") => do_triage(state, &req.body),
        ("GET", "/try") => page_try(state),
        ("POST", "/try") => do_try(state, &req.body),
        ("GET", "/hidden-authors") => page_hidden_authors(state),
        ("POST", "/unhide-author") => do_unhide_author(state, &req.body),
        ("GET", "/log") => authenticated_log(req, state),
        ("GET", "/stats") => page_stats(state),
        ("GET", "/accuracy") => page_accuracy(state),
        ("GET", "/approvals") => page_approvals(state),
        ("POST", "/approvals/approve") => decide_approval(state, &req.body, true),
        ("POST", "/approvals/deny") => decide_approval(state, &req.body, false),
        ("GET", "/m") => phone::page_list(req, state),
        ("POST", "/m/decide") => phone::do_decide(req, state),
        ("GET", "/labels") => labels::page_labels(req, state),
        ("POST", "/labels/save") => labels::save_labels(req, state),
        ("POST", "/labels/delete") => labels::delete_post(req, state),
        ("GET", path) if path.starts_with("/labels/thumb/") => {
            labels::serve_thumb(req, state, &path["/labels/thumb/".len()..])
        }
        ("GET", "/wiki") => page_wiki_index(state),
        ("GET", path) if path.starts_with("/wiki/") => {
            page_wiki_show(state, &path["/wiki/".len()..])
        }
        ("GET", path) if path.starts_with("/deeper/") => {
            page_deeper(state, &path["/deeper/".len()..])
        }
        ("POST", path) if path.starts_with("/deeper/") => {
            do_deeper(state, &path["/deeper/".len()..], &req.body)
        }
        ("GET", _) | ("POST", _) => not_found(state),
        _ => Response::status(
            405,
            error_page("Method not allowed", "Only GET and POST are served.", state),
        ),
    }
}

/// End of the HTTP headers.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// 400 response.
fn bad_request(msg: &str) -> Box<Response> {
    Box::new(Response::status(
        400,
        format!("<h1>Bad request</h1><p>{}</p>", escape(msg)),
    ))
}

/// Read one request: headers up to [`MAX_HEAD_BYTES`], body up to
/// [`MAX_BODY_BYTES`] (larger declared bodies get 413 without being read).
/// The error is boxed: `Response` carries binary bodies and exceeds the
/// large-error threshold.
fn read_request(stream: &mut std::net::TcpStream) -> Result<Request, Box<Response>> {
    use std::io::Read;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_len = loop {
        let n = stream
            .read(&mut chunk)
            .map_err(|_| bad_request("read failed"))?;
        if n == 0 {
            return Err(bad_request("empty request"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_HEAD_BYTES + 4 {
            return Err(bad_request("headers too large"));
        }
        if let Some(p) = find_head_end(&buf) {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v)) if parts.next().is_none() => (m, t, v),
        _ => return Err(bad_request("bad request line")),
    };
    if !version.starts_with("HTTP/") {
        return Err(bad_request("bad version"));
    }
    if method != "GET" && method != "POST" {
        return Err(Box::new(Response::status(
            405,
            "<h1>Method not allowed</h1>".to_string(),
        )));
    }
    if !target.starts_with('/') {
        return Err(bad_request("bad target"));
    }
    let mut content_length: usize = 0;
    let mut cookie: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(bad_request("bad header"));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| bad_request("bad content-length"))?;
        } else if name.trim().eq_ignore_ascii_case("cookie") {
            cookie = Some(value.trim().to_string());
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(Box::new(Response::status(
            413,
            "<h1>Content too large</h1><p>Body exceeds 64KB.</p>".to_string(),
        )));
    }
    let mut body = buf[head_len..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut chunk)
            .map_err(|_| bad_request("body read failed"))?;
        if n == 0 {
            return Err(bad_request("truncated body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (percent_decode(p), parse_pairs(q)),
        None => (percent_decode(target), BTreeMap::new()),
    };
    Ok(Request {
        method: method.to_string(),
        path,
        query,
        body,
        cookie,
    })
}

/// HTTP reason phrase.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

/// Permit at most five failures per peer per minute; bound bucket storage.
fn login_allowed(state: &State, ip: std::net::IpAddr, now: std::time::Instant) -> bool {
    let Ok(mut attempts) = state.login_attempts.lock() else {
        return false;
    };
    attempts.retain(|_, (start, _)| now.duration_since(*start).as_secs() < 60);
    match attempts.get(&ip) {
        Some((_, count)) => *count < 5,
        None => attempts.len() < 1024,
    }
}

/// Count one refused scoped login without retaining any credential material.
fn login_failed(state: &State, ip: std::net::IpAddr, now: std::time::Instant) {
    if let Ok(mut attempts) = state.login_attempts.lock() {
        let entry = attempts.entry(ip).or_insert((now, 0));
        entry.1 = entry.1.saturating_add(1);
    }
}

/// Serve one HTTP request.
fn serve_one(stream: &mut std::net::TcpStream, state: &State) {
    use std::io::Write;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let resp = match read_request(stream) {
        Ok(req) => {
            let scoped = req.path == "/m"
                || req.path.starts_with("/m/")
                || req.path == "/labels"
                || req.path.starts_with("/labels/");
            let peer = stream.peer_addr().ok().map(|a| a.ip());
            let now = std::time::Instant::now();
            if scoped && !peer.is_some_and(|ip| login_allowed(state, ip, now)) {
                Response::status(429, "<h1>Try again later</h1>".into())
            } else {
                let response = handle(&req, state);
                if scoped && response.status == 403 {
                    if let Some(ip) = peer {
                        login_failed(state, ip, now);
                    }
                }
                response
            }
        }
        Err(resp) => *resp,
    };
    let (content_type, bytes) = match &resp.raw_body {
        Some(raw) => (
            resp.content_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            raw.as_slice(),
        ),
        None => ("text/html; charset=utf-8".to_string(), resp.body.as_bytes()),
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        resp.status,
        reason(resp.status),
        bytes.len()
    );
    if let Some(loc) = &resp.location {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    if let Some(c) = &resp.set_cookie {
        head.push_str(&format!("Set-Cookie: {c}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(bytes);
}

/// Worker threads in the pool (bounded, std only).
pub const POOL_WORKERS: usize = 6;
/// Bounded accept queue: the accept loop blocks when all workers are busy
/// (backpressure instead of unbounded buffering).
pub const POOL_QUEUE: usize = 32;

/// Accept connections until `max_connections` is reached (`None` serves
/// forever). A bounded pool of [`POOL_WORKERS`] threads serves requests
/// concurrently; state is shared behind `Arc` (read-only clone) and the
/// `sync_channel` queue plus `Mutex` receiver are the only sync points.
pub fn serve(listener: std::net::TcpListener, state: &State, max_connections: Option<usize>) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<std::net::TcpStream>(POOL_QUEUE);
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    let state = std::sync::Arc::new(state.clone());
    let mut workers = Vec::with_capacity(POOL_WORKERS);
    for _ in 0..POOL_WORKERS {
        let rx = std::sync::Arc::clone(&rx);
        let state = std::sync::Arc::clone(&state);
        workers.push(std::thread::spawn(move || loop {
            let next = {
                let guard = rx.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.recv()
            };
            match next {
                Ok(mut s) => serve_one(&mut s, &state),
                Err(_) => break, // sender dropped: shutdown
            }
        }));
    }
    for (served, stream) in listener.incoming().enumerate() {
        match stream {
            Ok(s) => {
                if tx.send(s).is_err() {
                    break;
                }
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
        if max_connections.is_some_and(|m| served + 1 >= m) {
            break;
        }
    }
    drop(tx);
    for worker in workers {
        let _ = worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: English bookmark row JSON for the given id and text.
    fn bookmark(
        id: &str,
        text: &str,
        label: Option<&str>,
        media: Vec<serde_json::Value>,
    ) -> String {
        bookmark_lang(id, text, label, media, Some("en"))
    }

    /// Bookmark row with an explicit `lang` (`None` omits the key, so the
    /// offline heuristic decides).
    fn bookmark_lang(
        id: &str,
        text: &str,
        label: Option<&str>,
        media: Vec<serde_json::Value>,
        lang: Option<&str>,
    ) -> String {
        let media_json: Vec<String> = media.iter().map(ToString::to_string).collect();
        let label_json = label
            .map(|l| format!(",\"label\":{{\"parent_label\":\"{l}\"}}"))
            .unwrap_or_default();
        let lang_json = lang
            .map(|l| format!(",\"lang\":\"{l}\""))
            .unwrap_or_default();
        format!(
            "{{\"post_id\":\"{id}\",\"url\":\"https://example.invalid/a/status/{id}\",\
             \"text\":{},\"author_username\":\"syn_{id}\"{label_json},\"media\":[{media}]{lang}}}",
            serde_json::to_string(text).unwrap(),
            media = media_json.join(","),
            lang = lang_json,
        )
    }

    /// Test helper: photo media entry pointing at the given URL.
    fn photo(url: &str) -> serde_json::Value {
        serde_json::json!({"type": "photo", "url": url, "alt_text": "synthetic chart"})
    }

    /// Test helper: video media entry with preview and MP4 URLs.
    fn video(preview: &str, mp4: &str) -> serde_json::Value {
        serde_json::json!({"type": "video", "preview_image_url": preview, "video_url": mp4})
    }

    /// Temp data dir with p1 (try-it), p2 (HTML + media), p3 (plain).
    fn test_state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            bookmark(
                "p1",
                "[synthetic] Try this stretch: 1. Raise arms overhead 2. Hold 30 seconds 3. Repeat 4 rounds",
                Some("Health & Wellness"),
                Vec::new(),
            ),
            bookmark(
                "p2",
                "[synthetic] <script>alert('x')</script> Annotated chart below",
                Some("Rust & Systems"),
                vec![
                    photo("https://pbs.twimg.com/media/synthetic.jpg"),
                    video(
                        "https://pbs.twimg.com/thumb/synthetic.jpg",
                        "https://video.twimg.com/synthetic.mp4",
                    ),
                    photo("https://evil.example/x.jpg"),
                ],
            ),
            bookmark("p3", "[synthetic] Just a plain note", Some("Rust & Systems"), Vec::new()),
        ];
        std::fs::write(dir.path().join("store.jsonl"), rows.join("\n") + "\n").unwrap();
        let state = State::open(dir.path()).unwrap();
        (dir, state)
    }

    /// Test helper: verified decision-log entries for the given state.
    fn log_entries(state: &State) -> Vec<ancientos_core::log::Entry> {
        ancientos_core::log::DecisionLog::open(state.log_path())
            .unwrap()
            .verify()
            .unwrap()
    }

    #[test]
    fn log_access_requires_a_scoped_login() {
        let (_dir, mut state) = test_state();
        assert_eq!(handle(&Request::get("/log"), &state).status, 403);
        state.phone_token = Some("synthetic-log-credential".into());
        assert_eq!(
            handle(&Request::get("/log?k=synthetic-log-credential"), &state).status,
            303
        );
        assert_eq!(
            handle(
                &Request::get("/log").with_cookie("ap_k=synthetic-log-credential"),
                &state
            )
            .status,
            200
        );
    }

    #[test]
    fn unreadable_log_withholds_desktop_content_and_pending_counts() {
        let (_dir, state) = test_state();
        std::fs::write(state.log_path(), "corrupt decision log\n").unwrap();
        for path in ["/", "/ideas", "/try"] {
            let response = handle(&Request::get(path), &state);
            assert_eq!(response.status, 500);
            assert!(response.body.contains("log unreadable, hides not applied"));
            assert!(!response.body.contains("Try this stretch"));
            assert!(response.body.contains("Approvals (unavailable)"));
        }
    }

    #[test]
    fn login_failures_are_throttled_per_peer_and_expire() {
        let (_dir, state) = test_state();
        let ip = "127.0.0.1".parse().unwrap();
        let other = "127.0.0.2".parse().unwrap();
        let now = std::time::Instant::now();
        for _ in 0..5 {
            assert!(login_allowed(&state, ip, now));
            login_failed(&state, ip, now);
        }
        assert!(!login_allowed(&state, ip, now));
        assert!(login_allowed(&state, other, now));
        assert!(login_allowed(
            &state,
            ip,
            now + std::time::Duration::from_secs(61)
        ));
    }

    #[test]
    fn brief_renders_first_post_with_vote_form() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("post 1 of 3"), "{}", resp.body);
        assert!(resp.body.contains("Raise arms overhead"), "{}", resp.body);
        for v in [
            "approve",
            "skip",
            "never_post",
            "never_author",
            "never_category",
            "delegate",
        ] {
            assert!(resp.body.contains(&format!("value=\"{v}\"")), "{v} missing");
        }
        assert!(resp.body.contains("action=\"/vote\""), "{}", resp.body);
    }

    /// Test helper: record feedback votes into the state's decision log.
    fn seed_feedback(state: &State, votes: &[(&str, &str, bookmarks_agent::feedback::Verdict)]) {
        let mut log = ancientos_core::log::DecisionLog::open(state.log_path()).unwrap();
        for (id, cat, verdict) in votes {
            bookmarks_agent::feedback::record(
                &mut log,
                bookmarks_agent::feedback::FeedbackRecord {
                    pick_id: (*id).to_string(),
                    category: (*cat).to_string(),
                    verdict: *verdict,
                    tags: Vec::new(),
                },
                bookmarks_agent::feedback::ThresholdPolicy::default(),
            )
            .unwrap();
        }
    }

    #[test]
    fn brief_shows_why_this_is_here_with_reasons_when_ranker_active() {
        use bookmarks_agent::feedback::Verdict;
        let (_dir, state) = test_state();
        // Enough resolving votes to activate whatever the env minimum is.
        let min = bookmarks_agent::ranker::min_records_from_env().max(1);
        let mut votes = Vec::new();
        for i in 0..min {
            let (id, cat, verdict) = match i % 3 {
                0 => ("p1", "Health & Wellness", Verdict::Approve),
                1 => ("p3", "Rust & Systems", Verdict::Approve),
                _ => ("p2", "Rust & Systems", Verdict::Skip),
            };
            votes.push((id, cat, verdict));
        }
        seed_feedback(&state, &votes);
        let resp = handle(&Request::get("/"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("Why this is here"), "{}", resp.body);
        assert!(resp.body.contains("<li>"), "{}", resp.body);
        assert!(
            resp.body.contains("you approved") || resp.body.contains("author you often"),
            "{}",
            resp.body
        );
    }

    #[test]
    fn brief_omits_why_this_is_here_when_ranker_inactive() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/"), &state);
        assert_eq!(resp.status, 200);
        assert!(!resp.body.contains("Why this is here"), "{}", resp.body);
    }

    #[test]
    fn brief_second_post_via_query_and_thread_section() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/?i=1"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("post 2 of 3"), "{}", resp.body);
        assert!(resp.body.contains("Thread (1 post)"), "{}", resp.body);
        assert!(resp.body.contains("Annotated chart below"), "{}", resp.body);
    }

    #[test]
    fn brief_bad_and_missing_index() {
        let (_dir, state) = test_state();
        assert_eq!(handle(&Request::get("/?i=abc"), &state).status, 400);
        assert_eq!(handle(&Request::get("/?i=99"), &state).status, 404);
        let dir = tempfile::tempdir().unwrap();
        let empty = State::open(dir.path()).unwrap();
        let resp = handle(&Request::get("/"), &empty);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("No bookmarks"), "{}", resp.body);
    }

    #[test]
    fn brief_shows_try_it_steps_and_media() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/"), &state);
        assert!(resp.body.contains("[TRY IT]"), "{}", resp.body);
        assert!(resp.body.contains("Raise arms overhead"), "{}", resp.body);
        let resp = handle(&Request::get("/?i=1"), &state);
        assert!(
            resp.body
                .contains("<img src=\"https://pbs.twimg.com/media/synthetic.jpg\""),
            "{}",
            resp.body
        );
        assert!(resp.body.contains("Watch video"), "{}", resp.body);
        assert!(
            resp.body.contains("https://video.twimg.com/synthetic.mp4"),
            "{}",
            resp.body
        );
        assert!(
            !resp.body.contains("evil.example"),
            "unsafe URL leaked: {}",
            resp.body
        );
    }

    #[test]
    fn brief_try_it_badge_names_stage_with_reason_title_and_text() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/"), &state);
        assert!(resp.body.contains("[TRY IT]"), "{}", resp.body);
        // Badge: the stage is visible, the reason is visible and on `title`.
        assert!(
            resp.body.contains("<span class=\"badge-ok\" title=\""),
            "{}",
            resp.body
        );
        assert!(resp.body.contains(">rules</span>"), "{}", resp.body);
        assert!(
            resp.body.contains("topic &#39;Health &amp; Wellness&#39;"),
            "escaped reason missing: {}",
            resp.body
        );
        assert!(
            !resp.body.contains("Health & Wellness"),
            "unescaped reason leaked: {}",
            resp.body
        );
    }

    #[test]
    fn try_it_badge_escapes_reason_html() {
        let dir = tempfile::tempdir().unwrap();
        let row = bookmark(
            "evil1",
            "Try it: roll shoulders 10x, hold 20s each side.",
            Some("<script>alert('x')</script>"),
            Vec::new(),
        );
        std::fs::write(dir.path().join("store.jsonl"), row + "\n").unwrap();
        let state = State::open(dir.path()).unwrap();
        let resp = handle(&Request::get("/"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("[TRY IT]"), "{}", resp.body);
        assert!(
            !resp.body.contains("<script>alert"),
            "raw reason HTML leaked: {}",
            resp.body
        );
        assert!(
            resp.body
                .contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"),
            "escaped reason missing: {}",
            resp.body
        );
    }

    #[test]
    fn vote_post_appends_to_log_and_redirects() {
        let (_dir, state) = test_state();
        let resp = handle(
            &Request::post("/vote", "post_id=p3&verdict=approve&i=2"),
            &state,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/?i=0"));
        let entries = log_entries(&state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "feedback");
        assert_eq!(entries[0].payload["pick_id"], "p3");
        assert_eq!(entries[0].payload["verdict"], "approve");
        assert_eq!(entries[0].payload["category"], "Rust & Systems");
    }

    #[test]
    fn vote_delegate_queues_delegation() {
        let (_dir, state) = test_state();
        let resp = handle(
            &Request::post("/vote", "post_id=p1&verdict=delegate&i=0"),
            &state,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/?i=1"));
        let entries = log_entries(&state);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].payload["verdict"], "delegate");
        assert_eq!(entries[1].kind, "delegate.queued");
        assert_eq!(entries[1].payload["post_id"], "p1");
        // A delegate verdict also queues a wiki ingest (logged).
        assert_eq!(entries[2].kind, bookmarks_agent::wiki::WIKI_QUEUED_KIND);
        assert_eq!(entries[2].payload["post_id"], "p1");
    }

    #[test]
    fn vote_rejects_bad_input_without_logging() {
        let (_dir, state) = test_state();
        assert_eq!(
            handle(
                &Request::post("/vote", "post_id=p1&verdict=maybe&i=0"),
                &state
            )
            .status,
            400
        );
        assert_eq!(
            handle(
                &Request::post("/vote", "post_id=&verdict=approve&i=0"),
                &state
            )
            .status,
            400
        );
        assert_eq!(
            handle(&Request::post("/vote", "verdict=approve&i=0"), &state).status,
            400
        );
        assert!(
            !state.log_path().exists(),
            "bad votes must not create a log"
        );
    }

    #[test]
    fn vote_never_author_hides_author_from_brief_immediately() {
        let (_dir, state) = test_state();
        let resp = handle(
            &Request::post("/vote", "post_id=p1&verdict=never_author&i=0"),
            &state,
        );
        assert_eq!(resp.status, 303);
        let entries = log_entries(&state);
        assert_eq!(entries[0].payload["verdict"], "never_author");
        assert_eq!(entries[0].payload["pick_id"], "syn_p1");
        // Same State, no restart: p1 is gone, others unaffected.
        let resp = handle(&Request::get("/"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("post 1 of 2"), "{}", resp.body);
        assert!(!resp.body.contains("Raise arms overhead"), "{}", resp.body);
        assert!(resp.body.contains("Annotated chart below"), "{}", resp.body);
        let resp = handle(&Request::get("/?i=1"), &state);
        assert!(resp.body.contains("Just a plain note"), "{}", resp.body);
        // The store file is untouched: filtering is in memory only.
        let store = std::fs::read_to_string(state.store_path()).unwrap();
        assert!(store.contains("syn_p1"), "store must keep the author");
    }

    #[test]
    fn hidden_authors_page_lists_with_unhide_and_restores() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/hidden-authors"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("No hidden authors"), "{}", resp.body);
        assert_eq!(
            handle(
                &Request::post("/vote", "post_id=p2&verdict=never_author&i=1"),
                &state
            )
            .status,
            303
        );
        let resp = handle(&Request::get("/hidden-authors"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("syn_p2"), "{}", resp.body);
        assert!(
            resp.body.contains("action=\"/unhide-author\""),
            "{}",
            resp.body
        );
        assert!(resp.body.contains(">Unhide</button>"), "{}", resp.body);
        // POST unhide (different case/@ spelling) restores the post.
        let resp = handle(&Request::post("/unhide-author", "username=@SYN_P2"), &state);
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/hidden-authors"));
        // The log stays append-only: hide plus unhide, nothing rewritten.
        // (Checked before the final brief render, which marks try-it shown.)
        let entries = log_entries(&state);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].payload["verdict"], "unhide_author");
        let resp = handle(&Request::get("/hidden-authors"), &state);
        assert!(resp.body.contains("No hidden authors"), "{}", resp.body);
        let resp = handle(&Request::get("/"), &state);
        assert!(resp.body.contains("post 1 of 3"), "{}", resp.body);
    }

    #[test]
    fn unhide_rejects_bad_input_without_logging() {
        let (_dir, state) = test_state();
        assert_eq!(
            handle(&Request::post("/unhide-author", "username="), &state).status,
            400
        );
        assert_eq!(
            handle(&Request::post("/unhide-author", "tried=yes"), &state).status,
            400
        );
        assert!(
            !state.log_path().exists(),
            "bad unhides must not create a log"
        );
    }

    #[test]
    fn hiding_every_author_points_at_undo_page() {
        let (_dir, state) = test_state();
        let mut last = handle(
            &Request::post("/vote", "post_id=p1&verdict=never_author&i=0"),
            &state,
        );
        for (id, i) in [("p2", 0), ("p3", 0)] {
            last = handle(
                &Request::post("/vote", &format!("post_id={id}&verdict=never_author&i={i}")),
                &state,
            );
            assert_eq!(last.status, 303);
        }
        assert_eq!(last.location.as_deref(), Some("/hidden-authors"));
        let resp = handle(&Request::get("/"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("Every post is hidden"), "{}", resp.body);
        assert!(resp.body.contains("/hidden-authors"), "{}", resp.body);
    }

    #[test]
    fn try_page_skips_hidden_author_pending_without_ui_mention() {
        let (_dir, state) = test_state();
        assert_eq!(handle(&Request::get("/"), &state).status, 200);
        let mut log = ancientos_core::log::DecisionLog::open(state.log_path()).unwrap();
        // p2 is not try-it tagged, so mark it shown directly.
        bookmarks_agent::try_it::record_shown(&mut log, "p2", 1_700_000_000).unwrap();
        drop(log);
        // Hide p2's author via the real vote handler.
        assert_eq!(
            handle(
                &Request::post("/vote", "post_id=p2&verdict=never_author&i=1"),
                &state
            )
            .status,
            303
        );
        let resp = handle(&Request::get("/try"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("p1"), "{}", resp.body);
        assert!(!resp.body.contains("p2"), "no UI mention: {}", resp.body);
    }

    #[test]
    fn ideas_page_skips_hidden_author_cards_without_ui_mention() {
        let (_dir, state) = test_state();
        let card = |id: &str, source: &str| {
            serde_json::json!({
                "id": id,
                "source_url": source,
                "idea": format!("synthetic idea {id}"),
                "ancientos_mapping": "gate",
                "effort": "S",
                "status": "new",
                "created_at": 1_700_000_000u64,
            })
            .to_string()
        };
        std::fs::write(
            state.cards_path(),
            [
                card("card-p1", "https://example.invalid/a/status/p1"),
                card("card-p2", "https://example.invalid/a/status/p2"),
                card("card-ghost", "https://example.invalid/a/status/ghost"),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        assert_eq!(
            handle(
                &Request::post("/vote", "post_id=p1&verdict=never_author&i=0"),
                &state
            )
            .status,
            303
        );
        let resp = handle(&Request::get("/ideas"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("card-p2"), "{}", resp.body);
        assert!(
            resp.body.contains("card-ghost"),
            "unknown kept: {}",
            resp.body
        );
        assert!(
            !resp.body.contains("card-p1"),
            "no UI mention: {}",
            resp.body
        );
    }

    #[test]
    fn ideas_page_renders_cards_with_triage_radios() {
        let (_dir, state) = test_state();
        let out = "## Setups\n- synthetic setup\n## Worth borrowing for AncientOS\n\
                   - Drag a card to approve\n- Shared memory log with history\n";
        let made = bookmarks_agent::idea_card::capture_from_delegate(
            &state.cards_path(),
            "https://example.invalid/a/status/p1",
            out,
            100,
        )
        .unwrap();
        assert_eq!(made.len(), 2);
        let resp = handle(&Request::get("/ideas"), &state);
        assert_eq!(resp.status, 200);
        assert!(
            resp.body.contains("Drag a card to approve"),
            "{}",
            resp.body
        );
        assert!(
            resp.body.contains("Shared memory log with history"),
            "{}",
            resp.body
        );
        for d in ["adopt", "park", "drop"] {
            assert!(resp.body.contains(&format!("value=\"{d}\"")), "{d} missing");
        }
    }

    #[test]
    fn triage_adopt_updates_card_and_backlog() {
        let (_dir, state) = test_state();
        let out = "## Worth borrowing for AncientOS\n- Drag a card to approve\n";
        let made = bookmarks_agent::idea_card::capture_from_delegate(
            &state.cards_path(),
            "https://example.invalid/a/status/p1",
            out,
            100,
        )
        .unwrap();
        let resp = handle(
            &Request::post(
                "/ideas/triage",
                &format!("id={}&decision=adopt", made[0].id),
            ),
            &state,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/ideas"));
        let cards = bookmarks_agent::idea_card::load(&state.cards_path()).unwrap();
        assert_eq!(cards[0].status, bookmarks_agent::idea_card::Status::Adopted);
        let backlog = std::fs::read_to_string(state.backlog_path()).unwrap();
        assert!(backlog.contains("Drag a card to approve"), "{backlog}");
        assert!(handle(&Request::get("/ideas"), &state)
            .body
            .contains("No ideas due"));
    }

    #[test]
    fn triage_adopt_queues_wiki_ingest() {
        let (_dir, state) = test_state();
        let out = "## Worth borrowing for AncientOS\n- Drag a card to approve\n";
        let made = bookmarks_agent::idea_card::capture_from_delegate(
            &state.cards_path(),
            "https://example.invalid/a/status/p1",
            out,
            100,
        )
        .unwrap();
        let resp = handle(
            &Request::post(
                "/ideas/triage",
                &format!("id={}&decision=adopt", made[0].id),
            ),
            &state,
        );
        assert_eq!(resp.status, 303);
        let entries = log_entries(&state);
        let queued = entries
            .iter()
            .find(|e| e.kind == bookmarks_agent::wiki::WIKI_QUEUED_KIND)
            .expect("wiki.queued entry");
        assert_eq!(
            queued.payload["reason"],
            bookmarks_agent::wiki::QUEUE_REASON_ADOPT
        );
        assert_eq!(queued.payload["post_id"], "p1");
    }

    /// Seed one raw source plus one page (with HTML in the summary to prove
    /// escaping) in the state's wiki dir.
    fn seed_wiki_page(state: &State) {
        use bookmarks_agent::wiki;
        // Public class without naming the router crate (not a dependency).
        let public = bookmarks_agent::translate::class_of("synthetic public text");
        let dir = state.wiki_dir();
        wiki::write_raw(
            &dir,
            "p1",
            "https://example.invalid/a/status/p1",
            public,
            "2026-10-27",
            "synthetic source about the approval gate",
            &[],
            None,
        )
        .unwrap();
        let edits = vec![wiki::PageEdit {
            slug: "gate".to_string(),
            title: "Concept: gate".to_string(),
            summary: "Notes on the gate <script>alert('x')</script>".to_string(),
            claims: vec![wiki::ClaimEdit {
                text: "The gate reviews cards.".to_string(),
                sources: vec!["p1".to_string()],
            }],
            links: vec!["other".to_string()],
        }];
        let known = wiki::known_raw_ids(&dir);
        let (valid, rejected) = wiki::validate_edits(&edits, &known);
        assert!(rejected.is_empty());
        wiki::apply_edits(&dir, &valid, public).unwrap();
        wiki::sync_index(&dir).unwrap();
    }

    #[test]
    fn wiki_index_lists_pages_and_escapes() {
        let (_dir, state) = test_state();
        // Empty wiki: honest empty state.
        let resp = handle(&Request::get("/wiki"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("No wiki pages yet"), "{}", resp.body);
        seed_wiki_page(&state);
        let resp = handle(&Request::get("/wiki"), &state);
        assert_eq!(resp.status, 200);
        assert!(
            resp.body.contains("<a href=\"/wiki/gate\">[[gate]]</a>"),
            "{}",
            resp.body
        );
        assert!(resp.body.contains("(sources: 1)"), "{}", resp.body);
        // The HTML in the summary is escaped, never raw.
        assert!(!resp.body.contains("<script>"), "{}", resp.body);
        assert!(resp.body.contains("&lt;script&gt;"), "{}", resp.body);
        // The nav links to the wiki from every page.
        assert!(
            resp.body.contains("<a href=\"/wiki\">Wiki</a>"),
            "{}",
            resp.body
        );
    }

    #[test]
    fn wiki_show_renders_page_with_links_and_escapes() {
        let (_dir, state) = test_state();
        seed_wiki_page(&state);
        let resp = handle(&Request::get("/wiki/gate"), &state);
        assert_eq!(resp.status, 200);
        assert!(
            resp.body.contains("The gate reviews cards."),
            "{}",
            resp.body
        );
        assert!(
            resp.body.contains("<a href=\"/wiki/other\">[[other]]</a>"),
            "{}",
            resp.body
        );
        assert!(!resp.body.contains("<script>"), "{}", resp.body);
        assert!(resp.body.contains("Back to wiki"), "{}", resp.body);
        // Unknown slugs 404, unsafe names 400 (fail closed, no traversal).
        assert_eq!(handle(&Request::get("/wiki/ghost"), &state).status, 404);
        assert_eq!(handle(&Request::get("/wiki/..%2F.."), &state).status, 400);
        assert_eq!(handle(&Request::get("/wiki/"), &state).status, 400);
    }

    #[test]
    fn triage_rejects_bad_input() {
        let (_dir, state) = test_state();
        assert_eq!(
            handle(
                &Request::post("/ideas/triage", "id=nope&decision=adopt"),
                &state
            )
            .status,
            400
        );
        assert_eq!(
            handle(
                &Request::post("/ideas/triage", "id=x&decision=maybe"),
                &state
            )
            .status,
            400
        );
        assert_eq!(
            handle(&Request::post("/ideas/triage", "decision=adopt"), &state).status,
            400
        );
    }

    #[test]
    fn try_pending_flow_yes_no_later() {
        let (_dir, state) = test_state();
        // Rendering the brief marks the tagged pick shown.
        assert_eq!(handle(&Request::get("/"), &state).status, 200);
        let resp = handle(&Request::get("/try"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("p1"), "{}", resp.body);
        for t in ["yes", "no", "later"] {
            assert!(resp.body.contains(&format!("value=\"{t}\"")), "{t} missing");
        }
        let resp = handle(&Request::post("/try", "post_id=p1&tried=yes"), &state);
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/try"));
        let entries = log_entries(&state);
        assert!(entries.iter().any(|e| e.kind == "try_it.followup"
            && e.payload["tried"] == "yes"
            && e.payload["post_id"] == "p1"));
        assert!(handle(&Request::get("/try"), &state)
            .body
            .contains("No pending"));
    }

    #[test]
    fn try_page_shows_stage_badge_per_pending_post() {
        let (_dir, state) = test_state();
        // Rendering the brief marks the tagged pick shown (pending).
        assert_eq!(handle(&Request::get("/"), &state).status, 200);
        let resp = handle(&Request::get("/try"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("p1"), "{}", resp.body);
        assert!(
            resp.body.contains("<span class=\"badge-ok\" title=\""),
            "badge missing: {}",
            resp.body
        );
        assert!(resp.body.contains(">rules</span>"), "{}", resp.body);
        assert!(
            resp.body.contains("topic &#39;Health &amp; Wellness&#39;"),
            "escaped reason missing: {}",
            resp.body
        );
    }

    #[test]
    fn try_rejects_bad_input() {
        let (_dir, state) = test_state();
        assert_eq!(
            handle(&Request::post("/try", "post_id=p1&tried=maybe"), &state).status,
            400
        );
        assert_eq!(
            handle(&Request::post("/try", "tried=yes"), &state).status,
            400
        );
    }

    #[test]
    fn log_page_shows_verified_badge() {
        let (_dir, mut state) = test_state();
        state.phone_token = Some("synthetic-log-credential".into());
        assert_eq!(
            handle(
                &Request::post("/vote", "post_id=p3&verdict=skip&i=2"),
                &state
            )
            .status,
            303
        );
        let resp = handle(
            &Request::get("/log").with_cookie("ap_k=synthetic-log-credential"),
            &state,
        );
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("chain verified"), "{}", resp.body);
        assert!(resp.body.contains("feedback"), "{}", resp.body);
    }

    #[test]
    fn log_page_shows_tampered_badge() {
        let (_dir, mut state) = test_state();
        state.phone_token = Some("synthetic-log-credential".into());
        assert_eq!(
            handle(
                &Request::post("/vote", "post_id=p3&verdict=skip&i=2"),
                &state
            )
            .status,
            303
        );
        let body = std::fs::read_to_string(state.log_path()).unwrap();
        std::fs::write(state.log_path(), body.replacen("feedback", "xfeedback", 1)).unwrap();
        let resp = handle(
            &Request::get("/log").with_cookie("ap_k=synthetic-log-credential"),
            &state,
        );
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("TAMPERED at row 0"), "{}", resp.body);
    }

    #[test]
    fn post_text_is_html_escaped() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/?i=1"), &state);
        assert!(
            resp.body
                .contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"),
            "{}",
            resp.body
        );
        assert!(!resp.body.contains("<script>"), "raw script tag leaked");
    }

    #[test]
    fn unknown_route_is_404_and_unknown_method_is_405() {
        let (_dir, state) = test_state();
        assert_eq!(handle(&Request::get("/nope"), &state).status, 404);
        assert_eq!(handle(&Request::post("/nope", ""), &state).status, 404);
        assert_eq!(handle(&Request::post("/", ""), &state).status, 404);
        let req = Request {
            method: "DELETE".to_string(),
            path: "/".to_string(),
            query: BTreeMap::new(),
            body: Vec::new(),
            cookie: None,
        };
        assert_eq!(handle(&req, &state).status, 405);
    }

    #[test]
    fn bodies_over_64kb_are_rejected() {
        let (_dir, state) = test_state();
        let req = Request {
            method: "POST".to_string(),
            path: "/vote".to_string(),
            query: BTreeMap::new(),
            body: vec![b'x'; MAX_BODY_BYTES + 1],
            cookie: None,
        };
        assert_eq!(handle(&req, &state).status, 413);
    }

    #[test]
    fn non_loopback_bind_is_refused() {
        assert!(loopback_addr("127.0.0.1", 8787).is_ok());
        assert!(loopback_addr("localhost", 8787).is_ok());
        assert!(loopback_addr("::1", 8787).is_ok());
        for host in [
            "0.0.0.0",
            "192.168.1.1",
            "10.0.0.1",
            "example.com",
            "",
            "http://127.0.0.1",
        ] {
            assert!(loopback_addr(host, 8787).is_err(), "{host} must be refused");
        }
    }

    #[test]
    fn form_decoding_handles_plus_and_percent() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("p%33"), "p3");
        assert_eq!(percent_decode("100%"), "100%");
        let form = parse_pairs("post_id=p%33&verdict=approve&flag");
        assert_eq!(form.get("post_id").map(String::as_str), Some("p3"));
        assert!(!form.contains_key("flag"));
    }

    /// Test helper: raw request sent to the test server, response as text.
    fn http_roundtrip(addr: &std::net::SocketAddr, raw: &str) -> String {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn socket_roundtrip_serves_get_post_and_404() {
        let (_dir, state) = test_state();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || serve(listener, &state, Some(3)));
        let get = http_roundtrip(&addr, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(get.starts_with("HTTP/1.1 200 OK"), "{get}");
        assert!(get.contains("post 1 of 3"), "{get}");
        let body = "post_id=p3&verdict=skip&i=2";
        let post = http_roundtrip(
            &addr,
            &format!(
                "POST /vote HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(post.starts_with("HTTP/1.1 303 See Other"), "{post}");
        assert!(post.contains("Location: /?i=0"), "{post}");
        let missing = http_roundtrip(&addr, "GET /nope HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(missing.starts_with("HTTP/1.1 404 Not Found"), "{missing}");
        server.join().unwrap();
    }

    /// Temp data dir with deeper-valid ids (`demo-<digits>`) plus one
    /// non-digit id the deeper flow must refuse.
    fn deeper_state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            bookmark(
                "demo-9001",
                "[synthetic] Morning reset: 1. Stand up slowly 2. Stretch arms wide 3. Breathe deeply",
                Some("Health & Wellness"),
                Vec::new(),
            ),
            bookmark(
                "demo-9002",
                "[synthetic] <script>alert('x')</script> Just a plain note",
                Some("Rust & Systems"),
                Vec::new(),
            ),
            bookmark(
                "zz9",
                "[synthetic] Non-digit id",
                Some("Rust & Systems"),
                Vec::new(),
            ),
        ];
        std::fs::write(dir.path().join("store.jsonl"), rows.join("\n") + "\n").unwrap();
        let state = State::open(dir.path()).unwrap();
        (dir, state)
    }

    #[test]
    fn brief_has_go_deeper_button() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/"), &state);
        assert!(
            resp.body
                .contains("<form method=\"post\" action=\"/deeper/p1\">"),
            "{}",
            resp.body
        );
        assert!(resp.body.contains(">Go deeper</button>"), "{}", resp.body);
    }

    #[test]
    fn brief_has_later_form_with_datetime() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/"), &state);
        assert!(
            resp.body.contains("name=\"later\" value=\"1\""),
            "{}",
            resp.body
        );
        assert!(
            resp.body
                .contains("<input type=\"datetime-local\" name=\"at\">"),
            "{}",
            resp.body
        );
        assert!(resp.body.contains(">Later</button>"), "{}", resp.body);
    }

    #[test]
    fn deeper_later_queues_instead_of_running() {
        let (_dir, state) = deeper_state();
        let resp = handle(
            &Request::post("/deeper/demo-9001", "later=1&at=2030-05-01T09%3A30"),
            &state,
        );
        assert_eq!(resp.status, 303, "{}", resp.body);
        assert_eq!(resp.location.as_deref(), Some("/"));
        // Queued, not run: a deeper.queued entry, no walkthrough file.
        let entries = log_entries(&state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "deeper.queued");
        assert_eq!(entries[0].payload["post_id"], "demo-9001");
        assert_eq!(
            entries[0].payload["at"],
            bookmarks_agent::idea_card::parse_rfc3339("2030-05-01T09:30:00Z").unwrap()
        );
        assert!(!state.data_dir.join("deeper").join("demo-9001.md").exists());
        // With seconds, and full RFC 3339 with a zone, also queue.
        for body in [
            "later=1&at=2030-05-02T09%3A30%3A00",
            "later=1&at=2030-05-03T09%3A30%3A00%2B02%3A00",
        ] {
            let resp = handle(&Request::post("/deeper/demo-9002", body), &state);
            assert_eq!(resp.status, 303, "{}", resp.body);
        }
        assert_eq!(
            log_entries(&state)
                .iter()
                .filter(|e| e.kind == "deeper.queued")
                .count(),
            3
        );
    }

    #[test]
    fn deeper_later_refuses_bad_time_bad_flag_and_unknown() {
        let (_dir, state) = deeper_state();
        // Past, unparseable and missing times are refused.
        for body in [
            "later=1&at=2020-01-01T00%3A00",
            "later=1&at=someday",
            "later=1&at=",
            "later=1",
        ] {
            let resp = handle(&Request::post("/deeper/demo-9001", body), &state);
            assert_eq!(resp.status, 400, "{body}: {}", resp.body);
        }
        // Wrong flag value, and a time without the flag, are refused.
        for body in ["later=0&at=2030-05-01T09%3A30", "at=2030-05-01T09%3A30"] {
            let resp = handle(&Request::post("/deeper/demo-9001", body), &state);
            assert_eq!(resp.status, 400, "{body}: {}", resp.body);
        }
        assert!(!state.log_path().exists(), "refusals append nothing");
        // Unknown posts and bad ids still fail closed on the later path.
        assert_eq!(
            handle(
                &Request::post("/deeper/demo-9999", "later=1&at=2030-05-01T09%3A30"),
                &state
            )
            .status,
            404
        );
        assert_eq!(
            handle(
                &Request::post("/deeper/zz9", "later=1&at=2030-05-01T09%3A30"),
                &state
            )
            .status,
            400
        );
    }

    #[test]
    fn deeper_post_runs_offline_and_redirects_to_walkthrough() {
        let (_dir, state) = deeper_state();
        let resp = handle(&Request::post("/deeper/demo-9001", ""), &state);
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/deeper/demo-9001"));
        let md = state.data_dir.join("deeper").join("demo-9001.md");
        assert!(md.is_file(), "walkthrough file missing");
        let entries = log_entries(&state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "deeper.write");
        assert_eq!(entries[0].payload["mode"], "local");
        assert_eq!(entries[0].payload["post_id"], "demo-9001");
        let page = handle(&Request::get("/deeper/demo-9001"), &state);
        assert_eq!(page.status, 200);
        assert!(page.body.contains("Back to brief"), "{}", page.body);
        assert!(
            page.body.contains("Thread summary") || page.body.contains("Steps"),
            "{}",
            page.body
        );
    }

    #[test]
    fn deeper_rejects_bad_unknown_and_missing() {
        let (_dir, state) = deeper_state();
        // Valid shape but not in the store.
        assert_eq!(
            handle(&Request::post("/deeper/demo-9999", ""), &state).status,
            404
        );
        // In the store but not digits/demo-digits: the deeper flow refuses.
        assert_eq!(
            handle(&Request::post("/deeper/zz9", ""), &state).status,
            400
        );
        // Traversal and empty ids never reach the filesystem.
        assert_eq!(handle(&Request::post("/deeper/..", ""), &state).status, 400);
        assert_eq!(
            handle(&Request::post("/deeper/a/b", ""), &state).status,
            400
        );
        assert_eq!(handle(&Request::get("/deeper/.."), &state).status, 400);
        // Known post with no walkthrough yet.
        assert_eq!(
            handle(&Request::get("/deeper/demo-9002"), &state).status,
            404
        );
        // Nothing written, nothing logged.
        assert!(!state.data_dir.join("deeper").exists());
        assert!(!state.log_path().exists());
    }

    #[test]
    fn walkthrough_markdown_renders_readably_and_escaped() {
        let html = markdown_html(
            "# Title\n\n## Steps\n1. Do the thing\n2. Rest\n\n- note one\n\n<script>alert(1)</script>\n",
        );
        assert!(html.contains("<h1>Title</h1>"), "{html}");
        assert!(html.contains("<h2>Steps</h2>"), "{html}");
        assert!(
            html.contains("<ol><li>Do the thing</li><li>Rest</li></ol>"),
            "{html}"
        );
        assert!(html.contains("<ul><li>note one</li></ul>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn deeper_page_escapes_walkthrough_html() {
        let (_dir, state) = deeper_state();
        let dir = state.data_dir.join("deeper");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("demo-9002.md"),
            "# T\n<script>alert('x')</script>\n",
        )
        .unwrap();
        let resp = handle(&Request::get("/deeper/demo-9002"), &state);
        assert_eq!(resp.status, 200);
        assert!(
            resp.body
                .contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"),
            "{}",
            resp.body
        );
        assert!(!resp.body.contains("<script>"), "{}", resp.body);
    }

    #[test]
    fn deeper_end_to_end_never_leaks_raw_html() {
        let (_dir, state) = deeper_state();
        assert_eq!(
            handle(&Request::post("/deeper/demo-9002", ""), &state).status,
            303
        );
        let page = handle(&Request::get("/deeper/demo-9002"), &state);
        assert_eq!(page.status, 200);
        assert!(!page.body.contains("<script>"), "{}", page.body);
    }

    #[test]
    fn real_data_dir_bookmarks_jsonl_is_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bookmarks.jsonl"),
            "{\"post_id\":\"42\",\"url\":\"https://x.com/a/status/42\",\"text\":\"hello real\",\"lang\":\"en\"}\n",
        )
        .unwrap();
        let state = State::open(dir.path()).unwrap();
        assert_eq!(state.bookmarks.len(), 1);
    }

    /// Env guard for liveness tests: paid switch plus `X_CLIENT_ID` cleared
    /// or set under one local lock (web-ui tests run in their own process;
    /// clearing the client id guarantees no network even on a cache miss).
    struct PaidGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl PaidGuard {
        /// Test helper: held env lock plus snapshot of paid-related vars.
        fn capture() -> (
            std::sync::MutexGuard<'static, ()>,
            Vec<(&'static str, Option<String>)>,
        ) {
            static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let keys = [
                "ANCIENTOS_ALLOW_PAID",
                bookmarks_agent::liveness::OFFLINE_ENV_VAR,
                bookmarks_agent::liveness::CACHE_ENV_VAR,
                bookmarks_agent::translate::CACHE_ENV_VAR,
                "X_CLIENT_ID",
            ];
            let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
            (lock, saved)
        }

        /// Test helper: guard enabling paid mode with network vars cleared.
        fn paid_on() -> Self {
            let (lock, saved) = Self::capture();
            std::env::set_var("ANCIENTOS_ALLOW_PAID", "1");
            std::env::remove_var(bookmarks_agent::liveness::OFFLINE_ENV_VAR);
            std::env::remove_var(bookmarks_agent::liveness::CACHE_ENV_VAR);
            std::env::remove_var(bookmarks_agent::translate::CACHE_ENV_VAR);
            std::env::remove_var("X_CLIENT_ID");
            Self { _lock: lock, saved }
        }

        /// Test helper: guard disabling paid mode and clearing related vars.
        fn paid_off() -> Self {
            let (lock, saved) = Self::capture();
            std::env::remove_var("ANCIENTOS_ALLOW_PAID");
            std::env::remove_var(bookmarks_agent::liveness::OFFLINE_ENV_VAR);
            std::env::remove_var(bookmarks_agent::liveness::CACHE_ENV_VAR);
            std::env::remove_var(bookmarks_agent::translate::CACHE_ENV_VAR);
            Self { _lock: lock, saved }
        }
    }

    impl Drop for PaidGuard {
        /// Test helper: restores saved environment variables.
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// Test helper: writes given liveness statuses into the cache file.
    fn seed_cache(dir: &Path, entries: &[(&str, &str, Option<&str>, u64)]) {
        let mut map = serde_json::Map::new();
        for (id, status, reason, checked_at) in entries {
            let mut e = serde_json::json!({"status": status, "checked_at": checked_at});
            if let Some(r) = reason {
                e["reason"] = serde_json::json!(r);
            }
            map.insert((*id).to_string(), e);
        }
        std::fs::write(
            dir.join(bookmarks_agent::liveness::CACHE_FILE_NAME),
            serde_json::Value::Object(map).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn open_with_liveness_offline_keeps_everything_without_cache_writes() {
        let _env = PaidGuard::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            bookmark("dead1", "[synthetic] gone post", None, Vec::new()),
            bookmark("live1", "[synthetic] kept post", None, Vec::new()),
        ];
        std::fs::write(dir.path().join("store.jsonl"), rows.join("\n") + "\n").unwrap();
        // Offline (`--demo`) skips even with paid on: no network, no cache.
        let state = State::open_with_liveness(dir.path(), true).unwrap();
        assert_eq!(state.bookmarks.len(), 2);
        assert!(
            !dir.path()
                .join(bookmarks_agent::liveness::CACHE_FILE_NAME)
                .exists(),
            "offline must not write cache"
        );
    }

    #[test]
    fn open_with_liveness_paid_off_skips_without_network() {
        let _env = PaidGuard::paid_off();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("store.jsonl"),
            bookmark("p1", "[synthetic] post", None, Vec::new()) + "\n",
        )
        .unwrap();
        let state = State::open_with_liveness(dir.path(), false).unwrap();
        assert_eq!(state.bookmarks.len(), 1);
        assert!(!dir
            .path()
            .join(bookmarks_agent::liveness::CACHE_FILE_NAME)
            .exists());
    }

    #[test]
    fn open_with_liveness_drops_fresh_dead_without_deleting_or_logging() {
        let _env = PaidGuard::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            bookmark("dead1", "[synthetic] gone post", None, Vec::new()),
            bookmark("live1", "[synthetic] kept post", None, Vec::new()),
        ];
        let body = rows.join("\n") + "\n";
        std::fs::write(dir.path().join("store.jsonl"), &body).unwrap();
        // Fresh cache for every id: no lookup runs, so no network is possible.
        let now = now_secs();
        seed_cache(
            dir.path(),
            &[
                ("dead1", "unavailable", Some("deleted"), now),
                ("live1", "live", None, now),
            ],
        );
        let state = State::open_with_liveness(dir.path(), false).unwrap();
        let ids: Vec<&str> = state.bookmarks.iter().map(|b| b.post_id.as_str()).collect();
        assert_eq!(ids, vec!["live1"]);
        // The store file is byte-identical: nothing deleted.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("store.jsonl")).unwrap(),
            body
        );
        // No decision-log entry was written for the dead post.
        assert!(
            !state.log_path().exists(),
            "dead posts must not write log entries"
        );
        // And the brief page never mentions the dead post.
        let resp = handle(&Request::get("/"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("post 1 of 1"), "{}", resp.body);
        assert!(!resp.body.contains("dead1"), "{}", resp.body);
    }

    #[test]
    fn try_page_skips_fresh_dead_pending_without_ui_mention() {
        let _env = PaidGuard::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            bookmark(
                "dead1",
                "[synthetic] gone hands-on: 1. Do X 2. Do Y",
                None,
                Vec::new(),
            ),
            bookmark(
                "live1",
                "[synthetic] kept hands-on: 1. Do A 2. Do B",
                None,
                Vec::new(),
            ),
        ];
        std::fs::write(dir.path().join("store.jsonl"), rows.join("\n") + "\n").unwrap();
        let state = State::open(dir.path()).unwrap();
        assert_eq!(state.bookmarks.len(), 2);
        let mut log = ancientos_core::log::DecisionLog::open(state.log_path()).unwrap();
        bookmarks_agent::try_it::record_shown(&mut log, "dead1", 1_700_000_000).unwrap();
        bookmarks_agent::try_it::record_shown(&mut log, "live1", 1_700_000_000).unwrap();
        drop(log);
        let now = now_secs();
        seed_cache(
            dir.path(),
            &[("dead1", "unavailable", Some("deleted"), now)],
        );
        let resp = handle(&Request::get("/try"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("live1"), "{}", resp.body);
        assert!(!resp.body.contains("dead1"), "no UI mention: {}", resp.body);
    }

    #[test]
    fn open_keeps_non_english_with_translations_without_deleting_or_logging() {
        let _env = PaidGuard::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let es_text =
            "[synthetic] El gobierno presentó un plan económico para apoyar pequeñas empresas locales.";
        let fr_text = "[synthetic] Le gouvernement présente un plan économique pour soutenir les petites entreprises locales.";
        let rows = [
            bookmark_lang(
                "en1",
                "[synthetic] The government presented an economic plan to support small local businesses.",
                None,
                Vec::new(),
                None,
            ),
            bookmark_lang("en2", "[synthetic] short", None, Vec::new(), Some("en")),
            bookmark_lang("es1", es_text, None, Vec::new(), Some("es")),
            bookmark_lang("fr1", fr_text, None, Vec::new(), None),
        ];
        let body = rows.join("\n") + "\n";
        std::fs::write(dir.path().join("store.jsonl"), &body).unwrap();
        // Cached translation for es1 only; fr1 misses and stays Untranslated.
        let es_english = "[synthetic] Translated English for es1.";
        let mut tcache = std::collections::HashMap::new();
        tcache.insert(
            bookmarks_agent::translate::cache_key("es1", es_text),
            bookmarks_agent::translate::CacheEntry {
                english: es_english.to_string(),
                source_lang: Some("es".to_string()),
            },
        );
        let tcache_json = serde_json::to_string(&tcache).unwrap();
        std::fs::write(
            dir.path().join(bookmarks_agent::translate::CACHE_FILE_NAME),
            &tcache_json,
        )
        .unwrap();
        let state = State::open(dir.path()).unwrap();
        // Nothing hidden for language: all four surface (sorted by post_id).
        let ids: Vec<&str> = state.bookmarks.iter().map(|b| b.post_id.as_str()).collect();
        assert_eq!(ids, vec!["en1", "en2", "es1", "fr1"]);
        // The store file is byte-identical: nothing deleted.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("store.jsonl")).unwrap(),
            body
        );
        assert!(!state.log_path().exists());
        // en1 renders unchanged with no marker.
        let resp = handle(&Request::get("/"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("post 1 of 4"), "{}", resp.body);
        assert!(!resp.body.contains("Translated from"), "{}", resp.body);
        assert!(!resp.body.contains("Untranslated"), "{}", resp.body);
        // es1 (?i=2) shows the translation plus the marker and the original.
        let resp = handle(&Request::get("/?i=2"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("Translated from es"), "{}", resp.body);
        assert!(resp.body.contains(es_english), "{}", resp.body);
        assert!(resp.body.contains(es_text), "{}", resp.body);
        assert!(resp.body.contains("<details>"), "{}", resp.body);
        // fr1 (?i=3) has no cached translation: original plus the marker.
        let resp = handle(&Request::get("/?i=3"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("Untranslated (und)"), "{}", resp.body);
        assert!(resp.body.contains(fr_text), "{}", resp.body);
    }

    #[test]
    fn try_page_keeps_non_english_pending_with_translation() {
        let _env = PaidGuard::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let es_text =
            "[synthetic] El gobierno presentó un plan económico para apoyar pequeñas empresas locales.";
        let rows = [
            bookmark_lang(
                "en1",
                "[synthetic] kept hands-on: 1. Do A 2. Do B",
                None,
                Vec::new(),
                Some("en"),
            ),
            bookmark_lang("es1", es_text, None, Vec::new(), Some("es")),
        ];
        std::fs::write(dir.path().join("store.jsonl"), rows.join("\n") + "\n").unwrap();
        let es_english = "[synthetic] Translated English for es1.";
        let mut tcache = std::collections::HashMap::new();
        tcache.insert(
            bookmarks_agent::translate::cache_key("es1", es_text),
            bookmarks_agent::translate::CacheEntry {
                english: es_english.to_string(),
                source_lang: Some("es".to_string()),
            },
        );
        std::fs::write(
            dir.path().join(bookmarks_agent::translate::CACHE_FILE_NAME),
            serde_json::to_string(&tcache).unwrap(),
        )
        .unwrap();
        let state = State::open(dir.path()).unwrap();
        assert_eq!(state.bookmarks.len(), 2);
        let mut log = ancientos_core::log::DecisionLog::open(state.log_path()).unwrap();
        bookmarks_agent::try_it::record_shown(&mut log, "en1", 1_700_000_000).unwrap();
        bookmarks_agent::try_it::record_shown(&mut log, "es1", 1_700_000_000).unwrap();
        drop(log);
        let resp = handle(&Request::get("/try"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("en1"), "{}", resp.body);
        assert!(resp.body.contains("es1"), "{}", resp.body);
        assert!(resp.body.contains("Translated from es"), "{}", resp.body);
        assert!(resp.body.contains(es_english), "{}", resp.body);
        assert!(resp.body.contains(es_text), "{}", resp.body);
    }

    #[test]
    fn ideas_page_keeps_non_english_source_cards() {
        let _env = PaidGuard::paid_off();
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            bookmark_lang("en1", "[synthetic] kept post", None, Vec::new(), Some("en")),
            bookmark_lang(
                "es1",
                "[synthetic] post filtrado",
                None,
                Vec::new(),
                Some("es"),
            ),
        ];
        std::fs::write(dir.path().join("store.jsonl"), rows.join("\n") + "\n").unwrap();
        let card = |id: &str, source: &str| {
            serde_json::json!({
                "id": id,
                "source_url": source,
                "idea": format!("synthetic idea {id}"),
                "ancientos_mapping": "gate",
                "effort": "S",
                "status": "new",
                "created_at": 1_700_000_000u64,
            })
            .to_string()
        };
        let cards = [
            card("card-en", "https://example.invalid/a/status/en1"),
            card("card-es", "https://example.invalid/a/status/es1"),
            card("card-ghost", "https://example.invalid/a/status/ghost"),
        ];
        std::fs::write(dir.path().join("idea_cards.jsonl"), cards.join("\n") + "\n").unwrap();
        let state = State::open(dir.path()).unwrap();
        let resp = handle(&Request::get("/ideas"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("card-en"), "{}", resp.body);
        assert!(
            resp.body.contains("card-ghost"),
            "unknown kept: {}",
            resp.body
        );
        // Cards are never hidden for language: idea text is English.
        assert!(resp.body.contains("card-es"), "{}", resp.body);
    }

    #[test]
    fn pool_serves_two_concurrent_requests() {
        let (_dir, state) = deeper_state();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || serve(listener, &state, Some(2)));
        let other = std::thread::spawn(move || {
            http_roundtrip(&addr, "GET /?i=2 HTTP/1.1\r\nHost: x\r\n\r\n")
        });
        let first = http_roundtrip(&addr, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        let second = other.join().unwrap();
        assert!(first.starts_with("HTTP/1.1 200 OK"), "{first}");
        assert!(first.contains("post 1 of 3"), "{first}");
        assert!(second.starts_with("HTTP/1.1 200 OK"), "{second}");
        assert!(second.contains("post 3 of 3"), "{second}");
        server.join().unwrap();
    }

    /// Seed one approval request into this state's decision log.
    fn seed_approval(
        state: &State,
        summary: &str,
        draft: &str,
        now: u64,
        ttl: Option<u64>,
    ) -> ancientos_core::approvals::ApprovalRequest {
        let mut log = ancientos_core::log::DecisionLog::open(state.log_path()).unwrap();
        ancientos_core::approvals::request(
            &mut log,
            &bookmarks_agent::app_gate(),
            "bookmarks-agent",
            "x.post",
            summary,
            serde_json::json!({"draft": draft}),
            now,
            ttl,
        )
        .unwrap()
    }

    #[test]
    fn approvals_page_lists_pending_with_forms_nav_count_and_escaping() {
        let (_dir, state) = test_state();
        let now = now_secs();
        let a = seed_approval(&state, "synthetic web proposal one", "draft-a", now, None);
        let b = seed_approval(
            &state,
            "synthetic <script>alert('x')</script> & \"quotes\"",
            "draft-b",
            now,
            None,
        );
        let resp = handle(&Request::get("/approvals"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("<h1>Approvals</h1>"), "{}", resp.body);
        assert!(resp.body.contains("Approvals (2)"), "{}", resp.body);
        for r in [&a, &b] {
            for want in [
                r.id.as_str(),
                "bookmarks-agent",
                "x.post",
                &r.expires_at.to_string(),
                &format!("name=\"id\" value=\"{}\"", r.id),
                &format!("name=\"payload_hash\" value=\"{}\"", r.payload_hash),
            ] {
                assert!(resp.body.contains(want), "missing {want}: {}", resp.body);
            }
        }
        assert!(
            resp.body.contains("action=\"/approvals/approve\""),
            "{}",
            resp.body
        );
        assert!(resp.body.contains(">Approve</button>"), "{}", resp.body);
        assert!(
            resp.body.contains("action=\"/approvals/deny\""),
            "{}",
            resp.body
        );
        assert!(resp.body.contains(">Deny</button>"), "{}", resp.body);
        assert!(
            resp.body.contains(
                "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;quotes&quot;"
            ),
            "{}",
            resp.body
        );
        assert!(!resp.body.contains("<script>"), "raw script leaked");
        // The count travels with the shared nav on other pages too.
        let brief = handle(&Request::get("/"), &state);
        assert!(brief.body.contains("Approvals (2)"), "{}", brief.body);
    }

    #[test]
    fn approvals_page_empty_state() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/approvals"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("No pending approvals."), "{}", resp.body);
        assert!(resp.body.contains("No decisions yet."), "{}", resp.body);
        assert!(resp.body.contains("Approvals (0)"), "{}", resp.body);
    }

    #[test]
    fn approve_and_deny_handlers_record_and_list_decided() {
        let (_dir, state) = test_state();
        let now = now_secs();
        let a = seed_approval(&state, "synthetic web approve", "draft-c", now, None);
        let b = seed_approval(&state, "synthetic web deny", "draft-d", now, None);
        // Long-expired seed: shows under Recently decided without a sweep.
        let c = seed_approval(
            &state,
            "synthetic web stale",
            "draft-e",
            1_700_000_000,
            Some(60),
        );
        let resp = handle(
            &Request::post(
                "/approvals/approve",
                &format!("id={}&payload_hash={}", a.id, a.payload_hash),
            ),
            &state,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/approvals"));
        let resp = handle(
            &Request::post(
                "/approvals/deny",
                &format!(
                    "id={}&payload_hash={}&reason=needs+%3Cb%3Ework%3C%2Fb%3E",
                    b.id, b.payload_hash
                ),
            ),
            &state,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/approvals"));
        let entries = log_entries(&state);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[3].kind, "approval.approved");
        assert_eq!(entries[3].payload["decided_by"], "web-ui");
        assert_eq!(entries[4].kind, "approval.denied");
        assert_eq!(entries[4].payload["reason"], "needs <b>work</b>");
        let resp = handle(&Request::get("/approvals"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("Approvals (0)"), "{}", resp.body);
        assert!(resp.body.contains("No pending approvals."), "{}", resp.body);
        assert!(resp.body.contains("Recently decided"), "{}", resp.body);
        assert!(
            resp.body
                .contains(&format!("{} — approved by web-ui", a.id)),
            "{}",
            resp.body
        );
        assert!(
            resp.body.contains(&format!(
                "{} — denied by web-ui — needs &lt;b&gt;work&lt;/b&gt;",
                b.id
            )),
            "{}",
            resp.body
        );
        assert!(
            resp.body.contains(&format!("{} — expired by system", c.id)),
            "{}",
            resp.body
        );
        assert!(!resp.body.contains("<b>work</b>"), "raw HTML leaked");
    }

    #[test]
    fn stale_hash_unknown_and_bad_forms_rejected_without_state_change() {
        let (_dir, state) = test_state();
        let r = seed_approval(
            &state,
            "synthetic web stale-hash",
            "draft-f",
            now_secs(),
            None,
        );
        let bad = "0".repeat(64);
        let resp = handle(
            &Request::post(
                "/approvals/approve",
                &format!("id={}&payload_hash={bad}", r.id),
            ),
            &state,
        );
        assert_eq!(resp.status, 400);
        assert!(resp.body.contains("Stale approval"), "{}", resp.body);
        let resp = handle(
            &Request::post(
                "/approvals/deny",
                &format!("id={}&payload_hash={bad}", r.id),
            ),
            &state,
        );
        assert_eq!(resp.status, 400);
        assert!(resp.body.contains("Stale approval"), "{}", resp.body);
        let resp = handle(
            &Request::post(
                "/approvals/approve",
                &format!("id=deadbeefdeadbeef&payload_hash={bad}"),
            ),
            &state,
        );
        assert_eq!(resp.status, 400);
        assert!(resp.body.contains("Approval failed"), "{}", resp.body);
        assert_eq!(
            handle(
                &Request::post("/approvals/approve", "payload_hash=x"),
                &state
            )
            .status,
            400
        );
        assert_eq!(
            handle(&Request::post("/approvals/deny", "id=x"), &state).status,
            400
        );
        // Nothing was decided or appended.
        assert_eq!(log_entries(&state).len(), 1);
        let resp = handle(&Request::get("/approvals"), &state);
        assert!(resp.body.contains("Approvals (1)"), "{}", resp.body);
        assert!(resp.body.contains(&r.id), "{}", resp.body);
    }

    #[test]
    fn second_decision_is_rejected() {
        let (_dir, state) = test_state();
        let r = seed_approval(&state, "synthetic web twice", "draft-g", now_secs(), None);
        let form = format!("id={}&payload_hash={}", r.id, r.payload_hash);
        assert_eq!(
            handle(&Request::post("/approvals/approve", &form), &state).status,
            303
        );
        let resp = handle(&Request::post("/approvals/deny", &form), &state);
        assert_eq!(resp.status, 400);
        assert!(resp.body.contains("already decided"), "{}", resp.body);
        assert_eq!(log_entries(&state).len(), 2);
    }

    /// A browser-approved run still needs a token and the paid switch.
    #[test]
    fn approvals_page_lists_and_decides_a_muse_runs_proposal() {
        // Second app on the same core and log: a proposed Muse Code run shows
        // up and is decided here with no web-ui code specific to it.
        let (dir, state) = test_state();
        let prompt = dir.path().join("prompt.md");
        std::fs::write(&prompt, "synthetic prompt\n").unwrap();
        let spec = muse_runs::RunSpec::from_prompt("62", "synthetic web run", &prompt).unwrap();
        let mut log = ancientos_core::log::DecisionLog::open(state.log_path()).unwrap();
        let req = muse_runs::propose(&mut log, &spec, now_secs()).unwrap();
        let resp = handle(&Request::get("/approvals"), &state);
        assert!(resp.body.contains("Approvals (1)"), "{}", resp.body);
        assert!(resp.body.contains("muse-runs"), "{}", resp.body);
        assert!(resp.body.contains("Muse Code run #62"), "{}", resp.body);
        let resp = handle(
            &Request::post(
                "/approvals/approve",
                &format!("id={}&payload_hash={}", req.id, req.payload_hash),
            ),
            &state,
        );
        assert_eq!(resp.status, 303);
        let log = ancientos_core::log::DecisionLog::open(state.log_path()).unwrap();
        assert_eq!(
            muse_runs::status_of(&log, &req.id, now_secs()).unwrap(),
            ancientos_core::approvals::Status::Approved
        );
        // Approval in the browser records the decision only; running still
        // needs the paid switch (and never reaches real Muse in tests).
        let mut log = log;
        let runner = muse_runs::Runner {
            repo: dir.path().to_path_buf(),
            allow_paid: false,
        };
        let token =
            ancientos_core::approvals::claim_release_token(&log, &req.id, &req.payload_hash)
                .unwrap();
        assert!(muse_runs::run(&mut log, token, &runner).is_err());
    }

    #[test]
    fn approvals_page_shows_tampered_badge() {
        let (_dir, state) = test_state();
        seed_approval(&state, "synthetic web tamper", "draft-h", now_secs(), None);
        let body = std::fs::read_to_string(state.log_path()).unwrap();
        std::fs::write(state.log_path(), body.replacen("x.post", "x.POST", 1)).unwrap();
        let resp = handle(&Request::get("/approvals"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("TAMPERED at row 0"), "{}", resp.body);
    }

    /// Seed one approval per terminal state for `/stats` tests: pending,
    /// approved, denied, time-expired and released (distinct payloads so
    /// the pending dedupe never merges them).
    fn seed_all_approval_states(now: u64, log_path: &std::path::Path) {
        use ancientos_core::approvals::{approve, deny, release, request};
        use ancientos_core::log::DecisionLog;
        let mut log = DecisionLog::open(log_path).unwrap();
        let propose = |log: &mut DecisionLog, draft: &str, at: u64, ttl: Option<u64>| {
            request(
                log,
                &bookmarks_agent::app_gate(),
                "bookmarks-agent",
                "x.post",
                "synthetic stats proposal",
                serde_json::json!({"draft": draft}),
                at,
                ttl,
            )
            .unwrap()
        };
        propose(&mut log, "stats-pending", now, None);
        let approved = propose(&mut log, "stats-approved", now, None);
        let token = approve(
            &mut log,
            &approved.id,
            &approved.payload_hash,
            "synthetic-human",
            now,
        )
        .unwrap();
        drop(token);
        let denied = propose(&mut log, "stats-denied", now, None);
        deny(
            &mut log,
            &denied.id,
            &denied.payload_hash,
            "synthetic-human",
            "synthetic reason",
            now,
        )
        .unwrap();
        propose(&mut log, "stats-expired", 1_700_000_000, Some(60));
        let released = propose(&mut log, "stats-released", now, None);
        let token = approve(
            &mut log,
            &released.id,
            &released.payload_hash,
            "synthetic-human",
            now,
        )
        .unwrap();
        release(
            &mut log,
            token,
            &serde_json::json!({"draft": "stats-released"}),
        )
        .unwrap();
    }

    /// Seed a fully populated synthetic log for `/stats` tests: verdicts
    /// (one auto-approved category), every approval state, try-it funnel,
    /// cascade stages, daemon ticks and deeper jobs. Returns the seed `now`.
    fn seed_stats_log(state: &State) -> u64 {
        use ancientos_core::log::DecisionLog;
        use bookmarks_agent::feedback::{FeedbackRecord, ThresholdPolicy, Verdict};
        let now = now_secs();
        seed_all_approval_states(now, &state.log_path());
        let mut log = DecisionLog::open(state.log_path()).unwrap();
        for (id, cat, verdict) in [
            ("s1", "Synthetic Rust", Verdict::Approve),
            ("s2", "Synthetic Rust", Verdict::Approve),
            ("s3", "Synthetic Rust", Verdict::Approve),
            ("s4", "Synthetic Cooking", Verdict::Skip),
        ] {
            bookmarks_agent::feedback::record(
                &mut log,
                FeedbackRecord {
                    pick_id: id.to_string(),
                    category: cat.to_string(),
                    verdict,
                    tags: Vec::new(),
                },
                ThresholdPolicy::default(),
            )
            .unwrap();
        }
        bookmarks_agent::try_it::record_shown(&mut log, "p1", now).unwrap();
        bookmarks_agent::try_it::record_shown(&mut log, "p2", now).unwrap();
        bookmarks_agent::try_it::record_followup(
            &mut log,
            "p1",
            bookmarks_agent::try_it::Tried::Yes,
            None,
        )
        .unwrap();
        bookmarks_agent::try_it::record_followup(
            &mut log,
            "p2",
            bookmarks_agent::try_it::Tried::Later,
            Some("synthetic note"),
        )
        .unwrap();
        for (i, stage) in [
            "topic_filter",
            "rules",
            "rules",
            "jev",
            "muse",
            "fail_closed",
        ]
        .iter()
        .enumerate()
        {
            log.append(
                "try_it.cascade",
                serde_json::json!({"stage": stage, "post_id": format!("syn-{i}")}),
            )
            .unwrap();
        }
        for (agent, ok, at, error) in [
            ("brief", true, now, None),
            ("digest", false, now, Some("synthetic boom")),
            ("brief", true, now + 1, None),
        ] {
            let mut payload = serde_json::json!({"agent": agent, "ok": ok, "at": at});
            if let Some(e) = error {
                payload["error"] = serde_json::json!(e);
            }
            log.append(bookmarks_agent::daemon::DAEMON_TICK_KIND, payload)
                .unwrap();
        }
        bookmarks_agent::deeper::queue_later(&mut log, "demo-9001", now + 3600, now).unwrap();
        bookmarks_agent::deeper::queue_later(&mut log, "demo-9002", now, now).unwrap();
        log.append(
            bookmarks_agent::deeper::DEEPER_QUEUE_DONE_KIND,
            serde_json::json!({"post_id": "demo-9002", "at": now}),
        )
        .unwrap();
        now
    }

    /// Synthetic run log: two counted rows ($1.00 + $2.50) plus one `Not
    /// used` row the `demo.sh` rule excludes.
    fn write_stats_muse_log(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("muse-code-log.md");
        std::fs::write(
            &path,
            "# Synthetic run log\n\
             | 1 | 2026-10-26 | synthetic task one | Done; est. cost $1.00. |\n\
             | 2b | 2026-10-26 | synthetic scaffold | Not used: written directly. |\n\
             | 3 | 2026-10-26 | synthetic task two | Done; est. cost $2.50. |\n",
        )
        .unwrap();
        path
    }

    #[test]
    fn accuracy_panel_shows_synthetic_metrics_only() {
        let (_dir, state) = test_state();
        let resp = handle(&Request::get("/accuracy"), &state);
        assert_eq!(resp.status, 200);
        for want in [
            "<a href=\"/accuracy\">Accuracy</a>",
            "committed synthetic eval set",
            "Try-it classification",
            "Votes (personal ranker)",
            "Meaning (local embeddings)",
            "Votes + meaning (blend)",
        ] {
            assert!(resp.body.contains(want), "missing {want}: {}", resp.body);
        }
        let (_, cmp) = ancientos_eval::harness::synthetic_panel().unwrap();
        let blended = format!("{:.3}", cmp.blended_precision_at_5.unwrap());
        assert!(resp.body.contains(&blended), "{}", resp.body);
        // Aggregates only: no synthetic post ids or private paths.
        assert!(!resp.body.contains("eval-1"), "{}", resp.body);
        assert!(!resp.body.contains("eval-private"), "{}", resp.body);
    }

    #[test]
    fn stats_empty_dir_renders_with_nav_link() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::open(dir.path()).unwrap();
        state.muse_log_path = dir.path().join("missing.md");
        let resp = handle(&Request::get("/stats"), &state);
        assert_eq!(resp.status, 200);
        for want in [
            "<a href=\"/stats\">Stats</a>",
            "chain verified",
            "0 entries",
            "no cascade decisions logged",
            "No daemon ticks logged.",
            "Muse Code runs: unavailable",
        ] {
            assert!(resp.body.contains(want), "missing {want}: {}", resp.body);
        }
        assert!(
            resp.body.contains("<td>pending</td><td>0</td>"),
            "{}",
            resp.body
        );
        assert!(!resp.body.contains("withheld"), "{}", resp.body);
    }

    #[test]
    fn stats_populated_dir_renders_expected_numbers() {
        let (dir, mut state) = test_state();
        let now = seed_stats_log(&state);
        state.muse_log_path = write_stats_muse_log(dir.path());
        let resp = handle(&Request::get("/stats"), &state);
        assert_eq!(resp.status, 200);
        let total = log_entries(&state).len();
        for want in [
            "chain verified".to_string(),
            format!("{total} entries"),
            "<td>pending</td><td>1</td>".to_string(),
            "<td>approved</td><td>1</td>".to_string(),
            "<td>denied</td><td>1</td>".to_string(),
            "<td>expired</td><td>1</td>".to_string(),
            "<td>released</td><td>1</td>".to_string(),
            "<td>approve</td><td>3</td>".to_string(),
            "<td>skip</td><td>1</td>".to_string(),
            "<td>Synthetic Rust</td><td>3</td>".to_string(),
            "Auto-approved categories: Synthetic Rust".to_string(),
            "<td>shown</td><td>2</td>".to_string(),
            "<td>yes</td><td>1</td>".to_string(),
            "<td>no</td><td>0</td>".to_string(),
            "<td>later</td><td>1</td>".to_string(),
            "<td>pending</td><td>1</td>".to_string(),
            "<td>topic_filter</td><td>1</td>".to_string(),
            "<td>rules</td><td>2</td>".to_string(),
            "<td>jev</td><td>1</td>".to_string(),
            "<td>muse</td><td>1</td>".to_string(),
            "<td>fail_closed</td><td>1</td>".to_string(),
            format!("brief</td><td>ok at {}", now + 1),
            "digest</td><td>error: synthetic boom".to_string(),
            "1 outstanding (1 done)".to_string(),
            "Muse Code runs: 2 (est. cost $3.50)".to_string(),
        ] {
            assert!(resp.body.contains(&want), "missing {want}: {}", resp.body);
        }
        assert!(resp.body.contains("<svg"), "{}", resp.body);
        assert!(resp.body.contains("<rect"), "{}", resp.body);
        assert!(resp.body.contains("demo-9001"), "{}", resp.body);
        assert!(
            !resp.body.contains("demo-9002"),
            "done job must not list as queued: {}",
            resp.body
        );
        assert!(
            !resp.body.contains("no cascade decisions logged"),
            "{}",
            resp.body
        );
    }

    #[test]
    fn stats_broken_chain_shows_broken_and_withholds_rest() {
        let (dir, mut state) = test_state();
        seed_feedback(
            &state,
            &[
                (
                    "p1",
                    "Synthetic Rust",
                    bookmarks_agent::feedback::Verdict::Approve,
                ),
                (
                    "p2",
                    "Synthetic Rust",
                    bookmarks_agent::feedback::Verdict::Skip,
                ),
            ],
        );
        let body = std::fs::read_to_string(state.log_path()).unwrap();
        std::fs::write(state.log_path(), body.replacen("approve", "APPROVE", 1)).unwrap();
        state.muse_log_path = write_stats_muse_log(dir.path());
        let resp = handle(&Request::get("/stats"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("broken at line 0"), "{}", resp.body);
        assert_eq!(
            resp.body.matches("<p>withheld (fail closed)</p>").count(),
            5,
            "{}",
            resp.body
        );
        assert!(!resp.body.contains("chain verified"), "{}", resp.body);
        assert!(!resp.body.contains("No verdicts yet."), "{}", resp.body);
        // The independent run log still renders.
        assert!(
            resp.body.contains("Muse Code runs: 2 (est. cost $3.50)"),
            "{}",
            resp.body
        );
    }

    #[test]
    fn stats_escapes_dynamic_strings() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::open(dir.path()).unwrap();
        state.muse_log_path = dir.path().join("missing.md");
        seed_feedback(
            &state,
            &[(
                "p1",
                "<script>alert('x')</script>",
                bookmarks_agent::feedback::Verdict::Approve,
            )],
        );
        let mut log = ancientos_core::log::DecisionLog::open(state.log_path()).unwrap();
        log.append(
            bookmarks_agent::daemon::DAEMON_TICK_KIND,
            serde_json::json!({"agent": "brief", "ok": false, "at": 1, "error": "<b>bad</b>"}),
        )
        .unwrap();
        drop(log);
        let resp = handle(&Request::get("/stats"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("&lt;script&gt;"), "{}", resp.body);
        assert!(
            resp.body.contains("&lt;b&gt;bad&lt;/b&gt;"),
            "{}",
            resp.body
        );
        assert!(!resp.body.contains("<script>"), "{}", resp.body);
        assert!(!resp.body.contains("<b>bad</b>"), "{}", resp.body);
    }

    #[test]
    fn removed_on_x_never_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            r#"{"post_id":"r1","url":"","text":"[synthetic] kept"}"#,
            r#"{"post_id":"r2","url":"","text":"[synthetic] gone","x_bookmark_present":false}"#,
            r#"{"post_id":"r3","url":"","text":"[synthetic] present","x_bookmark_present":true}"#,
        ];
        std::fs::write(dir.path().join("store.jsonl"), rows.join("\n") + "\n").unwrap();
        let state = State::open(dir.path()).unwrap();
        let ids: Vec<&str> = state.bookmarks.iter().map(|b| b.post_id.as_str()).collect();
        assert_eq!(ids, vec!["r1", "r3"]);
    }

    #[test]
    fn stats_missing_muse_log_shows_unavailable() {
        let (dir, mut state) = test_state();
        seed_stats_log(&state);
        state.muse_log_path = dir.path().join("no-such-file.md");
        let resp = handle(&Request::get("/stats"), &state);
        assert_eq!(resp.status, 200);
        assert!(
            resp.body.contains("Muse Code runs: unavailable"),
            "{}",
            resp.body
        );
        // Log-derived sections still render.
        assert!(resp.body.contains("chain verified"), "{}", resp.body);
    }
}
