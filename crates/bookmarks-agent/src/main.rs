//! CLI: `login` | `sync [--max-pages N]` | `load [path]` | `auth-url`
//! | `brief [--fixture PATH] [--out PATH] [--offline] [--demo]`
//! | `vote <post_id> <verdict> [--tag TAG]... [--fixture PATH]`
//! | `delegate <post_id>` | `deeper <post_id> [--offline <fixture.jsonl>]`
//! | `ideas` | `triage <id> <adopt|park|drop>`
//! | `tried <post_id> <yes|no|later> [note]` | `try-pending`
//! | `digest [--now SECS] [--dry-run]`.
//!
//! `login` runs the PKCE loopback flow and saves tokens; `sync` pulls new
//! bookmarks into the local JSONL store (paid, off by default). `brief`
//! merges the live store with a fixture and prints a daily brief. `vote`
//! records a feedback verdict; `delegate` summarizes one thread with Muse
//! Spark into inspiration.md (paid, off by default); `deeper` writes a
//! step-by-step walkthrough of one thread plus its quoted/linked posts to
//! deeper/<post_id>.md (one paid call at most, offline fixture supported).
//! `ideas` lists idea cards due for triage; `triage` records an
//! adopt/park/drop decision.
//! `tried` records a try-it follow-up; `try-pending` lists tagged posts
//! without a yes/no follow-up yet. `digest` prints the weekly idea-card
//! triage digest plus "did you try it?" nudges, recording the showing
//! unless `--dry-run` or `--now` is given.
//! Tokens, codes, and verifiers are never printed.
use bookmarks_agent::{
    brief, callback, deeper, delegate, digest, fixture, idea_card, ingest, language, merge, oauth,
    token_store, try_it, vote, x_api,
};
use std::path::PathBuf;

const USAGE: &str = "usage: bookmarks-agent login | sync [--max-pages N] | load [path] | auth-url | brief [--fixture PATH] [--out PATH] [--offline] [--demo] | vote <post_id> <verdict> [--tag TAG]... [--fixture PATH] | delegate <post_id> | deeper <post_id> [--offline <fixture.jsonl>] | ideas | triage <id> <adopt|park|drop> | tried <post_id> <yes|no|later> [note] | try-pending | digest [--now SECS] [--dry-run]";

fn client_id() -> Result<String, String> {
    std::env::var("X_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "X_CLIENT_ID not set".to_string())
}

fn cmd_auth_url() -> Result<(), String> {
    let id = client_id()?;
    let verifier = oauth::generate_verifier().map_err(|e| e.to_string())?;
    let state = oauth::generate_state().map_err(|e| e.to_string())?;
    println!("{}", oauth::authorize_url(&id, &verifier, &state));
    Ok(())
}

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

fn cmd_vote(args: &[String]) -> Result<(), String> {
    let (post_id, verdict_s, tags, fixture) = parse_vote_args(args)?;
    let verdict: ancientos_core::feedback::Verdict = verdict_s.parse().map_err(|e: String| e)?;
    let log_path = brief::decision_log_path()?;
    let res = vote::run_vote(&post_id, verdict, &tags, fixture.as_deref(), &log_path)?;
    println!(
        "recorded {} for {} ({})",
        res.verdict.as_str(),
        res.post_id,
        res.category
    );
    if res.queued_delegate {
        println!("delegate queued for {}", res.post_id);
    }
    Ok(())
}

fn parse_deeper_args(args: &[String]) -> Result<(String, Option<String>), String> {
    if args.is_empty() || args[0].is_empty() {
        return Err(USAGE.to_string());
    }
    let post_id = args[0].clone();
    let mut offline: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(v) = arg.strip_prefix("--offline=") {
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            offline = Some(v.to_string());
            i += 1;
        } else if arg == "--offline" {
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
    Ok((post_id, offline))
}

fn cmd_deeper(args: &[String]) -> Result<(), String> {
    let (post_id, offline) = parse_deeper_args(args)?;
    let res = deeper::run(&post_id, offline.as_deref())?;
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

/// Stored bookmarks for language filtering of id/card surfaces. A missing
/// or unreadable store means every source is unknown (kept), so these
/// read-only listings never fail just because the store is absent.
fn bookmarks_for_filtering() -> Vec<fixture::Bookmark> {
    ingest::Store::load()
        .map(|s| s.by_id.into_values().collect())
        .unwrap_or_default()
}

fn cmd_ideas(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let path = idea_card::idea_cards_path()?;
    let cards = idea_card::list_due_for_triage(&path, idea_card::now_secs())?;
    let bookmarks = bookmarks_for_filtering();
    for card in &cards {
        // Cards from a known non-English source post are skipped silently.
        if language::source_post_is_english(&card.source_url, &bookmarks) == Some(false) {
            continue;
        }
        println!("{}", format_idea_line(card));
    }
    Ok(())
}

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
    println!("{}", status_str(card.status));
    Ok(())
}

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

fn cmd_try_pending(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let log_path = brief::decision_log_path()?;
    let log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let pending = try_it::pending_followups(&log);
    // Known non-English posts are skipped silently (unknown ids are kept).
    for id in language::retain_not_known_non_english(&pending, &bookmarks_for_filtering()) {
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("login") if args.len() == 1 => cmd_login(),
        Some("sync") => cmd_sync(&args[1..]),
        Some("load") => cmd_load(&args[1..]),
        Some("auth-url") if args.len() == 1 => cmd_auth_url(),
        Some("brief") => cmd_brief(&args[1..]),
        Some("vote") => cmd_vote(&args[1..]),
        Some("delegate") => cmd_delegate(&args[1..]),
        Some("deeper") => cmd_deeper(&args[1..]),
        Some("ideas") => cmd_ideas(&args[1..]),
        Some("triage") => cmd_triage(&args[1..]),
        Some("tried") => cmd_tried(&args[1..]),
        Some("try-pending") => cmd_try_pending(&args[1..]),
        Some("digest") => cmd_digest(&args[1..]),
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
