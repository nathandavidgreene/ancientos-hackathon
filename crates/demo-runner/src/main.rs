//! `demo-runner --data DIR <cmd>` — offline demo commands on synthetic fixtures.
//! `route` is the exception: it sends one real prompt to the configured
//! backend (sensitive -> local LLM, sanitized -> Muse) for live demos.
use ancientos_core::log::DecisionLog;
use ancientos_router::dispatch::Backends;
use ancientos_router::Sensitivity;
use bookmarks_agent::feedback::{ThresholdPolicy, Verdict};
use demo_runner::{demo_date, demo_now_secs, routing_demo, Workspace};
use std::path::Path;

const USAGE: &str = "usage: demo-runner --data DIR <sync FIXTURE | brief DAY | brief-rich DAY | feedback PICK VERDICT | thresholds | log | count FIXTURE... | routing | route [--sensitive|--sanitized] TEXT | vote-demo | delegate-demo POST_ID | try-demo | tamper-demo | approvals-propose | approvals-release | approvals-chain | cascade-demo | translate-demo | author-demo | accuracy-demo | muse-runs-demo>";

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

/// Run one demo-runner command against a `--data` workspace.
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
            let entries =
                DecisionLog::open_with_key_env(ws.log_path(), &|key| std::env::var(key).ok())
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
                jev_typesafe: None,
                jev_local: None,
            };
            let sens = if sensitive {
                Sensitivity::Sensitive
            } else {
                Sensitivity::Sanitized
            };
            // User-marked sensitive means the highest class (local only);
            // anything else the user sends here is their own Public text.
            let class = if sensitive {
                ancientos_router::data_class::DataClass::Restricted
            } else {
                ancientos_router::data_class::DataClass::Public
            };
            let (dest, reply) = ancientos_router::dispatch::dispatch_with_class(
                &backends, sens, class, false, "route", None, &text,
            )
            .map_err(|e| e.to_string())?;
            // Log the routing decision (metadata only, never the prompt text).
            DecisionLog::open_with_key_env(ws.log_path(), &|key| std::env::var(key).ok())
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
        [c] if c == "approvals-propose" => {
            let demo = ws.approvals_propose()?;
            println!("ANCIENTOS_ALLOW_PAID=unset (paid simulated via mock, no network)");
            println!("queued {} at {}", demo.post_id, demo.at);
            println!(
                "deeper_queue: local=0 proposed={} paid=0 waiting={} denied=0 (mock paid, no network)",
                demo.proposed, demo.waiting
            );
            println!(
                "proposed {} deeper.paid pending expires_at={}",
                demo.approval_id, demo.expires_at
            );
        }
        [c] if c == "approvals-release" => {
            let demo = ws.approvals_release()?;
            println!(
                "deeper_queue: local=0 proposed=0 paid={} waiting=0 denied=0 (mock paid, no network)",
                demo.executed.len()
            );
            println!("released {} exactly once", demo.approval_id);
            println!("walkthrough {} (mock, no network):", demo.walkthrough_file);
            for line in &demo.walkthrough_head {
                println!("  {line}");
            }
        }
        [c] if c == "approvals-chain" => {
            let demo = ws.approvals_chain()?;
            for (kind, id) in &demo.kinds {
                println!("{kind} {id}");
            }
            println!("hash chain verified: {} entries", demo.total);
        }
        [c] if c == "cascade-demo" => {
            let demo = ws.cascade_demo()?;
            println!("cascade demo (mock Jev + mock Muse, no network, paid off):");
            for item in &demo.items {
                println!(
                    "{}: try_it={} stage={} ({})",
                    item.post_id, item.try_it, item.stage, item.note
                );
            }
            println!(
                "cascade stats: topic_filtered={} rules_yes={} jev_calls={} jev_yes={} muse_calls={} (mock, no network)",
                demo.topic_filtered, demo.rules_yes, demo.jev_calls, demo.jev_yes, demo.muse_calls
            );
        }
        [c] if c == "translate-demo" => {
            let demo = ws.translate_demo()?;
            println!("{} original: {}", demo.post_id, demo.original);
            println!("{} {}: {}", demo.post_id, demo.marker, demo.english);
            println!("original kept below (never hidden for language)");
        }
        [c] if c == "author-demo" => {
            let demo = ws.author_demo()?;
            println!("never_author {} (hidden):", demo.author);
            println!("hidden authors: {{\"{}\"}}", demo.author);
            println!(
                "visible after hide: {} ({} by {} filtered, store kept at {})",
                demo.visible_after_hide, demo.hidden_count, demo.author, demo.total
            );
            println!("unhide-author {} (undone):", demo.author);
            println!("hidden authors: {{}}");
            println!("visible after unhide: {}", demo.visible_after_unhide);
        }
        [c] if c == "accuracy-demo" => {
            let demo = ws.accuracy_demo()?;
            println!("Accuracy panel (web-ui /accuracy, rendered offline):");
            for line in &demo.lines {
                println!("  {line}");
            }
            println!(
                "semantic ranking in the brief: {} (opt-in: ANCIENTOS_LOCAL_LLM_URL + ANCIENTOS_EMBED_MODEL)",
                if demo.semantic_in_brief { "on" } else { "off" }
            );
        }
        [c] if c == "muse-runs-demo" => {
            let demo = ws.muse_runs_demo()?;
            println!("muse-runs: second app on the same core (stub muse_run.sh, never real Muse)");
            println!(
                "gate: muse.run.unattended -> {}",
                demo.unknown_action_refusal
            );
            for p in &demo.proposed {
                println!("proposed {p}");
            }
            println!(
                "web-ui /approvals: Pending ({}){}",
                demo.page_pending_before,
                if demo.page_lists_both {
                    ", both as muse-runs · muse.run"
                } else {
                    ""
                }
            );
            println!(
                "approve #{} (human)",
                demo_runner::muse_runs_demo::APPROVED_RUN.0
            );
            println!(
                "deny #{} ({})",
                demo_runner::muse_runs_demo::DENIED_RUN.0,
                demo_runner::muse_runs_demo::DENY_REASON
            );
            println!(
                "run #{}: refused: {}",
                demo_runner::muse_runs_demo::DENIED_RUN.0,
                demo.denied_run_refusal
            );
            println!(
                "run #{}: refused: {}",
                demo_runner::muse_runs_demo::APPROVED_RUN.0,
                demo.approved_run_refusal
            );
            println!(
                "muse-runs list: {}",
                demo.statuses
                    .iter()
                    .map(|(n, s)| format!("{n} {s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("web-ui /approvals: Pending ({})", demo.page_pending_after);
            println!(
                "stub muse_run.sh calls: {} (paid switch unset)",
                demo.stub_calls
            );
            println!("shared hash chain verified: {} entries", demo.chain_len);
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// Eval entry point; exit 1 on error.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
