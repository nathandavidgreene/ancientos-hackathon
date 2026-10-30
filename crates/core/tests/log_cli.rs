//! `ancientos-log` operator CLI: verify, show, replay-approvals.
//! Seeded through the real log APIs on temp files; synthetic data only,
//! no network, no paid calls.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Path of the built `ancientos-log` binary.
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ancientos-log"))
}

/// Run the CLI with args and capture output.
fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("run ancientos-log")
}

/// Captured stdout as UTF-8.
fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

/// Captured stderr as UTF-8.
fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// Synthetic gate for seeding: `x.post` needs a human.
fn gate() -> ancientos_core::gate::Gate {
    let mut g = ancientos_core::gate::Gate::new();
    g.register("x.post", ancientos_core::gate::Risk::High);
    g
}

const NOW: u64 = 1_700_000_000; // 2023-11-14T22:13:20Z
const NOW_RFC3339: &str = "2023-11-14T22:13:20Z";

/// Valid mixed log: route, one approval request, three approve verdicts
/// (which auto-write one threshold_change). Six entries, seq 0..=5.
fn seed_mixed(path: &Path) {
    use ancientos_core::log::DecisionLog;
    let mut log = DecisionLog::open_with_key_env(path, &|key| std::env::var(key).ok()).unwrap();
    log.append(
        "route",
        serde_json::json!({"to": "local", "pick_id": "syn-pick-1", "category": "SynRust"}),
    )
    .unwrap();
    ancientos_core::approvals::request(
        &mut log,
        &gate(),
        "bookmarks-agent",
        "x.post",
        "synthetic proposal alpha",
        serde_json::json!({"draft": "alpha"}),
        NOW,
        None,
    )
    .unwrap();
    for pick in ["syn-p1", "syn-p2", "syn-p3"] {
        log.append(
            "feedback",
            serde_json::json!({"pick_id": pick, "category": "SynRust", "verdict": "approve"}),
        )
        .unwrap();
    }
    log.append(
        "threshold_change",
        serde_json::json!({"added": ["SynRust"]}),
    )
    .unwrap();
    assert_eq!(log.verify().unwrap().len(), 6);
}

/// Approval lifecycle log: one released, one denied, one pending.
fn seed_approvals(path: &Path) -> (String, String, String) {
    use ancientos_core::log::DecisionLog;
    let mut log = DecisionLog::open_with_key_env(path, &|key| std::env::var(key).ok()).unwrap();
    let a = ancientos_core::approvals::request(
        &mut log,
        &gate(),
        "bookmarks-agent",
        "x.post",
        "synthetic proposal one",
        serde_json::json!({"draft": "one"}),
        NOW,
        None,
    )
    .unwrap();
    let b = ancientos_core::approvals::request(
        &mut log,
        &gate(),
        "bookmarks-agent",
        "x.post",
        "synthetic proposal two",
        serde_json::json!({"draft": "two"}),
        NOW + 10,
        None,
    )
    .unwrap();
    let c = ancientos_core::approvals::request(
        &mut log,
        &gate(),
        "bookmarks-agent",
        "x.post",
        "synthetic proposal three",
        serde_json::json!({"draft": "three"}),
        NOW + 20,
        None,
    )
    .unwrap();
    let token =
        ancientos_core::approvals::approve(&mut log, &a.id, &a.payload_hash, "human", NOW + 30)
            .unwrap();
    ancientos_core::approvals::release(&mut log, token, &serde_json::json!({"draft": "one"}))
        .unwrap();
    ancientos_core::approvals::deny(
        &mut log,
        &b.id,
        &b.payload_hash,
        "human",
        "not today",
        NOW + 40,
    )
    .unwrap();
    let _ = c;
    assert_eq!(log.verify().unwrap().len(), 6);
    (a.id, b.id, c.id)
}

/// Bytes and mtime of a file, to prove it was not modified.
fn snapshot(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
    let bytes = std::fs::read(path).unwrap();
    let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
    (bytes, mtime)
}

/// Assert a file still matches an earlier snapshot.
fn assert_untouched(path: &Path, before: &(Vec<u8>, std::time::SystemTime)) {
    assert_eq!(&std::fs::read(path).unwrap(), &before.0, "bytes changed");
    assert_eq!(
        &std::fs::metadata(path).unwrap().modified().unwrap(),
        &before.1,
        "mtime changed"
    );
}

#[test]
fn verify_accepts_valid_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    seed_mixed(&path);
    let out = run(&["verify", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "hash chain verified: 6 entries\n");
}

#[test]
fn verify_empty_file_is_zero_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.jsonl");
    std::fs::write(&path, "").unwrap();
    let out = run(&["verify", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "hash chain verified: 0 entries\n");
}

#[test]
fn verify_rejects_tampered_log_with_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    seed_mixed(&path);
    // Same-length edit keeps the JSON valid, so the hash chain must catch it.
    let raw = std::fs::read_to_string(&path)
        .unwrap()
        .replacen("local", "LOCAL", 1);
    std::fs::write(&path, raw).unwrap();
    let out = run(&["verify", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("line 1"), "stderr: {}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[test]
fn verify_rejects_truncated_last_line_with_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    seed_mixed(&path);
    let raw = std::fs::read_to_string(&path).unwrap();
    let body = raw.trim_end_matches('\n');
    let last_start = body.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let cut = last_start + (body.len() - last_start) / 2;
    std::fs::write(&path, &raw[..cut]).unwrap();
    let out = run(&["verify", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("line 6"), "stderr: {}", stderr(&out));
}

#[test]
fn broken_log_fails_all_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    seed_mixed(&path);
    let raw = std::fs::read_to_string(&path)
        .unwrap()
        .replacen("local", "LOCAL", 1);
    std::fs::write(&path, raw).unwrap();
    let p = path.to_str().unwrap();
    for args in [vec!["show", p], vec!["replay-approvals", p]] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&out).contains("line 1"),
            "{args:?}: {}",
            stderr(&out)
        );
        assert!(stdout(&out).is_empty(), "{args:?}");
    }
}

#[test]
fn missing_file_is_an_error_for_all_commands() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.jsonl");
    let p = p.to_str().unwrap().to_string();
    for args in [
        vec!["verify", p.as_str()],
        vec!["show", p.as_str()],
        vec!["replay-approvals", p.as_str()],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(!stderr(&out).is_empty(), "{args:?}");
    }
    assert!(!dir.path().join("nope.jsonl").exists(), "must not create");
}

#[test]
fn show_prints_one_line_per_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    seed_mixed(&path);
    let out = run(&["show", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 6);
    assert!(lines[0].starts_with("#0 route"), "{}", lines[0]);
    assert!(lines[0].contains("\"to\":\"local\""), "{}", lines[0]);
    assert!(lines[1].contains("approval.requested"), "{}", lines[1]);
    assert!(lines[5].contains("threshold_change"), "{}", lines[5]);
}

#[test]
fn show_filters_by_kind_since_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    seed_mixed(&path);
    let p = path.to_str().unwrap();
    // --kind keeps only matching entries.
    let out = run(&["show", p, "--kind", "feedback"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).lines().count(), 3);
    // Unknown kind matches nothing but still exits 0.
    let out = run(&["show", p, "--kind", "nope"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).is_empty());
    // --since keeps timestamped entries at/after the bound; entries without
    // a timestamp (route, feedback, threshold_change) are skipped.
    let out = run(&["show", p, "--since", NOW_RFC3339]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("approval.requested"), "{}", lines[0]);
    // One second later drops the request too.
    let out = run(&["show", p, "--since", "2023-11-14T22:13:21Z"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).is_empty());
    // --limit caps, alone and combined; = form works too.
    let out = run(&["show", p, "--limit", "2"]);
    assert_eq!(stdout(&out).lines().count(), 2);
    let out = run(&["show", p, "--kind", "feedback", "--limit=2"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).lines().count(), 2);
}

#[test]
fn show_rejects_bad_flags() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    seed_mixed(&path);
    let p = path.to_str().unwrap().to_string();
    for args in [
        vec!["show", p.as_str(), "--since", "yesterday"],
        vec!["show", p.as_str(), "--since", "2023-11-14T22:13:20"], // naive: no offset
        vec!["show", p.as_str(), "--since", "2023-13-14T22:13:20Z"],
        vec!["show", p.as_str(), "--limit", "0"],
        vec!["show", p.as_str(), "--limit", "many"],
        vec!["show", p.as_str(), "--bogus", "x"],
        vec!["show", p.as_str(), "--kind"],
        vec!["show", p.as_str(), "--kind", "a", "--kind", "b"],
        vec!["show"],
        vec!["bogus", p.as_str()],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(stdout(&out).is_empty(), "{args:?}");
        assert!(!stderr(&out).is_empty(), "{args:?}");
    }
}

#[test]
fn replay_approvals_prints_each_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    let (a, b, c) = seed_approvals(&path);
    let out = run(&["replay-approvals", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].starts_with(&a), "{}", lines[0]);
    assert!(lines[0].contains("status=released"), "{}", lines[0]);
    assert!(lines[0].contains("decided_by=human"), "{}", lines[0]);
    assert!(lines[1].starts_with(&b), "{}", lines[1]);
    assert!(lines[1].contains("status=denied"), "{}", lines[1]);
    assert!(lines[1].contains("reason=not today"), "{}", lines[1]);
    assert!(lines[2].starts_with(&c), "{}", lines[2]);
    assert!(lines[2].contains("status=pending"), "{}", lines[2]);
    assert!(
        lines[2].contains("synthetic proposal three"),
        "{}",
        lines[2]
    );
}

#[test]
fn replay_approvals_empty_log_prints_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.jsonl");
    std::fs::write(&path, "").unwrap();
    let out = run(&["replay-approvals", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).is_empty());
}

#[test]
fn commands_never_modify_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let valid = dir.path().join("log.jsonl");
    seed_mixed(&valid);
    let p = valid.to_str().unwrap().to_string();
    let before = snapshot(&valid);
    for args in [
        vec!["verify", p.as_str()],
        vec!["show", p.as_str()],
        vec!["replay-approvals", p.as_str()],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        assert_untouched(&valid, &before);
    }
    // Failing runs must not touch the file either.
    let tampered = dir.path().join("tampered.jsonl");
    let raw = std::fs::read_to_string(&valid)
        .unwrap()
        .replacen("local", "LOCAL", 1);
    std::fs::write(&tampered, raw).unwrap();
    let t = tampered.to_str().unwrap().to_string();
    let before = snapshot(&tampered);
    for args in [
        vec!["verify", t.as_str()],
        vec!["show", t.as_str()],
        vec!["replay-approvals", t.as_str()],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_untouched(&tampered, &before);
    }
}
