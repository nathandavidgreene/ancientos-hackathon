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
    serde_json::json!({
        "post_id": id,
        "url": format!("https://x.com/i/status/{id}"),
        "text": text,
        "label": {"parent_label": topic},
    })
    .to_string()
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
        &[],
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
            "calls: jev=2 jev_yes=0 muse=2 muse_yes=1 rules_yes=1 fail_closed=0 errors=0"
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
    assert_eq!(rows[1]["post_id"], "c2");
    assert_eq!(rows[1]["predicted"], true);
    assert_eq!(rows[1]["stage"], "muse");
    assert_eq!(rows[2]["post_id"], "c3");
    assert_eq!(rows[2]["predicted"], false);
    assert_eq!(rows[2]["stage"], "muse");

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
        assert_eq!(sent["max_completion_tokens"], 200);
        assert_eq!(sent["reasoning_effort"], "low");
    }
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
            "calls: jev=0 jev_yes=0 muse=0 muse_yes=0 rules_yes=1 fail_closed=0 errors=0"
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
        &[],
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
            "calls: jev=1 jev_yes=0 muse=0 muse_yes=0 rules_yes=0 fail_closed=1 errors=1"
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
