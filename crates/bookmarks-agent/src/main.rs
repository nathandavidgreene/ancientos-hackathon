//! Tiny CLI: `bookmarks-agent load <fixture.jsonl>` | `bookmarks-agent auth-url`.
use bookmarks_agent::{fixture, oauth};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("load") => {
            let path = args
                .get(1)
                .map(String::as_str)
                .unwrap_or("fixtures/sample_bookmarks.jsonl");
            match fixture::load(path) {
                Ok(rows) => println!("loaded {} bookmarks", rows.len()),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("auth-url") => match std::env::var("X_CLIENT_ID") {
            Ok(id) if !id.is_empty() => {
                // Placeholder verifier: real random verifier/state generation is TODO.
                println!(
                    "{}",
                    oauth::authorize_url(&id, "TODO-random-verifier", "TODO-state")
                );
            }
            _ => {
                eprintln!("X_CLIENT_ID not set");
                std::process::exit(1);
            }
        },
        _ => eprintln!("usage: bookmarks-agent load [path] | auth-url"),
    }
}
