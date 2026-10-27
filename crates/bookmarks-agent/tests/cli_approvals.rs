//! CLI approval queue (`approvals` / `approve` / `deny`) on a temp data dir.
//! Requests are seeded through the core API (agents propose via API); the
//! binary under test only lists and decides. No network, no paid calls.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bookmarks-agent"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn run(args: &[&str], log_path: &Path) -> Output {
    Command::new(bin())
        .args(args)
        .env(
            bookmarks_agent::brief::DECISION_LOG_ENV_VAR,
            log_path.as_os_str(),
        )
        .output()
        .expect("run bookmarks-agent")
}

fn seed(summary: &str, draft: &str, log_path: &Path) -> ancientos_core::approvals::ApprovalRequest {
    seed_at(summary, draft, log_path, now_secs())
}

fn seed_at(
    summary: &str,
    draft: &str,
    log_path: &Path,
    now: u64,
) -> ancientos_core::approvals::ApprovalRequest {
    let mut log = ancientos_core::log::DecisionLog::open(log_path).unwrap();
    ancientos_core::approvals::request(
        &mut log,
        &bookmarks_agent::app_gate(),
        "bookmarks-agent",
        "x.post",
        summary,
        serde_json::json!({"draft": draft}),
        now,
        None,
    )
    .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("decision_log.jsonl");
    (dir, log)
}

#[test]
fn list_shows_pending_fields_and_empty_state() {
    let (_dir, log) = setup();
    let out = run(&["approvals"], &log);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("no pending approvals"),
        "{}",
        stdout(&out)
    );
    let r = seed("synthetic cli proposal", "synthetic draft one", &log);
    let out = run(&["approvals"], &log);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    for want in [
        r.id.as_str(),
        "bookmarks-agent",
        "x.post",
        "synthetic cli proposal",
        &r.expires_at.to_string(),
    ] {
        assert!(text.contains(want), "missing {want}: {text}");
    }
}

#[test]
fn approve_records_decision_prints_release_ready_and_is_once_only() {
    let (_dir, log) = setup();
    let r = seed("synthetic cli approve", "synthetic draft two", &log);
    let out = run(&["approve", &r.id], &log);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(&r.id), "{text}");
    assert!(text.contains("release-ready"), "{text}");
    let log_read = ancientos_core::log::DecisionLog::open(&log).unwrap();
    let entries = log_read.verify().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].kind, "approval.approved");
    assert_eq!(entries[1].payload["decided_by"], "cli");
    assert_eq!(entries[1].payload["id"], r.id);
    // Decided: gone from the list, and a second approve fails.
    let out = run(&["approvals"], &log);
    assert!(
        stdout(&out).contains("no pending approvals"),
        "{}",
        stdout(&out)
    );
    let out = run(&["approve", &r.id], &log);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("already decided"), "{}", stderr(&out));
    assert_eq!(
        ancientos_core::log::DecisionLog::open(&log)
            .unwrap()
            .verify()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn deny_records_reason_and_allows_empty_reason() {
    let (_dir, log) = setup();
    let a = seed("synthetic cli deny a", "synthetic draft three", &log);
    let b = seed("synthetic cli deny b", "synthetic draft four", &log);
    let out = run(&["deny", &a.id, "not", "convincing"], &log);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("denied {}", a.id)),
        "{}",
        stdout(&out)
    );
    let out = run(&["deny", &b.id], &log);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let entries = ancientos_core::log::DecisionLog::open(&log)
        .unwrap()
        .verify()
        .unwrap();
    let denied: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == "approval.denied")
        .collect();
    assert_eq!(denied.len(), 2);
    assert_eq!(denied[0].payload["reason"], "not convincing");
    assert_eq!(denied[0].payload["decided_by"], "cli");
    assert_eq!(denied[1].payload["reason"], "");
}

#[test]
fn approve_unknown_id_fails_without_logging() {
    let (_dir, log) = setup();
    seed("synthetic cli unknown", "synthetic draft five", &log);
    let out = run(&["approve", "deadbeefdeadbeef"], &log);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("unknown approval id"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        ancientos_core::log::DecisionLog::open(&log)
            .unwrap()
            .verify()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn approve_rejects_bad_args() {
    let (_dir, log) = setup();
    for args in [vec!["approve"], vec!["deny"], vec!["approvals", "extra"]] {
        let out = run(&args, &log);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(
            stderr(&out).contains("usage:"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn approve_now_records_fixed_clock_and_rejects_bad_clock() {
    let (_dir, log) = setup();
    let requested = bookmarks_agent::idea_card::parse_rfc3339("2030-01-01T00:00:00Z").unwrap();
    let r = seed_at(
        "synthetic cli fixed clock",
        "synthetic draft fixed",
        &log,
        requested,
    );
    let fixed = "2030-01-01T00:00:01Z";
    let expected = bookmarks_agent::idea_card::parse_rfc3339(fixed).unwrap();
    let out = run(&["approve", &r.id, "--now", fixed], &log);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let entries = ancientos_core::log::DecisionLog::open(&log)
        .unwrap()
        .verify()
        .unwrap();
    assert_eq!(entries[1].kind, "approval.approved");
    assert_eq!(entries[1].payload["decided_at"], expected);
    // Bad clocks fail closed without appending.
    let s = seed_at(
        "synthetic cli bad clock",
        "synthetic draft bad",
        &log,
        requested + 10,
    );
    for args in [
        vec!["approve", &s.id, "--now", "someday"],
        vec!["approve", &s.id, "--now"],
        vec!["deny", &s.id, "--now", "someday"],
    ] {
        let out = run(&args, &log);
        assert!(!out.status.success(), "{args:?} must fail");
    }
    assert_eq!(
        ancientos_core::log::DecisionLog::open(&log)
            .unwrap()
            .verify()
            .unwrap()
            .len(),
        3,
        "refusals append nothing"
    );
}
