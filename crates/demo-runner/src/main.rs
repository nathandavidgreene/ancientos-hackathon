//! `demo-runner --data DIR <cmd>` — offline demo commands on synthetic fixtures.
//! `route` is the exception: it sends one real prompt to the configured
//! backend (sensitive -> local LLM, sanitized -> Muse) for live demos.
use ancientos_core::feedback::{ThresholdPolicy, Verdict};
use ancientos_core::log::DecisionLog;
use ancientos_router::dispatch::{dispatch, Backends};
use ancientos_router::Sensitivity;
use demo_runner::{demo_date, demo_now_secs, routing_demo, Workspace};
use std::path::Path;

const USAGE: &str = "usage: demo-runner --data DIR <sync FIXTURE | brief DAY | brief-rich DAY | feedback PICK VERDICT | thresholds | log | count FIXTURE... | routing | route [--sensitive|--sanitized] TEXT | vote-demo | delegate-demo POST_ID | try-demo | tamper-demo>";

/// Parse `route` args: `--sensitive TEXT` (default) or `--sanitized TEXT`.
/// A bare `route TEXT` is treated as sensitive (fail closed).
fn parse_route_args(args: &[String]) -> Result<(bool, String), String> {
    let (sensitive, text) = match args {
        [flag, text] if flag == "--sensitive" => (true, text.clone()),
        [flag, text] if flag == "--sanitized" => (false, text.clone()),
        [text] => (true, text.clone()),
        _ => return Err(USAGE.into()),
    };
    if text.trim().is_empty() {
        return Err("route text must be non-empty".to_string());
    }
    Ok((sensitive, text))
}

fn run(args: &[String]) -> Result<(), String> {
    let (dir, rest) = match args {
        [flag, dir, rest @ ..] if flag == "--data" => (dir, rest),
        _ => return Err(USAGE.into()),
    };
    let ws = Workspace::new(dir)?;
    let p = ThresholdPolicy::default();
    match rest {
        [c, f] if c == "sync" => println!(
            "synced {} new, 0 removed (removals never inferred) (synthetic fixture)",
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
        [c, d] if c == "brief-rich" => {
            let day: usize = d.parse().map_err(|_| "bad DAY".to_string())?;
            let rich = ws.brief_rich(day, p, &demo_date())?;
            println!("Daily brief — {} (day {day}):", rich.date);
            for pk in &rich.picks {
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
                println!("{}", bookmarks_agent::brief::render_blockquote(&pk.text));
            }
            if !rich.thread.is_empty() {
                println!("unrolled thread {}:", rich.thread[0].id);
                for t in &rich.thread {
                    println!("  {} {}", t.id, t.text);
                }
            }
            if !rich.media_markdown.is_empty() {
                println!("Media for {}:", rich.media_for);
                print!("{}", rich.media_markdown);
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
        [c, rest @ ..] if c == "count" => {
            if rest.is_empty() {
                return Err(USAGE.into());
            }
            let mut total = 0usize;
            let mut parts = Vec::new();
            for f in rest {
                let rows =
                    bookmarks_agent::fixture::load(Path::new(f)).map_err(|e| e.to_string())?;
                parts.push(rows.len());
                total += rows.len();
            }
            println!(
                "synthetic bookmarks: {total} ({})",
                parts
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(" + ")
            );
            for (f, n) in rest.iter().zip(parts) {
                let base = Path::new(f)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(f);
                println!("  {base}: {n}");
            }
        }
        [c] if c == "routing" => {
            for (cat, dest) in routing_demo()? {
                println!("  {cat:<18} -> {dest:?}");
            }
            println!("test sensitive_data_never_reaches_muse ... ok");
        }
        [c, rest @ ..] if c == "route" => {
            let (sensitive, text) = parse_route_args(rest)?;
            let local = ancientos_router::local_llm::LocalLlmClient::from_env();
            let spark = ancientos_router::muse::MuseClient::from_env();
            let wonder = ancientos_router::MockClient(ancientos_router::Destination::MuseWonder);
            let backends = Backends {
                local: &local,
                muse_spark: &spark,
                muse_wonder: &wonder,
            };
            let sens = if sensitive {
                Sensitivity::Sensitive
            } else {
                Sensitivity::Sanitized
            };
            let (dest, reply) =
                dispatch(&backends, sens, false, &text).map_err(|e| e.to_string())?;
            // Log the routing decision (metadata only, never the prompt text).
            DecisionLog::open(ws.log_path())
                .and_then(|mut l| {
                    l.append(
                        "route",
                        serde_json::json!({
                            "sensitivity": if sensitive { "sensitive" } else { "sanitized" },
                            "to": dest,
                            "prompt_chars": text.chars().count(),
                        }),
                    )
                })
                .map_err(|e| e.to_string())?;
            println!("routed to {dest:?}");
            println!("{reply}");
        }
        [c] if c == "vote-demo" => {
            let demo = ws.vote_demo(p)?;
            for v in &demo.votes {
                println!("{}: {} ({})", v.pick_id, v.verdict.as_str(), v.category);
                if !v.shift.is_empty() {
                    println!(
                        "  threshold shift: +{:?} -{:?}",
                        v.shift.added, v.shift.removed
                    );
                }
            }
            println!("auto-approved categories: {:?}", demo.thresholds);
            println!("Next brief (learned category is auto-approved):");
            for pk in &demo.final_picks {
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
        [c, id] if c == "delegate-demo" => {
            let demo = ws.delegate_demo(id, demo_now_secs())?;
            println!("unroll {id}:");
            for t in &demo.thread {
                println!("  {} {}", t.id, t.text);
            }
            println!("delegate {id} -> idea card:");
            for card in &demo.made {
                println!(
                    "  {} source={} idea={} mapping={} effort={:?} status=new",
                    card.id,
                    card.source_url,
                    card.idea,
                    card.ancientos_mapping.join(","),
                    card.effort
                );
            }
            println!("ideas (due for triage):");
            for card in &demo.ideas {
                println!(
                    "  {} {:?} {} {} {}",
                    card.id,
                    card.effort,
                    card.ancientos_mapping.join(","),
                    card.idea,
                    card.source_url
                );
            }
            println!("triage {} adopt -> backlog item:", demo.adopted_id);
            println!("  {}", demo.backlog_line);
        }
        [c] if c == "try-demo" => {
            let demo = ws.try_demo()?;
            for t in &demo.tagged {
                println!("  {} [TRY IT] ({})", t.post_id, t.reason);
                for (i, step) in t.steps.iter().enumerate() {
                    println!("  {}. {step}", i + 1);
                }
            }
            println!("Did you try it? (yes/no/later)");
            println!("try-pending:");
            for id in &demo.pending_before {
                println!("  {id}");
            }
            println!("tried {} yes", demo.tried_id);
            println!("recorded yes for {}", demo.tried_id);
            println!("try-pending:");
            for id in &demo.pending_after {
                println!("  {id}");
            }
        }
        [c] if c == "tamper-demo" => {
            let demo = ws.tamper_demo()?;
            println!("hash chain verified: {} entries", demo.verified);
            println!(
                "tampered copy: verification failed as expected (fails closed: {})",
                demo.failure
            );
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
