//! Test for `scripts/muse_run.sh` cost accounting on a synthetic fixture.
use std::path::PathBuf;
use std::process::Command;

/// Test helper: script path.
fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/muse_run.sh")
}

/// Test helper: fixture path.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/muse_run_sample.jsonl")
}

#[test]
fn muse_run_appends_token_counts_and_cost() {
    let dir = tempfile::tempdir().unwrap();
    let prompt = dir.path().join("prompt.md");
    std::fs::write(&prompt, "synthetic prompt\n").unwrap();
    let log = dir.path().join("muse-log.md");
    let out = Command::new("bash")
        .arg(script_path())
        .args(["99", "synthetic muse-run test", &prompt.to_string_lossy()])
        .env("MUSE_RUN_JSONL", fixture_path())
        .env("MUSE_LOG", &log)
        .output()
        .expect("run scripts/muse_run.sh");
    assert!(
        out.status.success(),
        "muse_run.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(stdout.contains("in=1200000"), "{stdout}");
    assert!(stdout.contains("cached=200000"), "{stdout}");
    assert!(stdout.contains("out=800000"), "{stdout}");
    assert!(stdout.contains("cost=$4.68"), "{stdout}");
    let body = std::fs::read_to_string(&log).expect("read temp MUSE_LOG");
    assert!(body.contains("| 99 |"), "{body}");
    assert!(body.contains("synthetic muse-run test"), "{body}");
    assert!(body.contains("1,200,000/200,000/800,000"), "{body}");
    assert!(body.contains("$4.68"), "{body}");
    assert!(body.contains("exit 0"), "{body}");
}
