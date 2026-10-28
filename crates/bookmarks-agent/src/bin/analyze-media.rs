//! `analyze-media` binary; see [`bookmarks_agent::analyze_media_cli`].
use bookmarks_agent::analyze_media::{MediaBackend, PaidConfig};

/// Build the live backend from the process env: `muse_exec` (default) or
/// `http` or `baseten` per `ANCIENTOS_MEDIA_BACKEND`; anything else is refused.
fn live(paid: PaidConfig) -> Result<Box<dyn MediaBackend>, String> {
    let env = |k: &str| std::env::var(k).ok();
    match env("ANCIENTOS_MEDIA_BACKEND")
        .as_deref()
        .unwrap_or("muse_exec")
    {
        "muse_exec" => Ok(Box::new(bookmarks_agent::analyze_media_muse::muse_backend(
            &env, paid,
        )?)),
        "http" => Ok(Box::new(bookmarks_agent::analyze_media_live::live_backend(
            &env, paid,
        )?)),
        "baseten" => Ok(Box::new(
            bookmarks_agent::analyze_media_baseten::baseten_backend(&env, paid)?,
        )),
        other => Err(format!("unknown ANCIENTOS_MEDIA_BACKEND '{other}'")),
    }
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
