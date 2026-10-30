//! Phone approvals: a small-screen approve/deny page over the core
//! approvals queue and hash-chained decision log.
//!
//! - `GET /m?k=TOKEN` with a valid token answers 303 to `/m` and sets the
//!   [`COOKIE_NAME`] cookie (HttpOnly, Secure, SameSite=Strict, Path=/m,
//!   30 days), so the token leaves the address bar and history.
//! - `GET /m` with that cookie lists pending requests (big buttons), the
//!   chain status and the last few decisions.
//! - `POST /m/decide` (form: `id`, `payload_hash`, `decision` =
//!   `approve`|`deny`, optional `reason`; authorized by the cookie, or a
//!   form `k` as a fallback) records one decision as [`PHONE_DECIDER`] and
//!   redirects back to `/m`. Rendered pages never contain the token.
//!
//! Fail closed: with no phone token configured both routes answer 403, a
//! wrong or missing token answers 403 with nothing decided, a tampered log
//! withholds every request, and a stale payload hash decides nothing. Like
//! `/approvals`, the page only records decisions: nothing executes here.
//! The server still binds loopback only; reaching it from a phone is the
//! deployer's job. The reverse proxy MUST terminate HTTPS; Secure cookies
//! deliberately do not work over plain HTTP.
use crate::{escape, now_secs, parse_body, Request, Response, State};
use ancientos_core::approvals::{self, ApprovalError};
use ancientos_core::log::{DecisionLog, Entry, LogError};
use bookmarks_agent::brief::{DECISION_LOG_ENV_VAR, DECISION_LOG_FILE_NAME};
use std::path::{Path, PathBuf};

/// Decider recorded for phone decisions.
pub const PHONE_DECIDER: &str = "phone";
/// Env var holding the phone token (read by the binary at startup).
pub const TOKEN_ENV: &str = "ANCIENTOS_PHONE_TOKEN";
/// Separate credential for label writes and deletion.
pub const LABELS_TOKEN_ENV: &str = "ANCIENTOS_LABELS_TOKEN";
/// Shortest accepted token; shorter values leave phone approvals off.
pub const MIN_TOKEN_LEN: usize = 16;
/// Cookie that remembers a phone login.
pub const COOKIE_NAME: &str = "ap_k";
/// Remembered-login lifetime in seconds (30 days).
pub const COOKIE_MAX_AGE: u64 = 2_592_000;
/// Decisions shown under "Recent".
const RECENT: usize = 5;

/// Mobile-first inline CSS: large tap targets, readable without zoom.
const CSS: &str = "body{font-family:system-ui,sans-serif;margin:0;padding:12px;font-size:18px;\
background:#fafafa}h1{font-size:22px}.card{background:#fff;border:1px solid #ddd;\
border-radius:12px;padding:12px;margin:12px 0}.meta{color:#555;font-size:15px}\
button{width:100%;font-size:20px;padding:14px;border:0;border-radius:10px;margin-top:8px}\
.ok{background:#1b7f3b;color:#fff}.no{background:#b3261e;color:#fff}\
input{width:100%;box-sizing:border-box;font-size:17px;padding:10px;margin-top:8px}\
.bad{color:#b3261e;font-weight:bold}code{word-break:break-all}";

/// Percent-encode a query value (unreserved characters kept).
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A configured token, or `None` when it is missing or too short.
pub fn token_from(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| {
        s.len() >= MIN_TOKEN_LEN
            && !s.bytes().all(|b| b.is_ascii_digit())
            && s.chars().collect::<std::collections::BTreeSet<_>>().len() >= 6
    })
}

/// Use the legacy phone credential only when the labels variable is unset.
pub fn labels_token_from(raw: Option<String>, legacy: Option<String>) -> Option<String> {
    match raw {
        Some(value) => token_from(Some(value)),
        None => legacy,
    }
}

/// Generate 256 bits of random token material without storing it.
pub fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| "token entropy unavailable".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Match an explicitly scoped credential using fixed-length hashed equality.
pub fn token_matches(expected: Option<&str>, given: Option<&str>) -> bool {
    match (expected, given) {
        (Some(a), Some(b)) => same(a, b),
        _ => false,
    }
}

/// Authenticate a cookie against the explicitly selected credential scope.
pub fn cookie_matches(expected: Option<&str>, req: &Request) -> bool {
    token_matches(expected, cookie_token(req.cookie.as_deref()).as_deref())
}

/// Constant-time token equality: both sides are hashed to fixed-length
/// sha256 hex first, so neither a mismatch position nor a length
/// difference changes the comparison time.
fn same(a: &str, b: &str) -> bool {
    use ancientos_core::approvals::canonical_hash;
    let ha = canonical_hash(&serde_json::Value::String(a.to_string()));
    let hb = canonical_hash(&serde_json::Value::String(b.to_string()));
    ha.bytes()
        .zip(hb.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// True only when a token is configured and `given` matches it
/// (constant-time, see [`same`]).
fn token_ok(state: &State, given: Option<&str>) -> bool {
    match (&state.phone_token, given) {
        (Some(t), Some(g)) => same(t, g),
        _ => false,
    }
}

/// The [`COOKIE_NAME`] value from a raw `Cookie` header, percent-decoded.
fn cookie_token(cookie: Option<&str>) -> Option<String> {
    cookie?.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name.trim() == COOKIE_NAME).then(|| crate::percent_decode(value.trim()))
    })
}

/// True when the remembered-login cookie holds the token, or else when the
/// fallback `k` (query or form field) does. Both checks are constant-time.
fn authorized(state: &State, cookie: Option<&str>, k: Option<&String>) -> bool {
    if token_ok(state, cookie_token(cookie).as_deref()) {
        return true;
    }
    token_ok(state, k.map(String::as_str))
}

/// `Set-Cookie` value remembering `token` for [`COOKIE_MAX_AGE`] seconds.
fn login_cookie(token: &str) -> String {
    login_cookie_scoped(token, "/m")
}

/// `Set-Cookie` value remembering `token` for [`COOKIE_MAX_AGE`] seconds,
/// scoped to `path` (the labels page logs in separately under `/labels`).
pub fn login_cookie_scoped(token: &str, path: &str) -> String {
    format!(
        "{COOKIE_NAME}={}; HttpOnly; Secure; SameSite=Strict; Path={path}; Max-Age={COOKIE_MAX_AGE}",
        percent_encode(token)
    )
}

/// True when `given` matches the configured token (constant-time).
pub fn token_valid(state: &State, given: &str) -> bool {
    token_ok(state, Some(given))
}

/// True when the request's [`COOKIE_NAME`] cookie matches the configured
/// token (constant-time, see `token_ok`).
pub fn cookie_authorized(state: &State, req: &Request) -> bool {
    // Browsers may send both a scoped labels credential and the root credential.
    req.cookie.as_deref().is_some_and(|cookie| {
        cookie.split(';').any(|part| {
            let Some((name, value)) = part.trim().split_once('=') else {
                return false;
            };
            name == COOKIE_NAME && token_ok(state, Some(&crate::percent_decode(value)))
        })
    })
}

/// Full mobile page with the given title and body.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\"><title>{}</title>\
         <style>{CSS}</style></head><body>{body}</body></html>",
        escape(title)
    )
}

/// 403 page; says why without revealing anything about the queue.
fn forbidden(state: &State) -> Response {
    let why = if state.phone_token.is_none() {
        "Phone approvals are off (no token configured)."
    } else {
        "Missing or wrong token."
    };
    Response::status(
        403,
        page("Forbidden", &format!("<h1>Forbidden</h1><p>{why}</p>")),
    )
}

/// Error page with a status code and a token-free link back to `/m`.
fn error(status: u16, title: &str, msg: &str) -> Response {
    Response::status(
        status,
        page(
            title,
            &format!(
                "<h1>{}</h1><p>{}</p><p><a href=\"/m\">Back</a></p>",
                escape(title),
                escape(msg)
            ),
        ),
    )
}

/// Phone approvals log path, resolved without touching the filesystem:
///
/// 1. `$DECISION_LOG_PATH` when set and non-empty (explicit override wins,
///    matching `bookmarks-agent::brief::decision_log_path` and the
///    `wonder`/`analyze_media` CLIs);
/// 2. otherwise `<data_dir>/decision_log.jsonl`.
///
/// `$ANCIENTOS_HOME` is deliberately not consulted: the `--data` dir is this
/// server's workspace root (store, cards and caches all live under it), so
/// falling back to a different log when the workspace log is missing would
/// hide a misconfigured deploy instead of failing closed, and would let
/// ambient environment steer tests at real user data. `env` reads
/// environment variables so tests inject fakes and never touch the real
/// environment.
pub fn resolve_log_path(data_dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(p) = env(DECISION_LOG_ENV_VAR).filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    data_dir.join(DECISION_LOG_FILE_NAME)
}

/// Verify an opened phone approvals log, failing closed: a missing log is a
/// 500 naming the outage (never an empty queue — an empty queue would hide a
/// misconfigured path behind "nothing pending"), other read/parse failures
/// are a 500 with the cause, and a broken hash chain renders the tampered
/// page. No message contains the log path or the token. The error page is
/// boxed (`Box<Response>`) to keep the `Result` small; callers unbox it.
fn verify_for_phone(log: &DecisionLog) -> Result<Vec<Entry>, Box<Response>> {
    match log.verify() {
        Err(LogError::Chain(n)) => Err(Box::new(tampered(n))),
        Err(LogError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Err(Box::new(error(
            500,
            "Approvals unavailable",
            "The decision log is missing, so no requests are shown and nothing can be decided.",
        ))),
        Err(e) => Err(Box::new(error(500, "Approvals failed", &e.to_string()))),
        Ok(entries) => Ok(entries),
    }
}

/// Tampered-log page: every request is withheld (fail closed).
fn tampered(row: usize) -> Response {
    Response::ok(page(
        "Approvals",
        &format!(
            "<h1>Approvals</h1><p class=\"bad\">TAMPERED at row {row}</p>\
             <p>The hash chain failed verification; requests are withheld.</p>"
        ),
    ))
}

/// `GET /m`: a valid `?k=` answers 303 to `/m` and sets the login cookie
/// (an invalid one gets 403 and no cookie); otherwise the cookie must
/// authorize, and the page lists pending requests with approve/deny
/// buttons, chain status and recent decisions, with no token in the HTML.
/// Reads the real process environment; see [`page_list_with_env`].
pub fn page_list(req: &Request, state: &State) -> Response {
    page_list_with_env(req, state, &|k| std::env::var(k).ok())
}

/// [`page_list`] with an injected environment reader for the log path (see
/// [`resolve_log_path`]), so tests never touch the real environment.
///
/// Fails closed with the page from `verify_for_phone`, unboxed.
pub fn page_list_with_env(
    req: &Request,
    state: &State,
    env: &dyn Fn(&str) -> Option<String>,
) -> Response {
    if let Some(k) = req.query.get("k") {
        if !token_ok(state, Some(k)) {
            return forbidden(state);
        }
        let mut resp = Response::redirect("/m".to_string());
        resp.set_cookie = Some(login_cookie(k));
        return resp;
    }
    if !authorized(state, req.cookie.as_deref(), None) {
        return forbidden(state);
    }
    let path = resolve_log_path(&state.data_dir, env);
    let log = match DecisionLog::open_with_key_env(&path, &|key| std::env::var(key).ok()) {
        Err(LogError::Chain(n)) => return tampered(n),
        Err(e) => return error(500, "Approvals failed", &e.to_string()),
        Ok(log) => log,
    };
    // `verify`, not `verify_or_empty`: a missing log fails closed, it never
    // renders as "nothing pending".
    let entries = match verify_for_phone(&log) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let now = now_secs();
    let all = approvals::replay(&entries);
    let pend: Vec<_> = all.iter().filter(|a| a.is_pending_at(now)).collect();
    let mut body = format!(
        "<h1>Approvals ({})</h1><p class=\"meta\">hash chain verified: {} entries</p>",
        pend.len(),
        entries.len()
    );
    if pend.is_empty() {
        body.push_str("<p>Nothing waiting for you.</p>");
    }
    for a in &pend {
        let r = &a.request;
        let hidden = format!(
            "<input type=\"hidden\" name=\"id\" value=\"{}\">\
             <input type=\"hidden\" name=\"payload_hash\" value=\"{}\">",
            escape(&r.id),
            escape(&r.payload_hash)
        );
        body.push_str(&format!(
            "<div class=\"card\"><p><strong>{}</strong></p>\
             <p class=\"meta\">{} · {} · id <code>{}</code> · expires in {} min</p>\
             <form method=\"post\" action=\"/m/decide\">{hidden}\
             <input type=\"hidden\" name=\"decision\" value=\"approve\">\
             <button class=\"ok\" type=\"submit\">Approve</button></form>\
             <form method=\"post\" action=\"/m/decide\">{hidden}\
             <input type=\"hidden\" name=\"decision\" value=\"deny\">\
             <input name=\"reason\" placeholder=\"reason (optional)\">\
             <button class=\"no\" type=\"submit\">Deny</button></form></div>",
            escape(&r.summary),
            escape(&r.agent),
            escape(&r.action),
            escape(&r.id),
            r.expires_at.saturating_sub(now) / 60,
        ));
    }
    let mut done: Vec<_> = all
        .iter()
        .filter_map(|a| a.decision.as_ref().map(|d| (a, d)))
        .collect();
    done.sort_by_key(|x| std::cmp::Reverse(x.1.decided_at));
    if !done.is_empty() {
        body.push_str("<h2>Recent</h2><ul>");
        for (a, d) in done.into_iter().take(RECENT) {
            body.push_str(&format!(
                "<li>{} — {} by {}</li>",
                escape(&a.request.summary),
                escape(d.outcome.as_str()),
                escape(&d.decided_by)
            ));
        }
        body.push_str("</ul>");
    }
    Response::ok(page("Approvals", &body))
}

/// `POST /m/decide`: record one approve or deny as [`PHONE_DECIDER`].
/// Authorized by the login cookie, or a form `k` as a fallback; redirects
/// to a token-free `/m`. Reads the real process environment; see
/// [`do_decide_with_env`].
pub fn do_decide(req: &Request, state: &State) -> Response {
    do_decide_with_env(req, state, &|k| std::env::var(k).ok())
}

/// [`do_decide`] with an injected environment reader for the log path (see
/// [`resolve_log_path`]), so tests never touch the real environment. The
/// decision lands in the same resolved log the list reads.
///
/// Fails closed with the page from `verify_for_phone`, unboxed.
pub fn do_decide_with_env(
    req: &Request,
    state: &State,
    env: &dyn Fn(&str) -> Option<String>,
) -> Response {
    let form = parse_body(&req.body);
    if !authorized(state, req.cookie.as_deref(), form.get("k")) {
        return forbidden(state);
    }
    let field = |n: &str| form.get(n).map(String::as_str).unwrap_or("").trim();
    let (id, hash) = (field("id"), field("payload_hash"));
    if id.is_empty() || hash.is_empty() {
        return error(400, "Bad request", "Missing id or payload_hash.");
    }
    let approve_it = match field("decision") {
        "approve" => true,
        "deny" => false,
        _ => return error(400, "Bad request", "decision must be approve or deny."),
    };
    let path = resolve_log_path(&state.data_dir, env);
    let mut log = match DecisionLog::open_with_key_env(&path, &|key| std::env::var(key).ok()) {
        Err(LogError::Chain(n)) => return tampered(n),
        Err(e) => return error(500, "Approvals failed", &e.to_string()),
        Ok(log) => log,
    };
    // A missing log opens as empty, so verify before deciding: without this
    // a missing log would fail with a misleading "unknown id".
    if let Err(resp) = verify_for_phone(&log) {
        return *resp;
    }
    let now = now_secs();
    let outcome: Result<(), ApprovalError> = if approve_it {
        approvals::approve(&mut log, id, hash, PHONE_DECIDER, now).map(|_| ())
    } else {
        approvals::deny(&mut log, id, hash, PHONE_DECIDER, field("reason"), now)
    };
    match outcome {
        Ok(()) => Response::redirect("/m".to_string()),
        Err(e @ ApprovalError::HashMismatch(_)) => error(
            400,
            "Stale approval",
            &format!("{e}; nothing was decided. Reload and retry."),
        ),
        Err(ApprovalError::Log(e)) => error(500, "Approvals failed", &e.to_string()),
        Err(e) => error(400, "Approval failed", &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle_routes as handle;
    use ancientos_core::approvals::{ApprovalRequest, Status};

    const TOKEN: &str = "synthetic-phone-token-0001";
    /// Cookie header carrying the synthetic token.
    const COOKIE: &str = "ap_k=synthetic-phone-token-0001";

    /// Test helper: `GET /m` with the login cookie.
    fn get_m(s: &State) -> Response {
        handle(&Request::get("/m").with_cookie(COOKIE), s)
    }

    /// Test helper: state in a temp dir with the phone token set.
    fn state_with_token(dir: &std::path::Path) -> State {
        let mut s = State::open(dir).unwrap();
        s.phone_token = token_from(Some(TOKEN.to_string()));
        s
    }

    /// Test helper: one pending synthetic `x.post` request.
    fn pending_request(state: &State, n: u32) -> ApprovalRequest {
        let mut log = DecisionLog::open(state.log_path()).unwrap();
        approvals::request(
            &mut log,
            &bookmarks_agent::app_gate(),
            "bookmarks-agent",
            "x.post",
            &format!("synthetic post {n}"),
            serde_json::json!({"draft": n}),
            now_secs(),
            None,
        )
        .unwrap()
    }

    /// Test helper: decide form body.
    fn form(k: &str, r: &ApprovalRequest, decision: &str) -> String {
        format!(
            "k={k}&id={}&payload_hash={}&decision={decision}&reason=not+now",
            r.id, r.payload_hash
        )
    }

    /// Test helper: folded status of one request.
    fn status(state: &State, id: &str) -> Status {
        let log = DecisionLog::open(state.log_path()).unwrap();
        approvals::get(&log, id).unwrap().status_at(now_secs())
    }

    /// Test helper: environment with no variables set, so the resolved log
    /// path is hermetic (ambient `DECISION_LOG_PATH` can never steer the
    /// test at real user data).
    fn env_none(_: &str) -> Option<String> {
        None
    }

    /// Test helper: environment resolving `DECISION_LOG_PATH` to `path`.
    fn env_log_path(path: &std::path::Path) -> impl Fn(&str) -> Option<String> {
        let p = path.to_string_lossy().into_owned();
        move |k: &str| {
            if k == DECISION_LOG_ENV_VAR {
                Some(p.clone())
            } else {
                None
            }
        }
    }

    /// Test helper: one pending synthetic `x.post` request in the log file
    /// at `path` (the file's parent dir must already exist).
    fn pending_request_at(path: &std::path::Path, n: u32) -> ApprovalRequest {
        let mut log = DecisionLog::open(path).unwrap();
        approvals::request(
            &mut log,
            &bookmarks_agent::app_gate(),
            "bookmarks-agent",
            "x.post",
            &format!("synthetic post {n}"),
            serde_json::json!({"draft": n}),
            now_secs(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn token_generation_strength_and_legacy_scope_fallback() {
        assert!(token_from(Some("1234567890123456".into())).is_none());
        assert!(token_from(Some("aaaaaaaaaaaaaaaa".into())).is_none());
        let first = random_token().unwrap();
        assert_eq!(first.len(), 64);
        assert_ne!(first, random_token().unwrap());
        assert!(token_from(Some(first.clone())).is_some());
        assert_eq!(
            labels_token_from(None, Some(first.clone())),
            Some(first.clone())
        );
        assert_eq!(labels_token_from(Some(String::new()), Some(first)), None);
    }

    #[test]
    fn valid_k_redirects_and_sets_exact_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let resp = handle(&Request::get(&format!("/m?k={TOKEN}")), &s);
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/m"));
        assert_eq!(
            resp.set_cookie.as_deref(),
            Some(
                "ap_k=synthetic-phone-token-0001; HttpOnly; Secure; \
                 SameSite=Strict; Path=/m; Max-Age=2592000"
            )
        );
        assert!(!resp.body.contains(TOKEN));
    }

    #[test]
    fn invalid_k_is_forbidden_without_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let resp = handle(&Request::get("/m?k=wrong-token-0000000"), &s);
        assert_eq!(resp.status, 403);
        assert!(resp.set_cookie.is_none());
        // An invalid k is refused even alongside a valid cookie.
        let resp = handle(
            &Request::get("/m?k=wrong-token-0000000").with_cookie(COOKIE),
            &s,
        );
        assert_eq!(resp.status, 403);
        assert!(resp.set_cookie.is_none());
    }

    #[test]
    fn cookie_alone_authorizes_list_and_decide() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let r = pending_request(&s, 1);
        let resp = handle(
            &Request::get("/m").with_cookie(&format!("theme=dark; {COOKIE}")),
            &s,
        );
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("synthetic post 1"));
        let body = format!(
            "id={}&payload_hash={}&decision=deny&reason=not+now",
            r.id, r.payload_hash
        );
        let resp = handle(&Request::post("/m/decide", &body).with_cookie(COOKIE), &s);
        assert_eq!(resp.status, 303);
        assert_eq!(resp.location.as_deref(), Some("/m"));
        assert_eq!(status(&s, &r.id), Status::Denied);
    }

    #[test]
    fn bad_cookie_is_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let r = pending_request(&s, 1);
        for c in [
            "ap_k=wrong-token-0000000",
            "ap_k=",
            "other=synthetic-phone-token-0001",
        ] {
            assert_eq!(handle(&Request::get("/m").with_cookie(c), &s).status, 403);
            let body = format!(
                "id={}&payload_hash={}&decision=approve",
                r.id, r.payload_hash
            );
            let resp = handle(&Request::post("/m/decide", &body).with_cookie(c), &s);
            assert_eq!(resp.status, 403);
        }
        assert_eq!(status(&s, &r.id), Status::Pending);
    }

    #[test]
    fn rendered_html_never_holds_token_or_k() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let a = pending_request(&s, 1);
        let b = pending_request(&s, 2);
        handle(&Request::post("/m/decide", &form(TOKEN, &a, "approve")), &s);
        let list = get_m(&s).body;
        assert!(list.contains("synthetic post 2"));
        let bad = handle(&Request::post("/m/decide", &form(TOKEN, &b, "maybe")), &s).body;
        for html in [list, bad] {
            assert!(!html.contains(TOKEN));
            assert!(!html.contains("k="));
            assert!(!html.contains("name=\"k\""));
        }
    }

    #[test]
    fn token_compare_is_exact_for_any_length() {
        assert!(same(TOKEN, TOKEN));
        assert!(!same(TOKEN, &TOKEN[..TOKEN.len() - 1]));
        assert!(!same(TOKEN, &format!("{TOKEN}x")));
        assert!(!same(TOKEN, ""));
    }

    #[test]
    fn get_never_changes_state() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        // Missing log: GET must not create it.
        handle(&Request::get(&format!("/m?k={TOKEN}")), &s);
        assert!(!s.log_path().exists());
        let r = pending_request(&s, 1);
        let before = std::fs::read(s.log_path()).unwrap();
        // Even GETs carrying decide-looking query params change nothing.
        let q = format!(
            "/m?k={TOKEN}&id={}&payload_hash={}&decision=approve",
            r.id, r.payload_hash
        );
        for target in [q.as_str(), "/m", "/m?k=wrong-token-0000000"] {
            handle(&Request::get(target), &s);
        }
        assert_eq!(std::fs::read(s.log_path()).unwrap(), before);
        assert_eq!(status(&s, &r.id), Status::Pending);
        // GET on the decide path is not a route.
        assert_eq!(
            handle(&Request::get(&q.replace("/m?", "/m/decide?")), &s).status,
            404
        );
        assert_eq!(std::fs::read(s.log_path()).unwrap(), before);
    }

    #[test]
    fn decide_checks_hash_against_that_pending_item() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let a = pending_request(&s, 1);
        let b = pending_request(&s, 2);
        // a's id with b's (valid, but different) hash decides nothing.
        let body = format!(
            "k={TOKEN}&id={}&payload_hash={}&decision=approve",
            a.id, b.payload_hash
        );
        assert_eq!(handle(&Request::post("/m/decide", &body), &s).status, 400);
        assert_eq!(status(&s, &a.id), Status::Pending);
        assert_eq!(status(&s, &b.id), Status::Pending);
    }

    #[test]
    fn token_must_be_long_enough() {
        assert_eq!(token_from(None), None);
        assert_eq!(token_from(Some("short".into())), None);
        assert_eq!(token_from(Some(format!(" {TOKEN} "))), Some(TOKEN.into()));
    }

    #[test]
    fn off_without_token_and_wrong_token_is_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        let off = State::open(dir.path()).unwrap();
        let r = pending_request(&off, 1);
        assert_eq!(
            handle(&Request::get(&format!("/m?k={TOKEN}")), &off).status,
            403
        );
        let resp = handle(
            &Request::post("/m/decide", &form(TOKEN, &r, "approve")),
            &off,
        );
        assert_eq!(resp.status, 403);
        let on = state_with_token(dir.path());
        assert_eq!(handle(&Request::get("/m"), &on).status, 403);
        assert_eq!(
            handle(&Request::get("/m?k=wrong-token-0000000"), &on).status,
            403
        );
        let resp = handle(
            &Request::post("/m/decide", &form("wrong-token-0000000", &r, "approve")),
            &on,
        );
        assert_eq!(resp.status, 403);
        assert_eq!(status(&on, &r.id), Status::Pending, "nothing decided");
    }

    #[test]
    fn lists_pending_with_mobile_viewport_and_chain_status() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        pending_request(&s, 1);
        let resp = get_m(&s);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("width=device-width"));
        assert!(resp.body.contains("Approvals (1)"));
        assert!(resp.body.contains("synthetic post 1"));
        assert!(resp.body.contains("hash chain verified: 1 entries"));
    }

    #[test]
    fn approve_and_deny_record_phone_decider_once() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let a = pending_request(&s, 1);
        let b = pending_request(&s, 2);
        let resp = handle(&Request::post("/m/decide", &form(TOKEN, &a, "approve")), &s);
        assert_eq!(resp.status, 303);
        assert_eq!(status(&s, &a.id), Status::Approved);
        let resp = handle(&Request::post("/m/decide", &form(TOKEN, &b, "deny")), &s);
        assert_eq!(resp.status, 303);
        assert_eq!(status(&s, &b.id), Status::Denied);
        // A second tap fails closed.
        let resp = handle(&Request::post("/m/decide", &form(TOKEN, &a, "deny")), &s);
        assert_eq!(resp.status, 400);
        assert_eq!(status(&s, &a.id), Status::Approved);
        let page = get_m(&s).body;
        assert!(page.contains("Approvals (0)"));
        assert!(page.contains("approved by phone"));
        assert!(page.contains("denied by phone"));
    }

    #[test]
    fn stale_hash_and_bad_decision_decide_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let mut r = pending_request(&s, 1);
        let resp = handle(&Request::post("/m/decide", &form(TOKEN, &r, "maybe")), &s);
        assert_eq!(resp.status, 400);
        r.payload_hash = "0".repeat(64);
        let resp = handle(&Request::post("/m/decide", &form(TOKEN, &r, "approve")), &s);
        assert_eq!(resp.status, 400);
        assert_eq!(status(&s, &r.id), Status::Pending);
    }

    #[test]
    fn tampered_log_withholds_requests() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let r = pending_request(&s, 1);
        let raw = std::fs::read_to_string(s.log_path()).unwrap();
        std::fs::write(
            s.log_path(),
            raw.replace("synthetic post 1", "synthetic post 9"),
        )
        .unwrap();
        let page = get_m(&s).body;
        assert!(page.contains("TAMPERED"));
        assert!(!page.contains("synthetic post"));
        let resp = handle(&Request::post("/m/decide", &form(TOKEN, &r, "approve")), &s);
        assert!(resp.body.contains("TAMPERED"));
    }

    /// The env path wins when set; an unset or empty variable falls back to
    /// the data dir log. Pure path resolution: no filesystem or real env.
    #[test]
    fn resolve_prefers_env_path_over_data_dir() {
        let data = std::path::PathBuf::from("/synthetic/data");
        let shared = "/synthetic/shared/decision_log.jsonl".to_string();
        let some = |k: &str| {
            if k == DECISION_LOG_ENV_VAR {
                Some(shared.clone())
            } else {
                None
            }
        };
        assert_eq!(
            resolve_log_path(&data, &some),
            std::path::PathBuf::from(&shared)
        );
        let empty = |k: &str| {
            if k == DECISION_LOG_ENV_VAR {
                Some(String::new())
            } else {
                None
            }
        };
        for env in [&empty as &dyn Fn(&str) -> Option<String>, &env_none] {
            assert_eq!(
                resolve_log_path(&data, env),
                data.join("decision_log.jsonl")
            );
        }
    }

    /// The phone list reads pending items from the `$DECISION_LOG_PATH` log,
    /// not the `--data` dir log, when the variable is set.
    #[test]
    fn list_reads_pending_from_env_log() {
        let data_dir = tempfile::tempdir().unwrap();
        let s = state_with_token(data_dir.path());
        pending_request(&s, 1);
        let shared = tempfile::tempdir().unwrap();
        let env_log = shared.path().join("decision_log.jsonl");
        pending_request_at(&env_log, 2);
        let env = env_log_path(&env_log);
        let resp = page_list_with_env(&Request::get("/m").with_cookie(COOKIE), &s, &env);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("synthetic post 2"));
        assert!(!resp.body.contains("synthetic post 1"));
    }

    /// Decided items leave the pending queue: after one approve and one
    /// deny, only the remaining pending item renders with buttons.
    #[test]
    fn list_excludes_decided_items() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let a = pending_request(&s, 1);
        let b = pending_request(&s, 2);
        pending_request(&s, 3);
        for (r, decision) in [(&a, "approve"), (&b, "deny")] {
            let resp = do_decide_with_env(
                &Request::post("/m/decide", &form(TOKEN, r, decision)),
                &s,
                &env_none,
            );
            assert_eq!(resp.status, 303);
        }
        let resp = page_list_with_env(&Request::get("/m").with_cookie(COOKIE), &s, &env_none);
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("Approvals (1)"));
        assert!(resp.body.contains("synthetic post 3"));
        assert_eq!(resp.body.matches("value=\"approve\"").count(), 1);
        assert!(resp.body.contains("approved by phone"));
        assert!(resp.body.contains("denied by phone"));
    }

    /// A missing log fails closed: the list is a 500 with no queue and no
    /// approve buttons, and deciding is refused without creating the file.
    #[test]
    fn missing_log_fails_closed_on_list_and_decide() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        assert!(!s.log_path().exists());
        let resp = page_list_with_env(&Request::get("/m").with_cookie(COOKIE), &s, &env_none);
        assert_eq!(resp.status, 500);
        assert!(!resp.body.contains("Nothing waiting"));
        assert!(!resp.body.contains("<button"));
        assert!(!resp.body.contains("/m/decide"));
        assert!(!s.log_path().exists(), "GET must not create the log");
        let body = format!(
            "k={TOKEN}&id=synthetic-1&payload_hash={}&decision=approve",
            "0".repeat(64)
        );
        let resp = do_decide_with_env(&Request::post("/m/decide", &body), &s, &env_none);
        assert_eq!(resp.status, 500);
        assert!(!resp.body.contains("<button"));
        assert!(!s.log_path().exists(), "a refused decide creates nothing");
    }

    /// A malformed log (invalid JSON) fails closed on list and decide, and
    /// the file bytes are left untouched.
    #[test]
    fn malformed_log_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let r = pending_request(&s, 1);
        let mut raw = std::fs::read_to_string(s.log_path()).unwrap();
        raw.push_str("{ not valid json\n");
        std::fs::write(s.log_path(), &raw).unwrap();
        let resp = page_list_with_env(&Request::get("/m").with_cookie(COOKIE), &s, &env_none);
        assert_eq!(resp.status, 500);
        assert!(!resp.body.contains("synthetic post 1"));
        assert!(!resp.body.contains("<button"));
        let resp = do_decide_with_env(
            &Request::post("/m/decide", &form(TOKEN, &r, "approve")),
            &s,
            &env_none,
        );
        assert_eq!(resp.status, 500);
        assert_eq!(
            std::fs::read_to_string(s.log_path()).unwrap(),
            raw,
            "a refused decide writes nothing"
        );
    }

    /// An unreadable log (a directory at the log path) fails closed: the
    /// list is a 500 with no queue and no buttons, and decide is refused.
    #[test]
    fn unreadable_log_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        std::fs::create_dir(s.log_path()).unwrap();
        let resp = page_list_with_env(&Request::get("/m").with_cookie(COOKIE), &s, &env_none);
        assert_eq!(resp.status, 500);
        assert!(!resp.body.contains("Nothing waiting"));
        assert!(!resp.body.contains("<button"));
        assert!(!resp.body.contains("/m/decide"));
        let body = format!(
            "k={TOKEN}&id=synthetic-1&payload_hash={}&decision=approve",
            "0".repeat(64)
        );
        let resp = do_decide_with_env(&Request::post("/m/decide", &body), &s, &env_none);
        assert_eq!(resp.status, 500);
    }

    /// Decisions through the phone route land in the `$DECISION_LOG_PATH`
    /// log, leaving the `--data` dir log untouched.
    #[test]
    fn decide_writes_to_env_log() {
        let data_dir = tempfile::tempdir().unwrap();
        let s = state_with_token(data_dir.path());
        let shared = tempfile::tempdir().unwrap();
        let env_log = shared.path().join("decision_log.jsonl");
        let r = pending_request_at(&env_log, 7);
        let env = env_log_path(&env_log);
        let resp = do_decide_with_env(
            &Request::post("/m/decide", &form(TOKEN, &r, "approve")),
            &s,
            &env,
        );
        assert_eq!(resp.status, 303);
        let log = DecisionLog::open(&env_log).unwrap();
        assert_eq!(
            approvals::get(&log, &r.id).unwrap().status_at(now_secs()),
            Status::Approved
        );
        assert!(!s.log_path().exists(), "the data-dir log stays untouched");
    }

    /// A decide POST with no credential at all (no cookie, no form `k`) is
    /// 403 and appends nothing, even with a valid id and payload hash.
    #[test]
    fn decide_without_any_credential_is_forbidden_and_appends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let r = pending_request(&s, 1);
        let before = std::fs::read(s.log_path()).unwrap();
        let body = format!(
            "id={}&payload_hash={}&decision=approve",
            r.id, r.payload_hash
        );
        let resp = do_decide_with_env(&Request::post("/m/decide", &body), &s, &env_none);
        assert_eq!(resp.status, 403);
        assert_eq!(status(&s, &r.id), Status::Pending);
        assert_eq!(
            std::fs::read(s.log_path()).unwrap(),
            before,
            "a refused decide writes nothing"
        );
    }

    /// A log with its tail deleted fails the head check: the list is a 500
    /// with no queue and no buttons, and deciding is refused without
    /// touching the file.
    #[test]
    fn truncated_log_withholds_phone_requests() {
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_token(dir.path());
        let r = pending_request(&s, 1);
        assert_eq!(
            std::fs::read_to_string(s.log_path())
                .unwrap()
                .lines()
                .count(),
            1
        );
        std::fs::write(s.log_path(), "").unwrap();
        let resp = page_list_with_env(&Request::get("/m").with_cookie(COOKIE), &s, &env_none);
        assert_eq!(resp.status, 500);
        assert!(!resp.body.contains("synthetic post 1"));
        assert!(!resp.body.contains("<button"));
        assert!(!resp.body.contains("/m/decide"));
        let resp = do_decide_with_env(
            &Request::post("/m/decide", &form(TOKEN, &r, "approve")),
            &s,
            &env_none,
        );
        assert_eq!(resp.status, 500);
        assert_eq!(
            std::fs::read_to_string(s.log_path()).unwrap(),
            "",
            "a refused decide writes nothing"
        );
    }
}
