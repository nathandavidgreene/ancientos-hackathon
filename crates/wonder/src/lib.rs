//! Wonder, a Muse-powered creative app on the AncientOS core: ask Muse
//! Wonder (the router's creative route) an open "what if" question about
//! public, sanitized text, behind a human approval. It is the third app on
//! the same core, alongside bookmarks-agent and muse-runs, and adds nothing
//! to core (principle 7).
//!
//! Flow, all on the shared hash-chained decision log:
//! 1. [`propose`] checks the route first: only [`Sensitivity::Sanitized`]
//!    text with the creative flag routes to [`Destination::MuseWonder`];
//!    anything else is refused before any request is recorded (fail closed).
//!    It then records a `wonder.ask` approval whose payload binds the
//!    question's sha256, so an approval covers exactly that question.
//! 2. A person approves or denies it ([`approve`]/[`deny`], web-ui
//!    `/approvals`, or the phone page `/m`).
//! 3. [`ask`] takes the question approval's [`ReleaseToken`]
//!    by value and calls the model only with the paid switch on, and only
//!    if the client is the Wonder route. It appends `approval.released`
//!    before the call and `wonder.answered` (answer digest only; text stays in private local storage)
//!    after. A refused ask appends nothing and the approval stays usable; a
//!    failed call is logged with its error.
//!
//! The live client ([`live_client`]) needs `ANCIENTOS_ALLOW_PAID=1` and
//! `WONDER_MODEL` (no default model id is guessed). Tests use mocks.
use ancientos_core::approvals::{self, Approval, ApprovalRequest, ReleaseToken, Status};
use ancientos_core::gate::{Gate, Risk};
use ancientos_core::log::DecisionLog;
use ancientos_router::data_class::DataClass;
use ancientos_router::muse::MuseClient;
use ancientos_router::{route, Destination, ModelClient, RouterError, Sensitivity};
use sha2::{Digest, Sha256};

/// Agent name recorded on every request this app makes.
pub const AGENT: &str = "wonder";
/// The one gated action: one Muse Wonder call on one approved question.
pub const ASK_ACTION: &str = "wonder.ask";
/// Log kind appended after the call.
pub const ANSWERED_KIND: &str = "wonder.answered";
/// Paid-call switch; live asks run only when it equals `"1"`.
pub const PAID_ENV_VAR: &str = "ANCIENTOS_ALLOW_PAID";
/// Model id for the Wonder route; required for live asks.
pub const MODEL_ENV_VAR: &str = "WONDER_MODEL";
/// Longest accepted question, in characters.
pub const MAX_QUESTION_CHARS: usize = 2000;
/// Completion-token cap for one Wonder answer.
pub const MAX_COMPLETION_TOKENS: u32 = 600;
/// Decider recorded for CLI approvals.
pub const CLI_DECIDER: &str = "cli";
/// Caller name recorded on egress decisions.
const CALLER: &str = "wonder";
/// CLI usage line.
pub const USAGE: &str =
    "usage: wonder [--log PATH] propose <question> | list | approve <id> | deny <id> [reason] | ask <id> | show <id>";

/// This app's gate: `wonder.ask` always needs a human; everything else is
/// denied because it is not registered.
pub fn app_gate() -> Gate {
    let mut g = Gate::new();
    g.register(ASK_ACTION, Risk::High);
    g
}

/// Prompt sent to Muse Wonder for one question.
pub fn prompt(question: &str) -> String {
    format!(
        "You are Wonder, a creative companion. Offer three short, concrete \
         \"what if\" ideas that open up this question, one line each, then one \
         small experiment to try first.\n\nQuestion: {question}"
    )
}

/// sha256 hex of a string.
fn sha256(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Content-addressed private storage beside this log; names must be digests.
fn text_path(
    log: &DecisionLog,
    namespace: &str,
    digest: &str,
) -> Result<std::path::PathBuf, String> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid content digest".into());
    }
    Ok(log
        .path()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(namespace)
        .join(digest))
}

/// Persist full text locally with private directory and file permissions.
fn save_text(log: &DecisionLog, namespace: &str, text: &str) -> Result<String, String> {
    use std::io::Write;
    let digest = sha256(text);
    let path = text_path(log, namespace, &digest)?;
    let dir = path.parent().ok_or("text directory unavailable")?;
    std::fs::create_dir_all(dir).map_err(|_| "text directory unavailable".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "text permissions failed".to_string())?;
    }
    let mut file =
        tempfile::NamedTempFile::new_in(dir).map_err(|_| "text staging failed".to_string())?;
    file.write_all(text.as_bytes())
        .map_err(|_| "text write failed".to_string())?;
    file.persist(&path)
        .map_err(|_| "text persistence failed".to_string())?;
    Ok(digest)
}

/// Read full text only when it still matches the approved/logged digest.
fn load_text(log: &DecisionLog, namespace: &str, digest: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(text_path(log, namespace, digest)?)
        .map_err(|_| "approved text unavailable; propose again".to_string())?;
    if sha256(&text) != digest {
        return Err("stored text digest mismatch".into());
    }
    Ok(text)
}

/// Validate a question: non-empty, at most [`MAX_QUESTION_CHARS`].
fn check_question(q: &str) -> Result<(), String> {
    if q.trim().is_empty() {
        return Err("question is empty".into());
    }
    if q.chars().count() > MAX_QUESTION_CHARS {
        return Err(format!("question is over {MAX_QUESTION_CHARS} characters"));
    }
    Ok(())
}

/// Record a question as a pending `wonder.ask` approval (24h expiry). Only
/// sanitized text may route to Muse Wonder; sensitive or unknown text is
/// refused and nothing is appended.
pub fn propose(
    log: &mut DecisionLog,
    question: &str,
    sensitivity: Sensitivity,
    now: u64,
) -> Result<ApprovalRequest, String> {
    check_question(question)?;
    let class = bookmarks_agent::translate::class_of(question);
    if class != DataClass::Public {
        return Err("question is not public (refused)".into());
    }
    let question = bookmarks_agent::sanitize::sanitize_uncapped(question);
    let question = question.as_str();
    let dest = route(sensitivity, true);
    if dest != Destination::MuseWonder {
        return Err(format!(
            "{sensitivity:?} text routes to {dest:?}, not Muse Wonder (refused)"
        ));
    }
    let digest = save_text(log, "wonder-questions", question)?;
    let summary = format!("question {}", &digest[..12]);
    approvals::request(
        log,
        &app_gate(),
        AGENT,
        ASK_ACTION,
        &format!("Ask Muse Wonder: {summary}"),
        serde_json::json!({ "question_sha256": digest, "class": class.as_str() }),
        now,
        None,
    )
    .map_err(|e| e.to_string())
}

/// True if the approval belongs to this app.
fn is_ours(a: &Approval) -> bool {
    a.request.agent == AGENT && a.request.action == ASK_ACTION
}

/// Look up one of this app's requests; other apps' ids are refused.
fn own(log: &DecisionLog, id: &str) -> Result<Approval, String> {
    let a = approvals::get(log, id).map_err(|e| e.to_string())?;
    if !is_ours(&a) {
        return Err(format!("'{id}' is not a {AGENT} request (refused)"));
    }
    Ok(a)
}

/// Every question this app proposed, oldest first, with its folded state.
pub fn questions(log: &DecisionLog) -> Result<Vec<Approval>, String> {
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    Ok(approvals::replay(&entries)
        .into_iter()
        .filter(is_ours)
        .collect())
}

/// Approve a pending question. Records the decision only; nothing runs.
pub fn approve(log: &mut DecisionLog, id: &str, decided_by: &str, now: u64) -> Result<(), String> {
    let a = own(log, id)?;
    approvals::approve(log, id, &a.request.payload_hash, decided_by, now)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Deny a pending question.
pub fn deny(
    log: &mut DecisionLog,
    id: &str,
    decided_by: &str,
    reason: &str,
    now: u64,
) -> Result<(), String> {
    let a = own(log, id)?;
    approvals::deny(log, id, &a.request.payload_hash, decided_by, reason, now)
        .map_err(|e| e.to_string())
}

/// Ask Muse Wonder one approved question, exactly once, and return the
/// answer. Consumes the question approval's [`ReleaseToken`] by value:
/// without a token the model is never called. Refused (nothing appended,
/// approval kept) unless the token's approval belongs to this app,
/// `allow_paid` is on, the client is the Wonder route, and the stored
/// question still matches its sha256.
pub fn ask(
    log: &mut DecisionLog,
    token: ReleaseToken,
    client: &dyn ModelClient,
    allow_paid: bool,
) -> Result<String, String> {
    let a = own(log, token.approval_id())?;
    let id = a.request.id.clone();
    if token.payload_hash() != a.request.payload_hash {
        return Err(format!("'{id}' token does not match its request (refused)"));
    }
    if !allow_paid {
        return Err(format!(
            "Muse Wonder calls are paid: set {PAID_ENV_VAR}=1 (fail closed; approval kept)"
        ));
    }
    if client.destination() != Destination::MuseWonder {
        return Err(format!(
            "client goes to {:?}, not Muse Wonder (refused)",
            client.destination()
        ));
    }
    let digest = a.request.payload["question_sha256"]
        .as_str()
        .ok_or("missing question digest")?;
    let question = load_text(log, "wonder-questions", digest)?;
    if a.request.payload["class"].as_str() != Some("public")
        || bookmarks_agent::translate::class_of(&question) != DataClass::Public
        || bookmarks_agent::sanitize::sanitize_uncapped(&question) != question
    {
        return Err("question classification or sanitization changed (refused)".into());
    }
    approvals::release(log, token, &a.request.payload).map_err(|e| e.to_string())?;
    if bookmarks_agent::translate::class_of(&question) != DataClass::Public {
        return Err("released question is not public (refused)".into());
    }
    let result =
        client.complete_with_class(DataClass::Public, CALLER, Some(log), &prompt(&question));
    let entry = match &result {
        Ok(text) => {
            let digest = save_text(log, "wonder-answers", text)?;
            serde_json::json!({"id": id, "answer_sha256": digest, "error": null})
        }
        Err(e) => serde_json::json!({ "id": id, "answer": null, "error": e.to_string() }),
    };
    log.append(ANSWERED_KIND, entry)
        .map_err(|e| e.to_string())?;
    result.map_err(|e| format!("Muse Wonder call failed: {e}"))
}

/// The logged answer for a question, if one was recorded.
pub fn answer_of(log: &DecisionLog, id: &str) -> Result<Option<String>, String> {
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    let digest = entries
        .iter()
        .rev()
        .find(|e| e.kind == ANSWERED_KIND && e.payload["id"].as_str() == Some(id))
        .and_then(|e| e.payload["answer_sha256"].as_str());
    digest
        .map(|hash| load_text(log, "wonder-answers", hash))
        .transpose()
}

/// Muse client pointed at the Wonder route.
#[derive(Debug, Clone)]
pub struct WonderClient {
    inner: MuseClient,
}

impl ModelClient for WonderClient {
    /// The creative hosted route.
    fn destination(&self) -> Destination {
        Destination::MuseWonder
    }

    /// Refuses: the data class is unknown here (fail closed). Use
    /// [`complete_with_class`](Self::complete_with_class).
    fn complete(&self, _prompt: &str) -> Result<String, RouterError> {
        Err(RouterError::Refused(
            "wonder needs a data class; use complete_with_class".into(),
        ))
    }

    /// Complete through the Muse chat path with its class and egress checks.
    fn complete_with_class(
        &self,
        class: DataClass,
        caller: &str,
        log: Option<&mut DecisionLog>,
        prompt: &str,
    ) -> Result<String, RouterError> {
        self.inner
            .chat_with_class(class, caller, log, prompt)
            .map(|r| r.content)
    }
}

/// Live Wonder client from env: needs a non-empty [`MODEL_ENV_VAR`]; the
/// Muse client reads its base URL and paid switch from env and the API key
/// lazily per call (never here, never printed).
pub fn live_client(env: &dyn Fn(&str) -> Option<String>) -> Result<WonderClient, String> {
    let model = env(MODEL_ENV_VAR)
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| format!("set {MODEL_ENV_VAR} to the Muse Wonder model id (fail closed)"))?;
    Ok(WonderClient {
        inner: MuseClient::from_env()
            .with_model(model)
            .with_max_completion_tokens(MAX_COMPLETION_TOKENS),
    })
}

/// Current Unix time in seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Decision log path: `--log`, else `$DECISION_LOG_PATH`, else
/// `$ANCIENTOS_HOME/decision_log.jsonl`, else
/// `$HOME/.ancientos-hackathon/decision_log.jsonl` (the shared log).
fn log_path(
    flag: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<std::path::PathBuf, String> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(p) = flag
        .map(str::to_string)
        .or_else(|| get("DECISION_LOG_PATH"))
    {
        return Ok(p.into());
    }
    let dir = match get("ANCIENTOS_HOME") {
        Some(d) => std::path::PathBuf::from(d),
        None => std::path::PathBuf::from(get("HOME").ok_or("HOME not set")?)
            .join(".ancientos-hackathon"),
    };
    Ok(dir.join("decision_log.jsonl"))
}

/// The CLI (see [`USAGE`]). Questions from the CLI are treated as
/// sanitized: the person typing them is choosing to send them. `env` reads
/// environment variables so tests never touch the real environment.
pub fn cli(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    let (flag, rest) = match args {
        [f, p, rest @ ..] if f == "--log" => (Some(p.as_str()), rest),
        _ => (None, args),
    };
    let path = log_path(flag, env)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("log dir: {e}"))?;
    }
    let mut log = DecisionLog::open_with_key_env(&path, env).map_err(|e| e.to_string())?;
    let now = now_secs();
    let words: Vec<&str> = rest.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["propose", q @ ..] if !q.is_empty() => {
            let r = propose(&mut log, &q.join(" "), Sensitivity::Sanitized, now)?;
            println!("proposed {} ({})", r.id, r.summary);
        }
        ["list"] => {
            for a in questions(&log)? {
                println!(
                    "{}\t{}\t{}",
                    a.request.id,
                    a.status_at(now).as_str(),
                    a.request.summary
                );
            }
        }
        ["approve", id] => {
            approve(&mut log, id, CLI_DECIDER, now)?;
            println!("approved {id} (ask it with: wonder ask {id})");
        }
        ["deny", id, reason @ ..] => {
            deny(&mut log, id, CLI_DECIDER, &reason.join(" "), now)?;
            println!("denied {id}");
        }
        ["ask", id] => {
            let a = own(&log, id)?;
            if a.status_at(now) != Status::Approved {
                return Err(format!(
                    "'{id}' is not approved (status: {})",
                    a.status_at(now).as_str()
                ));
            }
            let paid = env(PAID_ENV_VAR).as_deref() == Some("1");
            if !paid {
                return Err(format!(
                    "Muse Wonder calls are paid: set {PAID_ENV_VAR}=1 (fail closed; approval kept)"
                ));
            }
            let client = live_client(env)?;
            // Claim first: only an approved question yields a token, and
            // `ask` consumes it, so the model cannot be called without one.
            let token = approvals::claim_release_token(&log, id, &a.request.payload_hash)
                .map_err(|e| e.to_string())?;
            println!("{}", ask(&mut log, token, &client, paid)?);
        }
        ["show", id] => match answer_of(&log, id)? {
            Some(a) => println!("{a}"),
            None => return Err(format!("no answer recorded for '{id}'")),
        },
        _ => return Err(USAGE.to_string()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const NOW: u64 = 1_900_000_000;

    /// Mock model: records prompts and answers with a fixed reply.
    struct Mock {
        dest: Destination,
        seen: RefCell<Vec<String>>,
        fail: bool,
    }

    impl Mock {
        /// Test helper: mock at a destination.
        fn at(dest: Destination) -> Self {
            Self {
                dest,
                seen: RefCell::new(Vec::new()),
                fail: false,
            }
        }
    }

    impl ModelClient for Mock {
        /// Mock destination.
        fn destination(&self) -> Destination {
            self.dest
        }

        /// Record the prompt and reply (or fail when set).
        fn complete(&self, prompt: &str) -> Result<String, RouterError> {
            self.seen.borrow_mut().push(prompt.to_string());
            if self.fail {
                Err(RouterError::Http("synthetic 500".into()))
            } else {
                Ok("1. what if A\n2. what if B\n3. what if C\nTry: A".into())
            }
        }
    }

    /// Test helper: fresh log in a temp dir.
    fn log() -> (tempfile::TempDir, DecisionLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path().join("log.jsonl")).unwrap();
        (dir, log)
    }

    /// Test helper: claim the release token for an approved question.
    fn claim(log: &DecisionLog, r: &ApprovalRequest) -> ReleaseToken {
        approvals::claim_release_token(log, &r.id, &r.payload_hash).unwrap()
    }

    /// Test helper: number of log entries.
    fn len(log: &DecisionLog) -> usize {
        log.verify_or_empty().unwrap().len()
    }

    #[test]
    fn only_sanitized_text_routes_to_wonder() {
        let (_d, mut log) = log();
        for s in [Sensitivity::Sensitive, Sensitivity::Unknown] {
            assert!(propose(&mut log, "what if", s, NOW).is_err());
        }
        assert!(propose(&mut log, " ", Sensitivity::Sanitized, NOW).is_err());
        let long = "x".repeat(MAX_QUESTION_CHARS + 1);
        assert!(propose(&mut log, &long, Sensitivity::Sanitized, NOW).is_err());
        assert_eq!(len(&log), 0, "refusals append nothing");
        let r = propose(
            &mut log,
            "what if bookmarks dreamed",
            Sensitivity::Sanitized,
            NOW,
        )
        .unwrap();
        assert_eq!(r.action, ASK_ACTION);
        assert_eq!(questions(&log).unwrap().len(), 1);
    }

    #[test]
    fn gate_denies_anything_but_ask() {
        use ancientos_core::gate::{Proposal, Verdict};
        let g = app_gate();
        let p = |a: &str| Proposal {
            agent: AGENT.into(),
            action: a.into(),
        };
        assert_eq!(g.evaluate(&p(ASK_ACTION)), Verdict::NeedsHuman);
        assert!(matches!(
            g.evaluate(&p("wonder.ask.unattended")),
            Verdict::Deny { .. }
        ));
    }

    /// Ask consumes a claimed token; refusals keep the approval usable.
    #[test]
    fn ask_needs_approval_paid_and_wonder_route() {
        let (_d, mut log) = log();
        let r = propose(&mut log, "what if", Sensitivity::Sanitized, NOW).unwrap();
        let mock = Mock::at(Destination::MuseWonder);
        // Pending: no token exists, so the model cannot be called.
        assert!(
            approvals::claim_release_token(&log, &r.id, &r.payload_hash).is_err(),
            "pending"
        );
        approve(&mut log, &r.id, "human", NOW + 1).unwrap();
        let before = len(&log);
        let unpaid = claim(&log, &r);
        assert!(ask(&mut log, unpaid, &mock, false)
            .unwrap_err()
            .contains(PAID_ENV_VAR));
        let spark = Mock::at(Destination::MuseSpark);
        let rerouted = claim(&log, &r);
        assert!(ask(&mut log, rerouted, &spark, true).is_err());
        assert_eq!(len(&log), before, "refusals append nothing");
        assert!(mock.seen.borrow().is_empty() && spark.seen.borrow().is_empty());
        let token = claim(&log, &r);
        let answer = ask(&mut log, token, &mock, true).unwrap();
        assert!(answer.contains("what if A"));
        assert_eq!(mock.seen.borrow().len(), 1);
        assert!(mock.seen.borrow()[0].contains("Question: what if"));
        assert_eq!(
            answer_of(&log, &r.id).unwrap().as_deref(),
            Some(answer.as_str())
        );
        // Exactly once: released approvals yield no second token.
        assert!(approvals::claim_release_token(&log, &r.id, &r.payload_hash).is_err());
        assert_eq!(mock.seen.borrow().len(), 1);
    }

    /// Denied questions yield no token, so the model is never called.
    #[test]
    fn denied_and_foreign_requests_never_call() {
        let (_d, mut log) = log();
        let r = propose(&mut log, "what if", Sensitivity::Sanitized, NOW).unwrap();
        deny(&mut log, &r.id, "human", "no", NOW + 1).unwrap();
        let mock = Mock::at(Destination::MuseWonder);
        assert!(approvals::claim_release_token(&log, &r.id, &r.payload_hash).is_err());
        let mut g = Gate::new();
        g.register("other.x", Risk::High);
        let other = approvals::request(
            &mut log,
            &g,
            "other",
            "other.x",
            "s",
            serde_json::json!({}),
            NOW,
            None,
        )
        .unwrap();
        assert!(approve(&mut log, &other.id, "human", NOW)
            .unwrap_err()
            .contains("not a wonder"));
        assert!(mock.seen.borrow().is_empty());
    }

    /// A failed model call is logged with its error and never retried.
    #[test]
    fn failed_call_is_logged_and_not_retried() {
        let (_d, mut log) = log();
        let r = propose(&mut log, "what if", Sensitivity::Sanitized, NOW).unwrap();
        approve(&mut log, &r.id, "human", NOW + 1).unwrap();
        let mut mock = Mock::at(Destination::MuseWonder);
        mock.fail = true;
        let token = claim(&log, &r);
        assert!(ask(&mut log, token, &mock, true)
            .unwrap_err()
            .contains("synthetic 500"));
        assert_eq!(answer_of(&log, &r.id).unwrap(), None);
        let last = log.verify().unwrap().pop().unwrap();
        assert_eq!(last.kind, ANSWERED_KIND);
        assert!(last.payload["error"]
            .as_str()
            .unwrap()
            .contains("synthetic 500"));
    }

    /// `ask` takes the release token by value (compile-time).
    #[test]
    fn ask_takes_release_token_by_value() {
        // Compiles only if `ask` takes `ReleaseToken` by value.
        let _sig: fn(
            &mut DecisionLog,
            ReleaseToken,
            &dyn ModelClient,
            bool,
        ) -> Result<String, String> = ask;
    }

    #[test]
    fn live_client_needs_model_and_refuses_classless_calls() {
        assert!(live_client(&|_| None).is_err());
        assert!(live_client(&|k| (k == MODEL_ENV_VAR).then(|| " ".into())).is_err());
        let c = live_client(&|k| (k == MODEL_ENV_VAR).then(|| "synthetic-wonder".into())).unwrap();
        assert_eq!(c.destination(), Destination::MuseWonder);
        assert!(c.complete("x").is_err());
    }

    #[test]
    fn cli_requires_configured_signing_key_before_log_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let missing_key = dir.path().join("absent-key");
        let env = |key: &str| {
            (key == ancientos_core::log::LOG_KEY_ENV_VAR)
                .then(|| missing_key.to_string_lossy().into_owned())
        };
        let args = vec![
            "--log".into(),
            path.to_string_lossy().into_owned(),
            "propose".into(),
            "what if".into(),
        ];
        assert!(cli(&args, &env).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn proposals_classify_before_sanitizing() {
        let (_dir, mut log) = log();
        for text in [
            "email person@example.com",
            "password=synthetic-secret",
            "Bearer synthetic-token",
        ] {
            assert!(propose(&mut log, text, Sensitivity::Sanitized, NOW).is_err());
        }
        assert!(log.verify_or_empty().unwrap().is_empty());
        let req = propose(
            &mut log,
            "try https://example.com/ @demo",
            Sensitivity::Sanitized,
            NOW,
        )
        .unwrap();
        assert_eq!(req.payload["class"], "public");
        assert!(req.payload.get("question").is_none());
        let stored = load_text(
            &log,
            "wonder-questions",
            req.payload["question_sha256"].as_str().unwrap(),
        )
        .unwrap();
        assert!(!stored.contains("https://"));
    }

    #[test]
    fn question_and_answer_text_stay_out_of_log_payloads() {
        let (_dir, mut log) = log();
        let text = "What if houses floated above parks";
        let request = propose(&mut log, text, Sensitivity::Sanitized, NOW).unwrap();
        assert!(request.payload.get("question").is_none());
        approve(&mut log, &request.id, "human", NOW + 1).unwrap();
        let token = claim(&log, &request);
        let answer = ask(&mut log, token, &Mock::at(Destination::MuseWonder), true).unwrap();
        let raw = std::fs::read_to_string(log.path()).unwrap();
        assert!(!raw.contains(text));
        assert!(!raw.contains(&answer));
        assert_eq!(answer_of(&log, &request.id).unwrap(), Some(answer));
    }

    #[test]
    fn cli_flow_without_paid_never_calls() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log.jsonl").to_string_lossy().to_string();
        let args = |v: &[&str]| -> Vec<String> {
            ["--log", p.as_str()]
                .iter()
                .chain(v)
                .map(|s| s.to_string())
                .collect()
        };
        let env = |_: &str| None;
        cli(&args(&["propose", "what", "if"]), &env).unwrap();
        let log = DecisionLog::open(&p).unwrap();
        let id = questions(&log).unwrap()[0].request.id.clone();
        assert!(cli(&args(&["ask", &id]), &env)
            .unwrap_err()
            .contains("not approved"));
        cli(&args(&["approve", &id]), &env).unwrap();
        assert!(cli(&args(&["ask", &id]), &env)
            .unwrap_err()
            .contains(PAID_ENV_VAR));
        let paid = |k: &str| (k == PAID_ENV_VAR).then(|| "1".to_string());
        assert!(cli(&args(&["ask", &id]), &paid)
            .unwrap_err()
            .contains(MODEL_ENV_VAR));
        assert!(cli(&args(&["show", &id]), &env).is_err());
        assert!(cli(&args(&["bogus"]), &env).is_err());
        cli(&args(&["list"]), &env).unwrap();
    }
}
