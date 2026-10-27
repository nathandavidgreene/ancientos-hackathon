//! Phone approvals: a small-screen approve/deny page over the core
//! approvals queue and hash-chained decision log.
//!
//! - `GET /m?k=TOKEN` lists pending requests (big buttons), the chain status
//!   and the last few decisions.
//! - `POST /m/decide` (form: `k`, `id`, `payload_hash`, `decision` =
//!   `approve`|`deny`, optional `reason`) records one decision as
//!   [`PHONE_DECIDER`] and redirects back to `/m`.
//!
//! Fail closed: with no phone token configured both routes answer 403, a
//! wrong or missing token answers 403 with nothing decided, a tampered log
//! withholds every request, and a stale payload hash decides nothing. Like
//! `/approvals`, the page only records decisions: nothing executes here.
//! The server still binds loopback only; reaching it from a phone is the
//! deployer's job (for example a tailnet-only reverse proxy).
use crate::{escape, now_secs, parse_body, Request, Response, State};
use ancientos_core::approvals::{self, ApprovalError};
use ancientos_core::log::{DecisionLog, LogError};

/// Decider recorded for phone decisions.
pub const PHONE_DECIDER: &str = "phone";
/// Env var holding the phone token (read by the binary at startup).
pub const TOKEN_ENV: &str = "ANCIENTOS_PHONE_TOKEN";
/// Shortest accepted token; shorter values leave phone approvals off.
pub const MIN_TOKEN_LEN: usize = 16;
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
    raw.map(|s| s.trim().to_string())
        .filter(|s| s.len() >= MIN_TOKEN_LEN)
}

/// Constant-time string equality (no early exit on the first mismatch).
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// True only when a token is configured and `given` matches it.
fn authorized(state: &State, given: Option<&String>) -> bool {
    match (&state.phone_token, given) {
        (Some(t), Some(g)) => same(t, g),
        _ => false,
    }
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

/// Error page with a status code and a link back.
fn error(status: u16, title: &str, msg: &str, k: &str) -> Response {
    Response::status(
        status,
        page(
            title,
            &format!(
                "<h1>{}</h1><p>{}</p><p><a href=\"/m?k={}\">Back</a></p>",
                escape(title),
                escape(msg),
                percent_encode(k)
            ),
        ),
    )
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

/// `GET /m`: pending requests with approve/deny buttons, chain status and
/// recent decisions.
pub fn page_list(req: &Request, state: &State) -> Response {
    let Some(k) = req
        .query
        .get("k")
        .filter(|_| authorized(state, req.query.get("k")))
    else {
        return forbidden(state);
    };
    let log = match DecisionLog::open(state.log_path()) {
        Err(LogError::Chain(n)) => return tampered(n),
        Err(e) => return error(500, "Approvals failed", &e.to_string(), k),
        Ok(log) => log,
    };
    let entries = match log.verify_or_empty() {
        Err(LogError::Chain(n)) => return tampered(n),
        Err(e) => return error(500, "Approvals failed", &e.to_string(), k),
        Ok(v) => v,
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
    let ke = escape(k);
    for a in &pend {
        let r = &a.request;
        let hidden = format!(
            "<input type=\"hidden\" name=\"k\" value=\"{ke}\">\
             <input type=\"hidden\" name=\"id\" value=\"{}\">\
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
pub fn do_decide(body: &[u8], state: &State) -> Response {
    let form = parse_body(body);
    if !authorized(state, form.get("k")) {
        return forbidden(state);
    }
    let k = form.get("k").map(String::as_str).unwrap_or("");
    let field = |n: &str| form.get(n).map(String::as_str).unwrap_or("").trim();
    let (id, hash) = (field("id"), field("payload_hash"));
    if id.is_empty() || hash.is_empty() {
        return error(400, "Bad request", "Missing id or payload_hash.", k);
    }
    let approve_it = match field("decision") {
        "approve" => true,
        "deny" => false,
        _ => return error(400, "Bad request", "decision must be approve or deny.", k),
    };
    let mut log = match DecisionLog::open(state.log_path()) {
        Err(LogError::Chain(n)) => return tampered(n),
        Err(e) => return error(500, "Approvals failed", &e.to_string(), k),
        Ok(log) => log,
    };
    let now = now_secs();
    let outcome: Result<(), ApprovalError> = if approve_it {
        approvals::approve(&mut log, id, hash, PHONE_DECIDER, now).map(|_| ())
    } else {
        approvals::deny(&mut log, id, hash, PHONE_DECIDER, field("reason"), now)
    };
    match outcome {
        Ok(()) => Response::redirect(format!("/m?k={}", percent_encode(k))),
        Err(e @ ApprovalError::HashMismatch(_)) => error(
            400,
            "Stale approval",
            &format!("{e}; nothing was decided. Reload and retry."),
            k,
        ),
        Err(ApprovalError::Log(e)) => error(500, "Approvals failed", &e.to_string(), k),
        Err(e) => error(400, "Approval failed", &e.to_string(), k),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle;
    use ancientos_core::approvals::{ApprovalRequest, Status};

    const TOKEN: &str = "synthetic-phone-token-0001";

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
        let resp = handle(&Request::get(&format!("/m?k={TOKEN}")), &s);
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
        let page = handle(&Request::get(&format!("/m?k={TOKEN}")), &s).body;
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
        let page = handle(&Request::get(&format!("/m?k={TOKEN}")), &s).body;
        assert!(page.contains("TAMPERED"));
        assert!(!page.contains("synthetic post"));
        let resp = handle(&Request::post("/m/decide", &form(TOKEN, &r, "approve")), &s);
        assert!(resp.body.contains("TAMPERED"));
    }
}
