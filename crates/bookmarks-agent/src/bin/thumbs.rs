//! `thumbs` binary; see [`bookmarks_agent::thumbs`].
use std::path::Path;

/// Entry point: run and print, exit 1 on error.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).ok();
    let fetch = |url: &str| bookmarks_agent::thumbs::fetch_bytes(url);
    let first_frame = |video: &[u8]| {
        bookmarks_agent::thumbs::extract_first_frame(
            video,
            Path::new(bookmarks_agent::thumbs::FFMPEG_BIN),
        )
    };
    match bookmarks_agent::thumbs::run(
        &args,
        &env,
        &fetch,
        &first_frame,
        &bookmarks_agent::topic_mute::current,
    ) {
        Ok(out) => print!("{out}"),
        Err(e) => {
            eprintln!("thumbs: {e}");
            std::process::exit(1);
        }
    }
}
