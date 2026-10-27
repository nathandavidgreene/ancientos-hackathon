//! Wiki CLI (`ingest`, `show`, `query`, `lint`, triage-adopt hook) on temp
//! data dirs. Fixture input only; paid calls off, no network.
use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bookmarks-agent"))
}

struct Setup {
    _dir: tempfile::TempDir,
    home: PathBuf,
    log: PathBuf,
    fixture: PathBuf,
    backlog: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let log = dir.path().join("decision_log.jsonl");
        let backlog = dir.path().join("backlog.md");
        let fixture = dir.path().join("fix.jsonl");
        std::fs::write(
            &fixture,
            "{\"post_id\":\"p1\",\"url\":\"https://x.com/i/status/p1\",\"text\":\"[synthetic] The weekly scheduler sends reminders every morning.\",\"author_username\":\"syn_p1\",\"label\":{\"parent_label\":\"Local tooling\"}}\n\
             {\"post_id\":\"p2\",\"url\":\"https://x.com/i/status/p2\",\"text\":\"[synthetic] The approval gate drags cards into review.\",\"author_username\":\"syn_p2\",\"label\":{\"parent_label\":\"Local tooling\"}}\n",
        )
        .unwrap();
        Self {
            _dir: dir,
            home,
            log,
            fixture,
            backlog,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .env(
                bookmarks_agent::brief::DECISION_LOG_ENV_VAR,
                self.log.as_os_str(),
            )
            .env("ANCIENTOS_HOME", self.home.as_os_str())
            .env("ANCIENTOS_FIXTURE", self.fixture.as_os_str())
            .env("IDEA_BACKLOG_PATH", self.backlog.as_os_str())
            .env_remove("ANCIENTOS_ALLOW_PAID")
            .env_remove("META_API_KEY")
            .env_remove("ANCIENTOS_LOCAL_LLM_URL")
            .env_remove("ANCIENTOS_LOCAL_LLM_MODEL")
            .output()
            .expect("run bookmarks-agent")
    }

    fn stdout(out: &Output) -> String {
        String::from_utf8(out.stdout.clone()).expect("stdout utf8")
    }

    fn stderr(out: &Output) -> String {
        String::from_utf8(out.stderr.clone()).expect("stderr utf8")
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn ingest_show_query_lint_round_trip() {
    let s = Setup::new();
    // Ingest one post (paid off: deterministic fallback).
    let out = s.run(&["wiki", "ingest", "p1"]);
    assert!(out.status.success(), "stderr: {}", Setup::stderr(&out));
    let stdout = Setup::stdout(&out);
    assert!(stdout.contains("post-p1"), "{stdout}");
    assert!(stdout.contains("fallback"), "{stdout}");
    assert!(s.home.join("wiki/raw/p1.md").is_file());
    assert!(s.home.join("wiki/pages/post-p1.md").is_file());
    assert!(s.home.join("wiki/index.md").is_file());
    let log_md = std::fs::read_to_string(s.home.join("wiki/log.md")).unwrap();
    assert!(log_md.contains("] ingest | "), "{log_md}");
    // Show prints the page; unknown slugs fail closed.
    let out = s.run(&["wiki", "show", "post-p1"]);
    assert!(out.status.success(), "stderr: {}", Setup::stderr(&out));
    assert!(
        Setup::stdout(&out).contains("scheduler sends reminders"),
        "{}",
        Setup::stdout(&out)
    );
    assert!(!s.run(&["wiki", "show", "ghost"]).status.success());
    assert!(!s.run(&["wiki", "show", "../evil"]).status.success());
    // Query answers with citations; file-back files a logged page.
    let out = s.run(&["wiki", "query", "how do scheduler reminders work"]);
    assert!(out.status.success(), "stderr: {}", Setup::stderr(&out));
    let stdout = Setup::stdout(&out);
    assert!(stdout.contains("[[post-p1]]"), "{stdout}");
    assert!(stdout.contains("p1"), "{stdout}");
    let out = s.run(&["wiki", "query", "scheduler reminders", "--file-back"]);
    assert!(out.status.success(), "stderr: {}", Setup::stderr(&out));
    assert!(
        Setup::stdout(&out).contains("filed as"),
        "{}",
        Setup::stdout(&out)
    );
    assert!(s.log_text().contains("wiki.fileback"), "{}", s.log_text());
    // Lint proposes approvals and never edits pages.
    let before = std::fs::read(s.home.join("wiki/pages/post-p1.md")).unwrap();
    let out = s.run(&["wiki", "lint"]);
    assert!(out.status.success(), "stderr: {}", Setup::stderr(&out));
    let stdout = Setup::stdout(&out);
    assert!(stdout.contains("wiki lint:"), "{stdout}");
    assert!(stdout.contains("wiki.edit"), "{stdout}");
    assert_eq!(
        std::fs::read(s.home.join("wiki/pages/post-p1.md")).unwrap(),
        before,
        "lint must not edit pages"
    );
    assert!(s.log_text().contains("wiki.lint"), "{}", s.log_text());
}

#[test]
fn triage_adopt_queues_wiki_ingest_and_delegated_ingests_it() {
    let s = Setup::new();
    std::fs::write(
        s.home.join("idea_cards.jsonl"),
        "{\"id\":\"0123456789abcdef\",\"source_url\":\"https://x.com/i/status/p2\",\"idea\":\"Drag a card to approve\",\"ancientos_mapping\":[\"gate\"],\"effort\":\"S\",\"status\":\"new\",\"created_at\":100}\n",
    )
    .unwrap();
    let out = s.run(&["triage", "0123456789abcdef", "adopt"]);
    assert!(out.status.success(), "stderr: {}", Setup::stderr(&out));
    assert_eq!(Setup::stdout(&out).trim(), "adopted");
    assert!(s.log_text().contains("wiki.queued"), "{}", s.log_text());
    assert!(s.log_text().contains("idea_adopt"), "{}", s.log_text());
    // --delegated picks up the adopted card's source post.
    let out = s.run(&["wiki", "ingest", "--delegated"]);
    assert!(out.status.success(), "stderr: {}", Setup::stderr(&out));
    assert!(s.home.join("wiki/raw/p2.md").is_file());
    assert!(s.home.join("wiki/pages/idea-0123456789abcdef.md").is_file());
    assert!(s.home.join("wiki/pages/gate.md").is_file());
}

#[test]
fn ingest_rejects_unknown_posts_and_bad_args() {
    let s = Setup::new();
    assert!(!s.run(&["wiki", "ingest", "ghost"]).status.success());
    assert!(!s.run(&["wiki", "ingest"]).status.success());
    assert!(!s.run(&["wiki", "query"]).status.success());
    assert!(!s
        .run(&["wiki", "query", "q", "--file-back", "extra"])
        .status
        .success());
    assert!(!s.run(&["wiki", "frobnicate"]).status.success());
    assert!(!s.run(&["wiki"]).status.success());
    // Nothing delegated yet: --delegated says so instead of guessing.
    let out = s.run(&["wiki", "ingest", "--delegated"]);
    assert!(!out.status.success());
    assert!(
        Setup::stderr(&out).contains("nothing delegated yet"),
        "{}",
        Setup::stderr(&out)
    );
}
