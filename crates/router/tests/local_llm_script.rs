//! Integration test for `scripts/local_llm.sh` against a mock `/api/tags` server.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Test helper: script path.
fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/local_llm.sh")
}

/// Test helper: curl present.
fn curl_present() -> bool {
    Command::new("curl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Mock tags server: replies `body` with 200 to `count` connections, then
/// exits. Returns the base URL and the join handle (served count).
fn mock_tags_server(body: &'static str, count: usize) -> (String, std::thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock tags server");
    listener
        .set_nonblocking(true)
        .expect("nonblocking mock server");
    let url = format!("http://{}", listener.local_addr().expect("mock addr"));
    let handle = std::thread::spawn(move || {
        let mut served = 0;
        let start = Instant::now();
        while served < count && start.elapsed() < Duration::from_secs(15) {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(_) => break,
            };
            let mut stream = stream;
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            // Drain the request headers (GET has no body).
            let mut head = Vec::new();
            let mut buf = [0u8; 1];
            while let Ok(1) = stream.read(&mut buf) {
                head.push(buf[0]);
                if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
                    break;
                }
                if head.len() > 65536 {
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            served += 1;
        }
        served
    });
    (url, handle)
}

/// A base URL with nothing listening (connection refused, fast).
fn closed_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let url = format!("http://{}", listener.local_addr().expect("probe addr"));
    drop(listener);
    url
}

/// Test helper: run health.
fn run_health(base_url: &str, model: &str) -> (Option<i32>, String, String) {
    assert!(curl_present(), "curl is required by scripts/local_llm.sh");
    let out = Command::new("bash")
        .arg(script_path())
        .arg("health")
        .env("ANCIENTOS_LOCAL_LLM_URL", base_url)
        .env("ANCIENTOS_LOCAL_LLM_MODEL", model)
        .output()
        .expect("run scripts/local_llm.sh");
    (
        out.status.code(),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

#[test]
fn health_ok_when_server_up_and_model_present() {
    let (url, handle) = mock_tags_server(r#"{"models":[{"name":"test-model","size":1}]}"#, 2);
    let (code, out, err) = run_health(&url, "test-model");
    assert_eq!(code, Some(0), "health failed: {err}");
    assert!(out.contains("healthy"), "{out}");
    assert!(out.contains(&url), "{out}");
    assert_eq!(handle.join().expect("server thread"), 2);
}

#[test]
fn health_fails_closed_when_server_down() {
    let url = closed_url();
    let (code, _, err) = run_health(&url, "test-model");
    assert_ne!(code, Some(0), "health must fail when down");
    assert!(err.contains("local LLM unavailable"), "{err}");
    assert!(err.contains("NOT sent elsewhere"), "{err}");
}

#[test]
fn health_fails_closed_when_model_missing() {
    let (url, handle) = mock_tags_server(r#"{"models":[{"name":"other"}]}"#, 2);
    let (code, _, err) = run_health(&url, "test-model");
    assert_ne!(code, Some(0), "health must fail when model missing");
    assert!(err.contains("not present"), "{err}");
    assert!(err.contains("test-model"), "{err}");
    assert_eq!(handle.join().expect("server thread"), 2);
}

#[test]
fn usage_error_on_unknown_subcommand() {
    let out = Command::new("bash")
        .arg(script_path())
        .arg("bogus")
        .output()
        .expect("run scripts/local_llm.sh");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr)
        .expect("stderr utf8")
        .contains("usage"));
}
