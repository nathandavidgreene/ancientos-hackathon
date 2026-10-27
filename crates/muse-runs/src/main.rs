//! `muse-runs [--log PATH] [--repo DIR] propose <log-no> <summary> <prompt-file>
//! | list | approve <id> | deny <id> [reason] | run <id>` — thin binary over
//! [`muse_runs::cli`]. The decision log defaults to the one bookmarks-agent and
//! web-ui use (`$DECISION_LOG_PATH`, else `$ANCIENTOS_HOME/decision_log.jsonl`,
//! else `~/.ancientos-hackathon/decision_log.jsonl`), so pending runs also show
//! on web-ui's `/approvals` page. `run` executes `<repo>/scripts/muse_run.sh`
//! only for an approved run and only with `ANCIENTOS_ALLOW_PAID=1`.

/// Run the CLI with the process args and environment; exit with its code.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).ok();
    match muse_runs::cli(&args, &env) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("muse-runs: {e}");
            std::process::exit(2);
        }
    }
}
