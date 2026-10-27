//! CLI: `login` | `sync [--max-pages N]` | `load [path]` | `auth-url`
//! | `brief [--fixture PATH] [--out PATH] [--offline] [--demo]`
//! | `rank [--top N]`
//! | `vote <post_id> <verdict> [--tag TAG]... [--fixture PATH]`
//! | `unhide-author <username>` | `hidden-authors`
//! | `delegate <post_id>` | `deeper <post_id> [--offline <fixture.jsonl>]`
//! | `deeper --later <post_id> --at <rfc3339>` | `deeper-queue`
//! | `ideas` | `triage <id> <adopt|park|drop>`
//! | `tried <post_id> <yes|no|later> [note]` | `try-pending`
//! | `digest [--now SECS] [--dry-run]`
//! | `approvals` | `approve <id> [--now <rfc3339>]`
//! | `deny <id> [reason] [--now <rfc3339>]`
//! | `run [--once] [--now <rfc3339>] [--offline <fixture>]`
//! | `wiki ingest <post_id>|--delegated` | `wiki query "<question>" [--file-back]`
//! | `wiki lint` | `wiki show <slug>` | `stats`.
//!
//! `login` runs the PKCE loopback flow and saves tokens; `sync` pulls new
//! bookmarks into the local JSONL store (paid, off by default). `brief`
//! merges the live store with a fixture and prints a daily brief. `rank`
//! prints the personal ranked list with reasons (trained on past verdicts,
//! inactive without enough labels). `vote`
//! records a feedback verdict (`never_author` hides every post by that
//! post's author); `unhide-author` undoes an author hide and
//! `hidden-authors` lists the currently hidden authors (read-only).
//! `delegate` summarizes one thread with Muse
//! Spark into inspiration.md (paid, off by default); `deeper` writes a
//! step-by-step walkthrough of one thread plus its quoted/linked posts to
//! deeper/<post_id>.md (one paid call at most, offline fixture supported).
//! `deeper --later` queues the walkthrough for a later time instead of
//! running it; `deeper-queue` lists queued/proposed/done jobs.
//! `ideas` lists idea cards due for triage; `triage` records an
//! adopt/park/drop decision.
//! `tried` records a try-it follow-up; `try-pending` lists tagged posts
//! without a yes/no follow-up yet. `digest` prints the weekly idea-card
//! triage digest plus "did you try it?" nudges, recording the showing
//! unless `--dry-run` or `--now` is given. `approvals` lists pending human
//! approvals; `approve`/`deny` record the decision (the CLI user is the
//! human) and print the outcome — nothing executes from the CLI. Both take
//! an optional `--now <rfc3339>` fixed clock for deterministic demos.
//! `run`
//! ticks the scheduled agents (`--once` runs every due agent once, exit
//! non-zero on any agent error; otherwise loops until Ctrl-C).
//! `wiki` compounds research bookmarks and idea cards into a cited wiki:
//! `ingest` writes an immutable raw source then updates pages (paid path
//! needs `ANCIENTOS_ALLOW_PAID=1` and Public sources; otherwise the local
//! model or the deterministic fallback compiles), `query` answers with
//! citations (`--file-back` files the answer as a new page), `lint`
//! proposes hygiene fixes as `wiki.edit` approvals (never applied), and
//! `show` prints one page. `stats` prints the same numbers as the web
//! UI's `/stats` dashboard as plain text, computed locally from the
//! decision log and the Muse Code run log (no network, no paid calls).
//! Tokens, codes, and verifiers are never printed.
use bookmarks_agent::{
    author_filter, brief, callback, daemon, deeper, delegate, digest, fixture, idea_card, ingest,
    merge, oauth, ranker, stats, token_store, try_it, vote, wiki, x_api,
};
use std::collections::HashMap;
use std::path::PathBuf;

const USAGE: &str = "usage: bookmarks-agent login | sync [--max-pages N] | load [path] | auth-url | brief [--fixture PATH] [--out PATH] [--offline] [--demo] | rank [--top N] | vote <post_id> <verdict> [--tag TAG]... [--fixture PATH] | unhide-author <username> | hidden-authors | delegate <post_id> | deeper <post_id> [--offline <fixture.jsonl>] | deeper --later <post_id> --at <rfc3339> | deeper-queue | ideas | triage <id> <adopt|park|drop> | tried <post_id> <yes|no|later> [note] | try-pending | digest [--now SECS] [--dry-run] | approvals | approve <id> [--now <rfc3339>] | deny <id> [reason] [--now <rfc3339>] | run [--once] [--now <rfc3339>] [--offline <fixture>] | wiki ingest <post_id> | wiki ingest --delegated | wiki query \"<question>\" [--file-back] | wiki lint | wiki show <slug> | stats";

/// X OAuth client id from the environment.
fn client_id() -> Result<String, String> {
    std::env::var("X_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "X_CLIENT_ID not set".to_string())
}

/// `auth-url`: print the X OAuth authorize URL.
fn cmd_auth_url() -> Result<(), String> {
    let id = client_id()?;
    let verifier = oauth::generate_verifier().map_err(|e| e.to_string())?;
    let state = oauth::generate_state().map_err(|e| e.to_string())?;
    println!("{}", oauth::authorize_url(&id, &verifier, &state));
    Ok(())
}

/// `login`: run the X OAuth PKCE flow and store tokens locally.
fn cmd_login() -> Result<(), String> {
    let id = client_id()?;
    let verifier = oauth::generate_verifier().map_err(|e| e.to_string())?;
    let state = oauth::generate_state().map_err(|e| e.to_string())?;
    println!("Open this URL in your browser:");
    println!("{}", oauth::authorize_url(&id, &verifier, &state));
    println!("Waiting for the browser callback (5 minutes)...");
    let code = callback::wait_for_code(&state).map_err(|e| e.to_string())?;
    let resp = oauth::exchange_code(&id, &code, &verifier, oauth::REDIRECT_URI)
        .map_err(|e| e.to_string())?;
    let now = token_store::unix_now().map_err(|e| e.to_string())?;
    let refresh = resp.refresh_token.ok_or_else(|| {
        "token response has no refresh_token (need offline.access scope)".to_string()
    })?;
    let tokens = token_store::StoredTokens::new(
        resp.access_token,
        refresh,
        now.saturating_add(resp.expires_in.unwrap_or(0)),
        resp.scope.unwrap_or_else(|| oauth::SCOPES.join(" ")),
    );
    let path = token_store::save(&tokens).map_err(|e| e.to_string())?;
    // Never print tokens here: only the path and scopes.
    println!("saved tokens to {}", path.display());
    println!("scopes: {}", tokens.scope);
    Ok(())
}

/// Parse `--max-pages N` for `sync` (fails closed on bad values).
fn parse_max_pages(args: &[String]) -> Result<usize, String> {
    if args.is_empty() {
        return Ok(1);
    }
    if args.len() == 2 && args[0] == "--max-pages" {
        return args[1]
            .parse::<usize>()
            .map_err(|_| "invalid --max-pages value".to_string())
            .and_then(|n| {
                if n == 0 {
                    Err("--max-pages must be at least 1".to_string())
                } else {
                    Ok(n)
                }
            });
    }
    if args.len() == 1 {
        if let Some(v) = args[0].strip_prefix("--max-pages=") {
            return v
                .parse::<usize>()
                .map_err(|_| "invalid --max-pages value".to_string())
                .and_then(|n| {
                    if n == 0 {
                        Err("--max-pages must be at least 1".to_string())
                    } else {
                        Ok(n)
                    }
                });
        }
    }
    Err(USAGE.to_string())
}

/// `sync`: incremental bookmark sync from the X API (paid, gated).
fn cmd_sync(args: &[String]) -> Result<(), String> {
    let max_pages = parse_max_pages(args)?;
    let id = client_id()?;
    let mut store = ingest::Store::load()?;
    let mut src = x_api::XApiSource::load_from_store(id)?;
    let report = ingest::sync(&mut store, &mut src, max_pages)?;
    let path = store.save()?;
    println!(
        "synced {} new bookmarks in {} pages to {}",
        report.new,
        report.pages,
        path.display()
    );
    Ok(())
}

/// `load`: load bookmarks from a JSONL fixture into the store.
fn cmd_load(args: &[String]) -> Result<(), String> {
    let path = args
        .first()
        .map_or("fixtures/sample_bookmarks.jsonl", String::as_str);
    match fixture::load(path) {
        Ok(rows) => {
            println!("loaded {} bookmarks", rows.len());
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Parse `brief` flags: (fixture, date, dry-run).
fn parse_brief_args(args: &[String]) -> Result<(Option<String>, Option<String>, bool), String> {
    let mut fixture: Option<String> = None;
    let mut out: Option<String> = None;
    let mut offline = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(v) = arg.strip_prefix("--fixture=") {
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            fixture = Some(v.to_string());
            i += 1;
        } else if arg == "--fixture" {
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            fixture = Some(v.clone());
            i += 1;
        } else if let Some(v) = arg.strip_prefix("--out=") {
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            out = Some(v.to_string());
            i += 1;
        } else if arg == "--out" {
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            out = Some(v.clone());
            i += 1;
        } else if arg == "--offline" || arg == "--demo" {
            offline = true;
            i += 1;
        } else {
            return Err(USAGE.to_string());
        }
    }
    Ok((fixture, out, offline))
}

/// `brief`: build today's brief.
fn cmd_brief(args: &[String]) -> Result<(), String> {
    let (fixture, out, offline) = parse_brief_args(args)?;
    let merged = merge::load_merged(fixture.as_deref())?;
    let client = ancientos_router::muse::MuseClient::from_env()
        .with_max_completion_tokens(700)
        .with_reasoning_effort("low");
    let log_path = brief::decision_log_path()?;
    let date = brief::current_date();
    let result = brief::run_offline_flag(&merged, &client, &log_path, &date, offline)?;
    print!("{}", result.markdown);
    if let Some(path) = out {
        std::fs::write(&path, &result.markdown)
            .map_err(|e| format!("brief write failed for {path}: {e}"))?;
    }
    Ok(())
}

/// `rank` limit: `--top N` / `--top=N` with N >= 1, else usage.
/// `Ok(None)` means no limit (print every ranked candidate).
fn parse_rank_args(args: &[String]) -> Result<Option<usize>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let value = if args.len() == 2 && args[0] == "--top" {
        args[1].as_str()
    } else if args.len() == 1 {
        args[0]
            .strip_prefix("--top=")
            .ok_or_else(|| USAGE.to_string())?
    } else {
        return Err(USAGE.to_string());
    };
    let n: usize = value.parse().map_err(|_| USAGE.to_string())?;
    if n == 0 {
        return Err(USAGE.to_string());
    }
    Ok(Some(n))
}

/// Print the personal ranked candidate list with reasons. Hidden posts and
/// hidden authors never surface. Local only: no model call, no log writes;
/// an inactive ranker prints today's order plus an inactive note.
fn cmd_rank(args: &[String]) -> Result<(), String> {
    let top = parse_rank_args(args)?;
    let merged = merge::load_merged(None)?;
    if merged.rows.is_empty() {
        return Err("no bookmarks to rank".to_string());
    }
    let log_path = brief::decision_log_path()?;
    let log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let records = bookmarks_agent::feedback::records_from_log(&log).map_err(|e| e.to_string())?;
    let hidden_posts = bookmarks_agent::feedback::hidden_posts(&records);
    let hidden_authors = bookmarks_agent::feedback::hidden_authors(&records);
    let rows: Vec<fixture::Bookmark> = merged
        .rows
        .iter()
        .filter(|b| !hidden_posts.contains(&b.post_id))
        .filter(|b| !author_filter::is_hidden(b, &hidden_authors))
        .cloned()
        .collect();
    if rows.is_empty() {
        return Err("no bookmarks to rank".to_string());
    }
    let events = ranker::events_from_log(&log, &merged.rows)?;
    let trained = ranker::Ranker::train(&events, ranker::min_records_from_env());
    let (scores, semantic_on) = if trained.is_active() {
        let cache = log_path.with_file_name(bookmarks_agent::semantic::CACHE_FILE_NAME);
        bookmarks_agent::semantic::personal_scores(&trained, &events, &merged.rows, &cache)
    } else {
        (HashMap::new(), false)
    };
    let candidates = if trained.is_active() {
        brief::select_candidates_ranked(&rows, &merged.live_ids, &scores)
    } else {
        brief::select_candidates(&rows, &merged.live_ids)
    };
    let by_id: HashMap<&str, &fixture::Bookmark> = merged
        .rows
        .iter()
        .map(|b| (b.post_id.as_str(), b))
        .collect();
    let n = top.unwrap_or(candidates.len());
    for c in candidates.iter().take(n) {
        if trained.is_active() {
            let score = by_id
                .get(c.post_id.as_str())
                .map(|b| trained.score(b))
                .unwrap_or(ranker::RankScore {
                    score: 0.0,
                    top_reasons: Vec::new(),
                });
            let shown = scores.get(&c.post_id).copied().unwrap_or(score.score);
            println!(
                "{}. {} [{}] score {:.3}",
                c.index, c.post_id, c.topic, shown
            );
            for reason in &score.top_reasons {
                println!("   - {reason}");
            }
        } else {
            println!("{}. {} [{}]", c.index, c.post_id, c.topic);
        }
    }
    if semantic_on {
        println!("semantic ranking: on (local embeddings blended with votes)");
    }
    if !trained.is_active() {
        println!(
            "ranker inactive: {} labeled records (min {})",
            trained.n_labeled(),
            trained.min_records()
        );
    }
    Ok(())
}

/// Parse `vote` arguments: (post id, verdict, reasons, note).
fn parse_vote_args(
    args: &[String],
) -> Result<(String, String, Vec<String>, Option<String>), String> {
    if args.len() < 2 {
        return Err(USAGE.to_string());
    }
    let post_id = args[0].clone();
    let verdict = args[1].clone();
    if post_id.is_empty() || verdict.is_empty() {
        return Err(USAGE.to_string());
    }
    let mut tags: Vec<String> = Vec::new();
    let mut fixture: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(v) = arg.strip_prefix("--tag=") {
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            tags.push(v.to_string());
            i += 1;
        } else if arg == "--tag" {
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            tags.push(v.clone());
            i += 1;
        } else if let Some(v) = arg.strip_prefix("--fixture=") {
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            fixture = Some(v.to_string());
            i += 1;
        } else if arg == "--fixture" {
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            fixture = Some(v.clone());
            i += 1;
        } else {
            return Err(USAGE.to_string());
        }
    }
    Ok((post_id, verdict, tags, fixture))
}

/// `vote`: record a feedback vote on a pick.
fn cmd_vote(args: &[String]) -> Result<(), String> {
    let (post_id, verdict_s, tags, fixture) = parse_vote_args(args)?;
    let verdict: bookmarks_agent::feedback::Verdict = verdict_s.parse().map_err(|e: String| e)?;
    let log_path = brief::decision_log_path()?;
    let res = vote::run_vote(&post_id, verdict, &tags, fixture.as_deref(), &log_path)?;
    println!(
        "recorded {} for {} ({})",
        res.verdict.as_str(),
        res.post_id,
        res.category
    );
    if let Some(author) = &res.author {
        println!("hidden author: {author}");
    }
    if res.queued_delegate {
        println!("delegate queued for {}", res.post_id);
    }
    Ok(())
}

/// `unhide-author`: remove an author from the never-this-author list.
fn cmd_unhide_author(args: &[String]) -> Result<(), String> {
    if args.len() != 1 || args[0].trim().is_empty() {
        return Err(USAGE.to_string());
    }
    let log_path = brief::decision_log_path()?;
    let unhidden = vote::run_unhide_author(&args[0], &log_path)?;
    println!("unhidden author: {unhidden}");
    Ok(())
}

/// `hidden-authors`: list hidden authors.
fn cmd_hidden_authors(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let log_path = brief::decision_log_path()?;
    for author in vote::list_hidden_authors(&log_path)? {
        println!("{author}");
    }
    Ok(())
}

/// Parsed `deeper` args: immediate run (`post_id` plus optional
/// `--offline` fixture) or a queued job (`--later` plus `--at`).
#[derive(Debug)]
struct DeeperArgs {
    post_id: String,
    offline: Option<String>,
    later_at: Option<u64>,
}

/// `deeper <post_id> [--offline PATH]` runs now;
/// `deeper --later <post_id> --at <rfc3339>` (or
/// `deeper <post_id> --later --at <rfc3339>`) queues instead. `--at`
/// without `--later`, `--later` without `--at`, `--later` with `--offline`,
/// duplicates and unknown args are errors; unparseable `--at` values are
/// refused (fail closed).
fn parse_deeper_args(args: &[String]) -> Result<DeeperArgs, String> {
    let mut post_id: Option<String> = None;
    let mut offline: Option<String> = None;
    let mut offline_seen = false;
    let mut later = false;
    let mut later_seen = false;
    let mut at: Option<u64> = None;
    let mut at_seen = false;
    let mut set_post_id = |v: &str| -> Result<(), String> {
        if v.is_empty() || post_id.is_some() {
            return Err(USAGE.to_string());
        }
        post_id = Some(v.to_string());
        Ok(())
    };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(v) = arg.strip_prefix("--offline=") {
            if offline_seen || v.is_empty() {
                return Err(USAGE.to_string());
            }
            offline_seen = true;
            offline = Some(v.to_string());
            i += 1;
        } else if arg == "--offline" {
            if offline_seen {
                return Err(USAGE.to_string());
            }
            offline_seen = true;
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            offline = Some(v.clone());
            i += 1;
        } else if let Some(v) = arg.strip_prefix("--later=") {
            if later_seen {
                return Err(USAGE.to_string());
            }
            later_seen = true;
            later = true;
            set_post_id(v)?;
            i += 1;
        } else if arg == "--later" {
            if later_seen {
                return Err(USAGE.to_string());
            }
            later_seen = true;
            later = true;
            // `--later <post_id>`: consume the id when the next token is a
            // bare value; otherwise `--later` is a bare flag.
            if let Some(next) = args.get(i + 1) {
                if !next.starts_with("--") {
                    set_post_id(next)?;
                    i += 1;
                }
            }
            i += 1;
        } else if let Some(v) = arg.strip_prefix("--at=") {
            if at_seen {
                return Err(USAGE.to_string());
            }
            at_seen = true;
            at = Some(
                idea_card::parse_rfc3339(v)
                    .ok_or_else(|| "invalid --at value (want RFC 3339)".to_string())?,
            );
            i += 1;
        } else if arg == "--at" {
            if at_seen {
                return Err(USAGE.to_string());
            }
            at_seen = true;
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            at = Some(
                idea_card::parse_rfc3339(v)
                    .ok_or_else(|| "invalid --at value (want RFC 3339)".to_string())?,
            );
            i += 1;
        } else if arg.starts_with("--") {
            return Err(USAGE.to_string());
        } else {
            set_post_id(arg)?;
            i += 1;
        }
    }
    let post_id = post_id.ok_or_else(|| USAGE.to_string())?;
    if later {
        if offline.is_some() {
            return Err("--later cannot be combined with --offline".to_string());
        }
        match at {
            Some(t) => Ok(DeeperArgs {
                post_id,
                offline: None,
                later_at: Some(t),
            }),
            None => Err("--later needs --at <rfc3339>".to_string()),
        }
    } else {
        if at.is_some() {
            return Err(USAGE.to_string());
        }
        Ok(DeeperArgs {
            post_id,
            offline,
            later_at: None,
        })
    }
}

/// `deeper`: read a post's thread and links in more depth.
fn cmd_deeper(args: &[String]) -> Result<(), String> {
    let parsed = parse_deeper_args(args)?;
    if let Some(at) = parsed.later_at {
        let log_path = brief::decision_log_path()?;
        deeper::queue_later_at_path(&parsed.post_id, at, idea_card::now_secs(), &log_path)?;
        println!("queued {} at {at}", parsed.post_id);
        return Ok(());
    }
    let res = deeper::run(&parsed.post_id, parsed.offline.as_deref())?;
    print!("{}", res.output);
    if !res.output.ends_with('\n') {
        println!();
    }
    println!("\nSaved: {}", res.path.display());
    if res.mode == "paid" {
        println!("Tokens: {}", res.usage.unwrap_or_default());
    }
    Ok(())
}

/// `deeper-queue`: list or work the queue of deeper requests.
fn cmd_deeper_queue(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let log_path = brief::decision_log_path()?;
    let log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    let jobs = deeper::list_queue(&entries);
    if jobs.is_empty() {
        println!("no queued go-deeper jobs");
        return Ok(());
    }
    for job in &jobs {
        println!(
            "{} at={} status={}",
            job.post_id,
            job.at,
            job.status.as_str()
        );
    }
    Ok(())
}

/// `delegate`: hand an idea off to another agent (proposal only).
fn cmd_delegate(args: &[String]) -> Result<(), String> {
    if args.len() != 1 || args[0].is_empty() {
        return Err(USAGE.to_string());
    }
    let res = delegate::run(&args[0])?;
    print!("{}", res.output);
    if !res.output.ends_with('\n') {
        println!();
    }
    println!("\nTokens: {}", res.usage.unwrap_or_default());
    println!("Idea cards: {}", res.idea_cards);
    Ok(())
}

/// Format one idea card as a single summary line.
fn format_idea_line(card: &idea_card::IdeaCard) -> String {
    format!(
        "{} {:?} {} {} {}",
        card.id,
        card.effort,
        card.ancientos_mapping.join(","),
        card.idea,
        card.source_url
    )
}

/// Stored bookmarks for author filtering of id/card surfaces. A missing
/// or unreadable store means every source is unknown (kept), so these
/// read-only listings never fail just because the store is absent. Cards are
/// never hidden for language.
fn bookmarks_for_filtering() -> Vec<fixture::Bookmark> {
    ingest::Store::load()
        .map(|s| s.by_id.into_values().collect())
        .unwrap_or_default()
}

/// `ideas`: list idea cards.
fn cmd_ideas(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let path = idea_card::idea_cards_path()?;
    let cards = idea_card::list_due_for_triage(&path, idea_card::now_secs())?;
    let bookmarks = bookmarks_for_filtering();
    let hidden = vote::list_hidden_authors(&brief::decision_log_path()?)?;
    for card in &cards {
        // Cards by a hidden author are skipped silently. Cards are never
        // hidden for language.
        if author_filter::source_post_is_hidden(&card.source_url, &bookmarks, &hidden) == Some(true)
        {
            continue;
        }
        println!("{}", format_idea_line(card));
    }
    Ok(())
}

/// Parse a triage verb (`adopt`, `park`, `drop`).
fn parse_triage_decision(s: &str) -> Result<idea_card::Triage, String> {
    match s {
        "adopt" => Ok(idea_card::Triage::Adopt),
        "park" => Ok(idea_card::Triage::Park),
        "drop" => Ok(idea_card::Triage::Drop),
        _ => Err(USAGE.to_string()),
    }
}

/// Backlog file for adopted ideas: `docs/backlog.md` unless overridden.
fn backlog_path_for(value: &str) -> PathBuf {
    if value.is_empty() {
        PathBuf::from("docs/backlog.md")
    } else {
        PathBuf::from(value)
    }
}

/// Backlog file for adopted ideas, from `IDEA_BACKLOG_PATH` or the default.
fn backlog_path() -> PathBuf {
    backlog_path_for(&std::env::var("IDEA_BACKLOG_PATH").unwrap_or_default())
}

/// Lowercase status spelling, matching serde.
fn status_str(status: idea_card::Status) -> &'static str {
    match status {
        idea_card::Status::New => "new",
        idea_card::Status::Adopted => "adopted",
        idea_card::Status::Parked => "parked",
        idea_card::Status::Dropped => "dropped",
    }
}

/// `triage`: apply a triage decision to an idea card.
fn cmd_triage(args: &[String]) -> Result<(), String> {
    if args.len() != 2 || args[0].is_empty() || args[1].is_empty() {
        return Err(USAGE.to_string());
    }
    let decision = parse_triage_decision(&args[1])?;
    let path = idea_card::idea_cards_path()?;
    let card = idea_card::triage(
        &path,
        &backlog_path(),
        &args[0],
        decision,
        idea_card::now_secs(),
    )?;
    // An adopt queues a wiki ingest (logged); the adoption itself already
    // landed, so a queue failure is reported honestly as both.
    if decision == idea_card::Triage::Adopt {
        let post_id = wiki::post_id_from_url(&card.source_url).unwrap_or_else(|| card.id.clone());
        let log_path = brief::decision_log_path()?;
        if let Err(e) = wiki::queue_ingest(
            &log_path,
            wiki::QUEUE_REASON_ADOPT,
            &card.id,
            &post_id,
            idea_card::now_secs(),
        ) {
            return Err(format!(
                "adopted {} but failed to queue wiki ingest: {e}",
                card.id
            ));
        }
    }
    println!("{}", status_str(card.status));
    Ok(())
}

/// `tried`: mark a try-it item as tried or not.
fn cmd_tried(args: &[String]) -> Result<(), String> {
    if args.len() < 2 || args[0].is_empty() {
        return Err(USAGE.to_string());
    }
    let tried: try_it::Tried = args[1].parse()?;
    let note = if args.len() > 2 {
        let joined = args[2..].join(" ");
        if joined.trim().is_empty() {
            None
        } else {
            Some(joined)
        }
    } else {
        None
    };
    let log_path = brief::decision_log_path()?;
    try_it::run_tried(&args[0], tried, note.as_deref(), &log_path)?;
    println!("recorded {} for {}", tried.as_str(), args[0]);
    Ok(())
}

/// `try-pending`: list try-it items still waiting for a result.
fn cmd_try_pending(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let log_path = brief::decision_log_path()?;
    let log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let pending = try_it::pending_followups(&log);
    let records = bookmarks_agent::feedback::records_from_log(&log).map_err(|e| e.to_string())?;
    let hidden = bookmarks_agent::feedback::hidden_authors(&records);
    let bookmarks = bookmarks_for_filtering();
    // Posts by a hidden author are skipped silently. Pending posts are never
    // hidden for language.
    for id in author_filter::retain_not_hidden(&pending, &bookmarks, &hidden) {
        println!("{id}");
    }
    Ok(())
}

/// Parsed `digest` args: `--now` override (unix secs) plus `--dry-run`.
#[derive(Debug)]
struct DigestArgs {
    now: Option<u64>,
    dry_run: bool,
}

/// `--now` override (unix secs) for tests/demo plus `--dry-run`, in any
/// order; `now: None` means the system clock. Duplicate `--now`/`--dry-run`
/// or unknown args are usage errors.
fn parse_digest_args(args: &[String]) -> Result<DigestArgs, String> {
    let mut now: Option<u64> = None;
    let mut now_seen = false;
    let mut dry_run = false;
    let mut dry_seen = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(v) = arg.strip_prefix("--now=") {
            if now_seen {
                return Err(USAGE.to_string());
            }
            now_seen = true;
            now = Some(
                v.parse::<u64>()
                    .map_err(|_| "invalid --now value (want unix secs)".to_string())?,
            );
            i += 1;
        } else if arg == "--now" {
            if now_seen {
                return Err(USAGE.to_string());
            }
            now_seen = true;
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            now = Some(
                v.parse::<u64>()
                    .map_err(|_| "invalid --now value (want unix secs)".to_string())?,
            );
            i += 1;
        } else if arg == "--dry-run" {
            if dry_seen {
                return Err(USAGE.to_string());
            }
            dry_seen = true;
            dry_run = true;
            i += 1;
        } else {
            return Err(USAGE.to_string());
        }
    }
    Ok(DigestArgs { now, dry_run })
}

/// `digest`: write the periodic digest.
fn cmd_digest(args: &[String]) -> Result<(), String> {
    let parsed = parse_digest_args(args)?;
    let now_override = parsed.now.is_some();
    let now = match parsed.now {
        Some(n) => n,
        None => idea_card::now_secs(),
    };
    let cards = idea_card::load(&idea_card::idea_cards_path()?)?;
    let log_path = brief::decision_log_path()?;
    let mut log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    let pending = try_it::pending_followups(&log);
    let bookmarks = bookmarks_for_filtering();
    let d = digest::weekly_digest_for_bookmarks(&cards, &pending, &entries, now, &bookmarks);
    print!("{}", digest::render(&d));
    if digest::should_record(parsed.dry_run, now_override) {
        digest::record_shown(&mut log, now)?;
    }
    Ok(())
}

/// Decider recorded for CLI approvals: the human at the keyboard.
const CLI_DECIDER: &str = "cli";

/// One-line rendering of a summary for the `approvals` list.
fn one_line_summary(summary: &str) -> String {
    summary
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `approvals`: list pending and decided approvals.
fn cmd_approvals(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let log_path = brief::decision_log_path()?;
    let log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let now = idea_card::now_secs();
    let list = ancientos_core::approvals::pending(&log, now).map_err(|e| e.to_string())?;
    if list.is_empty() {
        println!("no pending approvals");
        return Ok(());
    }
    for a in &list {
        println!(
            "{} {} {} expires_at={} {}",
            a.request.id,
            a.request.agent,
            a.request.action,
            a.request.expires_at,
            one_line_summary(&a.request.summary),
        );
    }
    Ok(())
}

/// Payload hash read from the stored request: the CLI caller never supplies
/// it, so the decision always binds to the request as logged.
fn stored_hash(log: &ancientos_core::log::DecisionLog, id: &str) -> Result<String, String> {
    ancientos_core::approvals::get(log, id)
        .map(|a| a.request.payload_hash)
        .map_err(|e| e.to_string())
}

/// Parsed `approve`/`deny` args: the approval id, an optional fixed RFC 3339
/// clock, and (deny only) the trailing reason words.
#[derive(Debug)]
struct DecideArgs {
    id: String,
    reason: String,
    now: Option<u64>,
}

/// `approve <id> [--now <rfc3339>]` and `deny <id> [reason] [--now
/// <rfc3339>]` (`--now=` form too). `--now` must come last when present;
/// duplicates, a missing value, an unparseable value, an empty id, or a
/// reason word that looks like `--now` are errors (fail closed).
fn parse_decide_args(args: &[String], is_deny: bool) -> Result<DecideArgs, String> {
    if args.is_empty() || args[0].is_empty() {
        return Err(USAGE.to_string());
    }
    let id = args[0].clone();
    let mut rest: &[String] = &args[1..];
    let mut now: Option<u64> = None;
    // Peel one trailing --now flag when present.
    if let Some(last) = rest.last() {
        if let Some(v) = last.strip_prefix("--now=") {
            now = Some(
                idea_card::parse_rfc3339(v)
                    .ok_or_else(|| "invalid --now value (want RFC 3339)".to_string())?,
            );
            rest = &rest[..rest.len() - 1];
        } else if last == "--now" {
            return Err(USAGE.to_string());
        } else if rest.len() >= 2 && rest[rest.len() - 2] == "--now" {
            let v = last.clone();
            now = Some(
                idea_card::parse_rfc3339(&v)
                    .ok_or_else(|| "invalid --now value (want RFC 3339)".to_string())?,
            );
            rest = &rest[..rest.len() - 2];
        }
    }
    // Any remaining --now is a duplicate or a misplaced flag.
    if rest.iter().any(|a| a == "--now" || a.starts_with("--now=")) {
        return Err(USAGE.to_string());
    }
    if !is_deny && !rest.is_empty() {
        return Err(USAGE.to_string());
    }
    Ok(DecideArgs {
        id,
        reason: rest.join(" "),
        now,
    })
}

/// `approve`: approve a pending approval by id.
fn cmd_approve(args: &[String]) -> Result<(), String> {
    let parsed = parse_decide_args(args, false)?;
    let log_path = brief::decision_log_path()?;
    let mut log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let hash = stored_hash(&log, &parsed.id)?;
    let now = parsed.now.unwrap_or_else(idea_card::now_secs);
    let token = ancientos_core::approvals::approve(&mut log, &parsed.id, &hash, CLI_DECIDER, now)
        .map_err(|e| e.to_string())?;
    // The release token is single-process by design (not Clone, not
    // Serialize): approving records the decision only. Nothing executes.
    drop(token);
    println!("approved {} (release-ready)", parsed.id);
    Ok(())
}

/// `deny`: deny a pending approval by id.
fn cmd_deny(args: &[String]) -> Result<(), String> {
    let parsed = parse_decide_args(args, true)?;
    let log_path = brief::decision_log_path()?;
    let mut log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let hash = stored_hash(&log, &parsed.id)?;
    let now = parsed.now.unwrap_or_else(idea_card::now_secs);
    ancientos_core::approvals::deny(
        &mut log,
        &parsed.id,
        &hash,
        CLI_DECIDER,
        &parsed.reason,
        now,
    )
    .map_err(|e| e.to_string())?;
    println!("denied {}", parsed.id);
    Ok(())
}

/// Parsed `run` args: `--once` plus a fixed RFC 3339 clock and an
/// offline fixture for the brief and local go-deeper runs.
#[derive(Debug)]
struct RunArgs {
    once: bool,
    now: Option<u64>,
    offline: Option<String>,
}

/// `run [--once] [--now <rfc3339>] [--offline <fixture>]`, in any order
/// (`--now=`/`--offline=` forms too). Duplicates and unknown args are
/// usage errors; an unparseable `--now` is refused (fail closed).
fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let mut once = false;
    let mut once_seen = false;
    let mut now: Option<u64> = None;
    let mut now_seen = false;
    let mut offline: Option<String> = None;
    let mut offline_seen = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--once" {
            if once_seen {
                return Err(USAGE.to_string());
            }
            once_seen = true;
            once = true;
            i += 1;
        } else if let Some(v) = arg.strip_prefix("--now=") {
            if now_seen {
                return Err(USAGE.to_string());
            }
            now_seen = true;
            now = Some(
                idea_card::parse_rfc3339(v)
                    .ok_or_else(|| "invalid --now value (want RFC 3339)".to_string())?,
            );
            i += 1;
        } else if arg == "--now" {
            if now_seen {
                return Err(USAGE.to_string());
            }
            now_seen = true;
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            now = Some(
                idea_card::parse_rfc3339(v)
                    .ok_or_else(|| "invalid --now value (want RFC 3339)".to_string())?,
            );
            i += 1;
        } else if let Some(v) = arg.strip_prefix("--offline=") {
            if offline_seen || v.is_empty() {
                return Err(USAGE.to_string());
            }
            offline_seen = true;
            offline = Some(v.to_string());
            i += 1;
        } else if arg == "--offline" {
            if offline_seen {
                return Err(USAGE.to_string());
            }
            offline_seen = true;
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            offline = Some(v.clone());
            i += 1;
        } else {
            return Err(USAGE.to_string());
        }
    }
    Ok(RunArgs { once, now, offline })
}

/// `run`: start the daemon with its scheduled agents.
fn cmd_run(args: &[String]) -> Result<(), String> {
    let parsed = parse_run_args(args)?;
    if let Some(fixture) = &parsed.offline {
        if !std::path::Path::new(fixture).exists() {
            return Err(format!("offline fixture not found: {fixture}"));
        }
    }
    let ctx = daemon::AgentCtx {
        log_path: brief::decision_log_path()?,
        cards_path: idea_card::idea_cards_path()?,
        deeper_dir: deeper::deeper_dir()?,
        live_store: ingest::store_path()?,
        fixture: parsed.offline.map(PathBuf::from),
        now_override: parsed.now,
    };
    let config = daemon::DaemonConfig::from_paths(ctx)?;
    if parsed.once {
        let report = daemon::run_once(&config);
        if report.ok() {
            return Ok(());
        }
        let failed: Vec<&str> = report
            .outcomes
            .iter()
            .filter(|o| o.result.is_err() || o.tick_logged.is_err())
            .map(|o| o.agent.as_str())
            .collect();
        return Err(format!(
            "{} agent(s) failed: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
    println!("daemon running; press Ctrl-C to stop");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let reports = daemon::run_loop(&config, &stop);
    // The loop only returns on the stop flag or DAEMON_MAX_TICKS (tests).
    if reports.iter().all(|r| r.ok()) {
        Ok(())
    } else {
        Err("daemon tick(s) failed (see daemon.tick entries)".to_string())
    }
}

/// `wiki ingest`: add sources to the wiki raw store.
fn cmd_wiki_ingest(args: &[String]) -> Result<(), String> {
    if args.len() == 1 && args[0] == "--delegated" {
        let reports = wiki::ingest_delegated()?;
        for report in &reports {
            println!(
                "{}: {} pages via {} ({})",
                report.raw.id,
                report.pages.len(),
                report.mode,
                report.pages.join(", ")
            );
        }
        return Ok(());
    }
    if args.len() == 1 && !args[0].is_empty() && !args[0].starts_with("--") {
        let report = wiki::ingest_post(&args[0])?;
        println!(
            "{}: {} pages via {} ({})",
            report.raw.id,
            report.pages.len(),
            report.mode,
            report.pages.join(", ")
        );
        if !report.rejected.is_empty() {
            for reason in &report.rejected {
                println!("note: {reason}");
            }
        }
        return Ok(());
    }
    Err(USAGE.to_string())
}

/// `wiki query`: answer a question from cited wiki pages.
fn cmd_wiki_query(args: &[String]) -> Result<(), String> {
    let (question, file_back) = match args {
        [q] if !q.trim().is_empty() => (q.as_str(), false),
        [q, flag] if !q.trim().is_empty() && flag == "--file-back" => (q.as_str(), true),
        _ => return Err(USAGE.to_string()),
    };
    let log_path = brief::decision_log_path()?;
    let wiki_dir = wiki::wiki_dir()?;
    if file_back {
        let answer = wiki::query_file_back(question, &wiki_dir, &log_path, idea_card::now_secs())?;
        print!("{}", answer.text);
        if let Some(slug) = answer.filed {
            println!("filed as [[{slug}]]");
        }
    } else {
        let answer = wiki::query(question, &wiki_dir)?;
        print!("{}", answer.text);
    }
    Ok(())
}

/// `wiki lint`: check the wiki and propose fixes.
fn cmd_wiki_lint(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let log_path = brief::decision_log_path()?;
    let wiki_dir = wiki::wiki_dir()?;
    let report = wiki::lint_and_propose(&wiki_dir, &log_path, idea_card::now_secs())?;
    print!("{}", wiki::render_lint_report(&report));
    Ok(())
}

/// `wiki show`: print one wiki page.
fn cmd_wiki_show(args: &[String]) -> Result<(), String> {
    if args.len() != 1 || args[0].is_empty() {
        return Err(USAGE.to_string());
    }
    let wiki_dir = wiki::wiki_dir()?;
    print!("{}", wiki::read_page(&wiki_dir, &args[0])?);
    Ok(())
}

/// `wiki`: dispatch to the wiki subcommands.
fn cmd_wiki(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("ingest") => cmd_wiki_ingest(&args[1..]),
        Some("query") => cmd_wiki_query(&args[1..]),
        Some("lint") => cmd_wiki_lint(&args[1..]),
        Some("show") => cmd_wiki_show(&args[1..]),
        _ => Err(USAGE.to_string()),
    }
}

/// Print the local stats dashboard as plain text: the decision-log chain
/// status plus entry counts, approvals, verdicts, try-it, cascade stages,
/// daemon ticks and Muse Code runs, all computed from local files (no
/// network, no paid calls). Args: `args` must be empty (any arg is a usage
/// error); the decision log comes from `brief::decision_log_path`, the run
/// log from `stats::muse_log_path_from_env`, and the clock is the system
/// time. Returns `Ok` after printing, or a usage/decision-log-path error
/// string. A broken chain prints `withheld` sections (fail closed) and a
/// missing run log prints `unavailable`; neither is an error.
fn cmd_stats(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let log_path = brief::decision_log_path()?;
    let muse_path = stats::muse_log_path_from_env();
    let now = idea_card::now_secs();
    print!(
        "{}",
        stats::render_text(&stats::collect(&log_path, &muse_path, now))
    );
    Ok(())
}

/// CLI entry point: dispatch `argv[1]` to one subcommand handler and exit
/// non-zero with `error: ...` on failure. Args: none (reads process args
/// and env); returns nothing; failure modes are whatever the chosen
/// subcommand reports.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("login") if args.len() == 1 => cmd_login(),
        Some("sync") => cmd_sync(&args[1..]),
        Some("load") => cmd_load(&args[1..]),
        Some("auth-url") if args.len() == 1 => cmd_auth_url(),
        Some("brief") => cmd_brief(&args[1..]),
        Some("rank") => cmd_rank(&args[1..]),
        Some("vote") => cmd_vote(&args[1..]),
        Some("unhide-author") => cmd_unhide_author(&args[1..]),
        Some("hidden-authors") => cmd_hidden_authors(&args[1..]),
        Some("delegate") => cmd_delegate(&args[1..]),
        Some("deeper") => cmd_deeper(&args[1..]),
        Some("deeper-queue") => cmd_deeper_queue(&args[1..]),
        Some("ideas") => cmd_ideas(&args[1..]),
        Some("triage") => cmd_triage(&args[1..]),
        Some("tried") => cmd_tried(&args[1..]),
        Some("try-pending") => cmd_try_pending(&args[1..]),
        Some("digest") => cmd_digest(&args[1..]),
        Some("approvals") => cmd_approvals(&args[1..]),
        Some("approve") => cmd_approve(&args[1..]),
        Some("deny") => cmd_deny(&args[1..]),
        Some("run") => cmd_run(&args[1..]),
        Some("wiki") => cmd_wiki(&args[1..]),
        Some("stats") => cmd_stats(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: sample idea card for formatting tests.
    fn sample_card() -> idea_card::IdeaCard {
        idea_card::IdeaCard {
            id: "abc123".to_string(),
            source_url: "https://x.com/i/status/1".to_string(),
            idea: "Drag a card to approve".to_string(),
            ancientos_mapping: vec!["gate".to_string()],
            effort: idea_card::Effort::S,
            status: idea_card::Status::New,
            created_at: 100,
            park_until: None,
        }
    }

    #[test]
    fn idea_line_holds_all_fields() {
        let line = format_idea_line(&sample_card());
        for field in [
            "abc123",
            "S",
            "gate",
            "Drag a card to approve",
            "https://x.com/i/status/1",
        ] {
            assert!(line.contains(field), "missing {field}: {line}");
        }
        assert!(!line.contains('\n'), "single line: {line}");
    }

    #[test]
    fn triage_decision_parses_three_verbs() {
        assert_eq!(
            parse_triage_decision("adopt").unwrap(),
            idea_card::Triage::Adopt
        );
        assert_eq!(
            parse_triage_decision("park").unwrap(),
            idea_card::Triage::Park
        );
        assert_eq!(
            parse_triage_decision("drop").unwrap(),
            idea_card::Triage::Drop
        );
        assert!(parse_triage_decision("Adopt").is_err());
        assert!(parse_triage_decision("keep").is_err());
        assert!(parse_triage_decision("").is_err());
    }

    #[test]
    fn backlog_path_defaults_and_overrides() {
        assert_eq!(backlog_path_for(""), PathBuf::from("docs/backlog.md"));
        assert_eq!(
            backlog_path_for("/tmp/custom.md"),
            PathBuf::from("/tmp/custom.md")
        );
    }

    #[test]
    fn status_prints_lowercase() {
        assert_eq!(status_str(idea_card::Status::Adopted), "adopted");
        assert_eq!(status_str(idea_card::Status::Parked), "parked");
        assert_eq!(status_str(idea_card::Status::Dropped), "dropped");
        assert_eq!(status_str(idea_card::Status::New), "new");
    }

    #[test]
    fn digest_now_parses_space_and_equals_forms() {
        let none: Vec<String> = vec![];
        let parsed = parse_digest_args(&none).unwrap();
        assert_eq!(parsed.now, None);
        assert!(!parsed.dry_run);
        let parsed = parse_digest_args(&["--now".into(), "123".into()]).unwrap();
        assert_eq!(parsed.now, Some(123));
        assert!(!parsed.dry_run);
        let parsed = parse_digest_args(&["--now=456".into()]).unwrap();
        assert_eq!(parsed.now, Some(456));
        assert!(!parsed.dry_run);
        for bad in [
            vec!["--now".to_string()],
            vec!["extra".to_string(), "args".to_string(), "here".to_string()],
        ] {
            assert_eq!(parse_digest_args(&bad).unwrap_err(), USAGE, "{bad:?}");
        }
        for bad in [
            vec!["--now".to_string(), "yesterday".to_string()],
            vec!["--now=".to_string()],
            vec!["--now".to_string(), "-1".to_string()],
        ] {
            assert_eq!(
                parse_digest_args(&bad).unwrap_err(),
                "invalid --now value (want unix secs)",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn digest_dry_run_parses_alone_and_with_now_either_order() {
        let parsed = parse_digest_args(&["--dry-run".into()]).unwrap();
        assert_eq!(parsed.now, None);
        assert!(parsed.dry_run);
        for args in [
            vec!["--now", "123", "--dry-run"],
            vec!["--dry-run", "--now", "123"],
            vec!["--now=456", "--dry-run"],
            vec!["--dry-run", "--now=456"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let parsed = parse_digest_args(&owned).unwrap();
            assert!(parsed.dry_run, "{args:?}");
            assert!(parsed.now.is_some(), "{args:?}");
        }
        let space: Vec<String> = ["--dry-run", "--now", "123"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_digest_args(&space).unwrap().now, Some(123));
        let equals: Vec<String> = ["--dry-run", "--now=456"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_digest_args(&equals).unwrap().now, Some(456));
    }

    #[test]
    fn author_commands_reject_bad_args() {
        assert_eq!(cmd_unhide_author(&[]).unwrap_err(), USAGE);
        assert_eq!(
            cmd_unhide_author(&["a".to_string(), "b".to_string()]).unwrap_err(),
            USAGE
        );
        assert_eq!(cmd_unhide_author(&["   ".to_string()]).unwrap_err(), USAGE);
        assert_eq!(
            cmd_hidden_authors(&["extra".to_string()]).unwrap_err(),
            USAGE
        );
    }

    #[test]
    fn approval_commands_reject_bad_args() {
        assert_eq!(cmd_approvals(&["extra".to_string()]).unwrap_err(), USAGE);
        assert_eq!(cmd_approve(&[]).unwrap_err(), USAGE);
        assert_eq!(cmd_approve(&["".to_string()]).unwrap_err(), USAGE);
        assert_eq!(
            cmd_approve(&["a".to_string(), "b".to_string()]).unwrap_err(),
            USAGE
        );
        assert_eq!(cmd_deny(&[]).unwrap_err(), USAGE);
        assert_eq!(cmd_deny(&["".to_string()]).unwrap_err(), USAGE);
    }

    #[test]
    fn decide_now_parses_space_and_equals_forms() {
        let at = idea_card::parse_rfc3339("2030-01-01T00:00:00Z").unwrap();
        // Approve: id alone, or id plus trailing --now.
        let parsed = parse_decide_args(&["abc".to_string()], false).unwrap();
        assert_eq!(parsed.id, "abc");
        assert_eq!(parsed.now, None);
        assert_eq!(parsed.reason, "");
        for args in [
            vec!["abc", "--now", "2030-01-01T00:00:00Z"],
            vec!["abc", "--now=2030-01-01T00:00:00Z"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let parsed = parse_decide_args(&owned, false).unwrap();
            assert_eq!(parsed.id, "abc");
            assert_eq!(parsed.now, Some(at), "{args:?}");
        }
        // Deny: reason words preserved, trailing --now peeled.
        let owned: Vec<String> = ["abc", "not", "convincing"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_decide_args(&owned, true).unwrap();
        assert_eq!(parsed.reason, "not convincing");
        assert_eq!(parsed.now, None);
        for args in [
            vec!["abc", "not", "convincing", "--now", "2030-01-01T00:00:00Z"],
            vec!["abc", "--now=2030-01-01T00:00:00Z"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let parsed = parse_decide_args(&owned, true).unwrap();
            assert_eq!(parsed.now, Some(at), "{args:?}");
        }
        let owned: Vec<String> = ["abc", "not", "convincing", "--now", "2030-01-01T00:00:00Z"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_decide_args(&owned, true).unwrap().reason,
            "not convincing"
        );
        // Bad: duplicates, misplaced, missing, unparseable, empty id.
        for (args, is_deny) in [
            (vec!["abc", "--now"], false),
            (vec!["abc", "--now", "someday"], false),
            (vec!["abc", "--now="], false),
            (
                vec![
                    "abc",
                    "--now",
                    "2030-01-01T00:00:00Z",
                    "--now",
                    "2030-01-02T00:00:00Z",
                ],
                false,
            ),
            (
                vec![
                    "abc",
                    "--now",
                    "2030-01-01T00:00:00Z",
                    "--now=2030-01-02T00:00:00Z",
                ],
                true,
            ),
            (vec!["abc", "--now", "2030-01-01T00:00:00Z", "extra"], false),
            (vec!["abc", "extra", "--now"], true),
            (vec![""], false),
            (vec!["", "--now", "2030-01-01T00:00:00Z"], true),
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let err = parse_decide_args(&owned, is_deny).unwrap_err();
            assert!(
                err == USAGE || err == "invalid --now value (want RFC 3339)",
                "{args:?}: {err}"
            );
        }
        assert_eq!(
            parse_decide_args(
                &[
                    "abc".to_string(),
                    "--now".to_string(),
                    "someday".to_string()
                ],
                false
            )
            .unwrap_err(),
            "invalid --now value (want RFC 3339)"
        );
    }

    #[test]
    fn summary_folds_to_one_line() {
        assert_eq!(one_line_summary("plain"), "plain");
        assert_eq!(one_line_summary("first\nsecond"), "first");
        assert_eq!(one_line_summary("  spaced   out  "), "spaced out");
        assert_eq!(one_line_summary(""), "");
    }

    #[test]
    fn deeper_later_forms_parse_and_validate() {
        let at = idea_card::parse_rfc3339("2030-01-01T00:00:00Z").unwrap();
        for args in [
            vec!["--later", "100", "--at", "2030-01-01T00:00:00Z"],
            vec!["100", "--later", "--at", "2030-01-01T00:00:00Z"],
            vec!["--later=100", "--at=2030-01-01T00:00:00Z"],
            vec!["--at", "2030-01-01T00:00:00Z", "--later", "100"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let parsed = parse_deeper_args(&owned).unwrap();
            assert_eq!(parsed.post_id, "100");
            assert_eq!(parsed.later_at, Some(at));
            assert!(parsed.offline.is_none());
        }
        // Immediate forms still parse (demo path unchanged).
        let owned: Vec<String> = ["demo-3001", "--offline", "f.jsonl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_deeper_args(&owned).unwrap();
        assert_eq!(parsed.post_id, "demo-3001");
        assert_eq!(parsed.offline.as_deref(), Some("f.jsonl"));
        assert!(parsed.later_at.is_none());
        // Bad combinations are refused.
        for bad in [
            vec!["--later", "100"],
            vec!["100", "--at", "2030-01-01T00:00:00Z"],
            vec![
                "--later",
                "100",
                "--at",
                "2030-01-01T00:00:00Z",
                "--offline",
                "f",
            ],
            vec!["--later", "--later", "100", "--at", "2030-01-01T00:00:00Z"],
            vec!["100", "101", "--later", "--at", "2030-01-01T00:00:00Z"],
            vec!["--later", "--at", "2030-01-01T00:00:00Z"],
            vec![
                "--at",
                "2030-01-01T00:00:00Z",
                "--at",
                "2030-01-02T00:00:00Z",
                "100",
            ],
        ] {
            let owned: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert!(parse_deeper_args(&owned).is_err(), "{bad:?}");
        }
        for bad in [
            vec!["--later", "100", "--at", "someday"],
            vec!["--later", "100", "--at="],
        ] {
            let owned: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                parse_deeper_args(&owned).unwrap_err(),
                "invalid --at value (want RFC 3339)",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn run_args_parse_space_equals_and_any_order() {
        let at = idea_card::parse_rfc3339("2026-10-27T00:00:00Z").unwrap();
        let none: Vec<String> = vec![];
        let parsed = parse_run_args(&none).unwrap();
        assert!(!parsed.once);
        assert_eq!(parsed.now, None);
        assert_eq!(parsed.offline, None);
        for args in [
            vec![
                "--once",
                "--now",
                "2026-10-27T00:00:00Z",
                "--offline",
                "f.jsonl",
            ],
            vec!["--offline=f.jsonl", "--now=2026-10-27T00:00:00Z", "--once"],
            vec!["--now", "2026-10-27T00:00:00Z"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let parsed = parse_run_args(&owned).unwrap();
            assert_eq!(parsed.now, Some(at), "{args:?}");
        }
        let owned: Vec<String> = ["--once", "--offline", "f.jsonl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_run_args(&owned).unwrap();
        assert!(parsed.once);
        assert_eq!(parsed.offline.as_deref(), Some("f.jsonl"));
        for bad in [
            vec!["--once", "--once"],
            vec![
                "--now",
                "2026-10-27T00:00:00Z",
                "--now=2026-10-28T00:00:00Z",
            ],
            vec!["--offline", "a", "--offline", "b"],
            vec!["--offline="],
            vec!["--now"],
            vec!["extra"],
            vec!["--dry-run"],
        ] {
            let owned: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert_eq!(parse_run_args(&owned).unwrap_err(), USAGE, "{bad:?}");
        }
        assert_eq!(
            parse_run_args(&["--now".to_string(), "yesterday".to_string()]).unwrap_err(),
            "invalid --now value (want RFC 3339)"
        );
    }

    #[test]
    fn deeper_queue_rejects_args() {
        assert_eq!(cmd_deeper_queue(&["extra".to_string()]).unwrap_err(), USAGE);
    }

    #[test]
    fn rank_top_parses_space_and_equals_forms() {
        let none: Vec<String> = vec![];
        assert_eq!(parse_rank_args(&none).unwrap(), None);
        let owned: Vec<String> = ["--top", "3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_rank_args(&owned).unwrap(), Some(3));
        let owned: Vec<String> = ["--top=5"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_rank_args(&owned).unwrap(), Some(5));
        for bad in [
            vec!["--top"],
            vec!["--top="],
            vec!["--top", "0"],
            vec!["--top=0"],
            vec!["--top", "many"],
            vec!["--top", "3", "--top", "4"],
            vec!["--top", "3", "extra"],
            vec!["extra"],
            vec!["3"],
        ] {
            let owned: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert_eq!(parse_rank_args(&owned).unwrap_err(), USAGE, "{bad:?}");
        }
    }

    #[test]
    fn digest_rejects_duplicates_and_unknown_args() {
        for bad in [
            vec!["--dry-run", "--dry-run"],
            vec!["--now", "1", "--now", "2"],
            vec!["--now=1", "--now=2"],
            vec!["--now", "1", "--now=2"],
            vec!["--now=1", "--dry-run", "--dry-run"],
            vec!["--dryrun"],
            vec!["--dry-run=x"],
            vec!["digest"],
            vec!["123"],
        ] {
            let owned: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert_eq!(parse_digest_args(&owned).unwrap_err(), USAGE, "{bad:?}");
        }
    }
}
