//! `demo-runner --data DIR <sync FIXTURE | brief DAY | feedback PICK VERDICT | thresholds | log>`
use ancientos_core::feedback::{ThresholdPolicy, Verdict};
use ancientos_core::log::DecisionLog;
use demo_runner::Workspace;
use std::path::Path;

const USAGE: &str = "usage: demo-runner --data DIR <sync FIXTURE | brief DAY | feedback PICK VERDICT | thresholds | log>";

fn run(args: &[String]) -> Result<(), String> {
    let (dir, rest) = match args {
        [flag, dir, rest @ ..] if flag == "--data" => (dir, rest),
        _ => return Err(USAGE.into()),
    };
    let ws = Workspace::new(dir)?;
    let p = ThresholdPolicy::default();
    match rest {
        [c, f] if c == "sync" => println!(
            "synced {} new bookmarks (synthetic fixture)",
            ws.sync(Path::new(f))?
        ),
        [c, d] if c == "brief" => {
            let day = d.parse().map_err(|_| "bad DAY".to_string())?;
            println!("Daily brief, day {day}:");
            for pk in ws.brief(day, p)? {
                println!(
                    "  {:<10} {:<18} -> {:<10} {}{}",
                    pk.pick_id,
                    pk.category,
                    pk.routed_to,
                    if pk.auto_approved {
                        "[AUTO-APPROVED] "
                    } else {
                        ""
                    },
                    pk.text
                );
            }
        }
        [c, id, v] if c == "feedback" => {
            let verdict: Verdict = v.parse()?;
            let s = ws.feedback(id, verdict, p)?;
            println!("{id}: {v}");
            if !s.is_empty() {
                println!("  threshold shift: +{:?} -{:?}", s.added, s.removed);
            }
        }
        [c] if c == "thresholds" => {
            println!("auto-approved categories: {:?}", ws.auto_approved(p)?)
        }
        [c] if c == "log" => {
            let entries = DecisionLog::open(ws.log_path())
                .and_then(|l| l.verify())
                .map_err(|e| e.to_string())?;
            for e in &entries {
                println!("  #{:<3} {:<16} {}", e.seq, e.kind, e.payload);
            }
            println!("hash chain verified: {} entries", entries.len());
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
