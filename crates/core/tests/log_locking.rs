//! Two-process locking tests: concurrent appends form one unbroken hash
//! chain, and racing releases yield exactly one success per approval.
//! Spawns the `log_stress` helper twice per test; synthetic data only,
//! no network, no paid calls.
use ancientos_core::log::DecisionLog;
use std::path::PathBuf;
use std::process::{Child, Command};

/// Entries each appender child writes.
const APPENDS_PER_CHILD: usize = 50;

/// Approvals raced in the release test.
const RACE_APPROVALS: usize = 10;

/// Synthetic request time (2023-11-14T22:13:20Z).
const NOW: u64 = 1_700_000_000;

/// Path of the built `log_stress` helper binary.
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_log_stress"))
}

/// Spawn one `log_stress` child with piped stdio and the given arguments.
fn spawn(args: &[&str]) -> Child {
    Command::new(bin())
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn log_stress")
}

/// Wait for a child, asserting exit 0; returns captured stdout.
fn wait_ok(child: Child, what: &str) -> String {
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait {what}: {e}"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "{what} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Synthetic gate for seeding: `x.post` needs a human.
fn gate() -> ancientos_core::gate::Gate {
    let mut g = ancientos_core::gate::Gate::new();
    g.register("x.post", ancientos_core::gate::Risk::High);
    g
}

/// Number of `approval.released` entries for one approval id.
fn released_count(entries: &[ancientos_core::log::Entry], id: &str) -> usize {
    entries
        .iter()
        .filter(|e| {
            e.kind == ancientos_core::approvals::RELEASED_KIND
                && e.payload.get("id").and_then(|v| v.as_str()) == Some(id)
        })
        .count()
}

/// Two processes appending concurrently form one unbroken hash chain holding
/// every entry from both writers, with contiguous sequence numbers.
#[test]
fn two_processes_append_concurrently_without_forking_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    let p = path.to_str().unwrap().to_string();
    let count = APPENDS_PER_CHILD.to_string();
    let a = spawn(&["append", &p, "a", &count]);
    let b = spawn(&["append", &p, "b", &count]);
    wait_ok(a, "appender a");
    wait_ok(b, "appender b");
    let log = DecisionLog::open(&path).unwrap();
    let entries = log.verify().unwrap();
    assert_eq!(entries.len(), 2 * APPENDS_PER_CHILD);
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.seq, i as u64, "seq gap or fork at index {i}");
    }
    for worker in ["a", "b"] {
        let n = entries
            .iter()
            .filter(|e| e.payload.get("worker").and_then(|v| v.as_str()) == Some(worker))
            .count();
        assert_eq!(n, APPENDS_PER_CHILD, "worker {worker} lost entries");
    }
}

/// Two processes racing to release the same approvals yield exactly one
/// success per approval: one `approval.released` entry each, all Released.
#[test]
fn two_processes_racing_release_yield_exactly_one_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.jsonl");
    let mut log = DecisionLog::open(&path).unwrap();
    let mut specs = Vec::new();
    for i in 0..RACE_APPROVALS {
        let payload = serde_json::json!({"draft": format!("race-{i}")});
        let req = ancientos_core::approvals::request(
            &mut log,
            &gate(),
            "bookmarks-agent",
            "x.post",
            "synthetic race",
            payload.clone(),
            NOW + i as u64,
            None,
        )
        .unwrap();
        ancientos_core::approvals::approve(
            &mut log,
            &req.id,
            &req.payload_hash,
            "human",
            NOW + 100 + i as u64,
        )
        .unwrap();
        specs.push(serde_json::json!({
            "id": req.id,
            "payload_hash": req.payload_hash,
            "payload": payload,
        }));
    }
    drop(log);
    let specs_json = serde_json::to_string(&specs).unwrap();
    let p = path.to_str().unwrap().to_string();
    let a = spawn(&["release-all", &p, &specs_json]);
    let b = spawn(&["release-all", &p, &specs_json]);
    let wins_a: usize = wait_ok(a, "racer a").trim().parse().unwrap();
    let wins_b: usize = wait_ok(b, "racer b").trim().parse().unwrap();
    assert_eq!(
        wins_a + wins_b,
        RACE_APPROVALS,
        "every approval released exactly once"
    );
    let log = DecisionLog::open(&path).unwrap();
    let entries = log.verify().unwrap();
    assert_eq!(entries.len(), 3 * RACE_APPROVALS);
    for spec in &specs {
        let id = spec["id"].as_str().unwrap();
        assert_eq!(released_count(&entries, id), 1, "approval {id}");
        assert_eq!(
            ancientos_core::approvals::get(&log, id).unwrap().status,
            ancientos_core::approvals::Status::Released,
            "approval {id}"
        );
    }
}
