//! `analyze-media` binary; see [`bookmarks_agent::analyze_media_cli`].
use bookmarks_agent::analyze_media::{MediaBackend, PaidConfig};

/// Build the live backend from the process env (shared selector, also used
/// by classify-on-import).
fn live(paid: PaidConfig) -> Result<Box<dyn MediaBackend>, String> {
    let env = |k: &str| std::env::var(k).ok();
    bookmarks_agent::analyze_media_cli::select_backend(&env, paid)
}

/// Entry point: run and print, exit 1 on error.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).ok();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match bookmarks_agent::analyze_media_cli::run(&args, &env, &live, now) {
        Ok(out) => print!("{out}"),
        Err(e) => {
            eprintln!("analyze-media: {e}");
            std::process::exit(1);
        }
    }
}
