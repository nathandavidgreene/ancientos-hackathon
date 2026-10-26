//! CLI: `login` | `sync [--max-pages N]` | `load [path]` | `auth-url`
//! | `brief [--fixture PATH] [--out PATH]`.
//!
//! `login` runs the PKCE loopback flow and saves tokens; `sync` pulls new
//! bookmarks into the local JSONL store (paid, off by default). `brief`
//! merges the live store with a fixture and prints a daily brief. Tokens,
//! codes, and verifiers are never printed.
use bookmarks_agent::{brief, callback, fixture, ingest, merge, oauth, token_store, x_api};

const USAGE: &str = "usage: bookmarks-agent login | sync [--max-pages N] | load [path] | auth-url | brief [--fixture PATH] [--out PATH]";

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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("login") if args.len() == 1 => cmd_login(),
        Some("sync") => cmd_sync(&args[1..]),
        Some("load") => cmd_load(&args[1..]),
        Some("auth-url") if args.len() == 1 => cmd_auth_url(),
        Some("brief") => cmd_brief(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
