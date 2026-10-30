//! `log_stress` — two-process stress helper for the decision-log locking tests.
//!
//! Test-only: `tests/log_locking.rs` spawns this binary twice concurrently.
//! - `append PATH WORKER COUNT` appends `COUNT` entries as `WORKER`.
//! - `release-all PATH SPECS_JSON` releases each approval in the JSON array
//!   `[{id, payload_hash, payload}]` and prints the number of successes.
//!
//! Exit 0 on success; exit 1 with a stderr message on any failure. Losing a
//! release race is not a failure (it just does not count as a success).
use serde::Deserialize;
use std::path::Path;

/// Usage text printed on bad arguments.
const USAGE: &str = "usage: log_stress append PATH WORKER COUNT | release-all PATH SPECS_JSON";

/// One approval a racer tries to release: log id, expected hash, payload.
#[derive(Debug, Deserialize)]
struct ReleaseSpec {
    id: String,
    payload_hash: String,
    payload: serde_json::Value,
}

/// Append `count` entries as `worker` to the log at `path`.
fn cmd_append(path: &Path, worker: &str, count: usize) -> Result<(), String> {
    let mut log = ancientos_core::log::DecisionLog::open(path).map_err(|e| e.to_string())?;
    for i in 0..count {
        log.append("stress", serde_json::json!({"worker": worker, "i": i}))
            .map_err(|e| format!("append {i}: {e}"))?;
    }
    Ok(())
}

/// Release every approval in `specs_json` against the log at `path`.
///
/// Returns the number of successful releases; approvals lost to the other
/// racer (claim or release failing) are skipped, not errors.
fn cmd_release_all(path: &Path, specs_json: &str) -> Result<usize, String> {
    let specs: Vec<ReleaseSpec> =
        serde_json::from_str(specs_json).map_err(|e| format!("bad specs: {e}"))?;
    let mut log = ancientos_core::log::DecisionLog::open(path).map_err(|e| e.to_string())?;
    let mut wins = 0;
    for spec in &specs {
        let token = match ancientos_core::approvals::claim_release_token(
            &log,
            &spec.id,
            &spec.payload_hash,
        ) {
            Ok(token) => token,
            Err(_) => continue,
        };
        if ancientos_core::approvals::release(&mut log, token, &spec.payload).is_ok() {
            wins += 1;
        }
    }
    Ok(wins)
}

/// Dispatch `argv` to one subcommand.
fn run(argv: &[String]) -> Result<(), String> {
    match argv.get(1).map(String::as_str) {
        Some("append") => {
            if argv.len() != 5 {
                return Err(USAGE.to_string());
            }
            let count: usize = argv[4]
                .parse()
                .map_err(|_| format!("bad COUNT: {}", argv[4]))?;
            cmd_append(Path::new(&argv[2]), &argv[3], count)
        }
        Some("release-all") => {
            if argv.len() != 4 {
                return Err(USAGE.to_string());
            }
            let wins = cmd_release_all(Path::new(&argv[2]), &argv[3])?;
            println!("{wins}");
            Ok(())
        }
        _ => Err(USAGE.to_string()),
    }
}

/// Entry point: run one subcommand, mapping failures to exit 1 with stderr.
fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if let Err(e) = run(&argv) {
        eprintln!("log_stress: {e}");
        std::process::exit(1);
    }
}
