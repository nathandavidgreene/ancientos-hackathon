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
//!   `GET /labels?ids=<id>,<id>` renders only the cards whose X status id
//!   is listed; `?ids=demo` reads the list from the server-side allowlist
//!   file named by `ANCIENTOS_LABELS_DEMO_IDS_PATH` (default
//!   `$HOME/.local/share/ancientos-hackathon/demo-safe-ids.txt`). Any
//!   malformed input or unreadable allowlist fails closed to zero cards,
//!   never to every post. With `ids` set, listed posts from the bookmarks
//!   store (`ANCIENTOS_LABELS_BOOKMARKS_PATH`, default the bookmarks-agent
//!   store path) also render as cards even when no eval query names them.
//!   Without `ids` the page is unchanged.
//! - `POST /labels/save` persists the submitted Relevant/Not labels
//!   through the bookmarks app endpoint
//!   (`bookmarks_agent::labels::save_labels`).
//! - `POST /labels/delete` removes one post via Social's single-post CLI
//!   (`remove_bookmark --post-id <id>`, one post per click, never
//!   batched). Besides eval-query posts it accepts a post id that is both
//!   in the `?ids=demo` allowlist and in the bookmarks store; anything else,
//!   or a missing/unreadable allowlist or store, is refused. There is no
//!   phone approval: the click itself is the human
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

/// Env var naming the server-side allowlist file used by `?ids=demo`.
pub const DEMO_IDS_ENV: &str = "ANCIENTOS_LABELS_DEMO_IDS_PATH";
/// Allowlist path under `$HOME` used when [`DEMO_IDS_ENV`] is unset or
/// empty. Deliberately outside the repo and outside eval-private.
pub const DEMO_IDS_DEFAULT_REL: &str = ".local/share/ancientos-hackathon/demo-safe-ids.txt";
/// Most ids one filter may name; more is treated as malformed.
const MAX_FILTER_IDS: usize = 1000;
/// Longest accepted id (X status ids are at most 20 digits).
const MAX_ID_LEN: usize = 25;

/// Which cards `GET /labels` renders, parsed from the `?ids=` param.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IdsFilter {
    /// No `ids` param: every query post (today's behavior).
    All,
    /// Only posts whose X status id is in the set. An empty set (from
    /// malformed input or a missing allowlist) renders no cards.
    Only(BTreeSet<String>),
}

impl IdsFilter {
    /// True if a card with this X status id should render. Posts without
    /// a parseable status id only render under [`IdsFilter::All`].
    fn allows(&self, status_id: Option<&str>) -> bool {
        match self {
            IdsFilter::All => true,
            IdsFilter::Only(ids) => status_id.is_some_and(|id| ids.contains(id)),
        }
    }
}

/// True if `s` looks like an X status id: 1..=[`MAX_ID_LEN`] ASCII digits.
fn valid_post_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID_LEN && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parse a comma-separated id list. Surrounding whitespace per id is
/// trimmed; any empty or non-numeric entry, or more than
/// [`MAX_FILTER_IDS`] ids, makes the whole list invalid (`None`).
fn parse_id_list(raw: &str) -> Option<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for tok in raw.split(',') {
        let tok = tok.trim();
        if !valid_post_id(tok) {
            return None;
        }
        out.insert(tok.to_string());
    }
    (out.len() <= MAX_FILTER_IDS).then_some(out)
}

/// Read an allowlist file: one id per line, blank lines ignored. An
/// unreadable file, any malformed line, or too many ids yields `None`.
fn load_id_file(path: &std::path::Path) -> Option<BTreeSet<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !valid_post_id(line) {
            return None;
        }
        out.insert(line.to_string());
    }
    (out.len() <= MAX_FILTER_IDS).then_some(out)
}

/// Resolve the `?ids=demo` allowlist path: [`DEMO_IDS_ENV`] if set and
/// non-empty, else `$HOME/`[`DEMO_IDS_DEFAULT_REL`]; `None` if neither.
fn demo_ids_path() -> Option<std::path::PathBuf> {
    match std::env::var_os(DEMO_IDS_ENV) {
        Some(p) if !p.is_empty() => Some(p.into()),
        _ => std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| std::path::PathBuf::from(h).join(DEMO_IDS_DEFAULT_REL)),
    }
}

/// Build the card filter from the raw `ids` param. Absent means
/// [`IdsFilter::All`]; `demo` reads `demo_path`; anything else is parsed
/// as a comma list. Every failure fails closed to an empty
/// [`IdsFilter::Only`], never to all posts.
fn ids_filter(raw: Option<&str>, demo_path: Option<&std::path::Path>) -> IdsFilter {
    let Some(raw) = raw else {
        return IdsFilter::All;
    };
    let ids = if raw.trim() == "demo" {
        demo_path.and_then(load_id_file)
    } else {
        parse_id_list(raw)
    };
    IdsFilter::Only(ids.unwrap_or_default())
}

/// Env var naming the bookmarks JSONL store that `?ids=` cards may come
/// from; unset means [`bookmarks_agent::ingest::store_path`].
pub const BOOKMARKS_PATH_ENV: &str = "ANCIENTOS_LABELS_BOOKMARKS_PATH";

/// Resolve the bookmarks store path: [`BOOKMARKS_PATH_ENV`] if set and
/// non-empty, else the bookmarks-agent default; `None` if neither works.
fn bookmarks_store_path() -> Option<std::path::PathBuf> {
    match std::env::var_os(BOOKMARKS_PATH_ENV) {
        Some(p) if !p.is_empty() => Some(p.into()),
        _ => bookmarks_agent::ingest::store_path().ok(),
    }
}

/// Add bookmarks-store posts named by `filter` that no eval query already
/// shows (matched by X status id) to `queries.posts`, keyed by status id.
/// Returns the added keys, sorted. Does nothing for [`IdsFilter::All`],
/// a missing path, or an unreadable store (fail closed: no extra cards).
fn add_store_posts(
    queries: &mut LabelQueries,
    filter: &IdsFilter,
    store: Option<&std::path::Path>,
) -> Vec<String> {
    let (IdsFilter::Only(ids), Some(path)) = (filter, store) else {
        return Vec::new();
    };
    let Ok(store) = bookmarks_agent::ingest::Store::load_from(path) else {
        return Vec::new();
    };
    let shown: BTreeSet<String> = queries
        .posts
        .values()
        .filter_map(|p| labels::status_id_of_url(&p.url))
        .collect();
    let mut added = Vec::new();
    for (id, b) in &store.by_id {
        if !ids.contains(id) || shown.contains(id) || queries.posts.contains_key(id) {
            continue;
        }
        queries.posts.insert(
            id.clone(),
            labels::QueryPost {
                author: b.author_username.clone().unwrap_or_default(),
                text: b.text.clone(),
                url: b.url.clone(),
            },
        );
        added.push(id.clone());
    }
    added
}

/// True only if `id` is a well-formed status id listed in the demo
/// allowlist at `allowlist` AND present in the bookmarks store at `store`
/// with a URL whose status id is `id`. Any missing path, unreadable or
/// malformed file, or mismatch returns false (Delete refuses).
fn demo_delete_allowed(
    id: &str,
    allowlist: Option<&std::path::Path>,
    store: Option<&std::path::Path>,
) -> bool {
    if !valid_post_id(id) {
        return false;
    }
    let Some(ids) = allowlist.and_then(load_id_file) else {
        return false;
    };
    if !ids.contains(id) {
        return false;
    }
    let Some(Ok(store)) = store.map(bookmarks_agent::ingest::Store::load_from) else {
        return false;
    };
    store
        .by_id
        .get(id)
        .and_then(|b| labels::status_id_of_url(&b.url))
        .is_some_and(|sid| sid == id)
}

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

/// Dry-run Delete preview line for one post card: names the bookmark, its
/// author when present, and its sorted categories, plus what the click
/// logs. Pure, deterministic, and HTML-escaped (no network, no writes).
fn delete_preview_line(key: &str, author: &str, categories: &[String]) -> String {
    let mut cats = categories.to_vec();
    cats.sort();
    cats.dedup();
    let cats_text = if cats.is_empty() {
        "none".to_string()
    } else {
        cats.iter()
            .map(|c| escape(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let who = if author.is_empty() {
        String::new()
    } else {
        format!(" (@{})", escape(author))
    };
    format!(
        "<p class=\"delete-preview\">Delete preview: removes bookmark {key}{who} \
         · categories: {cats} · logs: label=delete for {key} in the labels store; \
         a separate X worker unbookmarks it. Nothing else changes.</p>",
        key = escape(key),
        who = who,
        cats = cats_text,
    )
}

/// Default Delete return target (the full labels view).
const RETURN_DEFAULT: &str = "/labels";
/// Delete return target for the demo view.
const RETURN_DEMO: &str = "/labels?ids=demo";

/// Map a posted `return` value to an allowlisted redirect target. Only the
/// exact strings [`RETURN_DEFAULT`] and [`RETURN_DEMO`] are honored; any
/// other value (absent, external, or crafted) yields `fallback`, so this
/// is never an open redirect.
fn delete_return_target(posted: Option<&str>, fallback: &'static str) -> &'static str {
    match posted {
        Some(RETURN_DEFAULT) => RETURN_DEFAULT,
        Some(RETURN_DEMO) => RETURN_DEMO,
        _ => fallback,
    }
}

/// Redirect after a successful Delete: `target` plus `removed=<id>` so the
/// labels page can show the "Removed on X" notice.
fn removed_redirect(target: &str, id: &str) -> Response {
    let sep = if target.contains('?') { '&' } else { '?' };
    Response::redirect(format!("{target}{sep}removed={id}"))
}

/// Pending page for a Delete whose earlier run (same request id) is still
/// in progress: not a failure, and the bookmark may still be removed.
fn delete_pending(target: &str, state: &State) -> Response {
    Response::status(
        202,
        crate::error_page(
            "Removal in progress",
            &format!(
                "Removal in progress, reload in a few seconds. Back to labels: {}",
                escape(target)
            ),
            state,
        ),
    )
}

/// Inline style for the "Removed on X" banner: green background, white
/// large bold text (the layout has no stylesheet for it).
const NOTICE_STYLE: &str = "background:#1b7f3b;color:#fff;font-weight:bold;\
font-size:1.5em;padding:14px 16px;border-radius:8px;margin:8px 0 16px";

/// Inline style for a card whose post is already removed on X.
const REMOVED_CARD_STYLE: &str = "opacity:0.55;background:#e6e6e6;filter:grayscale(1)";

/// Banner-style "Removed on X" notice (bold, coloured, `role=status`) for
/// `?removed=<id>`; rendered at the top of the page; empty unless the value is
/// 1-25 ASCII digits, so arbitrary query text is never rendered.
fn removed_notice(removed: Option<&str>) -> String {
    match removed {
        Some(id) if !id.is_empty() && id.len() <= 25 && id.bytes().all(|b| b.is_ascii_digit()) => {
            format!("<div class=\"notice\" role=\"status\" style=\"{NOTICE_STYLE}\">Removed on X: bookmark {id}.</div>")
        }
        _ => String::new(),
    }
}

/// One post card: header, tags, text, thumbs, findings, radios, Delete
/// preview line, and a Delete form that posts `return_to` (the view to come
/// back to) and disables its button on submit (the form still posts
/// normally when JS is off). A card whose post is in `removed_ids` (by key
/// or X status id) is greyed, badged "Removed on X", and gets a disabled
/// Delete button with no form, so it cannot post.
#[allow(clippy::too_many_arguments)]
fn card_html(
    queries: &LabelQueries,
    key: &str,
    current: Option<Label>,
    thumbs: &BTreeMap<String, Vec<labels::ThumbEntry>>,
    findings: &BTreeMap<String, Vec<labels::ShownFinding>>,
    classify: &BTreeMap<String, BTreeSet<String>>,
    return_to: &str,
    removed_ids: &BTreeSet<String>,
) -> String {
    let post = &queries.posts[key];
    let status_id = labels::status_id_of_url(&post.url);
    let removed = removed_ids.contains(key)
        || status_id
            .as_deref()
            .is_some_and(|id| removed_ids.contains(id));
    let rel = current == Some(Label::Relevant);
    let not = current == Some(Label::Not);
    let mut preview_cats: Vec<String> = queries.member_of(key);
    if let Some(c) = classify.get(key) {
        preview_cats.extend(c.iter().cloned());
    }
    if let Some(id) = status_id.as_deref() {
        if let Some(c) = classify.get(id) {
            preview_cats.extend(c.iter().cloned());
        }
    }
    format!(
        "<div class=\"card{removed_class}\" id=\"post-{}\"{removed_style}>\
         <h2>#{key} by {author} {badge}{removed_badge}</h2>\
         <p>{tags}</p>\
         <blockquote>{text}</blockquote>\
         <p>{link}</p>\
         {thumbs}{findings}\
         <p><label><input type=\"radio\" form=\"{form}\" name=\"label-{key}\" \
         value=\"relevant\"{rel}> Relevant</label>\
         <label><input type=\"radio\" form=\"{form}\" name=\"label-{key}\" \
         value=\"not\"{not}> Not</label></p>\
         {preview}\
         {delete}\
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
        preview = delete_preview_line(key, &post.author, &preview_cats),
        delete = if removed {
            "<button type=\"button\" disabled>Delete</button>".to_string()
        } else {
            delete_form(key, return_to)
        },
        removed_class = if removed { " removed" } else { "" },
        removed_style = if removed {
            format!(" style=\"{REMOVED_CARD_STYLE}\"")
        } else {
            String::new()
        },
        removed_badge = if removed {
            " <strong class=\"removed-badge\" style=\"background:#b00020;color:#fff;\
             padding:2px 8px;border-radius:4px\">Removed on X</strong>"
        } else {
            ""
        },
    )
}

/// Active Delete form for one card: posts the key, a fresh request UUID,
/// and the allowlisted `return_to`; disables its button on submit.
fn delete_form(key: &str, return_to: &str) -> String {
    format!(
        "<form method=\"post\" action=\"/labels/delete\" \
         onsubmit=\"this.querySelector('button').disabled=true\">\
         <input type=\"hidden\" name=\"post\" value=\"{key}\">\
         <input type=\"hidden\" name=\"return\" value=\"{return_to}\">\
         <input type=\"hidden\" name=\"request_id\" value=\"{request_id}\">\
         <button type=\"submit\">Delete</button></form>",
        key = escape(key),
        request_id = request_uuid(),
        return_to = escape(return_to),
    )
}

/// `GET /labels`: log in via `?k=` (303 + scoped cookie, like `/m`), else
/// list every query post once with tags, thumbs, findings, and labels.
/// An `?ids=` param narrows the cards (see `ids_filter`).
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
    let mut queries = match labels::load_queries(dir) {
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
    let filter = ids_filter(
        req.query.get("ids").map(String::as_str),
        demo_ids_path().as_deref(),
    );
    let extra = add_store_posts(&mut queries, &filter, bookmarks_store_path().as_deref());
    let mut keys: Vec<String> = queries
        .ordered_keys()
        .into_iter()
        .filter(|k| {
            let sid = queries
                .posts
                .get(k)
                .and_then(|p| labels::status_id_of_url(&p.url));
            filter.allows(sid.as_deref())
        })
        .collect();
    keys.extend(extra);
    let labeled = keys.iter().filter(|k| stored.contains_key(*k)).count();
    let return_to = if req.query.get("ids").map(String::as_str) == Some("demo") {
        RETURN_DEMO
    } else {
        RETURN_DEFAULT
    };
    let removed_ids = labels::removed_post_ids(dir);
    let mut body = removed_notice(req.query.get("removed").map(String::as_str));
    body += &format!(
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
            return_to,
            &removed_ids,
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

/// `POST /labels/delete`: remove the one posted post via the X remover CLI
/// (no phone approval; the click is the authorization). Success records the
/// `delete` label and redirects to the allowlisted `return` view with
/// `removed=<id>`; a still-running earlier request shows a pending page;
/// failure keeps the bookmark and its label and shows the error. A post the
/// audit log already records as removed skips the remover entirely (no X
/// call) and just converges the label and redirects with the notice.
/// Already-removed posts just converge the label and redirect.
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
    let request_id = form.get("request_id").map(String::as_str).unwrap_or("");
    if !queries.posts.contains_key(key) {
        if !demo_delete_allowed(
            key,
            demo_ids_path().as_deref(),
            bookmarks_store_path().as_deref(),
        ) {
            return Response::status(
                400,
                crate::error_page("Bad request", "Unknown post.", state),
            );
        }
        // Demo-allowlisted store post: same remover, log, and replay as
        // eval posts. It has no eval label row, so no label is saved.
        let target = delete_return_target(form.get("return").map(String::as_str), RETURN_DEMO);
        if labels::removed_post_ids(dir).contains(key) {
            return removed_redirect(target, key);
        }
        let path_env = std::env::var("PATH").ok();
        return match labels::remove_one_id(
            dir,
            key,
            key,
            state.labels_remove_bin.as_deref(),
            path_env.as_deref(),
            now_secs(),
            request_id,
        ) {
            Ok(labels::RemoveOutcome::InProgress) => delete_pending(target, state),
            Ok(_) => removed_redirect(target, key),
            Err(e) => failed("Delete failed", &format!("{e} Bookmark kept."), state),
        };
    }
    let post_id = labels::status_id_of_url(&queries.posts[key].url).unwrap_or_default();
    if post_id.is_empty() || !post_id.bytes().all(|b| b.is_ascii_digit()) {
        return Response::status(
            400,
            crate::error_page("Bad request", "Post id must contain digits only.", state),
        );
    }
    let now = now_secs();
    let path_env = std::env::var("PATH").ok();
    let target = delete_return_target(form.get("return").map(String::as_str), RETURN_DEFAULT);
    if labels::removed_post_ids(dir).contains(&post_id) {
        // Already removed per the audit log: no remover run, no X call.
        return match labels::save_labels(dir, &[(key.to_string(), Label::Delete)], now) {
            Ok(_) => removed_redirect(target, &post_id),
            Err(e) => failed("Delete failed", &e, state),
        };
    }
    match labels::remove_one(
        dir,
        key,
        state.labels_remove_bin.as_deref(),
        path_env.as_deref(),
        now,
        request_id,
    ) {
        Ok(labels::RemoveOutcome::InProgress) => delete_pending(target, state),
        Ok(_) => match labels::save_labels(dir, &[(key.to_string(), Label::Delete)], now) {
            Ok(_) => removed_redirect(target, &post_id),
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

    /// Serializes tests that set process env vars (allowlist/store paths).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        assert_eq!(crate::handle_routes(&req, &state).status, 403);
        assert_eq!(
            crate::handle_routes(&Request::get(&format!("/labels?k={TOKEN}")), &state).status,
            303
        );
    }

    #[test]
    fn off_without_eval_dir() {
        let data = tempfile::tempdir().unwrap();
        let mut s = State::open(data.path()).unwrap();
        s.labels_token = crate::phone::token_from(Some(TOKEN.to_string()));
        assert_eq!(
            crate::handle_routes(&Request::get("/labels").with_cookie(COOKIE), &s).status,
            404
        );
        assert_eq!(
            crate::handle_routes(
                &Request::post("/labels/save", "label-7=relevant").with_cookie(COOKIE),
                &s
            )
            .status,
            404
        );
        assert_eq!(
            crate::handle_routes(
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
            crate::handle_routes(
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
        let authed = |t: crate::Request| crate::handle_routes(&t, &s).status;
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
        let resp = crate::handle_routes(&Request::get(&format!("/labels?k={TOKEN}")), &s);
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
        let bad = crate::handle_routes(&Request::get("/labels?k=wrong-token-00000000"), &s);
        assert_eq!(bad.status, 403);
        assert!(bad.set_cookie.is_none());
        let page = crate::handle_routes(&Request::get("/labels").with_cookie(COOKIE), &s);
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
        let resp = crate::handle_routes(&Request::get("/labels").with_cookie(COOKIE), &s);
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

    /// `?ids=` with a known status id renders only that card.
    #[test]
    fn ids_filter_renders_only_listed_posts() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let target = format!("/labels?ids={ID9}, 123");
        let resp = crate::handle_routes(&Request::get(&target).with_cookie(COOKIE), &s);
        assert_eq!(resp.status, 200);
        let body = &resp.body;
        assert_eq!(count(body, "id=\"post-9\""), 1, "{body}");
        assert!(!body.contains("id=\"post-7\""), "{body}");
        assert!(body.contains("Labels (1 post, 0 labeled)"), "{body}");
    }

    /// Malformed, empty, or unknown-only `?ids=` input fails closed to
    /// zero cards, never all posts.
    #[test]
    fn ids_filter_malformed_or_unknown_is_empty() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        for q in [
            "abc",
            "",
            ",",
            "7,,9",
            "9000000000000000007;rm",
            "123",
            "-1",
        ] {
            let target = format!("/labels?ids={q}");
            let resp = crate::handle_routes(&Request::get(&target).with_cookie(COOKIE), &s);
            assert_eq!(resp.status, 200, "{q}");
            assert_eq!(count(&resp.body, "id=\"post-"), 0, "{q}: {}", resp.body);
            assert!(resp.body.contains("Labels (0 posts"), "{q}");
        }
    }

    /// No `ids` param keeps today's behavior: every query post renders.
    #[test]
    fn ids_filter_absent_renders_all() {
        assert_eq!(ids_filter(None, None), IdsFilter::All);
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let resp = crate::handle_routes(&Request::get("/labels").with_cookie(COOKIE), &s);
        assert_eq!(count(&resp.body, "id=\"post-"), 2, "{}", resp.body);
    }

    /// `demo` reads the allowlist file; a missing, unreadable, or
    /// malformed file fails closed to an empty filter.
    #[test]
    fn ids_filter_demo_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("ids.txt");
        std::fs::write(&good, format!("{ID7}\n\n  {ID9}  \n")).unwrap();
        let want: BTreeSet<String> = [ID7, ID9].iter().map(|s| s.to_string()).collect();
        assert_eq!(ids_filter(Some("demo"), Some(&good)), IdsFilter::Only(want));
        let bad = dir.path().join("bad.txt");
        std::fs::write(&bad, format!("{ID7}\nnot-an-id\n")).unwrap();
        let empty = IdsFilter::Only(BTreeSet::new());
        assert_eq!(ids_filter(Some("demo"), Some(&bad)), empty);
        assert_eq!(
            ids_filter(Some("demo"), Some(&dir.path().join("missing"))),
            empty
        );
        assert_eq!(ids_filter(Some("demo"), None), empty);
        assert!(!empty.allows(Some(ID7)));
        assert!(!IdsFilter::Only([ID7.to_string()].into()).allows(None));
    }

    /// End to end: `?ids=demo` with the env var pointing at an allowlist
    /// renders only the listed card. Only this test sets the env var.
    #[test]
    fn ids_demo_page_uses_env_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("demo.txt");
        std::fs::write(&file, format!("{ID7}\n")).unwrap();
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(DEMO_IDS_ENV, &file);
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let resp = crate::handle_routes(&Request::get("/labels?ids=demo").with_cookie(COOKIE), &s);
        std::env::remove_var(DEMO_IDS_ENV);
        assert_eq!(count(&resp.body, "id=\"post-7\""), 1, "{}", resp.body);
        assert_eq!(count(&resp.body, "id=\"post-"), 1, "{}", resp.body);
        assert!(!resp.body.contains(&file.display().to_string()));
    }

    /// `?ids=` also pulls listed posts from the bookmarks store (keyed by
    /// status id, no duplicates of query posts); absent/All adds nothing
    /// and an unreadable store adds nothing.
    #[test]
    fn ids_filter_adds_store_posts() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("bookmarks.jsonl");
        std::fs::write(
            &store,
            format!(
                "{{\"post_id\":\"{ID7}\",\"url\":\"https://x.com/a/status/{ID7}\",\"text\":\"dup\"}}\n\
                 {{\"post_id\":\"8000000000000000001\",\"url\":\"https://x.com/syn_store/status/8000000000000000001\",\"text\":\"synthetic store post\",\"author_username\":\"syn_store\"}}\n\
                 {{\"post_id\":\"8000000000000000002\",\"url\":\"https://x.com/b/status/8000000000000000002\",\"text\":\"unlisted\"}}\n"
            ),
        )
        .unwrap();
        let eval = eval_fixture();
        let base = labels::load_queries(eval.path()).unwrap();
        let only = IdsFilter::Only([ID7.to_string(), "8000000000000000001".to_string()].into());
        let mut q = base.clone();
        assert_eq!(
            add_store_posts(&mut q, &only, Some(&store)),
            vec!["8000000000000000001".to_string()]
        );
        assert_eq!(q.posts["8000000000000000001"].author, "syn_store");
        assert_eq!(q.posts.len(), base.posts.len() + 1);
        let mut q = base.clone();
        assert!(add_store_posts(&mut q, &IdsFilter::All, Some(&store)).is_empty());
        assert!(add_store_posts(&mut q, &only, Some(&dir.path().join("nope"))).is_empty());
        assert!(add_store_posts(&mut q, &only, None).is_empty());
        std::fs::write(&store, "not json\n").unwrap();
        assert!(add_store_posts(&mut q, &only, Some(&store)).is_empty());
        assert_eq!(q.posts.len(), base.posts.len());
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
        let body = crate::handle_routes(&Request::get("/labels").with_cookie(COOKIE), &s).body;
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
        let jpg = crate::handle_routes(
            &Request::get("/labels/thumb/a_0.jpg").with_cookie(COOKIE),
            &s,
        );
        assert_eq!(jpg.status, 200);
        assert_eq!(jpg.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(
            jpg.raw_body.as_deref(),
            Some(b"fake-jpeg-bytes-7".as_slice())
        );
        let png = crate::handle_routes(
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
            let resp = crate::handle_routes(&Request::get(target).with_cookie(COOKIE), &s);
            assert_eq!(resp.status, 404, "{target}: {}", resp.status);
        }
        let missing = crate::handle_routes(
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
        let resp = crate::handle_routes(
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
        let resp = crate::handle_routes(&Request::post("/labels/save", "").with_cookie(COOKIE), &s);
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
            let resp =
                crate::handle_routes(&Request::post("/labels/save", body).with_cookie(COOKIE), &s);
            assert_eq!(resp.status, 400, "{body}");
        }
        assert_eq!(
            std::fs::read(eval.path().join(labels::LABELS_FILE)).unwrap(),
            before
        );
        // Non-label fields are ignored, not rejected.
        let resp = crate::handle_routes(
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
        let resp = crate::handle_routes(
            &Request::post(
                "/labels/delete",
                "post=7&request_id=00000000-0000-4000-8000-000000000001",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(
            resp.location.as_deref(),
            Some(format!("/labels?removed={ID7}").as_str())
        );
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

    /// A posted `return=/labels?ids=demo` sends the user back to the demo
    /// view with the removed notice param.
    #[test]
    #[cfg(unix)]
    fn delete_returns_to_demo_view() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        let calls = data.path().join("calls.txt");
        s.labels_remove_bin = Some(fake_remove_bin(data.path(), &calls, 0, ""));
        let resp = crate::handle_routes(
            &Request::post(
                "/labels/delete",
                "post=7&return=%2Flabels%3Fids%3Ddemo&request_id=00000000-0000-4000-8000-000000000011",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(
            resp.location.as_deref(),
            Some(format!("/labels?ids=demo&removed={ID7}").as_str())
        );
    }

    /// A `return` outside the allowlist (external URL) falls back to
    /// `/labels`; the helper itself honors only the two exact targets.
    #[test]
    #[cfg(unix)]
    fn delete_disallowed_return_falls_back() {
        assert_eq!(
            delete_return_target(Some("https://evil.example/"), RETURN_DEFAULT),
            "/labels"
        );
        assert_eq!(
            delete_return_target(Some("//evil.example"), RETURN_DEMO),
            RETURN_DEMO
        );
        assert_eq!(
            delete_return_target(Some("/labels?ids=demo"), RETURN_DEFAULT),
            RETURN_DEMO
        );
        assert_eq!(delete_return_target(None, RETURN_DEFAULT), RETURN_DEFAULT);
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        let calls = data.path().join("calls.txt");
        s.labels_remove_bin = Some(fake_remove_bin(data.path(), &calls, 0, ""));
        let resp = crate::handle_routes(
            &Request::post(
                "/labels/delete",
                "post=7&return=https%3A%2F%2Fevil.example%2F&request_id=00000000-0000-4000-8000-000000000012",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(
            resp.location.as_deref(),
            Some(format!("/labels?removed={ID7}").as_str())
        );
    }

    /// `?removed=<digits>` renders the notice and the form carries the
    /// return field and submit-disable; non-numeric values render nothing.
    #[test]
    fn removed_notice_renders() {
        assert_eq!(
            removed_notice(Some("2008023259649089960")),
            format!(
                "<div class=\"notice\" role=\"status\" style=\"{NOTICE_STYLE}\">\
                 Removed on X: bookmark 2008023259649089960.</div>"
            )
        );
        assert_eq!(removed_notice(Some("<script>")), "");
        assert_eq!(removed_notice(Some("")), "");
        assert_eq!(removed_notice(None), "");
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let resp =
            crate::handle_routes(&Request::get("/labels?removed=123").with_cookie(COOKIE), &s);
        assert_eq!(resp.status, 200);
        let body = resp.body.clone();
        assert!(body.contains("Removed on X: bookmark 123."), "{body}");
        assert!(body.contains("name=\"return\" value=\"/labels\""), "{body}");
        assert!(body.contains("disabled=true"), "{body}");
    }

    /// The banner is a bold, coloured `role=status` block.
    #[test]
    fn removed_banner_markup() {
        let b = removed_notice(Some("42"));
        assert!(
            b.starts_with("<div class=\"notice\" role=\"status\""),
            "{b}"
        );
        assert!(
            b.contains("font-weight:bold") && b.contains("background:#"),
            "{b}"
        );
        assert!(b.contains("font-size:1.5em"), "{b}");
    }

    /// A card whose X id is logged removed is greyed, badged, and has only
    /// a disabled Delete (no form); others keep an active Delete form.
    #[test]
    fn removed_card_disabled_and_others_active() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        std::fs::write(
            eval.path().join(labels::REMOVALS_FILE),
            format!("{{\"outcome\":\"removed\",\"post_id\":\"{ID7}\",\"post_key\":\"7\"}}\n"),
        )
        .unwrap();
        let s = labels_state(data.path(), eval.path());
        let resp = crate::handle_routes(&Request::get("/labels").with_cookie(COOKIE), &s);
        assert_eq!(resp.status, 200);
        let body = resp.body.clone();
        let card = |k: &str| {
            let start = body.find(&format!("id=\"post-{k}\"")).unwrap();
            let rest = &body[start..];
            rest[..rest[1..]
                .find("<div class=\"card")
                .map_or(rest.len(), |i| i + 1)]
                .to_string()
        };
        let c7 = card("7");
        assert!(c7.contains("Removed on X</strong>"), "{c7}");
        assert!(c7.contains(REMOVED_CARD_STYLE), "{c7}");
        assert!(
            c7.contains("<button type=\"button\" disabled>Delete</button>"),
            "{c7}"
        );
        assert!(!c7.contains("action=\"/labels/delete\""), "{c7}");
        let others: Vec<&str> = body
            .match_indices("id=\"post-")
            .map(|(i, _)| &body[i + 9..i + 12])
            .collect();
        let other = others
            .iter()
            .find(|k| !k.starts_with('7'))
            .expect("another card");
        let key: String = other.chars().take_while(|c| c.is_ascii_digit()).collect();
        let co = card(&key);
        assert!(co.contains("action=\"/labels/delete\""), "{co}");
        assert!(
            co.contains("<button type=\"submit\">Delete</button>"),
            "{co}"
        );
        assert!(!co.contains("Removed on X</strong>"), "{co}");
    }

    /// Delete on a post already logged removed never runs the remover and
    /// redirects with the notice.
    #[test]
    #[cfg(unix)]
    fn delete_already_removed_skips_remover() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        std::fs::write(
            eval.path().join(labels::REMOVALS_FILE),
            format!("{{\"outcome\":\"removed\",\"post_id\":\"{ID7}\",\"post_key\":\"7\"}}\n"),
        )
        .unwrap();
        let mut s = labels_state(data.path(), eval.path());
        let calls = data.path().join("calls.txt");
        s.labels_remove_bin = Some(fake_remove_bin(data.path(), &calls, 0, ""));
        let resp = crate::handle_routes(
            &Request::post(
                "/labels/delete",
                "post=7&request_id=00000000-0000-4000-8000-000000000021",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 303);
        assert_eq!(
            resp.location.as_deref(),
            Some(format!("/labels?removed={ID7}").as_str())
        );
        assert!(!calls.exists(), "remover must not run");
    }

    /// Exit 1 with `request_in_progress` on stdout shows the pending page,
    /// not "Delete failed", and saves no label.
    #[test]
    #[cfg(unix)]
    fn delete_in_progress_shows_pending() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        s.labels_remove_bin = Some(write_executable(
            data.path(),
            "remove_bookmark",
            "#!/bin/sh\nprintf '%s\\n' '{\"status\":\"failed\",\"detail\":\"request_in_progress\"}'\nexit 1\n",
        ));
        let resp = crate::handle_routes(
            &Request::post(
                "/labels/delete",
                "post=7&request_id=00000000-0000-4000-8000-000000000013",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 202);
        let body = resp.body.clone();
        assert!(
            body.contains("Removal in progress, reload in a few seconds"),
            "{body}"
        );
        assert!(!body.contains("Delete failed"), "{body}");
        let stored = labels::load_labels(eval.path()).unwrap();
        assert!(stored.get("7").is_none_or(|r| r.label != Label::Delete));
    }

    /// Demo delete gate: allowlisted + in store is accepted; unknown id,
    /// missing allowlist, missing store, or malformed key is refused.
    #[test]
    fn demo_delete_gate() {
        let dir = tempfile::tempdir().unwrap();
        let allow = dir.path().join("allow.txt");
        let store = dir.path().join("bookmarks.jsonl");
        std::fs::write(&allow, "8000000000000000001\n8000000000000000009\n").unwrap();
        std::fs::write(
            &store,
            "{\"post_id\":\"8000000000000000001\",\"url\":\"https://x.com/s/status/8000000000000000001\",\"text\":\"t\"}\n\
             {\"post_id\":\"8000000000000000002\",\"url\":\"https://x.com/s/status/8000000000000000002\",\"text\":\"t\"}\n",
        )
        .unwrap();
        let ok = |id: &str| demo_delete_allowed(id, Some(&allow), Some(&store));
        assert!(ok("8000000000000000001"));
        assert!(!ok("8000000000000000002"), "in store, not allowlisted");
        assert!(!ok("8000000000000000009"), "allowlisted, not in store");
        assert!(!ok("7"));
        assert!(!ok("8000000000000000001x"));
        let missing = dir.path().join("missing");
        assert!(!demo_delete_allowed(
            "8000000000000000001",
            Some(&missing),
            Some(&store)
        ));
        assert!(!demo_delete_allowed(
            "8000000000000000001",
            None,
            Some(&store)
        ));
        assert!(!demo_delete_allowed(
            "8000000000000000001",
            Some(&allow),
            Some(&missing)
        ));
        assert!(!demo_delete_allowed(
            "8000000000000000001",
            Some(&allow),
            None
        ));
    }

    /// End to end: an allowlisted store post reaches the stubbed remover
    /// once, is audit-logged, replays idempotently, and saves no eval
    /// label; an unknown id is refused without calling the remover. Only
    /// this test sets the allowlist/store env vars for Delete.
    #[test]
    #[cfg(unix)]
    fn delete_accepts_demo_allowlisted_store_post() {
        const SID: &str = "8000000000000000001";
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        let calls = data.path().join("calls.txt");
        s.labels_remove_bin = Some(fake_remove_bin(data.path(), &calls, 0, ""));
        let allow = data.path().join("allow.txt");
        let store = data.path().join("bookmarks.jsonl");
        std::fs::write(&allow, format!("{SID}\n")).unwrap();
        std::fs::write(
            &store,
            format!("{{\"post_id\":\"{SID}\",\"url\":\"https://x.com/s/status/{SID}\",\"text\":\"t\"}}\n"),
        )
        .unwrap();
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(DEMO_IDS_ENV, &allow);
        std::env::set_var(BOOKMARKS_PATH_ENV, &store);
        let post = |body: &str| {
            crate::handle_routes(
                &Request::post("/labels/delete", body).with_cookie(COOKIE),
                &s,
            )
        };
        let rid = "00000000-0000-4000-8000-0000000000d1";
        let first = post(&format!("post={SID}&request_id={rid}"));
        let replay = post(&format!("post={SID}&request_id={rid}"));
        let unknown =
            post("post=8000000000000000002&request_id=00000000-0000-4000-8000-0000000000d2");
        std::env::remove_var(DEMO_IDS_ENV);
        std::env::remove_var(BOOKMARKS_PATH_ENV);
        assert_eq!(first.status, 303, "{}", first.body);
        assert_eq!(
            first.location.as_deref(),
            Some(format!("/labels?ids=demo&removed={SID}").as_str())
        );
        assert_eq!(replay.status, 303, "{}", replay.body);
        assert_eq!(unknown.status, 400);
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            format!("--post-id {SID} --request-id {rid}\n"),
            "remover called exactly once, never for the unknown id"
        );
        let rows: Vec<serde_json::Value> =
            std::fs::read_to_string(eval.path().join(labels::REMOVALS_FILE))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], "removed");
        assert_eq!(rows[0]["post_id"], SID);
        assert!(!labels::load_labels(eval.path()).unwrap().contains_key(SID));
    }

    /// With no demo allowlist configured, a non-eval id is refused and the
    /// remover is never called (eval posts keep working as before).
    #[test]
    #[cfg(unix)]
    fn delete_refuses_non_eval_id_without_allowlist() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let mut s = labels_state(data.path(), eval.path());
        let calls = data.path().join("calls.txt");
        s.labels_remove_bin = Some(fake_remove_bin(data.path(), &calls, 0, ""));
        let resp = crate::handle_routes(
            &Request::post(
                "/labels/delete",
                "post=8000000000000000077&request_id=00000000-0000-4000-8000-0000000000d3",
            )
            .with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 400);
        assert!(!calls.exists());
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
        let resp = crate::handle_routes(
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
            let resp = crate::handle_routes(
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
        let resp = crate::handle_routes(
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
            let resp = crate::handle_routes(
                &Request::post("/labels/delete", body).with_cookie(COOKIE),
                &s,
            );
            assert_eq!(resp.status, 400, "{body}");
        }
    }

    /// Preview with categories sorts them and names the author; repeated
    /// calls with shuffled input render identically (deterministic).
    #[test]
    fn delete_preview_sorts_categories_and_names_author() {
        let a = delete_preview_line(
            "7",
            "syn_sleeper",
            &[
                "sleep".to_string(),
                "q2".to_string(),
                "q1".to_string(),
                "q1".to_string(),
            ],
        );
        let b = delete_preview_line(
            "7",
            "syn_sleeper",
            &["q1".to_string(), "q2".to_string(), "sleep".to_string()],
        );
        assert_eq!(a, b, "{a}");
        assert_eq!(
            a,
            "<p class=\"delete-preview\">Delete preview: removes bookmark 7 (@syn_sleeper) \
             · categories: q1, q2, sleep · logs: label=delete for 7 in the labels store; \
             a separate X worker unbookmarks it. Nothing else changes.</p>",
            "{a}"
        );
    }

    /// Preview with no categories says `none`, omits the author when empty,
    /// and escapes hostile markup in key, author, and categories.
    #[test]
    fn delete_preview_escapes_hostile_text_and_handles_no_categories() {
        let plain = delete_preview_line("9", "", &[]);
        assert!(plain.contains("removes bookmark 9"), "{plain}");
        assert!(!plain.contains("(@"), "{plain}");
        assert!(plain.contains("categories: none"), "{plain}");
        assert!(plain.contains("label=delete for 9"), "{plain}");
        let evil = delete_preview_line(
            "<script>7</script>",
            "a&b",
            &["<script>evil</script>".to_string()],
        );
        assert!(!evil.contains("<script>"), "{evil}");
        assert!(evil.contains("&lt;script&gt;7&lt;/script&gt;"), "{evil}");
        assert!(evil.contains("(@a&amp;b)"), "{evil}");
        assert!(
            evil.contains("categories: &lt;script&gt;evil&lt;/script&gt;"),
            "{evil}"
        );
        assert!(evil.contains("class=\"delete-preview\""), "{evil}");
    }

    /// Each labels card shows its Delete preview directly above its
    /// unchanged one-click Delete form.
    #[test]
    fn page_shows_delete_preview_above_delete_form() {
        let data = tempfile::tempdir().unwrap();
        let eval = eval_fixture();
        let s = labels_state(data.path(), eval.path());
        let body = crate::handle_routes(&Request::get("/labels").with_cookie(COOKIE), &s).body;
        assert_eq!(count(&body, "class=\"delete-preview\""), 2, "{body}");
        assert_eq!(count(&body, "action=\"/labels/delete\""), 2, "{body}");
        for want in [
            "removes bookmark 7 (@syn_sleeper)",
            "categories: q1, q2, sleep",
            "removes bookmark 9 (@syn_cook)",
            "categories: q1, rest",
            "label=delete for 7",
            "Nothing else changes.",
        ] {
            assert!(body.contains(want), "missing {want}: {body}");
        }
        let preview = body.find("delete-preview").unwrap();
        let form = body.find("action=\"/labels/delete\"").unwrap();
        assert!(preview < form, "{body}");
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
