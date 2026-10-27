//! Daemon CLI (`run`, `deeper --later`, `deeper-queue`) on temp data dirs.
//! Fixed `--now` clocks, fixture input only; no network, no paid calls
//! except where a test explicitly enables the switch to check proposals
//! (the daemon then proposes instead of calling).
use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bookmarks-agent"))
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/demo")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

struct Setup {
    _dir: tempfile::TempDir,
    home: PathBuf,
    log: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let log = dir.path().join("decision_log.jsonl");
        Self {
            _dir: dir,
            home,
            log,
        }
    }

    fn run(&self, args: &[&str], paid: bool) -> Output {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env(
                bookmarks_agent::brief::DECISION_LOG_ENV_VAR,
                self.log.as_os_str(),
            )
            .env("ANCIENTOS_HOME", self.home.as_os_str())
            .env_remove("DAEMON_MAX_TICKS")
            .env_remove("DAEMON_TICK_SLEEP_SECS")
            .env_remove("DAEMON_BRIEF_SECS")
            .env_remove("DAEMON_DIGEST_SECS")
            .env_remove("DAEMON_TRY_FOLLOWUP_SECS")
            .env_remove("DAEMON_DEEPER_QUEUE_SECS")
            .env_remove("DAEMON_SWEEP_SECS")
            .env_remove("DAEMON_WIKI_LINT_SECS");
        if paid {
            cmd.env("ANCIENTOS_ALLOW_PAID", "1");
        } else {
            cmd.env_remove("ANCIENTOS_ALLOW_PAID");
        }
        cmd.output().expect("run bookmarks-agent")
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn log_kinds(&self) -> Vec<String> {
        self.log_text()
            .lines()
            .filter_map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()?
                    .get("kind")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

#[test]
fn once_with_now_is_deterministic() {
    let fix = fixture("synthetic_bookmarks.jsonl");
    let now = "2026-10-27T00:00:00Z";
    let a = Setup::new();
    let b = Setup::new();
    let oa = a.run(&["run", "--once", "--now", now, "--offline", &fix], false);
    let ob = b.run(&["run", "--once", "--now", now, "--offline", &fix], false);
    assert!(oa.status.success(), "stderr: {}", stderr(&oa));
    assert!(ob.status.success(), "stderr: {}", stderr(&ob));
    assert_eq!(
        a.log_text(),
        b.log_text(),
        "same --now must log identically"
    );
    assert_eq!(
        stdout(&oa),
        stdout(&ob),
        "same --now must print identically"
    );
    assert_eq!(
        a.log_kinds()
            .iter()
            .filter(|k| k.as_str() == "daemon.tick")
            .count(),
        6
    );
}

#[test]
fn once_fails_closed_when_brief_has_no_bookmarks() {
    let s = Setup::new();
    // No --offline fixture and an empty home: the brief agent errors, the
    // other five still run, and the exit is non-zero.
    let out = s.run(&["run", "--once", "--now", "2026-10-27T00:00:00Z"], false);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("brief"), "{}", stderr(&out));
    let ticks = s
        .log_kinds()
        .into_iter()
        .filter(|k| k == "daemon.tick")
        .count();
    assert_eq!(ticks, 6, "every agent still ticks and logs");
    assert!(
        s.log_text().contains("\"agent\":\"brief\"") && s.log_text().contains("\"ok\":false"),
        "{}",
        s.log_text()
    );
}

#[test]
fn deeper_later_queue_and_run_flow() {
    let s = Setup::new();
    let fix = fixture("deeper_thread_bookmarks.jsonl");
    let out = s.run(
        &[
            "deeper",
            "--later",
            "demo-3001",
            "--at",
            "2030-01-01T00:00:00Z",
        ],
        false,
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("queued demo-3001"),
        "{}",
        stdout(&out)
    );
    let out = s.run(&["deeper-queue"], false);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("demo-3001"), "{}", stdout(&out));
    assert!(stdout(&out).contains("status=queued"), "{}", stdout(&out));
    // Before its time: not run.
    let out = s.run(
        &[
            "run",
            "--once",
            "--now",
            "2027-01-01T00:00:00Z",
            "--offline",
            &fix,
        ],
        false,
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(!s.log_text().contains("deeper.done"), "{}", s.log_text());
    assert!(!s.home.join("deeper").join("demo-3001.md").exists());
    // After its time: the free local fallback runs directly.
    let out = s.run(
        &[
            "run",
            "--once",
            "--now",
            "2030-06-01T00:00:00Z",
            "--offline",
            &fix,
        ],
        false,
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(s.log_text().contains("deeper.done"), "{}", s.log_text());
    assert!(s.home.join("deeper").join("demo-3001.md").is_file());
    let out = s.run(&["deeper-queue"], false);
    assert!(stdout(&out).contains("status=done"), "{}", stdout(&out));
}

#[test]
fn deeper_later_refuses_past_unparseable_and_bad_id() {
    let s = Setup::new();
    for args in [
        vec![
            "deeper",
            "--later",
            "demo-3001",
            "--at",
            "2020-01-01T00:00:00Z",
        ],
        vec!["deeper", "--later", "demo-3001", "--at", "someday"],
        vec!["deeper", "--later", "../x", "--at", "2030-01-01T00:00:00Z"],
        vec!["deeper", "--later", "demo-3001"],
        vec!["deeper", "demo-3001", "--at", "2030-01-01T00:00:00Z"],
    ] {
        let out = s.run(&args, false);
        assert!(!out.status.success(), "{args:?} must fail");
    }
    assert!(
        !s.log_text().contains("deeper.queued"),
        "refusals append nothing: {}",
        s.log_text()
    );
    let out = s.run(&["deeper-queue"], false);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("no queued go-deeper jobs"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn paid_on_proposes_instead_of_calling() {
    let s = Setup::new();
    let fix = fixture("synthetic_bookmarks.jsonl");
    // No META_API_KEY is set anywhere in this test: a paid call would fail.
    // The daemon proposes instead, and the brief stays local.
    let out = s.run(
        &[
            "run",
            "--once",
            "--now",
            "2026-10-27T00:00:00Z",
            "--offline",
            &fix,
        ],
        true,
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = s.log_text();
    assert!(text.contains("\"action\":\"brief.paid\""), "{text}");
    assert!(text.contains("\"fallback\":true"), "{text}");
    assert!(text.contains("\"model\":\"local\""), "{text}");
    // A queued deeper job under paid mode is proposed, never executed.
    let out = s.run(
        &[
            "deeper",
            "--later",
            "demo-3001",
            "--at",
            "2030-01-01T00:00:00Z",
        ],
        true,
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let out = s.run(
        &[
            "run",
            "--once",
            "--now",
            "2030-06-01T00:00:00Z",
            "--offline",
            &fix,
        ],
        true,
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = s.log_text();
    assert!(text.contains("\"action\":\"deeper.paid\""), "{text}");
    assert!(text.contains("deeper.proposed"), "{text}");
    assert!(
        !text.contains("deeper.done"),
        "paid job must not run: {text}"
    );
}

#[test]
fn run_rejects_bad_args() {
    let s = Setup::new();
    for args in [
        vec!["run", "--once", "--once"],
        vec!["run", "--now", "yesterday"],
        vec!["run", "--now"],
        vec!["run", "--offline"],
        vec!["run", "--offline", "does-not-exist.jsonl"],
        vec!["run", "--bogus"],
        vec!["deeper-queue", "extra"],
    ] {
        let out = s.run(&args, false);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(
            stderr(&out).contains("usage:")
                || stderr(&out).contains("invalid --now")
                || stderr(&out).contains("offline fixture not found"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}
