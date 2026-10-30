//! Muse runs, the second app on the AncientOS core: a proposed Muse Code run
//! is a gated action. It exists to show the core serves more than one app
//! (principle 7) with nothing app-specific added to core.
//!
//! Flow, all on the shared hash-chained decision log:
//! 1. [`propose`] sends a `muse.run` request through
//!    [`approvals::request`] with this
//!    app's own [`app_gate`] (fail closed: unknown actions are denied). The
//!    payload binds the log number, summary, prompt path and the prompt's
//!    sha256, so an approval covers exactly that prompt.
//! 2. A person calls [`approve`] or [`deny`] (here, or on web-ui's
//!    `/approvals` page, which lists every pending request in the log).
//! 3. [`run`] takes the run approval's [`ReleaseToken`]
//!    by value and executes `scripts/muse_run.sh` only with
//!    `ANCIENTOS_ALLOW_PAID=1` (every live Muse Code run is billed), and
//!    only if the prompt file still hashes to the approved sha256. It then
//!    appends `approval.released` before starting and `muse_runs.finished`
//!    with the exit code after. A refused run appends nothing and the
//!    approval stays usable.
//!
//! This crate only touches requests whose agent is [`AGENT`] and whose action
//! is [`RUN_ACTION`]; anything else in the log is refused (fail closed).
use ancientos_core::approvals::{self, Approval, ApprovalRequest, ReleaseToken, Status};
use ancientos_core::gate::{Gate, Risk};
use ancientos_core::log::DecisionLog;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Agent name recorded on every request this app makes.
pub const AGENT: &str = "muse-runs";
/// The one gated action: run Muse Code once on an approved prompt.
pub const RUN_ACTION: &str = "muse.run";
/// Log entry kind appended after the script exits.
pub const FINISHED_KIND: &str = "muse_runs.finished";
/// Paid-call switch; runs execute only when it equals `"1"`.
pub const PAID_ENV_VAR: &str = "ANCIENTOS_ALLOW_PAID";
/// Script executed for an approved run, relative to the repo root.
pub const SCRIPT: &str = "scripts/muse_run.sh";
/// Decider recorded for CLI approvals (the person at the terminal).
pub const CLI_DECIDER: &str = "cli";
/// CLI usage line.
pub const USAGE: &str = "usage: muse-runs [--log PATH] [--repo DIR] propose <log-no> <summary> <prompt-file> | list | approve <id> | deny <id> [reason] | run <id>";

/// This app's gate: `muse.run` always needs a human; every other action is
/// denied because it is not registered.
pub fn app_gate() -> Gate {
    let mut g = Gate::new();
    g.register(RUN_ACTION, Risk::High);
    g
}

/// What one approval authorizes: a single `muse_run.sh` call on one prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    pub log_no: String,
    pub summary: String,
    pub prompt_file: PathBuf,
    pub prompt_sha256: String,
}

/// sha256 hex of a file's bytes.
fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read prompt {}: {e}", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Classify original prompt bytes before sanitizing them for hosted execution.
fn public_prompt(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path).map_err(|_| "prompt unreadable".to_string())?;
    if bookmarks_agent::translate::class_of(&raw).as_str() != "public" {
        return Err("prompt is not public (refused)".into());
    }
    Ok(bookmarks_agent::sanitize::sanitize_uncapped(&raw))
}

impl RunSpec {
    /// Build a spec from a prompt file, hashing its current contents. The log
    /// number must be 1-16 ASCII letters, digits, `-` or `_`; the summary must
    /// be non-empty with no newline or `|` (it becomes a markdown table row in
    /// the Muse Code log).
    pub fn from_prompt(log_no: &str, summary: &str, prompt_file: &Path) -> Result<Self, String> {
        let ok_no = !log_no.is_empty()
            && log_no.len() <= 16
            && log_no
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok_no {
            return Err(format!("bad log number '{log_no}'"));
        }
        if summary.trim().is_empty() || summary.contains(['\n', '\r', '|']) {
            return Err("summary must be one non-empty line without '|'".to_string());
        }
        let prompt_file = std::fs::canonicalize(prompt_file)
            .map_err(|e| format!("cannot read prompt {}: {e}", prompt_file.display()))?;
        public_prompt(&prompt_file)?;
        let prompt_sha256 = sha256_file(&prompt_file)?;
        Ok(Self {
            log_no: log_no.to_string(),
            summary: summary.to_string(),
            prompt_file,
            prompt_sha256,
        })
    }

    /// Approval payload for this spec.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "log_no": self.log_no,
            "summary": self.summary,
            "prompt_file": self.prompt_file.file_name().unwrap_or_default().to_string_lossy(),
            "prompt_sha256": self.prompt_sha256,
            "class": "public",
        })
    }

    /// Read a spec back from a stored payload; `None` when malformed.
    pub fn from_payload(v: &serde_json::Value) -> Option<Self> {
        let s = |k: &str| v.get(k)?.as_str().map(str::to_string);
        if v["class"].as_str() != Some("public") {
            return None;
        }
        Some(Self {
            log_no: s("log_no")?,
            summary: s("summary")?,
            prompt_file: PathBuf::from(s("prompt_file")?),
            prompt_sha256: s("prompt_sha256")?,
        })
    }
}

/// Record a proposed run as a pending approval (24h expiry). A duplicate
/// pending proposal returns the existing request.
pub fn propose(log: &mut DecisionLog, spec: &RunSpec, now: u64) -> Result<ApprovalRequest, String> {
    public_prompt(&spec.prompt_file)?;
    approvals::request(
        log,
        &app_gate(),
        AGENT,
        RUN_ACTION,
        &format!("Muse Code run #{}: {}", spec.log_no, spec.summary),
        spec.payload(),
        now,
        None,
    )
    .map_err(|e| e.to_string())
}

/// True if the approval belongs to this app.
fn is_ours(a: &Approval) -> bool {
    a.request.agent == AGENT && a.request.action == RUN_ACTION
}

/// Every run this app proposed, oldest first, with its folded state.
pub fn runs(log: &DecisionLog, now: u64) -> Result<Vec<Approval>, String> {
    let mut out = approvals::pending(log, now).map_err(|e| e.to_string())?;
    out.extend(approvals::decided(log, now).map_err(|e| e.to_string())?);
    out.retain(is_ours);
    out.sort_by(|a, b| {
        (a.request.requested_at, &a.request.id).cmp(&(b.request.requested_at, &b.request.id))
    });
    Ok(out)
}

/// Look up one of this app's runs; other apps' requests are refused.
fn own(log: &DecisionLog, id: &str) -> Result<Approval, String> {
    let a = approvals::get(log, id).map_err(|e| e.to_string())?;
    if !is_ours(&a) {
        return Err(format!("'{id}' is not a {AGENT} request (refused)"));
    }
    Ok(a)
}

/// Approve a pending run. Records the decision only; nothing executes.
pub fn approve(log: &mut DecisionLog, id: &str, decided_by: &str, now: u64) -> Result<(), String> {
    let a = own(log, id)?;
    approvals::approve(log, id, &a.request.payload_hash, decided_by, now)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Deny a pending run.
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

/// Where and how [`run`] executes.
#[derive(Debug, Clone)]
pub struct Runner {
    /// Repo root holding `scripts/muse_run.sh` (also the working directory).
    pub repo: PathBuf,
    /// The paid-call switch (`ANCIENTOS_ALLOW_PAID=1`).
    pub allow_paid: bool,
}

/// Execute one approved run exactly once and return the script's exit code.
/// Consumes the run approval's [`ReleaseToken`] by value: without a token
/// the script cannot run. Refused (nothing appended, approval kept) unless
/// the token's approval belongs to this app, the paid switch is on, the
/// prompt still matches its approved sha256 and the script exists. Then
/// appends `approval.released`, runs the script with
/// `<log-no> <summary> <prompt-file>`, and appends `muse_runs.finished`.
pub fn run(log: &mut DecisionLog, token: ReleaseToken, runner: &Runner) -> Result<i32, String> {
    let a = own(log, token.approval_id())?;
    let id = a.request.id.clone();
    if token.payload_hash() != a.request.payload_hash {
        return Err(format!("'{id}' token does not match its request (refused)"));
    }
    if !runner.allow_paid {
        return Err(format!(
            "Muse Code runs are paid: set {PAID_ENV_VAR}=1 to run (fail closed; approval kept)"
        ));
    }
    let mut spec = RunSpec::from_payload(&a.request.payload)
        .ok_or_else(|| format!("'{id}' has a malformed payload (refused)"))?;
    if spec.prompt_file.components().count() != 1
        || !matches!(
            spec.prompt_file.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err("prompt path must be a repository basename; propose again".into());
    }
    spec.prompt_file = runner.repo.join(&spec.prompt_file);
    if sha256_file(&spec.prompt_file)? != spec.prompt_sha256 {
        return Err(format!(
            "prompt {} changed since approval (refused; propose again)",
            spec.prompt_file.display()
        ));
    }
    let sanitized = public_prompt(&spec.prompt_file)?;
    use std::io::Write;
    let mut staged =
        tempfile::NamedTempFile::new().map_err(|_| "prompt staging failed".to_string())?;
    staged
        .write_all(sanitized.as_bytes())
        .map_err(|_| "prompt staging failed".to_string())?;
    staged
        .flush()
        .map_err(|_| "prompt staging failed".to_string())?;
    let script = runner.repo.join(SCRIPT);
    if !script.is_file() {
        return Err(format!("{} not found (refused)", script.display()));
    }
    approvals::release(log, token, &a.request.payload).map_err(|e| e.to_string())?;
    let outcome = std::process::Command::new(&script)
        .arg(&spec.log_no)
        .arg(&spec.summary)
        .arg(staged.path())
        .current_dir(&runner.repo)
        .status();
    let (code, error) = match outcome {
        Ok(s) => (s.code().unwrap_or(-1), None),
        Err(e) => (-1, Some(e.to_string())),
    };
    log.append(
        FINISHED_KIND,
        serde_json::json!({ "id": id, "exit_code": code, "error": error }),
    )
    .map_err(|e| e.to_string())?;
    match error {
        Some(e) => Err(format!("could not start {}: {e}", script.display())),
        None => Ok(code),
    }
}

/// Decision log path: `$DECISION_LOG_PATH`, else
/// `$ANCIENTOS_HOME/decision_log.jsonl`, else
/// `$HOME/.ancientos-hackathon/decision_log.jsonl` (the same log
/// bookmarks-agent and web-ui use). Empty values are ignored.
pub fn log_path(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(p) = get("DECISION_LOG_PATH") {
        return Ok(PathBuf::from(p));
    }
    let dir = match get("ANCIENTOS_HOME") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(get("HOME").ok_or("HOME not set")?).join(".ancientos-hackathon"),
    };
    Ok(dir.join("decision_log.jsonl"))
}

/// Current Unix time in seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The CLI (see [`USAGE`]); returns the process exit code. `env` reads
/// environment variables so tests never touch the real environment.
pub fn cli(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<i32, String> {
    let mut log_flag: Option<PathBuf> = None;
    let mut repo: Option<PathBuf> = None;
    let mut rest = args;
    while let Some(flag) = rest.first().filter(|a| a.starts_with("--")) {
        let value = rest.get(1).filter(|v| !v.is_empty()).ok_or(USAGE)?;
        match flag.as_str() {
            "--log" => log_flag = Some(PathBuf::from(value)),
            "--repo" => repo = Some(PathBuf::from(value)),
            _ => return Err(USAGE.to_string()),
        }
        rest = &rest[2..];
    }
    let path = match log_flag {
        Some(p) => p,
        None => log_path(env)?,
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("log dir: {e}"))?;
    }
    let mut log = DecisionLog::open_with_key_env(&path, env).map_err(|e| e.to_string())?;
    let now = now_secs();
    let words: Vec<&str> = rest.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["propose", no, summary, prompt] => {
            let spec = RunSpec::from_prompt(no, summary, Path::new(prompt))?;
            let req = propose(&mut log, &spec, now)?;
            println!("proposed {} ({})", req.id, req.summary);
        }
        ["list"] => {
            for a in runs(&log, now)? {
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
            println!("approved {id} (run it with: muse-runs run {id})");
        }
        ["deny", id, reason @ ..] => {
            deny(&mut log, id, CLI_DECIDER, &reason.join(" "), now)?;
            println!("denied {id}");
        }
        ["run", id] => {
            let repo = match repo {
                Some(r) => r,
                None => std::env::current_dir().map_err(|e| e.to_string())?,
            };
            let runner = Runner {
                repo,
                allow_paid: env(PAID_ENV_VAR).as_deref() == Some("1"),
            };
            // Claim first: only an approved run yields a token, and `run`
            // consumes it, so the script cannot start without one.
            let a = own(&log, id)?;
            let token = approvals::claim_release_token(&log, id, &a.request.payload_hash)
                .map_err(|e| e.to_string())?;
            let code = run(&mut log, token, &runner)?;
            println!("ran {id}: exit {code}");
            return Ok(code);
        }
        _ => return Err(USAGE.to_string()),
    }
    Ok(0)
}

/// Current status of one of this app's runs (other apps' ids are refused).
pub fn status_of(log: &DecisionLog, id: &str, now: u64) -> Result<Status, String> {
    Ok(own(log, id)?.status_at(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const NOW: u64 = 1_700_000_000;

    /// Test fixture: temp repo with a stub `scripts/muse_run.sh` (never real
    /// Muse) that records its args to `calls.txt` and exits with `$STUB_EXIT`
    /// read from `exit.txt`, plus a prompt file and an empty log.
    struct Fx {
        dir: tempfile::TempDir,
        log: DecisionLog,
    }

    /// Spec for the fixture prompt.
    fn spec_of(dir: &tempfile::TempDir) -> RunSpec {
        RunSpec::from_prompt("60", "muse app test", &dir.path().join("prompt.md")).unwrap()
    }

    /// Runner over the fixture repo.
    fn runner_of(dir: &tempfile::TempDir, allow_paid: bool) -> Runner {
        Runner {
            repo: dir.path().to_path_buf(),
            allow_paid,
        }
    }

    /// Test helper: claim the release token for an approved run.
    fn claim(log: &DecisionLog, id: &str) -> ReleaseToken {
        let a = own(log, id).unwrap();
        approvals::claim_release_token(log, id, &a.request.payload_hash).unwrap()
    }

    impl Fx {
        /// Build the fixture.
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let scripts = dir.path().join("scripts");
            std::fs::create_dir_all(&scripts).unwrap();
            let stub = scripts.join("muse_run.sh");
            std::fs::write(
                &stub,
                "#!/bin/sh\nd=$(dirname \"$0\")/..\nprintf '%s\\n' \"$@\" >> \"$d/calls.txt\"\nexit $(cat \"$d/exit.txt\" 2>/dev/null || echo 0)\n",
            )
            .unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(dir.path().join("prompt.md"), "do the thing\n").unwrap();
            let log = DecisionLog::open(dir.path().join("decision_log.jsonl")).unwrap();
            Self { dir, log }
        }

        /// Lines the stub recorded (empty when it never ran).
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls.txt"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// Kinds of every log entry, in order.
        fn kinds(&self) -> Vec<String> {
            self.log
                .verify_or_empty()
                .unwrap()
                .into_iter()
                .map(|e| e.kind)
                .collect()
        }
    }

    #[test]
    fn gate_needs_human_for_runs_and_denies_unknown_actions() {
        let mut fx = Fx::new();
        let err = approvals::request(
            &mut fx.log,
            &app_gate(),
            AGENT,
            "muse.run.unattended",
            "x",
            serde_json::json!({}),
            NOW,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("fail closed"), "{err}");
        assert!(fx.kinds().is_empty());
        let req = propose(&mut fx.log, &spec_of(&fx.dir), NOW).unwrap();
        assert_eq!(fx.kinds(), vec![approvals::REQUESTED_KIND]);
        assert_eq!(status_of(&fx.log, &req.id, NOW).unwrap(), Status::Pending);
        assert_eq!(runs(&fx.log, NOW).unwrap().len(), 1);
    }

    #[test]
    fn prompt_classification_refuses_private_input_and_strips_urls() {
        let fx = Fx::new();
        let path = fx.dir.path().join("prompt.md");
        for text in [
            "email person@example.com",
            "password=synthetic-secret",
            "Bearer synthetic-token",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(RunSpec::from_prompt("1", "synthetic", &path).is_err());
        }
        std::fs::write(&path, "read https://example.com/ @demo").unwrap();
        assert_eq!(public_prompt(&path).unwrap(), "read");
    }

    /// An approved paid run consumes its token and executes the stub once.
    #[test]
    fn approved_paid_run_executes_stub_once_and_logs_each_step() {
        let mut fx = Fx::new();
        let spec = spec_of(&fx.dir);
        let id = propose(&mut fx.log, &spec, NOW).unwrap().id;
        approve(&mut fx.log, &id, "nathan", NOW + 1).unwrap();
        let token = claim(&fx.log, &id);
        assert_eq!(
            run(&mut fx.log, token, &runner_of(&fx.dir, true)).unwrap(),
            0
        );
        assert_eq!(
            &fx.calls()[..2],
            &["60".to_string(), "muse app test".to_string()]
        );
        assert_ne!(fx.calls()[2], spec.prompt_file.to_string_lossy());
        assert_eq!(
            fx.kinds(),
            vec![
                approvals::REQUESTED_KIND,
                approvals::APPROVED_KIND,
                approvals::RELEASED_KIND,
                FINISHED_KIND
            ]
        );
        // Released: no second token exists, so the stub cannot run again.
        let a = own(&fx.log, &id).unwrap();
        let err =
            approvals::claim_release_token(&fx.log, &id, &a.request.payload_hash).unwrap_err();
        assert!(err.to_string().contains("already released"), "{err}");
        assert_eq!(fx.calls().len(), 3, "stub ran exactly once");
    }

    /// Pending or denied runs yield no token, so the stub never runs.
    #[test]
    fn pending_or_denied_runs_never_execute() {
        let mut fx = Fx::new();
        let id = propose(&mut fx.log, &spec_of(&fx.dir), NOW).unwrap().id;
        let a = own(&fx.log, &id).unwrap();
        let err =
            approvals::claim_release_token(&fx.log, &id, &a.request.payload_hash).unwrap_err();
        assert!(err.to_string().contains("not approved"), "{err}");
        deny(&mut fx.log, &id, "nathan", "not today", NOW + 1).unwrap();
        let a = own(&fx.log, &id).unwrap();
        let err =
            approvals::claim_release_token(&fx.log, &id, &a.request.payload_hash).unwrap_err();
        assert!(err.to_string().contains("denied"), "{err}");
        assert!(fx.calls().is_empty());
        assert_eq!(
            fx.kinds(),
            vec![approvals::REQUESTED_KIND, approvals::DENIED_KIND]
        );
    }

    /// Paid-off refusal drops the token without releasing; a fresh claim runs.
    #[test]
    fn paid_switch_off_refuses_and_keeps_the_approval() {
        let mut fx = Fx::new();
        let id = propose(&mut fx.log, &spec_of(&fx.dir), NOW).unwrap().id;
        approve(&mut fx.log, &id, "nathan", NOW + 1).unwrap();
        let token = claim(&fx.log, &id);
        let err = run(&mut fx.log, token, &runner_of(&fx.dir, false)).unwrap_err();
        assert!(err.contains(PAID_ENV_VAR), "{err}");
        assert!(fx.calls().is_empty());
        assert_eq!(fx.kinds().len(), 2, "refusal appends nothing");
        let token = claim(&fx.log, &id);
        assert_eq!(
            run(&mut fx.log, token, &runner_of(&fx.dir, true)).unwrap(),
            0
        );
    }

    /// A prompt edited after approval is refused and the stub never runs.
    #[test]
    fn prompt_edited_after_approval_is_refused() {
        let mut fx = Fx::new();
        let id = propose(&mut fx.log, &spec_of(&fx.dir), NOW).unwrap().id;
        approve(&mut fx.log, &id, "nathan", NOW + 1).unwrap();
        std::fs::write(fx.dir.path().join("prompt.md"), "do something else\n").unwrap();
        let token = claim(&fx.log, &id);
        let err = run(&mut fx.log, token, &runner_of(&fx.dir, true)).unwrap_err();
        assert!(err.contains("changed since approval"), "{err}");
        assert!(fx.calls().is_empty());
        assert_eq!(fx.kinds().len(), 2);
    }

    /// A nonzero stub exit is returned and recorded in the log.
    #[test]
    fn nonzero_exit_is_returned_and_logged() {
        let mut fx = Fx::new();
        std::fs::write(fx.dir.path().join("exit.txt"), "3").unwrap();
        let id = propose(&mut fx.log, &spec_of(&fx.dir), NOW).unwrap().id;
        approve(&mut fx.log, &id, "nathan", NOW + 1).unwrap();
        let token = claim(&fx.log, &id);
        assert_eq!(
            run(&mut fx.log, token, &runner_of(&fx.dir, true)).unwrap(),
            3
        );
        let last = fx.log.verify().unwrap().pop().unwrap();
        assert_eq!(last.kind, FINISHED_KIND);
        assert_eq!(last.payload["exit_code"], 3);
    }

    /// Another app's approval cannot run here, even with a live token.
    #[test]
    fn other_apps_requests_are_refused() {
        let mut fx = Fx::new();
        let mut other = Gate::new();
        other.register("x.post", Risk::High);
        let req = approvals::request(
            &mut fx.log,
            &other,
            "bookmarks-agent",
            "x.post",
            "post",
            serde_json::json!({"text": "hi"}),
            NOW,
            None,
        )
        .unwrap();
        let err = approve(&mut fx.log, &req.id, "nathan", NOW).unwrap_err();
        assert!(err.contains("not a muse-runs request"), "{err}");
        // Approved at the core level and claimed: `run` still refuses it.
        approvals::approve(&mut fx.log, &req.id, &req.payload_hash, "nathan", NOW).unwrap();
        let token = approvals::claim_release_token(&fx.log, &req.id, &req.payload_hash).unwrap();
        let err = run(&mut fx.log, token, &runner_of(&fx.dir, true)).unwrap_err();
        assert!(err.contains("not a muse-runs request"), "{err}");
        assert!(fx.calls().is_empty());
        assert!(runs(&fx.log, NOW).unwrap().is_empty());
        assert_eq!(
            fx.kinds(),
            vec![approvals::REQUESTED_KIND, approvals::APPROVED_KIND]
        );
    }

    /// `run` takes the release token by value (compile-time).
    #[test]
    fn run_takes_release_token_by_value() {
        // Compiles only if `run` takes `ReleaseToken` by value.
        let _sig: fn(&mut DecisionLog, ReleaseToken, &Runner) -> Result<i32, String> = run;
    }

    #[test]
    fn spec_validation_fails_closed() {
        let fx = Fx::new();
        let p = fx.dir.path().join("prompt.md");
        assert!(RunSpec::from_prompt("", "s", &p).is_err());
        assert!(RunSpec::from_prompt("6 0", "s", &p).is_err());
        assert!(RunSpec::from_prompt("60", "a | b", &p).is_err());
        assert!(RunSpec::from_prompt("60", "a\nb", &p).is_err());
        assert!(RunSpec::from_prompt("60", " ", &p).is_err());
        assert!(RunSpec::from_prompt("60", "s", &fx.dir.path().join("missing.md")).is_err());
        let spec = spec_of(&fx.dir);
        let payload = spec.payload();
        assert_eq!(payload["prompt_file"], "prompt.md");
        assert!(!payload
            .to_string()
            .contains(fx.dir.path().to_str().unwrap()));
        let mut expected = spec;
        expected.prompt_file = "prompt.md".into();
        assert_eq!(RunSpec::from_payload(&payload), Some(expected));
    }

    #[test]
    fn log_path_follows_shared_convention() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            log_path(&env(&[
                ("DECISION_LOG_PATH", "/a/l.jsonl"),
                ("ANCIENTOS_HOME", "/b")
            ]))
            .unwrap(),
            PathBuf::from("/a/l.jsonl")
        );
        assert_eq!(
            log_path(&env(&[("DECISION_LOG_PATH", ""), ("ANCIENTOS_HOME", "/b")])).unwrap(),
            PathBuf::from("/b/decision_log.jsonl")
        );
        assert_eq!(
            log_path(&env(&[("HOME", "/h")])).unwrap(),
            PathBuf::from("/h/.ancientos-hackathon/decision_log.jsonl")
        );
        assert!(log_path(&env(&[])).is_err());
    }

    #[test]
    fn cli_propose_list_approve_run_end_to_end() {
        let fx = Fx::new();
        let d = fx.dir.path();
        let log = d.join("data/decision_log.jsonl");
        let base = |extra: &[&str]| -> Vec<String> {
            let mut v = vec![
                "--log".to_string(),
                log.to_string_lossy().to_string(),
                "--repo".to_string(),
                d.to_string_lossy().to_string(),
            ];
            v.extend(extra.iter().map(|s| s.to_string()));
            v
        };
        let paid_off = |_: &str| None;
        let paid_on = |k: &str| (k == PAID_ENV_VAR).then(|| "1".to_string());
        let prompt = d.join("prompt.md").to_string_lossy().to_string();
        assert_eq!(
            cli(&base(&["propose", "61", "cli test", &prompt]), &paid_off).unwrap(),
            0
        );
        let id = runs(&DecisionLog::open(&log).unwrap(), now_secs()).unwrap()[0]
            .request
            .id
            .clone();
        assert_eq!(cli(&base(&["list"]), &paid_off).unwrap(), 0);
        assert!(
            cli(&base(&["run", &id]), &paid_on).is_err(),
            "not approved yet"
        );
        cli(&base(&["approve", &id]), &paid_off).unwrap();
        assert!(
            cli(&base(&["run", &id]), &paid_off).is_err(),
            "paid switch off"
        );
        assert!(fx.calls().is_empty());
        assert_eq!(cli(&base(&["run", &id]), &paid_on).unwrap(), 0);
        assert_eq!(fx.calls()[0], "61");
        assert_eq!(cli(&base(&["bogus"]), &paid_off).unwrap_err(), USAGE);
        assert_eq!(cli(&["--log".to_string()], &paid_off).unwrap_err(), USAGE);
        assert_eq!(
            cli(&["--nope".to_string(), "x".to_string()], &paid_off).unwrap_err(),
            USAGE
        );
    }
}
