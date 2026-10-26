//! Loopback-only demo web UI: server-rendered HTML, no JS, std only.
//!
//! [`handle`] is a pure-ish `(Request, &State) -> Response` handler; `main.rs`
//! owns sockets and HTTP/1.1 parsing. No network or paid calls are ever made:
//! votes reuse `bookmarks_agent::vote`, triage reuses `idea_card`, try-it
//! follow-ups reuse `try_it`, and everything lands in files under the data
//! dir plus the append-only decision log.

use bookmarks_agent::fixture::Bookmark;
use std::collections::BTreeMap;
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
        }
    }

    /// Test helper: `POST` with a raw form-urlencoded body.
    pub fn post(path: &str, body: &str) -> Self {
        Self {
            method: "POST".to_string(),
            path: path.to_string(),
            query: BTreeMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }
}

/// Handler output; `main.rs` serializes it to HTTP/1.1 bytes.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub location: Option<String>,
    pub body: String,
}

impl Response {
    fn ok(body: String) -> Self {
        Self {
            status: 200,
            location: None,
            body,
        }
    }

    fn redirect(to: String) -> Self {
        Self {
            status: 303,
            location: Some(to),
            body: String::new(),
        }
    }

    /// Build an error response (used by the socket layer too).
    pub fn status(status: u16, body: String) -> Self {
        Self {
            status,
            location: None,
            body,
        }
    }
}

/// Serving state: the data dir plus the bookmark store loaded once at
/// startup (votes never change the store, so no reload is needed).
#[derive(Debug, Clone)]
pub struct State {
    pub data_dir: PathBuf,
    pub bookmarks: Vec<Bookmark>,
}

impl State {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, String> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| format!("data dir failed: {e}"))?;
        let mut bookmarks = match std::fs::read_to_string(dir.join("store.jsonl")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("store read failed: {e}")),
            Ok(text) => bookmarks_agent::fixture::parse_jsonl(&text)
                .map_err(|e| format!("store parse failed: {e}"))?,
        };
        bookmarks.sort_by(|a, b| a.post_id.cmp(&b.post_id));
        Ok(Self {
            data_dir: dir,
            bookmarks,
        })
    }

    pub fn store_path(&self) -> PathBuf {
        self.data_dir.join("store.jsonl")
    }

    pub fn log_path(&self) -> PathBuf {
        self.data_dir.join("decision_log.jsonl")
    }

    pub fn cards_path(&self) -> PathBuf {
        self.data_dir.join("idea_cards.jsonl")
    }

    pub fn backlog_path(&self) -> PathBuf {
        self.data_dir.join("backlog.md")
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

fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    match target.split_once('?') {
        Some((p, q)) => (percent_decode(p), parse_pairs(q)),
        None => (percent_decode(target), BTreeMap::new()),
    }
}

fn parse_body(body: &[u8]) -> BTreeMap<String, String> {
    parse_pairs(&String::from_utf8_lossy(body))
}

const CSS: &str = "body{font-family:system-ui,sans-serif;max-width:62em;margin:2em auto;\
     padding:0 1em;line-height:1.5;color:#111}\
     nav a{margin-right:1em}\
     .badge-ok{color:#fff;background:#137333;padding:.2em .6em;border-radius:.3em}\
     .badge-bad{color:#fff;background:#b3261e;padding:.2em .6em;border-radius:.3em}\
     .card{border:1px solid #ccc;padding:1em;margin:1em 0}\
     blockquote{border-left:3px solid #ccc;margin:1em 0;padding:.2em 1em}\
     img{max-width:100%}table{border-collapse:collapse}td,th{border:1px solid #ccc;\
     padding:.2em .6em;text-align:left;vertical-align:top}";

fn layout(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>{CSS}</style></head><body>\
         <nav><a href=\"/\">Brief</a><a href=\"/ideas\">Ideas</a>\
         <a href=\"/try\">Try it</a><a href=\"/log\">Log</a></nav><hr>{body}</body></html>",
        escape(title),
    )
}

fn error_page(title: &str, msg: &str) -> String {
    layout(
        title,
        &format!("<h1>{}</h1><p>{}</p>", escape(title), escape(msg)),
    )
}

fn not_found() -> Response {
    Response::status(404, error_page("Not found", "Unknown route."))
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
        format!("<h2>Media</h2>{items}")
    }
}

/// `[TRY IT]` section via the existing classifier and step extractor.
fn try_it_html(b: &Bookmark) -> String {
    let Some(tag) = bookmarks_agent::try_it::classify(b) else {
        return String::new();
    };
    let mut out = format!("<h2>[TRY IT] {}</h2>", escape(&tag.reason));
    let steps = bookmarks_agent::try_it::extract_steps(&b.text);
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

/// Mark a rendered tagged pick as shown (metadata only), once: entries
/// already in the log are never duplicated.
fn mark_shown(state: &State, post_id: &str) {
    let Ok(mut log) = ancientos_core::log::DecisionLog::open(state.log_path()) else {
        return;
    };
    let shown: bool = log.verify_or_empty().unwrap_or_default().iter().any(|e| {
        e.kind == bookmarks_agent::try_it::SHOWN_KIND
            && e.payload.get("post_id").and_then(|v| v.as_str()) == Some(post_id)
    });
    if !shown {
        let _ = log.append(
            bookmarks_agent::try_it::SHOWN_KIND,
            serde_json::json!({"post_id": post_id}),
        );
    }
}

fn page_brief(state: &State, query: &BTreeMap<String, String>) -> Response {
    let raw = query.get("i").map(String::as_str).unwrap_or("0");
    let idx: usize = match raw.parse() {
        Ok(n) => n,
        Err(_) => {
            return Response::status(400, error_page("Bad index", "Query ?i=N needs a number."))
        }
    };
    if state.bookmarks.is_empty() {
        return Response::ok(layout(
            "Brief",
            "<h1>Brief</h1><p>No bookmarks synced into this data dir yet.</p>",
        ));
    }
    let Some(bm) = state.bookmarks.get(idx) else {
        return Response::status(404, error_page("Not found", "No post at that index."));
    };
    let root = thread_post_of(bm);
    let candidates: Vec<_> = state
        .bookmarks
        .iter()
        .filter(|b| b.post_id != bm.post_id)
        .map(thread_post_of)
        .collect();
    let chain = bookmarks_agent::unroll::unroll(&root, &candidates);
    let try_section = try_it_html(bm);
    if !try_section.is_empty() {
        // Best-effort: a broken log must not break the brief (see /log).
        mark_shown(state, &bm.post_id);
    }
    let mut body = format!(
        "<h1>Brief — post {} of {}</h1>\
         <p><strong>{}</strong> {}</p><blockquote>{}</blockquote>",
        idx + 1,
        state.bookmarks.len(),
        escape(&bm.post_id),
        bm.author_username
            .as_deref()
            .map(|u| format!("by {}", escape(u)))
            .unwrap_or_default(),
        escape(&bm.text),
    );
    body.push_str(&thread_html(&chain));
    body.push_str(&media_html(&bm.media));
    body.push_str(&try_section);
    body.push_str(&format!(
        "<h2>Vote</h2><form method=\"post\" action=\"/vote\">\
         <input type=\"hidden\" name=\"post_id\" value=\"{}\">\
         <input type=\"hidden\" name=\"i\" value=\"{idx}\">\
         <label><input type=\"radio\" name=\"verdict\" value=\"approve\" checked> approve</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"skip\"> skip</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"never_post\"> never_post</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"never_category\"> never_category</label> \
         <label><input type=\"radio\" name=\"verdict\" value=\"delegate\"> delegate</label> \
         <button type=\"submit\">Vote</button></form>",
        escape(&bm.post_id),
    ));
    if idx + 1 < state.bookmarks.len() {
        body.push_str(&format!("<p><a href=\"/?i={}\">Next post</a></p>", idx + 1));
    }
    Response::ok(layout("Brief", &body))
}

fn do_vote(state: &State, body: &[u8]) -> Response {
    let form = parse_body(body);
    let post_id = form.get("post_id").map(String::as_str).unwrap_or("");
    if post_id.trim().is_empty() {
        return Response::status(400, error_page("Bad vote", "Missing post_id."));
    }
    let verdict: ancientos_core::feedback::Verdict = match form
        .get("verdict")
        .map(String::as_str)
        .unwrap_or("")
        .parse()
    {
        Ok(v) => v,
        Err(e) => return Response::status(400, error_page("Bad vote", &e)),
    };
    let idx: usize = form.get("i").and_then(|s| s.parse().ok()).unwrap_or(0);
    let store = state.store_path();
    let Some(store_str) = store.to_str() else {
        return Response::status(
            500,
            error_page("Vote failed", "Data dir path is not UTF-8."),
        );
    };
    if let Err(e) =
        bookmarks_agent::vote::run_vote(post_id, verdict, &[], Some(store_str), &state.log_path())
    {
        return Response::status(500, error_page("Vote failed", &e));
    }
    let next = if state.bookmarks.is_empty() {
        0
    } else {
        (idx + 1) % state.bookmarks.len()
    };
    Response::redirect(format!("/?i={next}"))
}

fn page_ideas(state: &State) -> Response {
    use bookmarks_agent::idea_card::list_due_for_triage;
    let now = bookmarks_agent::idea_card::now_secs();
    let cards = match list_due_for_triage(&state.cards_path(), now) {
        Ok(c) => c,
        Err(e) => return Response::status(500, error_page("Ideas failed", &e)),
    };
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
    Response::ok(layout("Ideas", &body))
}

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
                error_page("Bad triage", "Decision must be adopt, park or drop."),
            )
        }
    };
    if id.trim().is_empty() {
        return Response::status(400, error_page("Bad triage", "Missing id."));
    }
    let now = bookmarks_agent::idea_card::now_secs();
    match bookmarks_agent::idea_card::triage(
        &state.cards_path(),
        &state.backlog_path(),
        id,
        decision,
        now,
    ) {
        Ok(_) => Response::redirect("/ideas".to_string()),
        Err(e) if e.starts_with("no idea card") => {
            Response::status(400, error_page("Bad triage", &e))
        }
        Err(e) => Response::status(500, error_page("Triage failed", &e)),
    }
}

fn page_try(state: &State) -> Response {
    let pending: Vec<String> = match ancientos_core::log::DecisionLog::open(state.log_path()) {
        Ok(log) => bookmarks_agent::try_it::pending_followups(&log),
        Err(_) => Vec::new(),
    };
    let mut body = String::from("<h1>Try it — pending check-ins</h1>");
    if pending.is_empty() {
        body.push_str("<p>No pending try-it follow-ups.</p>");
    }
    for id in &pending {
        let text = state
            .bookmarks
            .iter()
            .find(|b| &b.post_id == id)
            .map(|b| escape(&b.text))
            .unwrap_or_else(|| "(not in store)".to_string());
        body.push_str(&format!(
            "<div class=\"card\"><p><strong>{}</strong></p><blockquote>{}</blockquote>\
             <form method=\"post\" action=\"/try\">\
             <input type=\"hidden\" name=\"post_id\" value=\"{}\">\
             <label><input type=\"radio\" name=\"tried\" value=\"yes\" checked> yes</label> \
             <label><input type=\"radio\" name=\"tried\" value=\"no\"> no</label> \
             <label><input type=\"radio\" name=\"tried\" value=\"later\"> later</label> \
             <button type=\"submit\">Record</button></form></div>",
            escape(id),
            text,
            escape(id),
        ));
    }
    Response::ok(layout("Try it", &body))
}

fn do_try(state: &State, body: &[u8]) -> Response {
    let form = parse_body(body);
    let post_id = form.get("post_id").map(String::as_str).unwrap_or("");
    if post_id.trim().is_empty() {
        return Response::status(400, error_page("Bad follow-up", "Missing post_id."));
    }
    let tried: bookmarks_agent::try_it::Tried =
        match form.get("tried").map(String::as_str).unwrap_or("").parse() {
            Ok(t) => t,
            Err(e) => return Response::status(400, error_page("Bad follow-up", &e)),
        };
    if let Err(e) = bookmarks_agent::try_it::run_tried(post_id, tried, None, &state.log_path()) {
        return Response::status(500, error_page("Follow-up failed", &e));
    }
    Response::redirect("/try".to_string())
}

fn tampered_page(row: usize) -> Response {
    Response::ok(layout(
        "Decision log",
        &format!(
            "<h1>Decision log</h1><p><span class=\"badge-bad\">TAMPERED at row {row}</span></p>\
             <p>The hash chain failed verification; entries are withheld (fail closed).</p>"
        ),
    ))
}

fn page_log(state: &State) -> Response {
    use ancientos_core::log::{DecisionLog, LogError};
    let log = match DecisionLog::open(state.log_path()) {
        Err(LogError::Chain(n)) => return tampered_page(n),
        Err(e) => return Response::status(500, error_page("Log failed", &e.to_string())),
        Ok(log) => log,
    };
    let entries = match log.verify_or_empty() {
        Err(LogError::Chain(n)) => return tampered_page(n),
        Err(e) => return Response::status(500, error_page("Log failed", &e.to_string())),
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
    Response::ok(layout("Decision log", &body))
}

/// Route one request. Pure over `(Request, &State)` apart from the file
/// appends that votes, triage and follow-ups require by design.
pub fn handle(req: &Request, state: &State) -> Response {
    if req.body.len() > MAX_BODY_BYTES {
        return Response::status(413, error_page("Too large", "Request body exceeds 64KB."));
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => page_brief(state, &req.query),
        ("POST", "/vote") => do_vote(state, &req.body),
        ("GET", "/ideas") => page_ideas(state),
        ("POST", "/ideas/triage") => do_triage(state, &req.body),
        ("GET", "/try") => page_try(state),
        ("POST", "/try") => do_try(state, &req.body),
        ("GET", "/log") => page_log(state),
        ("GET", _) | ("POST", _) => not_found(),
        _ => Response::status(
            405,
            error_page("Method not allowed", "Only GET and POST are served."),
        ),
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn bad_request(msg: &str) -> Response {
    Response::status(400, format!("<h1>Bad request</h1><p>{}</p>", escape(msg)))
}

/// Read one request: headers up to [`MAX_HEAD_BYTES`], body up to
/// [`MAX_BODY_BYTES`] (larger declared bodies get 413 without being read).
fn read_request(stream: &mut std::net::TcpStream) -> Result<Request, Response> {
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
        return Err(Response::status(
            405,
            "<h1>Method not allowed</h1>".to_string(),
        ));
    }
    if !target.starts_with('/') {
        return Err(bad_request("bad target"));
    }
    let mut content_length: usize = 0;
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
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(Response::status(
            413,
            "<h1>Content too large</h1><p>Body exceeds 64KB.</p>".to_string(),
        ));
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
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn serve_one(stream: &mut std::net::TcpStream, state: &State) {
    use std::io::Write;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let resp = match read_request(stream) {
        Ok(req) => handle(&req, state),
        Err(resp) => resp,
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n",
        resp.status,
        reason(resp.status),
        resp.body.len()
    );
    if let Some(loc) = &resp.location {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(resp.body.as_bytes());
}

/// Accept connections sequentially until `max_connections` is reached
/// (`None` serves forever). Single-threaded; fine for a local demo UI.
pub fn serve(listener: std::net::TcpListener, state: &State, max_connections: Option<usize>) {
    for (served, stream) in listener.incoming().enumerate() {
        match stream {
            Ok(mut s) => serve_one(&mut s, state),
            Err(e) => eprintln!("accept failed: {e}"),
        }
        if max_connections.is_some_and(|m| served + 1 >= m) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bookmark(
        id: &str,
        text: &str,
        label: Option<&str>,
        media: Vec<serde_json::Value>,
    ) -> String {
        let media_json: Vec<String> = media.iter().map(ToString::to_string).collect();
        let label_json = label
            .map(|l| format!(",\"label\":{{\"parent_label\":\"{l}\"}}"))
            .unwrap_or_default();
        format!(
            "{{\"post_id\":\"{id}\",\"url\":\"https://example.invalid/a/status/{id}\",\
             \"text\":{},\"author_username\":\"syn_{id}\"{label_json},\"media\":[{}]}}",
            serde_json::to_string(text).unwrap(),
            media_json.join(","),
        )
    }

    fn photo(url: &str) -> serde_json::Value {
        serde_json::json!({"type": "photo", "url": url, "alt_text": "synthetic chart"})
    }

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

    fn log_entries(state: &State) -> Vec<ancientos_core::log::Entry> {
        ancientos_core::log::DecisionLog::open(state.log_path())
            .unwrap()
            .verify()
            .unwrap()
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
            "never_category",
            "delegate",
        ] {
            assert!(resp.body.contains(&format!("value=\"{v}\"")), "{v} missing");
        }
        assert!(resp.body.contains("action=\"/vote\""), "{}", resp.body);
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
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].payload["verdict"], "delegate");
        assert_eq!(entries[1].kind, "delegate.queued");
        assert_eq!(entries[1].payload["post_id"], "p1");
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
        let (_dir, state) = test_state();
        assert_eq!(
            handle(
                &Request::post("/vote", "post_id=p3&verdict=skip&i=2"),
                &state
            )
            .status,
            303
        );
        let resp = handle(&Request::get("/log"), &state);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("chain verified"), "{}", resp.body);
        assert!(resp.body.contains("feedback"), "{}", resp.body);
    }

    #[test]
    fn log_page_shows_tampered_badge() {
        let (_dir, state) = test_state();
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
        let resp = handle(&Request::get("/log"), &state);
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
}
