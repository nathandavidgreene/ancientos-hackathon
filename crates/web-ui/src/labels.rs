//! Labels page: one card per eval-query post with its query and
//! classifier tags, thumbnails, media findings, and relevance labels.
//!
//! Routes (all behind the `ap_k` cookie, like `/m`, and failing closed):
//!
//! - `GET /labels` lists every post referenced by the eval queries exactly
//!   once, each with its matching categories (`q1..q5` plus `classify.jsonl`
//!   tags when present), thumbnails, accepted `analyze_media` findings, a
//!   Relevant/Not radio pair, and its own Delete button. `GET /labels?k=`
//!   logs in like `/m?k=` and sets a `/labels`-scoped cookie.
//! - `POST /labels/save` persists the submitted Relevant/Not labels
//!   through the bookmarks app endpoint
//!   (`bookmarks_agent::labels::save_labels`).
//! - `POST /labels/delete` removes one post via Social's single-post CLI
//!   (`remove_bookmark --post-id <id>`, one post per click, never
//!   batched). There is no phone approval: the click itself is the human
//!   authorization, and every attempt is logged. Failures keep the
//!   bookmark and its label and show the error.
//! - `GET /labels/thumb/<file>` serves thumbnail bytes, but only for
//!   files listed in the manifest (exact match, no traversal).
//!
//! The server still binds loopback only; reaching it from elsewhere is the
//! deployer's job (for example a tailnet-only reverse proxy, as with
//! `/m`). Rendered pages and errors never contain the token or any
//! filesystem path.
use crate::{escape, now_secs, parse_body, Request, Response, State};
use bookmarks_agent::labels::{self, Label, LabelQueries};
use std::collections::{BTreeMap, BTreeSet};

/// Cookie scope for labels logins (separate from `/m`'s).
pub const LOGIN_PATH: &str = "/labels";
/// Id of the shared save form (card inputs join it via `form=`).
const SAVE_FORM_ID: &str = "labels-save";
/// Largest thumbnail served (fail closed above this).
pub const MAX_THUMB_BYTES: usize = 5 * 1024 * 1024;

/// 404 page for labels routes when no eval dir is configured.
fn labels_off(state: &State) -> Response {
    Response::status(
        404,
        crate::error_page(
            "Labels are off",
            "No label data is configured for this server.",
            state,
        ),
    )
}

/// 403 page; says why without revealing anything about the data.
fn forbidden(state: &State) -> Response {
    let why = if state.labels_token.is_none() {
        "Labels are off (no token configured)."
    } else {
        "Missing or wrong token."
    };
    Response::status(403, crate::error_page("Forbidden", why, state))
}

/// 500 page for labels failures (messages name no paths by construction).
fn failed(title: &str, msg: &str, state: &State) -> Response {
    Response::status(500, crate::error_page(title, msg, state))
}

/// True for an `https://` url (the only shape rendered as a link).
fn linkable(url: &str) -> bool {
    url.starts_with("https://")
}

/// Post link: an `https://` url becomes an "Open on X" link, anything else
/// renders as plain text (never a guessed link).
fn post_link(url: &str) -> String {
    if linkable(url) {
        format!("<a href=\"{}\">Open on X</a>", escape(url))
    } else {
        "<span>no link</span>".to_string()
    }
}

/// Badge for the current label (`None` = not yet labeled).
fn label_badge(label: Option<Label>) -> String {
    match label {
        Some(Label::Relevant) => "<span class=\"badge-ok\">Relevant</span>".to_string(),
        Some(Label::Not) => "<span class=\"badge-bad\">Not</span>".to_string(),
        Some(Label::Delete) => "<span class=\"badge-bad\">Delete</span>".to_string(),
        None => "<span>Unlabeled</span>".to_string(),
    }
}

/// Query and classifier tags for one post card.
fn tags_html(
    queries: &LabelQueries,
    key: &str,
    status_id: Option<&str>,
    classify: &BTreeMap<String, BTreeSet<String>>,
) -> String {
    let mut out = String::new();
    for qid in queries.member_of(key) {
        let text = queries
            .queries
            .iter()
            .find(|q| q.id == qid)
            .map(|q| q.q.as_str())
            .unwrap_or("");
        out.push_str(&format!(
            "<span class=\"tag\" title=\"{}\">{}</span> ",
            escape(text),
            escape(&qid)
        ));
    }
    let mut cats = BTreeSet::new();
    if let Some(c) = classify.get(key) {
        cats.extend(c.iter().cloned());
    }
    if let Some(id) = status_id {
        if let Some(c) = classify.get(id) {
            cats.extend(c.iter().cloned());
        }
    }
    for cat in cats {
        out.push_str(&format!("<span class=\"tag cls\">{}</span> ", escape(&cat)));
    }
    if out.is_empty() {
        out.push_str("<span>no tags</span>");
    }
    out
}

/// Thumbnail `<img>` tags for one status id (empty when there are none).
fn thumbs_html(
    thumbs: &BTreeMap<String, Vec<labels::ThumbEntry>>,
    status_id: Option<&str>,
) -> String {
    let Some(id) = status_id else {
        return String::new();
    };
    let Some(entries) = thumbs.get(id) else {
        return String::new();
    };
    let mut out = String::from("<div class=\"media\">");
    for e in entries {
        let caption = if e.kind == "video" { " (video)" } else { "" };
        out.push_str(&format!(
            "<p><img src=\"/labels/thumb/{}\" alt=\"{}{}\"></p>",
            escape(&e.path),
            escape(&e.kind),
            caption,
        ));
    }
    out.push_str("</div>");
    out
}

/// Accepted media findings for one status id (empty when there are none).
fn findings_html(
    findings: &BTreeMap<String, Vec<labels::ShownFinding>>,
    status_id: Option<&str>,
) -> String {
    let Some(id) = status_id else {
        return String::new();
    };
    let Some(list) = findings.get(id) else {
        return String::new();
    };
    let mut out = String::from("<h3>Media findings</h3><ul>");
    for f in list {
        out.push_str(&format!(
            "<li><strong>{}</strong>: {}",
            escape(&f.kind),
            escape(&f.text)
        ));
        if let Some(q) = &f.quote {
            out.push_str(&format!(" <blockquote>{}</blockquote>", escape(q)));
        }
        out.push_str("</li>");
    }
    out.push_str("</ul>");
    out
}

/// One post card: header, tags, text, thumbs, findings, radios, Delete.
fn card_html(
    queries: &LabelQueries,
    key: &str,
    current: Option<Label>,
    thumbs: &BTreeMap<String, Vec<labels::ThumbEntry>>,
    findings: &BTreeMap<String, Vec<labels::ShownFinding>>,
    classify: &BTreeMap<String, BTreeSet<String>>,
) -> String {
    let post = &queries.posts[key];
    let status_id = labels::status_id_of_url(&post.url);
    let rel = current == Some(Label::Relevant);
    let not = current == Some(Label::Not);
    format!(
        "<div class=\"card\" id=\"post-{}\">\
         <h2>#{key} by {author} {badge}</h2>\
         <p>{tags}</p>\
         <blockquote>{text}</blockquote>\
         <p>{link}</p>\
         {thumbs}{findings}\
         <p><label><input type=\"radio\" form=\"{form}\" name=\"label-{key}\" \
         value=\"relevant\"{rel}> Relevant</label>\
         <label><input type=\"radio\" form=\"{form}\" name=\"label-{key}\" \
         value=\"not\"{not}> Not</label></p>\
         <form method=\"post\" action=\"/labels/delete\">\
         <input type=\"hidden\" name=\"post\" value=\"{key}\">\
         <input type=\"hidden\" name=\"request_id\" value=\"{request_id}\">\
         <button type=\"submit\">Delete</button></form>\
         </div>",
        escape(key),
        key = escape(key),
        author = escape(&post.author),
        badge = label_badge(current),
        tags = tags_html(queries, key, status_id.as_deref(), classify),
        text = escape(&post.text),
        link = post_link(&post.url),
        thumbs = thumbs_html(thumbs, status_id.as_deref()),
        findings = findings_html(findings, status_id.as_deref()),
        form = SAVE_FORM_ID,
        rel = if rel { " checked" } else { "" },
        not = if not { " checked" } else { "" },
        request_id = request_uuid(),
    )
}

/// `GET /labels`: log in via `?k=` (303 + scoped cookie, like `/m`), else
/// list every query post once with tags, thumbs, findings, and labels.
pub fn page_labels(req: &Request, state: &State) -> Response {
    let Some(dir) = state.labels_dir.as_deref() else {
        return labels_off(state);
    };
    if let Some(k) = req.query.get("k") {
        if !crate::phone::token_matches(state.labels_token.as_deref(), Some(k)) {
            return forbidden(state);
        }
        let mut resp = Response::redirect("/labels".to_string());
        resp.set_cookie = Some(crate::phone::login_cookie_scoped(k, LOGIN_PATH));
        return resp;
    }
    if !crate::phone::cookie_matches(state.labels_token.as_deref(), req) {
        return forbidden(state);
    }
    let queries = match labels::load_queries(dir) {
        Ok(q) => q,
        Err(e) => return failed("Labels failed", &e, state),
    };
    let stored = match labels::load_labels(dir) {
        Ok(l) => l,
        Err(e) => return failed("Labels failed", &e, state),
    };
    let thumbs = labels::load_thumb_manifest(dir);
    let findings = labels::load_findings(dir);
    let classify = labels::load_classify(dir);
    let keys = queries.ordered_keys();
    let labeled = keys.iter().filter(|k| stored.contains_key(*k)).count();
    let mut body = format!(
        "<h1>Labels ({} post{}, {} labeled)</h1>\
         <ul>{}</ul>\
         <form id=\"{form}\" method=\"post\" action=\"/labels/save\">\
         <button type=\"submit\">Save labels</button></form>",
        keys.len(),
        if keys.len() == 1 { "" } else { "s" },
        labeled,
        queries
            .queries
            .iter()
            .map(|q| format!(
                "<li><strong>{}</strong>: {}</li>",
                escape(&q.id),
                escape(&q.q)
            ))
            .collect::<String>(),
        form = SAVE_FORM_ID,
    );
    for key in &keys {
        body.push_str(&card_html(
            &queries,
            key,
            stored.get(key).map(|r| r.label),
            &thumbs,
            &findings,
            &classify,
        ));
    }
    body.push_str(&format!(
        "<p><button type=\"submit\" form=\"{SAVE_FORM_ID}\">Save labels</button></p>"
    ));
    Response::ok(crate::layout("Labels", &body, state))
}

/// `POST /labels/save`: persist submitted Relevant/Not labels through the
/// bookmarks app endpoint; any bad value or unknown key rejects the whole
/// save and writes nothing.
pub fn save_labels(req: &Request, state: &State) -> Response {
    let Some(dir) = state.labels_dir.as_deref() else {
        return labels_off(state);
    };
    if !crate::phone::cookie_matches(state.labels_token.as_deref(), req) {
        return forbidden(state);
    }
    let form = parse_body(&req.body);
    let mut items: Vec<(String, Label)> = Vec::new();
    for (name, value) in &form {
        let Some(key) = name.strip_prefix("label-") else {
            continue;
        };
        if key.is_empty() {
            return Response::status(
                400,
                crate::error_page("Bad request", "Empty label key.", state),
            );
        }
        match Label::parse(value) {
            Some(label @ (Label::Relevant | Label::Not)) => {
                items.push((key.to_string(), label));
            }
            _ => {
                return Response::status(
                    400,
                    crate::error_page("Bad request", "Labels must be relevant or not.", state),
                );
            }
        }
    }
    let queries = match labels::load_queries(dir) {
        Ok(q) => q,
        Err(e) => return failed("Save failed", &e, state),
    };
    if items
        .iter()
        .any(|(key, _)| !queries.posts.contains_key(key))
    {
        return Response::status(
            400,
            crate::error_page("Bad request", "Unknown post.", state),
        );
    }
    match labels::save_labels(dir, &items, now_secs()) {
        Ok(_) => Response::redirect("/labels".to_string()),
        Err(e) => failed("Save failed", &e, state),
    }
}

/// `POST /labels/delete`: remove the one posted post via Social's CLI (no
/// phone approval; the click is the authorization). Success records the
/// `delete` label; failure keeps the bookmark and its label and shows the
/// error. Already-removed posts just converge the label and redirect.
pub fn delete_post(req: &Request, state: &State) -> Response {
    let Some(dir) = state.labels_dir.as_deref() else {
        return labels_off(state);
    };
    if !crate::phone::cookie_matches(state.labels_token.as_deref(), req) {
        return forbidden(state);
    }
    let form = parse_body(&req.body);
    let key = form.get("post").map(String::as_str).unwrap_or("").trim();
    if key.is_empty() {
        return Response::status(
            400,
            crate::error_page("Bad request", "Missing post.", state),
        );
    }
    let queries = match labels::load_queries(dir) {
        Ok(q) => q,
        Err(e) => return failed("Delete failed", &e, state),
    };
    if !queries.posts.contains_key(key) {
        return Response::status(
            400,
            crate::error_page("Bad request", "Unknown post.", state),
        );
    }
    let request_id = form.get("request_id").map(String::as_str).unwrap_or("");
    let post_id = labels::status_id_of_url(&queries.posts[key].url).unwrap_or_default();
    if post_id.is_empty() || !post_id.bytes().all(|b| b.is_ascii_digit()) {
        return Response::status(
            400,
            crate::error_page("Bad request", "Post id must contain digits only.", state),
        );
    }
    let now = now_secs();
    let path_env = std::env::var("PATH").ok();
    match labels::remove_one(
        dir,
        key,
        state.labels_remove_bin.as_deref(),
        path_env.as_deref(),
        now,
        request_id,
    ) {
        Ok(_) => match labels::save_labels(dir, &[(key.to_string(), Label::Delete)], now) {
            Ok(_) => Response::redirect("/labels".to_string()),
            Err(e) => failed(
                "Delete half-saved",
                &format!("Removed on X, but the label was not saved: {e}"),
                state,
            ),
        },
        Err(e) => failed("Delete failed", &format!("{e} Bookmark kept."), state),
    }
}

/// Generate a fresh UUID v4 for a rendered Delete form.
fn request_uuid() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("operating system random source available");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Content type for a thumbnail file name; `None` for anything but
/// jpeg/png (fail closed).
fn thumb_content_type(name: &str) -> Option<&'static str> {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        _ => None,
    }
}

/// `GET /labels/thumb/<file>`: serve thumbnail bytes only for files listed
/// in the manifest (exact match; anything with a separator, an unknown
/// extension, or no manifest row is a 404).
pub fn serve_thumb(req: &Request, state: &State, name: &str) -> Response {
    let Some(dir) = state.labels_dir.as_deref() else {
        return labels_off(state);
    };
    if !crate::phone::cookie_matches(state.labels_token.as_deref(), req) {
        return forbidden(state);
    }
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return crate::not_found(state);
    }
    let Some(content_type) = thumb_content_type(name) else {
        return crate::not_found(state);
    };
    if !labels::thumb_allow_list(dir).contains(name) {
        return crate::not_found(state);
    }
    match std::fs::read(dir.join(labels::THUMBS_DIR).join(name)) {
        Ok(bytes) if bytes.len() <= MAX_THUMB_BYTES => Response::bytes(content_type, bytes),
        Ok(_) => failed("Thumbnail failed", "Thumbnail too large.", state),
        Err(_) => failed("Thumbnail failed", "Thumbnail could not be read.", state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Synthetic phone token shared by the labels fixtures.
    const TOKEN: &str = "synthetic-labels-token-0002";
    /// Cookie header carrying the synthetic token.
    const COOKIE: &str = "ap_k=synthetic-labels-token-0002";
    /// X status id of synthetic post 7.
    const ID7: &str = "9000000000000000007";
    /// X status id of synthetic post 9.
    const ID9: &str = "9000000000000000009";

    /// Synthetic label-queries file: q1/q2 share post 7; post 11 is in the
    /// map but in no query, so it never surfaces. All text is invented.
    fn queries_fixture() -> String {
        serde_json::json!({
            "source": "synthetic",
            "queries": [
                {"id": "q1", "q": "synthetic sleep tips", "posts": [7, 9]},
                {"id": "q2", "q": "synthetic local agents", "posts": [7]},
            ],
            "posts": {
                "7": {"a": "syn_sleeper", "t": "synthetic post about naps",
                      "u": format!("https://x.com/syn_sleeper/status/{ID7}")},
                "9": {"a": "syn_cook", "t": "synthetic post about soup",
                      "u": format!("https://x.com/syn_cook/status/{ID9}")},
                "11": {"a": "syn_broken", "t": "synthetic post with bad url",
                       "u": "not-a-status-url"},
            },
        })
        .to_string()
    }

    /// Temp eval-private dir with queries, thumbs (+manifest), findings,
    /// and classify rows. `evil.gif` is manifest-listed but unservable;
    /// `gone.jpg` is manifest-listed but missing on disk.
    fn eval_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("nathan")).unwrap();
        std::fs::write(dir.path().join(labels::QUERIES_REL), queries_fixture()).unwrap();
        std::fs::create_dir_all(dir.path().join(labels::THUMBS_DIR)).unwrap();
        std::fs::write(
            dir.path().join(labels::THUMB_MANIFEST_REL),
            format!(
                "{{\"post_id\":\"{ID7}\",\"index\":0,\"kind\":\"photo\",\"path\":\"a_0.jpg\"}}\n\
                 {{\"post_id\":\"{ID9}\",\"index\":0,\"kind\":\"photo\",\"path\":\"b_0.png\"}}\n\
                 {{\"post_id\":\"{ID7}\",\"index\":1,\"kind\":\"photo\",\"path\":\"evil.gif\"}}\n\
                 {{\"post_id\":\"{ID9}\",\"index\":1,\"kind\":\"photo\",\"path\":\"gone.jpg\"}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(labels::THUMBS_DIR).join("a_0.jpg"),
            b"fake-jpeg-bytes-7",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(labels::THUMBS_DIR).join("b_0.png"),
            b"fake-png-bytes-9",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(labels::FINDINGS_FILE),
            format!(
                "{{\"status\":\"accepted\",\"finding\":{{\"kind\":\"text\",\"text\":\"synthetic ocr\",\"quote\":\"nap\",\"provenance\":{{\"post_id\":\"{ID7}\",\"media_key\":\"k\",\"media_url\":\"u\"}}}}}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(labels::CLASSIFY_FILE),
            format!(
                "{{\"post_id\":\"{ID7}\",\"category\":\"sleep\"}}\n\
                 {{\"post_id\":\"9\",\"category\":\"rest\"}}\n"
            ),
        )
        .unwrap();
        dir
    }

    /// Test helper: state with the phone token and labels pointed at the
    /// given eval dir.
    fn labels_state(data: &std::path::Path, eval: &std::path::Path) -> State {
        let mut s = State::open(data).unwrap();
        s.labels_token = crate::phone::token_from(Some(TOKEN.to_string()));
        s.labels_dir = Some(eval.to_path_buf());
        s
    }

    /// Test helper: count non-overlapping occurrences of `needle`.
    fn count(body: &str, needle: &str) -> usize {
        body.matches(needle).count()
    }

    /// Test helper: write `body` to `name` under `dir`, executable (unix).
    ///
    /// The executable itself is never opened for writing by this (test)
    /// process: the body goes to a sibling `.src` file and a `cp` child
    /// creates the executable. Writing it directly raced with other test
    /// threads forking concurrently: a child forked while our write fd was
    /// open inherits it until its exec, so exec'ing the script could fail
    /// with ETXTBSY ("Text file busy"). Only `cp` ever holds a write fd
    /// on the executable, and it exits before we return.
    #[cfg(unix)]
    fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let src = dir.join(format!("{name}.src"));
        std::fs::write(&src, body.as_bytes()).unwrap();
        let st = std::process::Command::new("cp")
            .arg(&src)
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "cp failed: {st}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Test helper: fake `remove_bookmark` recording its args to
    /// `calls_file`, exiting `code` with `stderr` on stderr.
    #[cfg(unix)]
    fn fake_remove_bin(
        dir: &std::path::Path,
        calls_file: &std::path::Path,
        code: i32,
        stderr: &str,
    ) -> std::path::PathBuf {
        write_executable(
            dir,
            "remove_bookmark",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n[ -n '{}' ] && printf '%s' '{}' >&2\nexit {code}\n",
                calls_file.display(),
                stderr,
                stderr,
            ),
        )
    }

    /// Test helper: one raw HTTP round trip, returning raw response bytes.
    fn http_roundtrip_bytes(addr: &std::net::SocketAddr, raw: &str) -> Vec<u8> {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        out
    }

    /// Without an eval dir every labels route is 404, even authorized.
    #[test]
    fn phone_credential_cannot_authorize_separate_labels_scope() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut state = labels_state(data.path(), eval.path());
        state.phone_token = Some("different-phone-credential-123".into());
        let req = Request::get("/labels?k=different-phone-credential-123");
        assert_eq!(crate::handle(&req, &state).status, 403);
        assert_eq!(
            crate::handle(&Request::get(&format!("/labels?k={TOKEN}")), &state).status,
            303
        );
    }

    #[test]
    fn off_without_eval_dir() {
        let data = tempfile::tempdir().unwrap();
        let mut s = State::open(data.path()).unwrap();
        s.labels_token = crate::phone::token_from(Some(TOKEN.to_string()));
        assert_eq!(
            crate::handle(&Request::get("/labels").with_cookie(COOKIE), &s).status,
            404
        );
        assert_eq!(
            crate::handle(
                &Request::post("/labels/save", "label-7=relevant").with_cookie(COOKIE),
                &s
            )
            .status,
            404
        );
        assert_eq!(
            crate::handle(
                &Request::post(
                    "/labels/delete",
                    "post=7&request_id=00000000-0000-4000-8000-000000000001"
                )
                .with_cookie(COOKIE),
                &s
            )
            .status,
            404
        );
        assert_eq!(
            crate::handle(
                &Request::get("/labels/thumb/a_0.jpg").with_cookie(COOKIE),
                &s
            )
            .status,
            404
        );
    }

    /// Missing or wrong cookies are 403 on every labels route.
    #[test]
    fn missing_or_wrong_token_is_403() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let authed = |t: crate::Request| crate::handle(&t, &s).status;
        assert_eq!(authed(Request::get("/labels")), 403);
        assert_eq!(
            authed(Request::post("/labels/save", "label-7=relevant")),
            403
        );
        assert_eq!(
            authed(Request::post(
                "/labels/delete",
                "post=7&request_id=00000000-0000-4000-8000-000000000001"
            )),
            403
        );
        assert_eq!(authed(Request::get("/labels/thumb/a_0.jpg")), 403);
        for bad in ["ap_k=wrong-token-00000000", "ap_k=", "other=x"] {
            assert_eq!(authed(Request::get("/labels").with_cookie(bad)), 403);
            assert_eq!(
                authed(
                    Request::post(
                        "/labels/delete",
                        "post=7&request_id=00000000-0000-4000-8000-000000000001"
                    )
                    .with_cookie(bad)
                ),
                403
            );
        }
        // Nothing was written by the refused requests.
        assert!(!eval.path().join(labels::LABELS_FILE).exists());
        assert!(!eval.path().join(labels::REMOVALS_FILE).exists());
    }

    /// A valid `?k=` logs in with a `/labels`-scoped cookie; rendered
    /// pages never contain the token.
    #[test]
    fn k_login_sets_labels_scoped_cookie() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let resp = crate::handle(&Request::get(&format!("/labels?k={TOKEN}")), &s);
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/labels"));
        assert_eq!(
            resp.set_cookie.as_deref(),
            Some(
                "ap_k=synthetic-labels-token-0002; HttpOnly; Secure; \
                 SameSite=Strict; Path=/labels; Max-Age=2592000"
            )
        );
        assert!(!resp.body.contains(TOKEN));
        let bad = crate::handle(&Request::get("/labels?k=wrong-token-00000000"), &s);
        assert_eq!(bad.status, 403);
        assert!(bad.set_cookie.is_none());
        let page = crate::handle(&Request::get("/labels").with_cookie(COOKIE), &s);
        assert_eq!(page.status, 200);
        assert!(!page.body.contains(TOKEN));
    }

    /// Each query post renders exactly once with its tags, thumbnail,
    /// finding, radios, and Delete button; map-only posts stay hidden.
    #[test]
    fn page_lists_each_post_once_with_tags_thumbs_findings() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let resp = crate::handle(&Request::get("/labels").with_cookie(COOKIE), &s);
        assert_eq!(resp.status, 200);
        let body = &resp.body;
        assert_eq!(count(body, "id=\"post-7\""), 1, "{body}");
        assert_eq!(count(body, "id=\"post-9\""), 1, "{body}");
        assert!(!body.contains("id=\"post-11\""), "{body}");
        for want in [
            "Labels (2 posts, 0 labeled)",
            "synthetic sleep tips",
            "synthetic local agents",
            ">q1</span>",
            ">q2</span>",
            ">sleep</span>",
            ">rest</span>",
            "synthetic ocr",
            "src=\"/labels/thumb/a_0.jpg\"",
            "name=\"label-7\"",
            "name=\"label-9\"",
            "form=\"labels-save\"",
            ">Save labels</button>",
            "action=\"/labels/delete\"",
            "Open on X",
        ] {
            assert!(body.contains(want), "missing {want}: {body}");
        }
        // No secrets or private paths leak into the page.
        assert!(!body.contains(TOKEN), "{body}");
        assert!(!body.contains("eval-private"), "{body}");
        assert!(!body.contains(&data.path().display().to_string()), "{body}");
    }

    /// Saved labels show as checked radios and counts.
    #[test]
    fn page_reflects_saved_labels() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        labels::save_labels(
            eval.path(),
            &[
                ("7".to_string(), Label::Relevant),
                ("9".to_string(), Label::Not),
            ],
            100,
        )
        .unwrap();
        let body = crate::handle(&Request::get("/labels").with_cookie(COOKIE), &s).body;
        assert!(body.contains("Labels (2 posts, 2 labeled)"), "{body}");
        assert!(
            body.contains("name=\"label-7\" value=\"relevant\" checked"),
            "{body}"
        );
        assert!(
            body.contains("name=\"label-9\" value=\"not\" checked"),
            "{body}"
        );
    }

    /// Manifest-listed thumbnails serve with the right content type.
    #[test]
    fn thumb_serves_manifest_bytes() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let jpg = crate::handle(
            &Request::get("/labels/thumb/a_0.jpg").with_cookie(COOKIE),
            &s,
        );
        assert_eq!(jpg.status, 200);
        assert_eq!(jpg.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(
            jpg.raw_body.as_deref(),
            Some(b"fake-jpeg-bytes-7".as_slice())
        );
        let png = crate::handle(
            &Request::get("/labels/thumb/b_0.png").with_cookie(COOKIE),
            &s,
        );
        assert_eq!(png.status, 200);
        assert_eq!(png.content_type.as_deref(), Some("image/png"));
        assert_eq!(
            png.raw_body.as_deref(),
            Some(b"fake-png-bytes-9".as_slice())
        );
    }

    /// Traversal, unknown names, and bad extensions are 404; a listed but
    /// missing file is a 500 that names no path.
    #[test]
    fn thumb_rejects_traversal_unknown_and_bad_ext() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        for target in [
            "/labels/thumb/../nathan/label-queries.json",
            "/labels/thumb/%2e%2e/%2e%2e/x",
            "/labels/thumb/nope.jpg",
            "/labels/thumb/evil.gif",
            "/labels/thumb/",
            "/labels/thumb/a_0.jpg/extra",
        ] {
            let resp = crate::handle(&Request::get(target).with_cookie(COOKIE), &s);
            assert_eq!(resp.status, 404, "{target}: {}", resp.status);
        }
        let missing = crate::handle(
            &Request::get("/labels/thumb/gone.jpg").with_cookie(COOKIE),
            &s,
        );
        assert_eq!(missing.status, 500);
        assert!(
            !missing.body.contains(&eval.path().display().to_string()),
            "{}",
            missing.body
        );
    }

    /// Saving persists labels through the app endpoint and redirects.
    #[test]
    fn save_persists_and_redirects() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let resp = crate::handle(
            &Request::post("/labels/save", "label-7=relevant&label-9=not").with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/labels"));
        let stored = labels::load_labels(eval.path()).unwrap();
        assert_eq!(stored["7"].label, Label::Relevant);
        assert_eq!(stored["9"].label, Label::Not);
        assert_eq!(stored["7"].source, "ui");
        // An empty save is a no-op redirect.
        let resp = crate::handle(&Request::post("/labels/save", "").with_cookie(COOKIE), &s);
        assert_eq!(resp.status, 303);
    }

    /// Bad values and unknown keys reject the whole save unwritten.
    #[test]
    fn save_rejects_bad_values_and_unknown_keys() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        labels::save_labels(eval.path(), &[("7".to_string(), Label::Relevant)], 50).unwrap();
        let before = std::fs::read(eval.path().join(labels::LABELS_FILE)).unwrap();
        for body in [
            "label-7=delete",
            "label-7=maybe",
            "label-zzz=relevant",
            "label-=relevant",
        ] {
            let resp = crate::handle(&Request::post("/labels/save", body).with_cookie(COOKIE), &s);
            assert_eq!(resp.status, 400, "{body}");
        }
        assert_eq!(
            std::fs::read(eval.path().join(labels::LABELS_FILE)).unwrap(),
            before
        );
        // Non-label fields are ignored, not rejected.
        let resp = crate::handle(
            &Request::post("/labels/save", "k=x&other=1").with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 303);
    }

    /// Delete calls the CLI once for the one post, records `delete`, and
    /// logs the attempt.
    #[test]
    #[cfg(unix)]
    fn delete_removes_and_records_label() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        let calls = data.path().join("calls.txt");
        s.labels_remove_bin = Some(fake_remove_bin(data.path(), &calls, 0, ""));
        let resp = crate::handle(
            &Request::post(
                "/labels/delete",
                "post=7&request_id=00000000-0000-4000-8000-000000000001",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/labels"));
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            format!("--post-id {ID7} --request-id 00000000-0000-4000-8000-000000000001\n")
        );
        let stored = labels::load_labels(eval.path()).unwrap();
        assert_eq!(stored["7"].label, Label::Delete);
        let rows: Vec<serde_json::Value> =
            std::fs::read_to_string(eval.path().join(labels::REMOVALS_FILE))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], "removed");
        assert_eq!(rows[0]["post_id"], ID7);
    }

    /// A failing CLI keeps the bookmark and label and shows the error.
    #[test]
    #[cfg(unix)]
    fn delete_failure_keeps_bookmark_and_label() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        labels::save_labels(eval.path(), &[("7".to_string(), Label::Relevant)], 50).unwrap();
        let calls = data.path().join("calls.txt");
        s.labels_remove_bin = Some(fake_remove_bin(data.path(), &calls, 1, "synthetic boom"));
        let resp = crate::handle(
            &Request::post(
                "/labels/delete",
                "post=7&request_id=00000000-0000-4000-8000-000000000001",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 500);
        assert!(resp.body.contains("exit code 1"), "{}", resp.body);
        assert!(!resp.body.contains("synthetic boom"), "{}", resp.body);
        assert!(resp.body.contains("kept"), "{}", resp.body);
        let stored = labels::load_labels(eval.path()).unwrap();
        assert_eq!(stored["7"].label, Label::Relevant);
    }

    /// A second delete of the same post never calls the CLI again.
    #[test]
    #[cfg(unix)]
    fn delete_is_idempotent() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        let calls = data.path().join("calls.txt");
        s.labels_remove_bin = Some(fake_remove_bin(data.path(), &calls, 0, ""));
        for _ in 0..2 {
            let resp = crate::handle(
                &Request::post(
                    "/labels/delete",
                    "post=7&request_id=00000000-0000-4000-8000-000000000001",
                )
                .with_cookie(COOKIE),
                &s,
            );
            assert_eq!(resp.status, 303);
        }
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            format!("--post-id {ID7} --request-id 00000000-0000-4000-8000-000000000001\n")
        );
    }

    /// A missing binary fails closed: error shown, label and log kept safe.
    #[test]
    fn delete_without_bin_fails_closed() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        s.labels_remove_bin = Some(data.path().join("no-such-bin"));
        let resp = crate::handle(
            &Request::post(
                "/labels/delete",
                "post=7&request_id=00000000-0000-4000-8000-000000000001",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 500);
        assert!(resp.body.contains("not executable"), "{}", resp.body);
        assert!(
            !resp.body.contains(&data.path().display().to_string()),
            "{}",
            resp.body
        );
        assert!(labels::load_labels(eval.path()).unwrap().is_empty());
        // Unknown keys and missing fields are 400 with no CLI and no log.
        for body in ["post=zzz", "post=", "other=7"] {
            let resp = crate::handle(
                &Request::post("/labels/delete", body).with_cookie(COOKIE),
                &s,
            );
            assert_eq!(resp.status, 400, "{body}");
        }
    }

    /// Thumbnails cross the socket as bytes with an image content type.
    #[test]
    fn socket_serves_thumbnail_bytes() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || crate::serve(listener, &s, Some(1)));
        let raw = http_roundtrip_bytes(
            &addr,
            &format!("GET /labels/thumb/a_0.jpg HTTP/1.1\r\nHost: x\r\nCookie: {COOKIE}\r\n\r\n"),
        );
        let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("Content-Type: image/jpeg"), "{head}");
        assert!(head.contains("Content-Length: 17"), "{head}");
        assert_eq!(&raw[head_end..], b"fake-jpeg-bytes-7");
        server.join().unwrap();
    }
}
