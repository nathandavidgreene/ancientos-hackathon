//! `rank` CLI: ranked candidate list with reasons on a temp data dir.
//! Votes are seeded through the `vote` CLI; paid calls off, no network.
use std::path::PathBuf;
use std::process::{Command, Output};

/// Test helper: bin.
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bookmarks-agent"))
}

struct Setup {
    _dir: tempfile::TempDir,
    home: PathBuf,
    log: PathBuf,
    fixture: PathBuf,
}

impl Setup {
    /// Test helper: new.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let log = dir.path().join("decision_log.jsonl");
        let fixture = dir.path().join("fix.jsonl");
        let mut lines = Vec::new();
        for i in 0..6 {
            lines.push(format!(
                "{{\"post_id\":\"keep-{i}\",\"url\":\"https://x.com/i/status/keep-{i}\",\
                 \"text\":\"[synthetic] keepable alpha post number {i}\",\
                 \"author_username\":\"syn_keep\",\"created_at\":\"2026-01-{:02}T00:00:00+00:00\",\
                 \"label\":{{\"parent_label\":\"Rust\",\"confidence\":0.5}}}}",
                i + 1
            ));
            lines.push(format!(
                "{{\"post_id\":\"drop-{i}\",\"url\":\"https://x.com/i/status/drop-{i}\",\
                 \"text\":\"[synthetic] droppable beta post number {i}\",\
                 \"author_username\":\"syn_drop\",\"created_at\":\"2026-02-{:02}T00:00:00+00:00\",\
                 \"label\":{{\"parent_label\":\"Rust\",\"confidence\":0.5}}}}",
                i + 1
            ));
        }
        std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
        Self {
            _dir: dir,
            home,
            log,
            fixture,
        }
    }

    /// Test helper: run.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .env(
                bookmarks_agent::brief::DECISION_LOG_ENV_VAR,
                self.log.as_os_str(),
            )
            .env("ANCIENTOS_HOME", self.home.as_os_str())
            .env("ANCIENTOS_FIXTURE", self.fixture.as_os_str())
            .env_remove("ANCIENTOS_ALLOW_PAID")
            .env_remove(bookmarks_agent::ranker::MIN_RECORDS_ENV_VAR)
            .output()
            .expect("run bookmarks-agent")
    }

    /// Test helper: stdout.
    fn stdout(out: &Output) -> String {
        String::from_utf8(out.stdout.clone()).expect("stdout utf8")
    }

    /// Record a vote through the CLI and assert it succeeded.
    fn vote(&self, post_id: &str, verdict: &str) {
        let out = self.run(&["vote", post_id, verdict]);
        assert!(out.status.success(), "vote failed: {out:?}");
    }
}

#[test]
fn rank_without_votes_prints_baseline_order_and_inactive_note() {
    let s = Setup::new();
    let out = s.run(&["rank"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = Setup::stdout(&out);
    assert!(stdout.contains("ranker inactive"), "{stdout}");
    assert!(stdout.contains("0 labeled records"), "{stdout}");
    assert!(!stdout.contains("score "), "{stdout}");
    // Baseline order is newest first: a drop-* post leads.
    let first = stdout.lines().next().unwrap_or("");
    assert!(first.contains("drop-"), "{first}");
    assert!(stdout.contains("keep-0"), "{stdout}");
}

#[test]
fn rank_after_votes_prints_scores_reasons_and_top_limit() {
    let s = Setup::new();
    for i in 0..5 {
        s.vote(&format!("keep-{i}"), "approve");
        s.vote(&format!("drop-{i}"), "skip");
    }
    let out = s.run(&["rank"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = Setup::stdout(&out);
    assert!(!stdout.contains("ranker inactive"), "{stdout}");
    assert!(stdout.contains("score "), "{stdout}");
    assert!(stdout.contains("you approved"), "{stdout}");
    assert!(stdout.contains("author you often approve"), "{stdout}");
    let first = stdout.lines().next().unwrap_or("");
    assert!(
        first.contains("keep-"),
        "votes pulled keep-* first: {first}"
    );
    // --top limits the printed entries (one header line each).
    for args in [&["rank", "--top", "2"][..], &["rank", "--top=2"][..]] {
        let out = s.run(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        let stdout = Setup::stdout(&out);
        let headers: Vec<&str> = stdout.lines().filter(|l| l.contains("score ")).collect();
        assert_eq!(headers.len(), 2, "{args:?}: {stdout}");
        assert!(headers[0].starts_with("1. "), "{stdout}");
        assert!(headers[1].starts_with("2. "), "{stdout}");
    }
}

#[test]
fn rank_hides_never_posts_and_rejects_bad_args() {
    let s = Setup::new();
    for i in 0..5 {
        s.vote(&format!("keep-{i}"), "approve");
        s.vote(&format!("drop-{i}"), "skip");
    }
    s.vote("keep-0", "never_post");
    let out = s.run(&["rank"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = Setup::stdout(&out);
    assert!(!stdout.contains("keep-0"), "{stdout}");
    assert!(stdout.contains("keep-1"), "{stdout}");
    for bad in [&["rank", "--top", "0"][..], &["rank", "extra"][..]] {
        let out = s.run(bad);
        assert!(!out.status.success(), "{bad:?} must fail");
    }
}
