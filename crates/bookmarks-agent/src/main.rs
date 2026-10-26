//! CLI: `login` | `sync [--max-pages N]` | `load [path]` | `auth-url`
//! | `brief [--fixture PATH] [--out PATH]`
//! | `vote <post_id> <verdict> [--tag TAG]... [--fixture PATH]`
//! | `delegate <post_id>` | `ideas` | `triage <id> <adopt|park|drop>`
//! | `tried <post_id> <yes|no|later> [note]` | `try-pending`.
//!
//! `login` runs the PKCE loopback flow and saves tokens; `sync` pulls new
//! bookmarks into the local JSONL store (paid, off by default). `brief`
//! merges the live store with a fixture and prints a daily brief. `vote`
//! records a feedback verdict; `delegate` summarizes one thread with Muse
//! Spark into inspiration.md (paid, off by default). `ideas` lists idea
//! cards due for triage; `triage` records an adopt/park/drop decision.
//! `tried` records a try-it follow-up; `try-pending` lists tagged posts
//! without a yes/no follow-up yet.
//! Tokens, codes, and verifiers are never printed.
use bookmarks_agent::{
    brief, callback, delegate, fixture, idea_card, ingest, merge, oauth, token_store, try_it, vote,
    x_api,
};
use std::path::PathBuf;

const USAGE: &str = "usage: bookmarks-agent login | sync [--max-pages N] | load [path] | auth-url | brief [--fixture PATH] [--out PATH] | vote <post_id> <verdict> [--tag TAG]... [--fixture PATH] | delegate <post_id> | ideas | triage <id> <adopt|park|drop> | tried <post_id> <yes|no|later> [note] | try-pending";

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

fn parse_brief_args(args: &[String]) -> Result<(Option<String>, Option<String>), String> {
    let mut fixture: Option<String> = None;
    let mut out: Option<String> = None;
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
        } else {
            return Err(USAGE.to_string());
        }
    }
    Ok((fixture, out))
}

fn cmd_brief(args: &[String]) -> Result<(), String> {
    let (fixture, out) = parse_brief_args(args)?;
    let merged = merge::load_merged(fixture.as_deref())?;
    let client = ancientos_router::muse::MuseClient::from_env()
        .with_max_completion_tokens(700)
        .with_reasoning_effort("low");
    let log_path = brief::decision_log_path()?;
    let date = brief::current_date();
    let result = brief::run(&merged, &client, &log_path, &date)?;
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

fn cmd_ideas(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_string());
    }
    let path = idea_card::idea_cards_path()?;
    let cards = idea_card::list_due_for_triage(&path, idea_card::now_secs())?;
    for card in &cards {
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
    for id in try_it::pending_followups(&log) {
        println!("{id}");
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
        Some("ideas") => cmd_ideas(&args[1..]),
        Some("triage") => cmd_triage(&args[1..]),
        Some("tried") => cmd_tried(&args[1..]),
        Some("try-pending") => cmd_try_pending(&args[1..]),
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
}
