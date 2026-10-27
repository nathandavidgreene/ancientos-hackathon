//! `rank` CLI: ranked candidate list with reasons on a temp data dir.
//! Votes are seeded through the `vote` CLI; paid calls off, no network.
use std::path::PathBuf;
use std::process::{Command, Output};

/// Test helper: path of the compiled agent binary under test.
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
    /// Test helper: isolated temp home with keep/drop ranking fixture.
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

    /// Test helper: invoke the CLI with isolated env and capture output.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    /// Test helper: like [`Setup::run`] plus extra env vars set after the isolation.
    fn run_with(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env(
                bookmarks_agent::brief::DECISION_LOG_ENV_VAR,
                self.log.as_os_str(),
            )
            .env("ANCIENTOS_HOME", self.home.as_os_str())
            .env("ANCIENTOS_FIXTURE", self.fixture.as_os_str())
            .env_remove("ANCIENTOS_ALLOW_PAID")
            .env_remove(bookmarks_agent::ranker::MIN_RECORDS_ENV_VAR)
            .env_remove(bookmarks_agent::semantic::MODEL_ENV);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().expect("run bookmarks-agent")
    }

    /// Test helper: captured standard output decoded as UTF-8.
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

/// Test helper: loopback fake of the local embedding server for one
/// request. "keepable" texts point one way, everything else the other.
/// Returns the base URL.
fn fake_embed_server() -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).unwrap();
        let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let rows: Vec<Vec<f32>> = req["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let keep = t.as_str().unwrap().contains("keepable");
                vec![f32::from(u8::from(keep)), f32::from(u8::from(!keep)), 0.1]
            })
            .collect();
        let reply = serde_json::json!({ "embeddings": rows }).to_string();
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
    });
    format!("http://{addr}")
}

#[test]
fn rank_blends_local_embeddings_and_fails_closed_without_the_model() {
    let s = Setup::new();
    for i in 0..5 {
        s.vote(&format!("keep-{i}"), "approve");
        s.vote(&format!("drop-{i}"), "skip");
    }
    let votes_only = Setup::stdout(&s.run(&["rank"]));
    // Model configured but unreachable: same order as votes only, no claim.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    let model = bookmarks_agent::semantic::MODEL_ENV;
    let out = s.run_with(
        &["rank"],
        &[
            ("ANCIENTOS_LOCAL_LLM_URL", &dead_url),
            (model, "fake-embed"),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(Setup::stdout(&out), votes_only);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("fail closed"), "{stderr}");
    assert!(!s.log.with_file_name("embeddings.json").exists());
    // Model up: blended scores, a note, and a vector cache next to the log.
    let url = fake_embed_server();
    let out = s.run_with(
        &["rank"],
        &[("ANCIENTOS_LOCAL_LLM_URL", &url), (model, "fake-embed")],
    );
    assert!(out.status.success(), "{out:?}");
    let stdout = Setup::stdout(&out);
    assert!(stdout.contains("semantic ranking: on"), "{stdout}");
    assert!(
        stdout.lines().next().unwrap_or("").contains("keep-"),
        "{stdout}"
    );
    let cache = std::fs::read_to_string(s.log.with_file_name("embeddings.json")).unwrap();
    assert!(!cache.contains("keepable"), "cache holds no post text");
    // Cached: works again with the server gone.
    let out = s.run_with(
        &["rank"],
        &[
            ("ANCIENTOS_LOCAL_LLM_URL", &dead_url),
            (model, "fake-embed"),
        ],
    );
    assert!(
        Setup::stdout(&out).contains("semantic ranking: on"),
        "{out:?}"
    );
}
