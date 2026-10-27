//! Integration test for `scripts/demo.sh`: all 14 shots, determinism, no leaks.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

/// Test helper: demo.sh script location.
fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/demo.sh")
}

/// Test helper: demo.sh exit code plus captured output.
fn run_demo(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new("bash")
        .arg(script_path())
        .args(args)
        .env("CARGO_HOME", "/tmp/cargo-home")
        .output()
        .expect("run scripts/demo.sh");
    (
        out.status.code(),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

/// Test helper: demo storyboard document location.
fn storyboard_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/demo-storyboard.md")
}

/// Backtick-quoted strings from the shot table's "On-screen action" column.
/// Every one must appear verbatim in `scripts/demo.sh` output.
fn storyboard_quotes() -> Vec<String> {
    let text = std::fs::read_to_string(storyboard_path()).expect("read demo-storyboard.md");
    let mut quotes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.split('|').collect();
        if cells.len() < 6 {
            continue;
        }
        if cells[1].trim().parse::<u32>().is_err() {
            continue; // header or separator row
        }
        let mut rest = cells[4];
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else {
                break;
            };
            let quote = &after[..end];
            if !quote.is_empty() {
                quotes.push(quote.to_string());
            }
            rest = &after[end + 1..];
        }
    }
    quotes
}

/// Same rule as `scripts/demo.sh` shot 13: log rows matching
/// `^\| [0-9]+[a-z]? `, excluding rows marked `Not used`.
fn muse_code_runs_from_log() -> usize {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/muse-code-log.md");
    let text = std::fs::read_to_string(path).expect("read docs/muse-code-log.md");
    text.lines()
        .filter(|line| {
            if line.contains("Not used") {
                return false;
            }
            let Some(after) = line.strip_prefix("| ") else {
                return false;
            };
            let digits = after.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digits == 0 {
                return false;
            }
            let mut rest = &after[digits..];
            if let Some(tail) = rest.strip_prefix(|c: char| c.is_ascii_lowercase()) {
                rest = tail;
            }
            rest.starts_with(' ')
        })
        .count()
}

/// Test helper: whether text contains a 15+ digit run.
fn has_15plus_digits(s: &str) -> bool {
    let mut run = 0usize;
    for c in s.chars() {
        if c.is_ascii_digit() {
            run += 1;
            if run >= 15 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Test helper: whether a char is valid in a handle.
fn is_handle_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Handles (`@word`) and 15+ digit ids found in the real data dir, if present.
/// Synthetic `demo_*` handles are excluded: they are expected in demo output.
fn private_strings() -> Vec<String> {
    let mut out = BTreeSet::new();
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }
    let dir = PathBuf::from(home).join(".ancientos-hackathon");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    for entry in entries.flatten() {
        let text = match std::fs::read_to_string(entry.path()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '@' {
                let mut j = i + 1;
                while j < chars.len() && is_handle_char(chars[j]) {
                    j += 1;
                }
                if j - (i + 1) >= 2 {
                    let handle: String = chars[i..j].iter().collect();
                    if !handle.starts_with("@demo_") {
                        out.insert(handle);
                    }
                }
                i = j;
            } else if chars[i].is_ascii_digit() {
                let mut j = i;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if j - i >= 15 {
                    out.insert(chars[i..j].iter().collect());
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }
    out.into_iter().collect()
}

#[test]
fn demo_runs_all_shots_deterministically_without_private_data() {
    let (code1, out1, err1) = run_demo(&[]);
    assert_eq!(code1, Some(0), "demo.sh failed: {err1}");
    for n in 1..=14 {
        assert!(
            out1.contains(&format!("Shot {n}/14")),
            "missing Shot {n}/14"
        );
    }
    assert!(
        !has_15plus_digits(&out1),
        "output contains a 15+ digit number"
    );
    for secret in private_strings() {
        assert!(
            !out1.contains(&secret),
            "output leaks private string {secret}"
        );
    }
    let (code2, out2, err2) = run_demo(&[]);
    assert_eq!(code2, Some(0), "second run failed: {err2}");
    assert_eq!(out1, out2, "demo output must be byte-identical");
    let quotes = storyboard_quotes();
    assert!(
        quotes.len() >= 50,
        "expected 50+ storyboard quotes, found {}",
        quotes.len()
    );
    for key in [
        "[TRY IT]",
        "[AUTO-APPROVED]",
        "hash chain verified",
        "threshold shift:",
        "tampered copy:",
        "idea=Drag a card to approve",
        "deeper.paid",
        "topic_filter",
        "Translated from",
        "never_author",
        "already decided",
        "exactly once",
    ] {
        assert!(
            quotes.iter().any(|q| q.contains(key)),
            "storyboard must quote {key}"
        );
    }
    for q in &quotes {
        if q.starts_with("Muse Code runs:") {
            // The storyboard quotes the `N` placeholder form; the concrete
            // count is checked against the derived value below.
            continue;
        }
        assert!(
            out1.contains(q),
            "storyboard quote missing from demo output: {q:?}"
        );
    }
    // Shot 13 derives N from the log with the same rule as scripts/demo.sh.
    let expected_runs = muse_code_runs_from_log();
    assert!(
        out1.contains(&format!(
            "Muse Code runs: {expected_runs} (from docs/muse-code-log.md)"
        )),
        "demo output must show the derived run count {expected_runs}"
    );
    let storyboard = std::fs::read_to_string(storyboard_path()).expect("read demo-storyboard.md");
    assert!(
        storyboard.contains("Muse Code runs: N (from docs/muse-code-log.md)"),
        "storyboard must quote the N placeholder form"
    );
    assert!(
        storyboard.contains("derived from"),
        "storyboard must note that N is derived from the log"
    );
}

#[test]
fn demo_shot_8_alone() {
    let (code, out, err) = run_demo(&["--shot", "8"]);
    assert_eq!(code, Some(0), "--shot 8 failed: {err}");
    assert!(out.contains("Shot 8/14"), "missing Shot 8/14");
    assert!(!out.contains("Shot 1/14"), "shot 8 leaked shot 1");
    for want in [
        "demo-2001",
        "demo-2002",
        "demo-2003",
        "idea card",
        "ideas",
        "triage",
        "adopt",
    ] {
        assert!(out.contains(want), "shot 8 missing {want}");
    }
    assert!(!has_15plus_digits(&out));
}

#[test]
fn demo_each_shot_runs_alone() {
    for n in 1..=14 {
        let arg = n.to_string();
        let (code, out, err) = run_demo(&["--shot", &arg]);
        assert_eq!(code, Some(0), "--shot {n} failed: {err}");
        assert!(
            out.contains(&format!("==== Shot {n}/14:")),
            "--shot {n} missing its header"
        );
        for m in 1..=14 {
            if m != n {
                assert!(
                    !out.contains(&format!("==== Shot {m}/14:")),
                    "--shot {n} leaked shot {m}"
                );
            }
        }
        assert!(!has_15plus_digits(&out), "--shot {n} has 15+ digit run");
    }
}

#[test]
fn demo_shot_11_approvals_flow() {
    let (code, out, err) = run_demo(&["--shot", "11"]);
    assert_eq!(code, Some(0), "--shot 11 failed: {err}");
    for want in [
        "Shot 11/14",
        "ANCIENTOS_ALLOW_PAID=unset",
        "queued demo-3001",
        "deeper.paid pending",
        "expires_at=",
        "bookmarks-agent approvals:",
        "release-ready",
        "exactly once",
        "walkthrough demo-3001.md",
        "approval.requested",
        "approval.approved",
        "approval.released",
        "hash chain verified",
        "already decided",
    ] {
        assert!(out.contains(want), "--shot 11 missing {want}");
    }
    assert!(!has_15plus_digits(&out));
}

#[test]
fn demo_shot_12_cascade_stages() {
    let (code, out, err) = run_demo(&["--shot", "12"]);
    assert_eq!(code, Some(0), "--shot 12 failed: {err}");
    for want in [
        "Shot 12/14",
        "mock Jev + mock Muse",
        "stage=topic_filter",
        "stage=rules",
        "stage=jev",
        "topic_filtered=1",
        "jev_calls=1",
        "muse_calls=0",
    ] {
        assert!(out.contains(want), "--shot 12 missing {want}");
    }
    assert!(!has_15plus_digits(&out));
}

#[test]
fn demo_rejects_invalid_shot() {
    for bad in ["15", "99"] {
        let (code, _, err) = run_demo(&["--shot", bad]);
        assert_eq!(code, Some(2), "--shot {bad} must exit 2");
        assert!(err.contains("usage"), "--shot {bad} must print usage");
    }
}
