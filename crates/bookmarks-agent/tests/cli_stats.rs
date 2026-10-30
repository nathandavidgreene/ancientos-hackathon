//! `bookmarks-agent stats` prints the same numbers as the web UI's
//! `/stats` dashboard as plain text. Seeded through the real log APIs on a
//! temp data dir; no network, no paid calls.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Test helper: path of the compiled agent binary under test.
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bookmarks-agent"))
}

/// Current Unix time in seconds (0 if the clock is before the epoch).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run the bookmarks-agent binary with the given decision log and Muse log paths.
fn run(args: &[&str], log_path: &Path, muse_path: &Path) -> Output {
    Command::new(bin())
        .args(args)
        .env(
            bookmarks_agent::brief::DECISION_LOG_ENV_VAR,
            log_path.as_os_str(),
        )
        .env(
            bookmarks_agent::stats::MUSE_LOG_ENV_VAR,
            muse_path.as_os_str(),
        )
        .output()
        .expect("run bookmarks-agent")
}

/// Test helper: captured standard output decoded as UTF-8.
fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

/// Test helper: captured standard error decoded as UTF-8.
fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// Test helper: temp dir with decision-log and muse-log paths.
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("decision_log.jsonl");
    let muse = dir.path().join("muse-code-log.md");
    (dir, log, muse)
}

/// Seed a small synthetic log: one pending approval, two verdicts, one
/// shown try-it post with a yes follow-up, one cascade decision and one
/// daemon tick. Returns the seed `now`.
fn seed(log_path: &Path) -> u64 {
    use ancientos_core::log::DecisionLog;
    use bookmarks_agent::feedback::{FeedbackRecord, ThresholdPolicy, Verdict};
    let now = now_secs();
    let mut log = DecisionLog::open_with_key_env(log_path, &|key| std::env::var(key).ok()).unwrap();
    ancientos_core::approvals::request(
        &mut log,
        &bookmarks_agent::app_gate(),
        "bookmarks-agent",
        "x.post",
        "synthetic stats cli proposal",
        serde_json::json!({"draft": "synthetic cli draft"}),
        now,
        None,
    )
    .unwrap();
    for (id, verdict) in [("c1", Verdict::Approve), ("c2", Verdict::Skip)] {
        bookmarks_agent::feedback::record(
            &mut log,
            FeedbackRecord {
                pick_id: id.to_string(),
                category: "Synthetic Cli".to_string(),
                verdict,
                tags: Vec::new(),
            },
            ThresholdPolicy::default(),
        )
        .unwrap();
    }
    bookmarks_agent::try_it::record_shown(&mut log, "p1", now).unwrap();
    bookmarks_agent::try_it::record_followup(
        &mut log,
        "p1",
        bookmarks_agent::try_it::Tried::Yes,
        None,
    )
    .unwrap();
    log.append(
        "try_it.cascade",
        serde_json::json!({"stage": "rules", "post_id": "p1"}),
    )
    .unwrap();
    log.append(
        bookmarks_agent::daemon::DAEMON_TICK_KIND,
        serde_json::json!({"agent": "brief", "ok": true, "at": now}),
    )
    .unwrap();
    now
}

/// Write a synthetic two-row Muse Code run log.
fn write_muse_log(path: &Path) {
    std::fs::write(
        path,
        "# Synthetic run log\n\
         | 1 | 2026-10-26 | synthetic task one | Done; est. cost $1.00. |\n\
         | 2 | 2026-10-26 | synthetic task two | Done; est. cost $0.50. |\n",
    )
    .unwrap();
}

#[test]
fn stats_matches_collected_numbers() {
    let (_dir, log, muse) = setup();
    seed(&log);
    write_muse_log(&muse);
    let out = run(&["stats"], &log, &muse);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    // Same numbers `stats::collect` computes (the web UI renders these).
    let expected = bookmarks_agent::stats::render_text(&bookmarks_agent::stats::collect(
        &log,
        &muse,
        now_secs(),
    ));
    assert_eq!(text, expected, "CLI output must match render_text");
    for want in [
        "decision log: verified",
        "entries: 7",
        "approvals: pending=1 approved=0 denied=0 expired=0 released=0",
        "approve: 1",
        "Synthetic Cli: 2",
        "auto-approved categories: none",
        "try-it: shown=1 yes=1 no=0 later=0 pending=0",
        "cascade: topic_filter=0 rules=1 jev=0 muse=0 fail_closed=0",
        "brief: ok at=",
        "deeper queue: outstanding=0 done=0",
        "built with meta: runs=2 est_cost=$1.50",
    ] {
        assert!(text.contains(want), "missing {want}: {text}");
    }
}

#[test]
fn stats_empty_dir_renders_zeros() {
    let (_dir, log, muse) = setup();
    let out = run(&["stats"], &log, &muse);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    for want in [
        "decision log: verified",
        "entries: 0",
        "cascade: no cascade decisions logged",
        "built with meta: unavailable",
    ] {
        assert!(text.contains(want), "missing {want}: {text}");
    }
}

#[test]
fn stats_broken_chain_withholds_numbers() {
    let (_dir, log, muse) = setup();
    seed(&log);
    write_muse_log(&muse);
    let body = std::fs::read_to_string(&log).unwrap();
    std::fs::write(&log, body.replacen("synthetic cli draft", "tampered!", 1)).unwrap();
    let out = run(&["stats"], &log, &muse);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("broken at line 0"), "{text}");
    assert!(text.contains("approvals: withheld"), "{text}");
    assert!(!text.contains("verified"), "{text}");
    assert!(text.contains("built with meta: runs=2"), "{text}");
}

#[test]
fn stats_rejects_args() {
    let (_dir, log, muse) = setup();
    let out = run(&["stats", "extra"], &log, &muse);
    assert!(!out.status.success(), "must fail: {}", stdout(&out));
    assert!(stderr(&out).contains("usage:"), "{}", stderr(&out));
}
