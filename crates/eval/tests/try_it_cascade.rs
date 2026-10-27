//! End-to-end `try_it_cascade` runs against 127.0.0.1 mock servers and small
//! temp files. No real network, no real data: every backend URL points at a
//! mock or a closed port, and all keys are fake.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Captured request bodies, one per hit.
type SeenBodies = Arc<Mutex<Vec<String>>>;

/// Read one request: headers until the blank line, then Content-Length bytes.
fn read_request_body(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut content_length = 0usize;
    let mut line = Vec::new();
    let mut buf = [0u8; 1];
    let mut raw_len = 0usize;
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                raw_len += 1;
                line.push(buf[0]);
                if buf[0] == b'\n' {
                    let text = String::from_utf8_lossy(&line);
                    if text.to_lowercase().starts_with("content-length:") {
                        content_length = text
                            .split(':')
                            .nth(1)
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                    if line == b"\r\n" || line == b"\n" {
                        break;
                    }
                    line.clear();
                }
                if raw_len > 65536 {
                    break;
                }
            }
        }
    }
    let mut rest = vec![0u8; content_length.min(1 << 20)];
    let mut filled = 0;
    while filled < rest.len() {
        match stream.read(&mut rest[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    rest.truncate(filled);
    String::from_utf8_lossy(&rest).into_owned()
}

/// Multi-shot mock HTTP server on 127.0.0.1: serves the scripted
/// `(status, body)` replies in order, captures request bodies, then signals
/// done. The test fails unless the binary makes exactly this many calls.
fn mock_server(replies: Vec<(u16, String)>) -> (String, SeenBodies, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let url = format!("http://{}", listener.local_addr().expect("mock addr"));
    let seen: SeenBodies = Arc::new(Mutex::new(Vec::new()));
    let seen_thread = Arc::clone(&seen);
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (status, body) in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            seen_thread
                .lock()
                .expect("seen lock")
                .push(read_request_body(&mut stream));
            let reason = match status {
                200 => "OK",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Error",
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
        let _ = done_tx.send(());
    });
    (url, seen, done_rx)
}

/// A base URL with nothing listening. Calls attempted against it fail fast
/// with connection refused, so tests using it prove zero network use.
fn closed_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let url = format!("http://{}", listener.local_addr().expect("probe addr"));
    drop(listener);
    url
}

fn jev_noul_body(noul: f64) -> String {
    format!(
        "{{\"model\":\"m\",\"answers\":{{\"try_it\":{{\"type\":\"noul\",\"noul\":{noul}}}}},\
         \"usage\":{{\"input_tokens\":10,\"output_tokens\":2}}}}"
    )
}

fn muse_body(content: &str) -> String {
    serde_json::json!({
        "model": "muse-spark-1.3",
        "choices": [{"message": {"role": "assistant", "content": content},
                     "finish_reason": "stop"}],
    })
    .to_string()
}

/// Muse reply with an explicit content value (string or null) and finish
/// reason, for the truncation test.
fn muse_body_full(content: serde_json::Value, finish_reason: &str) -> String {
    serde_json::json!({
        "model": "muse-spark-1.3",
        "choices": [{"message": {"role": "assistant", "content": content},
                     "finish_reason": finish_reason}],
    })
    .to_string()
}

/// Run the binary with explicit env, killing it if it hangs. Outputs are
/// small, so reading them after exit cannot deadlock.
fn run_bin(args: &[&str], env: &[(&str, &str)], remove_env: &[&str]) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_try_it_cascade"));
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    for k in remove_env {
        cmd.env_remove(k);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bin");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let out = child.wait_with_output().expect("output");
                return (
                    status.success(),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                );
            }
            None if Instant::now() > deadline => {
                child.kill().expect("kill hung bin");
                panic!("try_it_cascade hung (possible real network attempt)");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn bookmark(id: &str, topic: &str, text: &str) -> String {
    bookmark_lang(id, topic, text, None)
}

/// Bookmark carrying stored expanded link URLs for --enrich tests.
fn bookmark_with_urls(id: &str, topic: &str, text: &str, urls: &[String]) -> String {
    serde_json::json!({
        "post_id": id,
        "url": format!("https://x.com/i/status/{id}"),
        "text": text,
        "label": {"parent_label": topic},
        "urls": urls,
    })
    .to_string()
}

/// GET mock server for link fetches: serves `(content_type, body)` by path,
/// counts hits, exits after `max_hits` or ~30s idle. The thread is
/// detached; the returned counter is the assertion point.
fn link_mock_server(routes: Vec<(&str, &str, String)>, max_hits: usize) -> (String, SeenBodies) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind link mock");
    let url = format!("http://{}", listener.local_addr().expect("link addr"));
    listener.set_nonblocking(true).expect("nonblocking");
    let seen: SeenBodies = Arc::new(Mutex::new(Vec::new()));
    let thread_seen = Arc::clone(&seen);
    let owned: Vec<(String, String, Vec<u8>)> = routes
        .into_iter()
        .map(|(p, ct, b)| (p.to_string(), ct.to_string(), b.into_bytes()))
        .collect();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if thread_seen.lock().expect("seen").len() >= max_hits || Instant::now() > deadline {
                break;
            }
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout");
            let mut head = Vec::new();
            let mut buf = [0u8; 1];
            while head.len() < 65536 {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        head.push(buf[0]);
                        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
                            break;
                        }
                    }
                }
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");
            thread_seen.lock().expect("seen").push(path.to_string());
            let found = owned.iter().find(|(p, _, _)| p == path);
            let (status, reason, content_type, body): (u16, &str, &str, &[u8]) = match found {
                Some((_, ct, body)) => (200, "OK", ct, body),
                None => (404, "Not Found", "text/plain", b"nope"),
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });
    (url, seen)
}

fn link_hits(seen: &SeenBodies) -> usize {
    seen.lock().expect("seen").len()
}

fn bookmark_lang(id: &str, topic: &str, text: &str, lang: Option<&str>) -> String {
    let mut row = serde_json::json!({
        "post_id": id,
        "url": format!("https://x.com/i/status/{id}"),
        "text": text,
        "label": {"parent_label": topic},
    });
    if let Some(lang) = lang {
        row["lang"] = serde_json::json!(lang);
    }
    row.to_string()
}

/// Three synthetic bookmarks: one rules-confident yes, two rules-unsure.
fn fixtures() -> (String, String) {
    let bookmarks = [
        bookmark(
            "c1",
            "Health & Wellness",
            "Try it: 1) roll shoulders 10x 2) neck tilt 20s each side.",
        ),
        bookmark(
            "c2",
            "AI Agents",
            "A tip. Roll your shoulders and hold for a moment.",
        ),
        bookmark("c3", "AI Agents", "An interesting read, you should try it."),
    ]
    .join("\n")
        + "\n";
    let labels = concat!(
        "{\"post_id\":\"c1\",\"try_it\":true}\n",
        "{\"post_id\":\"c2\",\"try_it\":true}\n",
        "{\"post_id\":\"c3\",\"try_it\":false}\n",
        "{\"post_id\":\"ghost\",\"try_it\":false}\n",
        "not a json line\n",
    )
    .to_string();
    (labels, bookmarks)
}

/// One rules-unsure bookmark plus its label, for threshold tests.
fn single_unsure_fixture() -> (String, String) {
    let bookmarks = bookmark(
        "u1",
        "AI Agents",
        "A tip. Roll your shoulders and hold for a moment.",
    ) + "\n";
    let labels = "{\"post_id\":\"u1\",\"try_it\":true}\n".to_string();
    (labels, bookmarks)
}

fn read_out_rows(out_path: &str) -> Vec<serde_json::Value> {
    let out = std::fs::read_to_string(out_path).expect("read out");
    out.lines()
        .map(|l| serde_json::from_str(l).expect("out is jsonl"))
        .collect()
}

fn write_fixtures(
    dir: &std::path::Path,
    labels: &str,
    bookmarks: &str,
) -> (String, String, String) {
    let labels_path = dir.join("labels.jsonl");
    let bookmarks_path = dir.join("bookmarks.jsonl");
    let out_path = dir.join("out.jsonl");
    std::fs::write(&labels_path, labels).expect("write labels");
    std::fs::write(&bookmarks_path, bookmarks).expect("write bookmarks");
    (
        labels_path.to_str().unwrap().to_string(),
        bookmarks_path.to_str().unwrap().to_string(),
        out_path.to_str().unwrap().to_string(),
    )
}

#[test]
fn full_cascade_through_all_stages() {
    let (jev_url, jev_seen, jev_done) =
        mock_server(vec![(200, jev_noul_body(0.2)), (200, jev_noul_body(0.1))]);
    let (muse_url, muse_seen, muse_done) = mock_server(vec![
        (200, muse_body("{\"try_it\":true}")),
        (200, muse_body("{\"try_it\":false}")),
    ]);
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = fixtures();
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_url),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
        ],
        &["ANCIENTOS_JEV_TRY_IT_THRESHOLD"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 2 calls");
    muse_done
        .recv_timeout(Duration::from_secs(15))
        .expect("muse got exactly 2 calls");

    assert!(
        stdout.contains("3 decided, 1 missing, 1 malformed"),
        "{stdout}"
    );
    assert!(stdout.contains("paid: on"), "{stdout}");
    assert!(
        stdout.contains("cascade try_it: P=1.000 R=1.000 F1=1.000 (tp=2 fp=0 fn=0 tn=1)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("rules-only try_it: P=1.000 R=0.500 F1=0.667 (tp=1 fp=0 fn=1 tn=1)"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "calls: jev=2 jev_yes=0 muse=2 muse_yes=1 muse_truncated=0 rules_yes=1 topic_filtered=0 fail_closed=0 errors=0"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("tokens: jev_in=20 jev_out=4"), "{stdout}");

    // Per-item stages: rules yes, then jev-no -> muse yes, jev-no -> muse no.
    let out = std::fs::read_to_string(&out_path).expect("read out");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("out is jsonl"))
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["post_id"], "c1");
    assert_eq!(rows[0]["predicted"], true);
    assert_eq!(rows[0]["stage"], "rules");
    assert_eq!(rows[0]["truncated"], false);
    assert_eq!(rows[1]["post_id"], "c2");
    assert_eq!(rows[1]["predicted"], true);
    assert_eq!(rows[1]["stage"], "muse");
    assert_eq!(rows[1]["truncated"], false);
    assert_eq!(rows[2]["post_id"], "c3");
    assert_eq!(rows[2]["predicted"], false);
    assert_eq!(rows[2]["stage"], "muse");
    assert_eq!(rows[2]["truncated"], false);

    // Wire proof: Jev got the noul question, Muse got the pinned stage-3 config.
    let jev_seen = jev_seen.lock().expect("seen lock");
    assert_eq!(jev_seen.len(), 2);
    for body in jev_seen.iter() {
        let sent: serde_json::Value = serde_json::from_str(body).expect("jev request is json");
        assert_eq!(sent["questions"]["try_it"]["type"], "noul");
    }
    let muse_seen = muse_seen.lock().expect("seen lock");
    assert_eq!(muse_seen.len(), 2);
    for body in muse_seen.iter() {
        let sent: serde_json::Value = serde_json::from_str(body).expect("muse request is json");
        assert_eq!(sent["model"], "muse-spark-1.3");
        assert_eq!(sent["max_completion_tokens"], 1024);
        assert_eq!(sent["reasoning_effort"], "low");
    }
}

#[test]
fn excluded_topic_is_filtered_before_any_call() {
    // t1 mentions OpenClaw and would be rules-confident without stage 0;
    // it must answer no with the topic_filter stage and trigger no call.
    // t2 is an ordinary unsure item flowing through jev (no) to muse (yes).
    let t1_text =
        "Try it: OpenClaw shoulder routine 1) roll shoulders 10x 2) neck tilt 20s each side.";
    assert!(
        bookmarks_agent::try_it::rules_confident(
            "Health & Wellness",
            "Try it: shoulder routine 1) roll shoulders 10x 2) neck tilt 20s each side.",
        ),
        "premise: t1's cues would be rules-confident without the topic word"
    );
    let t2_text = "A tip. Roll your shoulders and hold for a moment.";
    let (jev_url, jev_seen, jev_done) = mock_server(vec![(200, jev_noul_body(0.1))]);
    let (muse_url, muse_seen, muse_done) = mock_server(vec![(200, muse_body("{\"try_it\":true}"))]);
    let dir = tempfile::tempdir().expect("tempdir");
    let bookmarks = [
        bookmark("t1", "Health & Wellness", t1_text),
        bookmark("t2", "AI Agents", t2_text),
    ]
    .join("\n")
        + "\n";
    let labels = "{\"post_id\":\"t1\",\"try_it\":false}\n{\"post_id\":\"t2\",\"try_it\":true}\n";
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), labels, &bookmarks);

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_url),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
        ],
        &["ANCIENTOS_JEV_TRY_IT_THRESHOLD"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 1 call");
    muse_done
        .recv_timeout(Duration::from_secs(15))
        .expect("muse got exactly 1 call");

    assert!(stdout.contains("2 decided"), "{stdout}");
    assert!(
        stdout.contains(
            "calls: jev=1 jev_yes=0 muse=1 muse_yes=1 muse_truncated=0 rules_yes=0 topic_filtered=1 fail_closed=0 errors=0"
        ),
        "{stdout}"
    );
    // Cascade is perfect; rules alone also say no on t1 (the local
    // classifier filters it too) but miss t2.
    assert!(
        stdout.contains("cascade try_it: P=1.000 R=1.000 F1=1.000 (tp=1 fp=0 fn=0 tn=1)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("rules-only try_it: P=0.000 R=0.000 F1=0.000 (tp=0 fp=0 fn=1 tn=1)"),
        "{stdout}"
    );

    let out = std::fs::read_to_string(&out_path).expect("read out");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("out is jsonl"))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["post_id"], "t1");
    assert_eq!(rows[0]["predicted"], false);
    assert_eq!(rows[0]["stage"], "topic_filter");
    assert_eq!(rows[0]["truncated"], false);
    assert_eq!(rows[1]["post_id"], "t2");
    assert_eq!(rows[1]["predicted"], true);
    assert_eq!(rows[1]["stage"], "muse");

    // The single Jev/Muse calls are t2's; t1 triggered none.
    let jev_seen = jev_seen.lock().expect("seen lock");
    assert_eq!(jev_seen.len(), 1);
    assert!(
        jev_seen[0].contains("A tip. Roll your shoulders"),
        "{}",
        jev_seen[0]
    );
    let muse_seen = muse_seen.lock().expect("seen lock");
    assert_eq!(muse_seen.len(), 1);
    assert!(
        muse_seen[0].contains("A tip. Roll your shoulders"),
        "{}",
        muse_seen[0]
    );
}

#[test]
fn paid_off_runs_rules_only_with_zero_calls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = fixtures();
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    // Closed ports prove zero network use: any attempted call would fail
    // closed and show up as fail_closed/errors in the report.
    let jev_closed = closed_url();
    let muse_closed = closed_url();

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_closed),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
        ],
        &["ANCIENTOS_ALLOW_PAID"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    assert!(stdout.contains("paid: off"), "{stdout}");
    assert!(stdout.contains("rules only"), "{stdout}");
    assert!(
        stdout.contains(
            "calls: jev=0 jev_yes=0 muse=0 muse_yes=0 muse_truncated=0 rules_yes=1 topic_filtered=0 fail_closed=0 errors=0"
        ),
        "{stdout}"
    );
    // Rules alone: c1 yes, c2/c3 no — so cascade equals rules-only here.
    assert!(
        stdout.contains("cascade try_it: P=1.000 R=0.500 F1=0.667 (tp=1 fp=0 fn=1 tn=1)"),
        "{stdout}"
    );
    let out = std::fs::read_to_string(&out_path).expect("read out");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("out is jsonl"))
        .collect();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row["stage"], "rules", "{row}");
    }
    assert_eq!(rows[0]["predicted"], true);
    assert_eq!(rows[1]["predicted"], false);
    assert_eq!(rows[2]["predicted"], false);
}

#[test]
fn non_english_bookmarks_are_sampled_and_scored_on_translation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let es_text =
        "[synthetic] El gobierno presentó un plan económico para apoyar pequeñas empresas locales.";
    // Hands-on English: the gold try-it label applies to the translation.
    let es_english = "[synthetic] Try it: 1. Do A 2. Do B";
    let bookmarks = [
        bookmark_lang(
            "c-en",
            "Art",
            "[synthetic] The gallery reopened this week with a new exhibition of local artists.",
            None,
        ),
        bookmark_lang("c-es", "Art", es_text, Some("es")),
    ]
    .join("\n")
        + "\n";
    let labels =
        "{\"post_id\":\"c-en\",\"try_it\":false}\n{\"post_id\":\"c-es\",\"try_it\":true}\n";
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), labels, &bookmarks);
    // Cached translation for c-es next to the bookmarks file (read-only).
    let key = bookmarks_agent::translate::cache_key("c-es", es_text);
    let tcache = serde_json::json!({ key: {"english": es_english, "source_lang": "es"} });
    std::fs::write(dir.path().join("translations.json"), tcache.to_string())
        .expect("write translations");
    let jev_closed = closed_url();
    let muse_closed = closed_url();

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_closed),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
        ],
        &["ANCIENTOS_ALLOW_PAID", "TRANSLATIONS_CACHE_PATH"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    // Both sampled; c-es scores on its translation (rules-confident yes).
    assert!(
        stdout.contains("2 decided, 0 missing, 0 malformed, 1 translated, 0 untranslated"),
        "{stdout}"
    );
    let out = std::fs::read_to_string(&out_path).expect("read out");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("out is jsonl"))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["post_id"], "c-en");
    assert_eq!(rows[0]["predicted"], false);
    assert_eq!(rows[1]["post_id"], "c-es");
    assert_eq!(
        rows[1]["predicted"],
        true,
        "translation must flip the prediction: {row}",
        row = rows[1]
    );
    // Aggregate counts only in stdout: no post text leaks.
    assert!(!stdout.contains(es_text), "{stdout}");
    assert!(!stdout.contains(es_english), "{stdout}");
}

#[test]
fn stops_on_rate_limit() {
    let (jev_url, _, jev_done) = mock_server(vec![(429, r#"{"error":"slow down"}"#.to_string())]);
    let dir = tempfile::tempdir().expect("tempdir");
    let bookmarks = [
        bookmark(
            "c2",
            "AI Agents",
            "A tip. Roll your shoulders and hold for a moment.",
        ),
        bookmark("c3", "AI Agents", "An interesting read, you should try it."),
    ]
    .join("\n")
        + "\n";
    let labels = "{\"post_id\":\"c2\",\"try_it\":true}\n{\"post_id\":\"c3\",\"try_it\":false}\n";
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), labels, &bookmarks);
    let muse_closed = closed_url();

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
        ],
        &["ANCIENTOS_JEV_TRY_IT_THRESHOLD"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 1 call");
    assert!(stdout.contains("1 decided"), "{stdout}");
    assert!(
        stdout.contains("stopped early: rate limited (HTTP 429)"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "calls: jev=1 jev_yes=0 muse=0 muse_yes=0 muse_truncated=0 rules_yes=0 topic_filtered=0 fail_closed=1 errors=1"
        ),
        "{stdout}"
    );
    let out = std::fs::read_to_string(&out_path).expect("read out");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("out is jsonl"))
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["stage"], "fail_closed");
    assert_eq!(rows[0]["predicted"], false);
    assert!(rows[0]["error"].as_str().unwrap().contains("429"));
}

#[test]
fn truncation_is_counted_separately_from_ordinary_no() {
    let (jev_url, _, jev_done) =
        mock_server(vec![(200, jev_noul_body(0.2)), (200, jev_noul_body(0.1))]);
    // c2: empty content + finish_reason "length" (truncated); c3: normal no.
    let (muse_url, muse_seen, muse_done) = mock_server(vec![
        (200, muse_body_full(serde_json::json!(""), "length")),
        (200, muse_body("{\"try_it\":false}")),
    ]);
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = fixtures();
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_url),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
        ],
        &["ANCIENTOS_JEV_TRY_IT_THRESHOLD"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 2 calls");
    muse_done
        .recv_timeout(Duration::from_secs(15))
        .expect("muse got exactly 2 calls");

    // The truncation still fails closed (fail_closed/errors) but is also
    // counted separately, not as an ordinary Muse "no".
    assert!(
        stdout.contains(
            "calls: jev=2 jev_yes=0 muse=2 muse_yes=0 muse_truncated=1 rules_yes=1 topic_filtered=0 fail_closed=1 errors=1"
        ),
        "{stdout}"
    );
    let out = std::fs::read_to_string(&out_path).expect("read out");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("out is jsonl"))
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["truncated"], false);
    assert_eq!(rows[1]["post_id"], "c2");
    assert_eq!(rows[1]["predicted"], false);
    assert_eq!(rows[1]["stage"], "fail_closed");
    assert_eq!(rows[1]["truncated"], true);
    assert!(rows[1]["error"].is_string());
    assert_eq!(rows[2]["stage"], "muse");
    assert_eq!(rows[2]["truncated"], false);

    // Both calls carry the shared 1024-token classify cap.
    let muse_seen = muse_seen.lock().expect("seen lock");
    assert_eq!(muse_seen.len(), 2);
    for body in muse_seen.iter() {
        let sent: serde_json::Value = serde_json::from_str(body).expect("muse request is json");
        assert_eq!(sent["max_completion_tokens"], 1024);
    }
}

#[test]
fn jev_threshold_flag_overrides_env() {
    let (labels, bookmarks) = single_unsure_fixture();

    // Env 0.9, no flag: noul 0.5 falls through to Muse, and the summary
    // prints the env threshold.
    let (jev_url, _, jev_done) = mock_server(vec![(200, jev_noul_body(0.5))]);
    let (muse_url, _, muse_done) = mock_server(vec![(200, muse_body("{\"try_it\":true}"))]);
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_url),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
            ("ANCIENTOS_JEV_TRY_IT_THRESHOLD", "0.9"),
        ],
        &[],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 1 call");
    muse_done
        .recv_timeout(Duration::from_secs(15))
        .expect("muse got exactly 1 call");
    assert!(stdout.contains("1 decided"), "{stdout}");
    assert!(stdout.contains("jev threshold: 0.900"), "{stdout}");
    let rows = read_out_rows(&out_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["post_id"], "u1");
    assert_eq!(rows[0]["predicted"], true);
    assert_eq!(rows[0]["stage"], "muse");

    // Same env plus --jev-threshold 0.1: the flag wins, noul 0.5 answers
    // yes at Jev, and Muse (a closed port) is never reached.
    let (jev_url, _, jev_done) = mock_server(vec![(200, jev_noul_body(0.5))]);
    let muse_closed = closed_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
            "--jev-threshold",
            "0.1",
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
            ("ANCIENTOS_JEV_TRY_IT_THRESHOLD", "0.9"),
        ],
        &[],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 1 call");
    assert!(stdout.contains("jev threshold: 0.100"), "{stdout}");
    assert!(
        stdout.contains(
            "calls: jev=1 jev_yes=1 muse=0 muse_yes=0 muse_truncated=0 rules_yes=0 topic_filtered=0 fail_closed=0 errors=0"
        ),
        "{stdout}"
    );
    let rows = read_out_rows(&out_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["predicted"], true);
    assert_eq!(rows[0]["stage"], "jev");
}

#[test]
fn jev_threshold_invalid_env_falls_back_to_default() {
    let (labels, bookmarks) = single_unsure_fixture();

    // Bogus env falls back to the 0.3 default: noul 0.35 answers yes at
    // Jev, and Muse (a closed port) is never reached.
    let (jev_url, _, jev_done) = mock_server(vec![(200, jev_noul_body(0.35))]);
    let muse_closed = closed_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
            ("ANCIENTOS_JEV_TRY_IT_THRESHOLD", "bogus"),
        ],
        &[],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 1 call");
    assert!(stdout.contains("jev threshold: 0.300"), "{stdout}");
    let rows = read_out_rows(&out_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["predicted"], true);
    assert_eq!(rows[0]["stage"], "jev");

    // Unset env means the same default: noul 0.29 falls through to Muse.
    let (jev_url, _, jev_done) = mock_server(vec![(200, jev_noul_body(0.29))]);
    let (muse_url, _, muse_done) = mock_server(vec![(200, muse_body("{\"try_it\":false}"))]);
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_url),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
        ],
        &["ANCIENTOS_JEV_TRY_IT_THRESHOLD"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 1 call");
    muse_done
        .recv_timeout(Duration::from_secs(15))
        .expect("muse got exactly 1 call");
    assert!(stdout.contains("jev threshold: 0.300"), "{stdout}");
    let rows = read_out_rows(&out_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["predicted"], false);
    assert_eq!(rows[0]["stage"], "muse");
}

/// Rules-unsure, sanitized post text (mirrors the c2 fixture: no URLs,
/// handles, or other strippable tokens).
const ENRICH_TEXT: &str = "A tip. Roll your shoulders and hold for a moment.";

/// Same idea but carrying an email address, so the translate rule treats
/// it as Confidential: enrichment stays local-only AND the paid stages never
/// run (rules verdict instead).
const ENRICH_PRIVATE_TEXT: &str =
    "A tip with the full routine at quokka@example.invalid roll your shoulders and hold.";

/// Same idea carrying only a link: Public, so enrichment is appended and the
/// paid stages run on sanitized text (no raw URL in any hosted prompt).
const ENRICH_URL_TEXT: &str =
    "A tip with the full routine at https://example.invalid/guide roll your shoulders and hold.";

const ENRICH_PAGE: &str =
    "<title>ZestfulQuokkaStretch</title><p>Bend gently left and hold ten seconds.</p>";

fn enrich_fixtures(text: &str, page_url: &str) -> (String, String) {
    let bookmarks = bookmark_with_urls("e1", "AI Agents", text, &[page_url.to_string()]) + "\n";
    let labels = "{\"post_id\":\"e1\",\"try_it\":true}\n".to_string();
    (labels, bookmarks)
}

#[test]
fn enrich_is_off_by_default_with_zero_link_requests() {
    let (link_url, link_seen) =
        link_mock_server(vec![("/page", "text/html", ENRICH_PAGE.into())], 10);
    let page_url = format!("{link_url}/page");
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = enrich_fixtures(ENRICH_TEXT, &page_url);
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let jev_closed = closed_url();
    let muse_closed = closed_url();

    // No --enrich flag: the stored URL is never fetched.
    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_closed),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
        ],
        &[
            "ANCIENTOS_ALLOW_PAID",
            "ANCIENTOS_FETCH_LINKS",
            "LINK_CONTEXT_CACHE_PATH",
        ],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    assert!(stdout.contains("1 decided"), "{stdout}");
    assert!(stdout.contains("0 enriched"), "{stdout}");
    assert_eq!(link_hits(&link_seen), 0);
    // No link cache file is written without --enrich either.
    assert!(
        !dir.path().join("link_context_cache.json").exists(),
        "no cache file without --enrich"
    );
}

#[test]
fn enrich_without_fetch_opt_in_makes_zero_requests() {
    let (link_url, link_seen) =
        link_mock_server(vec![("/page", "text/html", ENRICH_PAGE.into())], 10);
    let page_url = format!("{link_url}/page");
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = enrich_fixtures(ENRICH_TEXT, &page_url);
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let cache_path = dir.path().join("links.json");
    let cache_str = cache_path.to_str().unwrap().to_string();
    let jev_closed = closed_url();
    let muse_closed = closed_url();

    // --enrich without ANCIENTOS_FETCH_LINKS=1: nothing fetched, nothing
    // appended, but the (empty) cache is still saved.
    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
            "--enrich",
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_closed),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
            ("LINK_CONTEXT_CACHE_PATH", &cache_str),
        ],
        &["ANCIENTOS_ALLOW_PAID", "ANCIENTOS_FETCH_LINKS"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    assert!(stdout.contains("0 enriched"), "{stdout}");
    assert_eq!(link_hits(&link_seen), 0);
    let cached = std::fs::read_to_string(&cache_path).expect("cache written");
    assert_eq!(cached, "{}");
}

#[test]
fn enrich_fetches_once_then_reuses_cache() {
    let (link_url, link_seen) =
        link_mock_server(vec![("/page", "text/html", ENRICH_PAGE.into())], 100);
    let page_url = format!("{link_url}/page");
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = enrich_fixtures(ENRICH_TEXT, &page_url);
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let cache_path = dir.path().join("links.json");
    let cache_str = cache_path.to_str().unwrap().to_string();
    let jev_closed = closed_url();
    let muse_closed = closed_url();
    let args = [
        "--labels",
        labels_path.as_str(),
        "--bookmarks",
        bookmarks_path.as_str(),
        "--out",
        out_path.as_str(),
        "--enrich",
    ];

    // First run: one fetch, one enriched item (rules-only: paid off, so the
    // closed Jev/Muse ports are never touched).
    let (ok, stdout, stderr) = run_bin(
        &args,
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_closed),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_FETCH_LINKS", "1"),
            ("LINK_CONTEXT_ALLOW_PRIVATE_HOSTS", "1"),
            ("LINK_CONTEXT_CACHE_PATH", &cache_str),
        ],
        &["ANCIENTOS_ALLOW_PAID"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    assert!(stdout.contains("1 decided"), "{stdout}");
    assert!(stdout.contains("1 enriched"), "{stdout}");
    assert!(stdout.contains("paid: off"), "{stdout}");
    assert_eq!(link_hits(&link_seen), 1);
    // The cache is keyed by sha256(url) and holds the page text.
    let cached = std::fs::read_to_string(&cache_path).expect("cache written");
    let parsed: serde_json::Value = serde_json::from_str(&cached).expect("cache is json");
    let key = bookmarks_agent::translate::sha256_hex(&page_url);
    assert!(
        parsed.get(&key).is_some_and(|e| e["text"]
            .as_str()
            .is_some_and(|t| t.contains("ZestfulQuokkaStretch"))),
        "{cached}"
    );

    // Second run: the cache hit enriches with zero new requests.
    let (ok, stdout, stderr) = run_bin(
        &args,
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_closed),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_FETCH_LINKS", "1"),
            ("LINK_CONTEXT_ALLOW_PRIVATE_HOSTS", "1"),
            ("LINK_CONTEXT_CACHE_PATH", &cache_str),
        ],
        &["ANCIENTOS_ALLOW_PAID"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    assert!(stdout.contains("1 enriched"), "{stdout}");
    assert_eq!(link_hits(&link_seen), 1);
}

#[test]
fn enrichment_reaches_paid_prompts_for_sanitized_posts() {
    assert!(
        !bookmarks_agent::try_it::rules_confident("AI Agents", ENRICH_TEXT),
        "fixture must stay rules-unsure so the paid stages run"
    );
    let (link_url, link_seen) =
        link_mock_server(vec![("/page", "text/html", ENRICH_PAGE.into())], 10);
    let page_url = format!("{link_url}/page");
    let (jev_url, jev_seen, jev_done) = mock_server(vec![(200, jev_noul_body(0.1))]);
    let (muse_url, muse_seen, muse_done) = mock_server(vec![(200, muse_body("{\"try_it\":true}"))]);
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = enrich_fixtures(ENRICH_TEXT, &page_url);
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let cache_path = dir.path().join("links.json");
    let cache_str = cache_path.to_str().unwrap().to_string();

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
            "--enrich",
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_url),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
            ("ANCIENTOS_FETCH_LINKS", "1"),
            ("LINK_CONTEXT_ALLOW_PRIVATE_HOSTS", "1"),
            ("LINK_CONTEXT_CACHE_PATH", &cache_str),
        ],
        &["ANCIENTOS_JEV_TRY_IT_THRESHOLD"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 1 call");
    muse_done
        .recv_timeout(Duration::from_secs(15))
        .expect("muse got exactly 1 call");
    assert!(stdout.contains("1 enriched"), "{stdout}");
    assert_eq!(link_hits(&link_seen), 1);
    // Both paid prompts carry the linked-page text.
    let jev_body = jev_seen.lock().expect("jev seen");
    assert_eq!(jev_body.len(), 1);
    assert!(
        jev_body[0].contains("ZestfulQuokkaStretch"),
        "{}",
        jev_body[0]
    );
    assert!(
        jev_body[0].contains("Linked/Media context:"),
        "{}",
        jev_body[0]
    );
    let muse_body = muse_seen.lock().expect("muse seen");
    assert_eq!(muse_body.len(), 1);
    assert!(
        muse_body[0].contains("ZestfulQuokkaStretch"),
        "{}",
        muse_body[0]
    );
    assert!(
        muse_body[0].contains("Linked/Media context:"),
        "{}",
        muse_body[0]
    );
    let rows = read_out_rows(&out_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["predicted"], true);
    assert_eq!(rows[0]["stage"], "muse");
}

#[test]
fn private_enrichment_never_reaches_paid_prompts() {
    assert!(
        !bookmarks_agent::try_it::rules_confident("AI Agents", ENRICH_PRIVATE_TEXT),
        "fixture must stay rules-unsure"
    );
    let (link_url, link_seen) =
        link_mock_server(vec![("/page", "text/html", ENRICH_PAGE.into())], 10);
    let page_url = format!("{link_url}/page");
    // Closed ports: any paid-stage attempt would fail the run's verdict, so a
    // rules verdict proves the Confidential item never reached hosted stages.
    let jev_url = closed_url();
    let muse_url = closed_url();
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = enrich_fixtures(ENRICH_PRIVATE_TEXT, &page_url);
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let cache_path = dir.path().join("links.json");
    let cache_str = cache_path.to_str().unwrap().to_string();

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
            "--enrich",
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_url),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
            ("ANCIENTOS_FETCH_LINKS", "1"),
            ("LINK_CONTEXT_ALLOW_PRIVATE_HOSTS", "1"),
            ("LINK_CONTEXT_CACHE_PATH", &cache_str),
        ],
        &["ANCIENTOS_JEV_TRY_IT_THRESHOLD"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    // Rules fallback with zero paid calls: stage rules, not a stage error.
    let rows = read_out_rows(&out_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["stage"], "rules");
    assert_eq!(rows[0]["predicted"], false);
    // The page is fetched (local use) but never appended: 0 enriched.
    assert!(stdout.contains("0 enriched"), "{stdout}");
    assert_eq!(link_hits(&link_seen), 1);
}

#[test]
fn url_post_is_enriched_and_hosted_with_sanitized_prompts() {
    assert!(
        !bookmarks_agent::try_it::rules_confident("AI Agents", ENRICH_URL_TEXT),
        "fixture must stay rules-unsure so the paid stages run"
    );
    let (link_url, link_seen) =
        link_mock_server(vec![("/page", "text/html", ENRICH_PAGE.into())], 10);
    let page_url = format!("{link_url}/page");
    let (jev_url, jev_seen, jev_done) = mock_server(vec![(200, jev_noul_body(0.1))]);
    let (muse_url, muse_seen, muse_done) = mock_server(vec![(200, muse_body("{\"try_it\":true}"))]);
    let dir = tempfile::tempdir().expect("tempdir");
    let (labels, bookmarks) = enrich_fixtures(ENRICH_URL_TEXT, &page_url);
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let cache_path = dir.path().join("links.json");
    let cache_str = cache_path.to_str().unwrap().to_string();

    let (ok, stdout, stderr) = run_bin(
        &[
            "--labels",
            &labels_path,
            "--bookmarks",
            &bookmarks_path,
            "--out",
            &out_path,
            "--enrich",
        ],
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_url),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_url),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_ALLOW_PAID", "1"),
            ("ANCIENTOS_FETCH_LINKS", "1"),
            ("LINK_CONTEXT_ALLOW_PRIVATE_HOSTS", "1"),
            ("LINK_CONTEXT_CACHE_PATH", &cache_str),
        ],
        &["ANCIENTOS_JEV_TRY_IT_THRESHOLD"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    jev_done
        .recv_timeout(Duration::from_secs(15))
        .expect("jev got exactly 1 call");
    muse_done
        .recv_timeout(Duration::from_secs(15))
        .expect("muse got exactly 1 call");
    assert!(stdout.contains("1 enriched"), "{stdout}");
    assert_eq!(link_hits(&link_seen), 1);
    // Both paid prompts carry the linked-page text ...
    let jev_body = jev_seen.lock().expect("jev seen");
    assert_eq!(jev_body.len(), 1);
    assert!(
        jev_body[0].contains("ZestfulQuokkaStretch"),
        "{}",
        jev_body[0]
    );
    assert!(
        jev_body[0].contains("Linked/Media context:"),
        "{}",
        jev_body[0]
    );
    let muse_body = muse_seen.lock().expect("muse seen");
    assert_eq!(muse_body.len(), 1);
    assert!(
        muse_body[0].contains("ZestfulQuokkaStretch"),
        "{}",
        muse_body[0]
    );
    assert!(
        muse_body[0].contains("Linked/Media context:"),
        "{}",
        muse_body[0]
    );
    // ... while the raw post URL never left the device.
    assert!(
        !jev_body[0].contains("https://example.invalid/guide"),
        "raw URL leaked to jev: {}",
        jev_body[0]
    );
    assert!(
        !muse_body[0].contains("https://example.invalid/guide"),
        "raw URL leaked to muse: {}",
        muse_body[0]
    );
    let rows = read_out_rows(&out_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["predicted"], true);
    assert_eq!(rows[0]["stage"], "muse");
}

/// Bookmark with one video attachment and no links: `--enrich` can only
/// enrich it via local transcription.
fn bookmark_with_video(id: &str, topic: &str, text: &str, video_url: &str) -> String {
    serde_json::json!({
        "post_id": id,
        "url": format!("https://x.com/i/status/{id}"),
        "text": text,
        "label": {"parent_label": topic},
        "media": [{
            "kind": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/synthetic.jpg",
            "video_url": video_url,
        }],
    })
    .to_string()
}

/// Write an executable fake tool script for child-process tests.
fn write_fake_script(dir: &std::path::Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fake script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path.to_str().unwrap().to_string()
}

/// Parent `PATH` with `dir` prepended, for child env.
fn path_with(dir: &std::path::Path) -> String {
    let mut parts = vec![dir.to_path_buf()];
    if let Some(old) = std::env::var_os("PATH") {
        parts.extend(std::env::split_paths(&old));
    }
    std::env::join_paths(parts)
        .expect("join PATH")
        .into_string()
        .expect("PATH utf8")
}

#[test]
fn enrich_counts_transcripts() {
    let (video_base, video_seen) =
        link_mock_server(vec![("/v.mp4", "video/mp4", "fake-mp4-bytes".into())], 10);
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("bin dir");
    write_fake_script(
        &bin,
        "ffmpeg",
        "#!/bin/sh\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf 'fake-audio' > \"$out\"\nexit 0\n",
    );
    let whisper = write_fake_script(
        &bin,
        "whisper-fake",
        "#!/bin/sh\necho WobbleQuokkaTranscript stretch and hold\nexit 0\n",
    );
    let model = dir.path().join("model.bin");
    std::fs::write(&model, b"fake model").expect("model");
    let model_str = model.to_str().unwrap().to_string();
    let path_str = path_with(&bin);
    let bookmarks = bookmark_with_video(
        "t1",
        "AI Agents",
        ENRICH_TEXT,
        "https://video.twimg.com/v.mp4",
    ) + "\n";
    let labels = "{\"post_id\":\"t1\",\"try_it\":true}\n".to_string();
    let (labels_path, bookmarks_path, out_path) = write_fixtures(dir.path(), &labels, &bookmarks);
    let cache_path = dir.path().join("links.json");
    let cache_str = cache_path.to_str().unwrap().to_string();
    let jev_closed = closed_url();
    let muse_closed = closed_url();
    let args = [
        "--labels",
        labels_path.as_str(),
        "--bookmarks",
        bookmarks_path.as_str(),
        "--out",
        out_path.as_str(),
        "--enrich",
    ];

    // First run: the transcript enriches the linkless video item (rules-only:
    // paid off, so the closed Jev/Muse ports are never touched).
    let (ok, stdout, stderr) = run_bin(
        &args,
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_closed),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_FETCH_LINKS", "1"),
            ("LINK_CONTEXT_VIDEO_BASE_URL", &video_base),
            ("ANCIENTOS_WHISPER_CMD", &whisper),
            ("ANCIENTOS_WHISPER_MODEL", &model_str),
            ("PATH", &path_str),
            ("LINK_CONTEXT_CACHE_PATH", &cache_str),
        ],
        &["ANCIENTOS_ALLOW_PAID"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    assert!(stdout.contains("1 decided"), "{stdout}");
    assert!(stdout.contains("1 enriched"), "{stdout}");
    assert_eq!(link_hits(&video_seen), 1);
    let cached = std::fs::read_to_string(&cache_path).expect("cache written");
    let parsed: serde_json::Value = serde_json::from_str(&cached).expect("cache is json");
    let key =
        bookmarks_agent::translate::sha256_hex("video-transcript:https://video.twimg.com/v.mp4");
    assert!(
        parsed.get(&key).is_some_and(|e| e["text"]
            .as_str()
            .is_some_and(|t| t.contains("WobbleQuokkaTranscript"))),
        "{cached}"
    );

    // Second run: the cache hit enriches with zero new video requests.
    let (ok, stdout, stderr) = run_bin(
        &args,
        &[
            ("JEV_BACKEND", "typesafe"),
            ("TYPESAFE_BASE_URL", &jev_closed),
            ("TYPESAFE_API_KEY", "test-key"),
            ("META_API_BASE", &muse_closed),
            ("META_API_KEY", "test-key"),
            ("ANCIENTOS_FETCH_LINKS", "1"),
            ("LINK_CONTEXT_VIDEO_BASE_URL", &video_base),
            ("ANCIENTOS_WHISPER_CMD", &whisper),
            ("ANCIENTOS_WHISPER_MODEL", &model_str),
            ("PATH", &path_str),
            ("LINK_CONTEXT_CACHE_PATH", &cache_str),
        ],
        &["ANCIENTOS_ALLOW_PAID"],
    );
    assert!(ok, "exit ok, stderr: {stderr}");
    assert!(stdout.contains("1 enriched"), "{stdout}");
    assert_eq!(link_hits(&video_seen), 1);
}
