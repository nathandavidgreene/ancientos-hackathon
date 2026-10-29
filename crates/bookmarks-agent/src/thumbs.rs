//! Sharp 240px thumbnails for bookmark media, minus muted posts.
//!
//! Reads bookmark rows (the [`crate::fixture::Bookmark`] JSONL store with
//! [`crate::media::Media`] attachments), downloads each item's pixels, and
//! writes one thumbnail per media item plus a `manifest.jsonl` describing
//! every item. Photos use the image itself; videos and animated GIFs use the
//! poster when the row carries one, else the first video frame extracted with
//! the local `/usr/bin/ffmpeg` binary (no remote service).
//!
//! Thumbnails are 240px on the long edge with the aspect preserved, downscaled
//! with Lanczos3 plus a light unsharp mask so text stays crisp, encoded as
//! JPEG quality 85 (PNG when the source has alpha), and never upscaled.
//!
//! Muted posts are skipped, never thumbnailed: the shared
//! [`crate::topic_mute`] list (case-insensitive) plus any `muted` /
//! `mute_check_failed` flags already present on the input rows. When the mute
//! check cannot run, every post is skipped with reason `mute_check_failed`
//! (fail closed). Skip reasons and the manifest never carry post text or
//! muted terms.
//!
//! Reads are incremental (a thumbnail already on disk is reused without any
//! download) and bookmark access is read-only. Downloads are size- and
//! timeout-bounded `https` fetches to X media hosts only, re-validated after
//! redirects. No paid model calls are made here.
//!
//! Runner: `thumbs [--rows FILE] [--out-dir DIR]`, wired like
//! [`crate::analyze_media_cli::run`]: [`run`] takes the argv slice, an env
//! lookup, and injectable download / frame-extraction / mute-list closures so
//! tests run with mocks and no network. The output directory defaults to
//! `~/.ancientos-hackathon/eval-private/thumbs/` (mode 0700, files 0600).

use crate::fixture::Bookmark;
use crate::media::Media;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Long edge of every thumbnail, in pixels.
pub const LONG_EDGE: u32 = 240;
/// JPEG quality for thumbnails from sources without alpha.
pub const JPEG_QUALITY: u8 = 85;
/// Local ffmpeg binary used for first-frame extraction; never a remote call.
pub const FFMPEG_BIN: &str = "/usr/bin/ffmpeg";
/// Env override for the thumbnail output directory (a CLI flag wins over it).
pub const OUT_DIR_ENV_VAR: &str = "ANCIENTOS_THUMBS_DIR";
/// Manifest file written inside the output directory.
pub const MANIFEST_FILE_NAME: &str = "manifest.jsonl";
/// Private eval directory under the data dir that holds the thumbnails.
pub const EVAL_PRIVATE_DIR_NAME: &str = "eval-private";
/// Thumbnail directory under the eval-private directory.
pub const THUMBS_DIR_NAME: &str = "thumbs";
/// Largest single download accepted, in bytes (images and videos alike).
pub const MAX_DOWNLOAD_BYTES: usize = 32 * 1024 * 1024;
/// Whole-request download timeout, in seconds.
pub const FETCH_TIMEOUT_SECS: u64 = 15;
/// Redirects followed per download; the final URL is re-checked afterwards.
pub const MAX_REDIRECTS: u32 = 3;
/// Unsharp-mask radius applied after downscaling: small enough to crisp text
/// without ringing halos around it.
pub const UNSHARP_SIGMA: f32 = 0.8;
/// Unsharp-mask threshold: pixels already this close are left untouched.
pub const UNSHARP_THRESHOLD: i32 = 3;
/// Skip reason recorded when a post matches the muted list.
pub const SKIPPED_MUTED: &str = "muted";
/// Skip reason recorded when the mute check cannot run (fail closed).
pub const SKIPPED_MUTE_CHECK_FAILED: &str = "mute_check_failed";
/// CLI usage line printed with argument errors.
const USAGE: &str = "usage: thumbs [--rows FILE] [--out-dir DIR]";

/// Downloads one media URL into memory (production: [`fetch_bytes`]; tests
/// inject a mock).
type Fetch<'a> = dyn Fn(&str) -> Result<Vec<u8>, String> + 'a;
/// Extracts a PNG first frame from video bytes (production:
/// [`extract_first_frame`]; tests inject a mock).
type FirstFrame<'a> = dyn Fn(&[u8]) -> Result<Vec<u8>, String> + 'a;
/// Loads the active mute list (production: [`crate::topic_mute::current`];
/// tests inject fixed lists or failures).
type MuteLoader<'a> = dyn Fn() -> Result<crate::topic_mute::MuteList, String> + 'a;

/// Parsed `thumbs` flags: an optional rows file and output directory.
#[derive(Debug, PartialEq)]
struct ThumbArgs {
    /// Bookmarks JSONL input; the default store when absent.
    rows: Option<String>,
    /// Where thumbnails and the manifest go; resolved when absent.
    out_dir: Option<String>,
}

/// One manifest line: either a finished thumbnail or a skip reason, never
/// post text.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ManifestEntry {
    /// Owning bookmark's post id.
    pub post_id: String,
    /// Index of the media item within the post, starting at 0.
    pub index: usize,
    /// Media kind as stored (`photo`, `video`, `animated_gif`, ...).
    pub kind: String,
    /// Thumbnail file name inside the output directory (relative, so the
    /// manifest never leaks personal paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Thumbnail width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Thumbnail height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Why no thumbnail was made; present only on skipped items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// Where a media item's thumbnail pixels come from.
#[derive(Debug, Clone, PartialEq)]
enum ThumbSource {
    /// A downloadable still image: a photo, or a video/GIF poster.
    Still(String),
    /// Video bytes whose first frame ffmpeg must extract (no poster stored).
    FirstFrame(String),
}

/// How one media item ended: written fresh, reused from disk, or skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Thumbnail bytes were downloaded, scaled, and written.
    Fresh,
    /// A thumbnail file already on disk was reused without downloading.
    Cached,
    /// No thumbnail was made; the manifest entry carries the reason.
    Skipped,
}

/// An encoded thumbnail ready to write: bytes, pixel size, and extension.
#[derive(Debug, Clone, PartialEq)]
struct EncodedThumb {
    /// Encoded JPEG or PNG bytes.
    bytes: Vec<u8>,
    /// Pixel width of the thumbnail.
    width: u32,
    /// Pixel height of the thumbnail.
    height: u32,
    /// File extension matching the encoding: `jpg` or `png`.
    ext: &'static str,
}

/// Parse `thumbs` flags in `--flag value` and `--flag=value` form; anything
/// unknown is an error.
fn parse_args(args: &[String]) -> Result<ThumbArgs, String> {
    let mut rows = None;
    let mut out_dir = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(v) = arg.strip_prefix("--rows=") {
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            rows = Some(v.to_string());
            i += 1;
        } else if arg == "--rows" {
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            rows = Some(v.clone());
            i += 1;
        } else if let Some(v) = arg.strip_prefix("--out-dir=") {
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            out_dir = Some(v.to_string());
            i += 1;
        } else if arg == "--out-dir" {
            i += 1;
            let v = args.get(i).ok_or_else(|| USAGE.to_string())?;
            if v.is_empty() {
                return Err(USAGE.to_string());
            }
            out_dir = Some(v.clone());
            i += 1;
        } else {
            return Err(format!("unknown argument {arg}\n{USAGE}"));
        }
    }
    Ok(ThumbArgs { rows, out_dir })
}

/// Output directory: the `--out-dir` flag, else `$ANCIENTOS_THUMBS_DIR`, else
/// `$ANCIENTOS_HOME/eval-private/thumbs`, else
/// `$HOME/.ancientos-hackathon/eval-private/thumbs`.
fn resolve_out_dir(
    flag: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(dir) = flag.map(str::to_string).or_else(|| get(OUT_DIR_ENV_VAR)) {
        return Ok(PathBuf::from(dir));
    }
    let base = match get("ANCIENTOS_HOME") {
        Some(d) => d,
        None => format!(
            "{}/.ancientos-hackathon",
            get("HOME").ok_or("HOME not set")?
        ),
    };
    Ok(PathBuf::from(base)
        .join(EVAL_PRIVATE_DIR_NAME)
        .join(THUMBS_DIR_NAME))
}

/// Read the rows file: the explicit `--rows` path, or the default bookmark
/// store (a missing default store means zero rows, not an error, since there
/// is simply nothing bookmarked yet).
fn read_rows_file(rows: Option<&str>) -> Result<String, String> {
    match rows {
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}")),
        None => {
            let store = crate::ingest::store_path()?;
            match std::fs::read_to_string(&store) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
                Err(e) => Err(format!("{}: {e}", store.display())),
                Ok(text) => Ok(text),
            }
        }
    }
}

/// Parse rows file text as JSONL values (kept as values so `muted` flags
/// survive); any bad line fails the run.
fn parse_input(text: &str) -> Result<Vec<serde_json::Value>, String> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1)))
        .collect()
}

/// True when the input row carries the named boolean flag set to true (used
/// for `muted` / `mute_check_failed` left by an earlier pass).
fn is_flagged(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(serde_json::Value::as_bool) == Some(true)
}

/// Manifest entry for a skipped item, carrying the reason and no pixels.
fn skip_entry(post_id: &str, index: usize, kind: &str, reason: &str) -> ManifestEntry {
    ManifestEntry {
        post_id: post_id.to_string(),
        index,
        kind: kind.to_string(),
        path: None,
        width: None,
        height: None,
        skipped: Some(reason.to_string()),
    }
}

/// Pick the thumbnail pixels for one stored media item: photos use the image
/// itself, videos and GIFs prefer the poster and fall back to first-frame
/// extraction, and unknown kinds try the same chain; rows without any usable
/// URL fail with a skip reason.
fn source_for_media(media: &Media) -> Result<ThumbSource, String> {
    match media.kind.as_str() {
        "photo" => media
            .url
            .clone()
            .or_else(|| media.preview_url.clone())
            .map(ThumbSource::Still)
            .ok_or_else(|| "photo has no image URL".to_string()),
        "video" | "animated_gif" => {
            if let Some(poster) = media.preview_url.clone() {
                Ok(ThumbSource::Still(poster))
            } else if let Some(video) = media.video_url.clone() {
                Ok(ThumbSource::FirstFrame(video))
            } else {
                Err("video has no poster or video URL".to_string())
            }
        }
        _ => {
            if let Some(still) = media.url.clone().or_else(|| media.preview_url.clone()) {
                Ok(ThumbSource::Still(still))
            } else if let Some(video) = media.video_url.clone() {
                Ok(ThumbSource::FirstFrame(video))
            } else {
                Err("media has no usable URL".to_string())
            }
        }
    }
}

/// Filesystem-safe thumbnail stem for one media item: the post id reduced to
/// word characters (anything else becomes `_`, capped at 64 chars) plus the
/// media index, so reruns map to the same file.
fn safe_stem(post_id: &str, index: usize) -> String {
    let mut clean: String = post_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if clean.is_empty() {
        clean = "post".to_string();
    }
    format!("{clean}_{index}")
}

/// Thumbnail file already on disk for this stem, if any (either extension, so
/// a cached file is found without knowing the source's alpha upfront).
fn existing_thumb(out_dir: &Path, stem: &str) -> Option<PathBuf> {
    for ext in ["jpg", "png"] {
        let candidate = out_dir.join(format!("{stem}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Thumbnail dimensions for a source size: the long edge becomes 240px with
/// the aspect preserved; sources already at or below that keep their size
/// (never upscale).
fn thumbnail_size(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height);
    if long == 0 || long <= LONG_EDGE {
        return (width, height);
    }
    let scale = f64::from(LONG_EDGE) / f64::from(long);
    let scaled_w = (f64::from(width) * scale).round() as u32;
    let scaled_h = (f64::from(height) * scale).round() as u32;
    (scaled_w.max(1), scaled_h.max(1))
}

/// Turn downloaded image bytes into an encoded thumbnail: decode, Lanczos3
/// downscale to [`LONG_EDGE`] plus a light unsharp mask when shrinking (left
/// untouched when already small), then JPEG quality 85, or PNG when the
/// source carries alpha.
fn make_thumbnail(bytes: &[u8]) -> Result<EncodedThumb, String> {
    use image::GenericImageView as _;
    use image::ImageEncoder as _;
    let img = image::load_from_memory(bytes).map_err(|_| "unreadable image".to_string())?;
    let (width, height) = img.dimensions();
    let (target_w, target_h) = thumbnail_size(width, height);
    let shrink = target_w != width || target_h != height;
    if img.color().has_alpha() {
        let src = img.to_rgba8();
        let small = if shrink {
            let down = image::imageops::resize(
                &src,
                target_w,
                target_h,
                image::imageops::FilterType::Lanczos3,
            );
            image::imageops::unsharpen(&down, UNSHARP_SIGMA, UNSHARP_THRESHOLD)
        } else {
            src
        };
        let mut buf = Vec::new();
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(
                small.as_raw(),
                small.width(),
                small.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|_| "png encode failed".to_string())?;
        Ok(EncodedThumb {
            bytes: buf,
            width: small.width(),
            height: small.height(),
            ext: "png",
        })
    } else {
        let src = img.to_rgb8();
        let small = if shrink {
            let down = image::imageops::resize(
                &src,
                target_w,
                target_h,
                image::imageops::FilterType::Lanczos3,
            );
            image::imageops::unsharpen(&down, UNSHARP_SIGMA, UNSHARP_THRESHOLD)
        } else {
            src
        };
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY)
            .write_image(
                small.as_raw(),
                small.width(),
                small.height(),
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|_| "jpeg encode failed".to_string())?;
        Ok(EncodedThumb {
            bytes: buf,
            width: small.width(),
            height: small.height(),
            ext: "jpg",
        })
    }
}

/// True when a transport error was caused by a timeout: ureq wraps the
/// deadline expiry as an `io::Error` of kind `TimedOut` in the source chain.
fn is_timeout(t: &ureq::Transport) -> bool {
    use std::error::Error;
    let mut next = t.source();
    while let Some(e) = next {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::TimedOut {
                return true;
            }
        }
        next = e.source();
    }
    t.message().is_some_and(|m| {
        let m = m.to_ascii_lowercase();
        m.contains("timed out") || m.contains("timeout")
    })
}

/// Map a download error to a short skip reason: static strings plus the HTTP
/// status only, never URLs or hostnames.
fn map_fetch_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("http status {code}"),
        ureq::Error::Transport(t) => {
            if is_timeout(&t) {
                return "timeout".to_string();
            }
            match t.kind() {
                ureq::ErrorKind::TooManyRedirects => "too many redirects".to_string(),
                ureq::ErrorKind::Dns => "dns failed".to_string(),
                ureq::ErrorKind::ConnectionFailed => "connection failed".to_string(),
                ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => {
                    "invalid url".to_string()
                }
                _ => "request failed".to_string(),
            }
        }
    }
}

/// Download one media URL with the production client: allowlisted https X
/// media hosts only, bounded time and size, and the final URL re-checked
/// after redirects (fail closed on anything else).
pub fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let safe = crate::media::safe_media_url(url).ok_or_else(|| "unsafe media URL".to_string())?;
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .redirects(MAX_REDIRECTS)
        .build();
    let resp = agent.get(&safe).call().map_err(map_fetch_error)?;
    crate::media::safe_media_url(resp.get_url())
        .ok_or_else(|| "redirect left X media hosts".to_string())?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_DOWNLOAD_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|_| "unreadable body".to_string())?;
    if buf.len() > MAX_DOWNLOAD_BYTES {
        return Err("media exceeds size cap".to_string());
    }
    if buf.is_empty() {
        return Err("empty download".to_string());
    }
    Ok(buf)
}

/// Extract the first frame of downloaded video bytes as PNG bytes using the
/// local ffmpeg binary: the clip is staged in a temp dir and ffmpeg reads
/// only that file. A missing binary or any failure is an error (fail closed),
/// with a reason that carries no paths.
pub fn extract_first_frame(video: &[u8], ffmpeg_bin: &Path) -> Result<Vec<u8>, String> {
    if video.is_empty() {
        return Err("empty video".to_string());
    }
    let dir = tempfile::tempdir().map_err(|e| format!("temp dir failed: {e}"))?;
    let input = dir.path().join("clip.mp4");
    std::fs::write(&input, video).map_err(|e| format!("temp video failed: {e}"))?;
    let out = std::process::Command::new(ffmpeg_bin)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&input)
        .args(["-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"])
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "ffmpeg not available".to_string(),
            _ => "frame extraction failed".to_string(),
        })?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err("frame extraction failed".to_string());
    }
    Ok(out.stdout)
}

/// Make (or reuse) the thumbnail for one media item: reuse the file on disk
/// when present, else download the still or extract the first frame, scale,
/// and write the file with mode 0600. Every failure becomes a manifest skip
/// reason, never an error.
fn thumbnail_one(
    out_dir: &Path,
    post_id: &str,
    index: usize,
    media: &Media,
    fetch: &Fetch<'_>,
    first_frame: &FirstFrame<'_>,
) -> (ManifestEntry, Outcome) {
    let stem = safe_stem(post_id, index);
    if let Some(hit) = existing_thumb(out_dir, &stem) {
        if let Ok((width, height)) = image::image_dimensions(&hit) {
            let name = hit.file_name().map_or_else(
                || format!("{stem}.jpg"),
                |n| n.to_string_lossy().into_owned(),
            );
            let entry = ManifestEntry {
                post_id: post_id.to_string(),
                index,
                kind: media.kind.clone(),
                path: Some(name),
                width: Some(width),
                height: Some(height),
                skipped: None,
            };
            return (entry, Outcome::Cached);
        }
        // Unreadable file on disk: fall through and regenerate it.
    }
    let skipped = |reason: &str| {
        (
            skip_entry(post_id, index, &media.kind, reason),
            Outcome::Skipped,
        )
    };
    let image_bytes = match source_for_media(media) {
        Err(reason) => return skipped(&reason),
        Ok(ThumbSource::Still(url)) => {
            if crate::media::safe_media_url(&url).is_none() {
                return skipped("unsafe media URL");
            }
            match fetch(&url) {
                Ok(bytes) => bytes,
                Err(e) => return skipped(&format!("download failed: {e}")),
            }
        }
        Ok(ThumbSource::FirstFrame(url)) => {
            if crate::media::safe_media_url(&url).is_none() {
                return skipped("unsafe media URL");
            }
            let video = match fetch(&url) {
                Ok(bytes) => bytes,
                Err(e) => return skipped(&format!("download failed: {e}")),
            };
            match first_frame(&video) {
                Ok(frame) => frame,
                Err(e) => return skipped(&format!("frame extraction failed: {e}")),
            }
        }
    };
    let thumb = match make_thumbnail(&image_bytes) {
        Ok(t) => t,
        Err(e) => return skipped(&e),
    };
    let name = format!("{stem}.{}", thumb.ext);
    if let Err(e) = crate::token_store::atomic_write_0600(&out_dir.join(&name), &thumb.bytes) {
        return skipped(&format!("write failed: {e}"));
    }
    let entry = ManifestEntry {
        post_id: post_id.to_string(),
        index,
        kind: media.kind.clone(),
        path: Some(name),
        width: Some(thumb.width),
        height: Some(thumb.height),
        skipped: None,
    };
    (entry, Outcome::Fresh)
}

/// Run the `thumbs` CLI: thumbnail every media item in the rows file,
/// skipping muted posts, and rewrite the manifest. `fetch` downloads one URL,
/// `first_frame` extracts a PNG frame from video bytes, and `mute` loads the
/// mute list; the binary wires the production versions while tests inject
/// mocks. Returns the text to print (counts and skip reasons only, never post
/// text, muted terms, or personal paths).
pub fn run(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    fetch: &Fetch<'_>,
    first_frame: &FirstFrame<'_>,
    mute: &MuteLoader<'_>,
) -> Result<String, String> {
    let parsed = parse_args(args)?;
    let out_dir = resolve_out_dir(parsed.out_dir.as_deref(), env)?;
    crate::token_store::ensure_dir_0700(&out_dir)
        .map_err(|e| format!("thumbnail dir failed: {e}"))?;
    let text = read_rows_file(parsed.rows.as_deref())?;
    let values = parse_input(&text)?;
    // Load the mute list once: when it cannot load, every post is skipped
    // with a reason instead of thumbnailed (fail closed).
    let mute_list = mute().ok();
    let mut entries: Vec<ManifestEntry> = Vec::new();
    let mut made = 0usize;
    let mut cached = 0usize;
    for (line_no, value) in values.iter().enumerate() {
        let bookmark: Bookmark = serde_json::from_value(value.clone())
            .map_err(|e| format!("line {}: {e}", line_no + 1))?;
        let check_failed = is_flagged(value, "mute_check_failed") || mute_list.is_none();
        let muted = !check_failed
            && (is_flagged(value, "muted")
                || mute_list
                    .as_ref()
                    .is_some_and(|m| m.is_muted(&bookmark, &[])));
        for (index, media) in bookmark.media.iter().enumerate() {
            if check_failed {
                entries.push(skip_entry(
                    &bookmark.post_id,
                    index,
                    &media.kind,
                    SKIPPED_MUTE_CHECK_FAILED,
                ));
            } else if muted {
                entries.push(skip_entry(
                    &bookmark.post_id,
                    index,
                    &media.kind,
                    SKIPPED_MUTED,
                ));
            } else {
                let (entry, outcome) = thumbnail_one(
                    &out_dir,
                    &bookmark.post_id,
                    index,
                    media,
                    fetch,
                    first_frame,
                );
                match outcome {
                    Outcome::Fresh => made += 1,
                    Outcome::Cached => cached += 1,
                    Outcome::Skipped => {}
                }
                entries.push(entry);
            }
        }
    }
    let mut manifest = String::new();
    for entry in &entries {
        let line = serde_json::to_string(entry).map_err(|e| format!("manifest encode: {e}"))?;
        manifest.push_str(&line);
        manifest.push('\n');
    }
    crate::token_store::atomic_write_0600(&out_dir.join(MANIFEST_FILE_NAME), manifest.as_bytes())
        .map_err(|e| format!("manifest write failed: {e}"))?;
    let skipped = entries.iter().filter(|e| e.skipped.is_some()).count();
    let mut out = format!(
        "{} post(s), {} media item(s): {made} thumbnail(s), {cached} cached, {skipped} skipped\n",
        values.len(),
        entries.len(),
    );
    for entry in &entries {
        if let Some(reason) = &entry.skipped {
            out.push_str(&format!(
                "  skip {} media {}: {reason}\n",
                entry.post_id, entry.index
            ));
        }
    }
    out.push_str(&format!("manifest: {MANIFEST_FILE_NAME}\n"));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topic_mute::MuteList;
    use std::cell::{Cell, RefCell};

    /// Test helper: argument vector from string slices.
    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Test helper: env lookup that answers nothing (flags carry everything).
    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Test helper: mute loader with an empty list (nothing muted).
    fn no_mutes() -> Result<MuteList, String> {
        Ok(MuteList::default())
    }

    /// Test helper: PNG bytes of a synthetic RGB gradient, generated in-test
    /// so no fixture files are needed.
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// Test helper: PNG bytes of a synthetic RGBA gradient (exercises the
    /// PNG-with-alpha path).
    fn rgba_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 200])
        });
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// Test helper: one bookmark JSONL line with a synthetic id and the given
    /// media array.
    fn bookmark_line(post_id: &str, text: &str, media_json: &str) -> String {
        format!(
            "{{\"post_id\":\"{post_id}\",\"url\":\"https://x.com/i/status/{post_id}\",\"text\":{text:?},\"media\":[{media_json}]}}"
        )
    }

    /// Test helper: bookmark line carrying extra top-level fields (mute
    /// flags), mirroring rows annotated by an earlier pass.
    fn bookmark_line_extra(post_id: &str, text: &str, media_json: &str, extra: &str) -> String {
        format!(
            "{{\"post_id\":\"{post_id}\",\"url\":\"https://x.com/i/status/{post_id}\",\"text\":{text:?},\"media\":[{media_json}],{extra}}}"
        )
    }

    /// Test helper: photo media JSON on a synthetic X media URL.
    fn photo_json() -> &'static str {
        "{\"kind\":\"photo\",\"url\":\"https://pbs.twimg.com/media/synthetic.jpg\"}"
    }

    /// Test helper: video media JSON with both poster and mp4 URLs.
    fn video_json() -> &'static str {
        "{\"kind\":\"video\",\"preview_url\":\"https://pbs.twimg.com/thumb/synthetic.jpg\",\"video_url\":\"https://video.twimg.com/synthetic.mp4\"}"
    }

    /// Test helper: video media JSON with only an mp4 URL, forcing frame
    /// extraction.
    fn video_no_poster_json() -> &'static str {
        "{\"kind\":\"video\",\"video_url\":\"https://video.twimg.com/synthetic.mp4\"}"
    }

    /// Test helper: write rows text to a temp file, returning the dir (kept
    /// alive by the caller) and the path.
    fn rows_file(rows: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rows.jsonl");
        std::fs::write(&path, rows).unwrap();
        (dir, path.to_string_lossy().to_string())
    }

    /// Test helper: fresh temp output directory with its string path.
    fn out_dir() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thumbs");
        (dir, path.to_string_lossy().to_string())
    }

    /// A landscape photo becomes a 240px-wide JPEG with manifest dimensions,
    /// private permissions, and no post text anywhere.
    #[test]
    fn photo_is_resized_to_240px_jpeg() {
        let (_rows_dir, rows) = rows_file(&bookmark_line(
            "shot1",
            "synthetic skyline photo",
            photo_json(),
        ));
        let (_dir_guard, dir) = out_dir();
        let shot = png_bytes(800, 400);
        let fetch = |_: &str| Ok(shot.clone());
        let no_frame =
            |_: &[u8]| -> Result<Vec<u8>, String> { panic!("photos never extract frames") };
        let summary = run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &fetch,
            &no_frame,
            &no_mutes,
        )
        .unwrap();
        assert!(summary.contains("1 thumbnail(s)"), "{summary}");
        let thumb = PathBuf::from(&dir).join("shot1_0.jpg");
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (240, 120));
        let head = std::fs::read(&thumb).unwrap();
        assert_eq!(&head[..2], &[0xFF, 0xD8]);
        let manifest =
            std::fs::read_to_string(PathBuf::from(&dir).join(MANIFEST_FILE_NAME)).unwrap();
        assert!(manifest.contains(r#""width":240"#), "{manifest}");
        assert!(manifest.contains(r#""height":120"#), "{manifest}");
        assert!(!manifest.contains("skyline"), "{manifest}");
        assert!(!summary.contains("skyline"), "{summary}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&thumb).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    /// A video with a poster downloads only the poster; ffmpeg is never
    /// touched.
    #[test]
    fn poster_is_preferred_over_ffmpeg() {
        let (_rows_dir, rows) = rows_file(&bookmark_line("clip2", "synthetic clip", video_json()));
        let (_dir_guard, dir) = out_dir();
        let fetched: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let shot = png_bytes(400, 800);
        let fetch = |url: &str| {
            fetched.borrow_mut().push(url.to_string());
            Ok(shot.clone())
        };
        let no_frame =
            |_: &[u8]| -> Result<Vec<u8>, String> { panic!("a poster must win over ffmpeg") };
        let summary = run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &fetch,
            &no_frame,
            &no_mutes,
        )
        .unwrap();
        assert!(summary.contains("1 thumbnail(s)"), "{summary}");
        assert_eq!(
            fetched.borrow().as_slice(),
            ["https://pbs.twimg.com/thumb/synthetic.jpg".to_string()]
        );
        let thumb = PathBuf::from(&dir).join("clip2_0.jpg");
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (120, 240));
    }

    /// A poster-less video thumbnails from its first frame via the real local
    /// ffmpeg; the test passes trivially when the binary is absent.
    #[test]
    fn first_frame_fallback_uses_ffmpeg() {
        if !Path::new(FFMPEG_BIN).exists() {
            return;
        }
        let probe = tempfile::tempdir().unwrap();
        let clip = probe.path().join("probe.mp4");
        let made = std::process::Command::new(FFMPEG_BIN)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=1",
                "-frames:v",
                "2",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let video = std::fs::read(&clip).unwrap();
        let (_rows_dir, rows) = rows_file(&bookmark_line(
            "clip9",
            "synthetic clip without poster",
            video_no_poster_json(),
        ));
        let (_dir_guard, dir) = out_dir();
        let fetch = |_: &str| Ok(video.clone());
        let frame = |b: &[u8]| extract_first_frame(b, Path::new(FFMPEG_BIN));
        let summary = run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &fetch,
            &frame,
            &no_mutes,
        )
        .unwrap();
        assert!(summary.contains("1 thumbnail(s)"), "{summary}");
        let thumb = PathBuf::from(&dir).join("clip9_0.jpg");
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (240, 180));
    }

    /// A post matching the mute list yields no thumbnail and no download, and
    /// the muted term appears in neither the summary nor the manifest.
    #[test]
    fn muted_posts_are_skipped() {
        let (_rows_dir, rows) = rows_file(&bookmark_line("mut1", "a ZORBLAX post", photo_json()));
        let (_dir_guard, dir) = out_dir();
        let no_fetch =
            |_: &str| -> Result<Vec<u8>, String> { panic!("muted posts must not download") };
        let no_frame =
            |_: &[u8]| -> Result<Vec<u8>, String> { panic!("muted posts must not extract frames") };
        let mute = || Ok::<MuteList, String>(MuteList::new(["zorblax"]));
        let summary = run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &no_fetch,
            &no_frame,
            &mute,
        )
        .unwrap();
        assert!(summary.contains("1 skipped"), "{summary}");
        let manifest =
            std::fs::read_to_string(PathBuf::from(&dir).join(MANIFEST_FILE_NAME)).unwrap();
        assert!(manifest.contains(r#""skipped":"muted""#), "{manifest}");
        assert!(!summary.contains("zorblax"), "{summary}");
        assert!(!manifest.contains("zorblax"), "{manifest}");
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1, "only the manifest may exist");
    }

    /// An unreadable mute config thumbnails nothing; every item reports the
    /// check failure instead.
    #[test]
    fn failed_mute_check_skips_everything() {
        let rows_text = format!(
            "{}\n{}",
            bookmark_line("a1", "plain synthetic post", photo_json()),
            bookmark_line("a2", "another plain post", photo_json())
        );
        let (_rows_dir, rows) = rows_file(&rows_text);
        let (_dir_guard, dir) = out_dir();
        let no_fetch = |_: &str| -> Result<Vec<u8>, String> {
            panic!("a failed mute check must not download")
        };
        let no_frame = |_: &[u8]| -> Result<Vec<u8>, String> {
            panic!("a failed mute check must not extract frames")
        };
        let broken = || Err::<MuteList, String>("boom".to_string());
        let summary = run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &no_fetch,
            &no_frame,
            &broken,
        )
        .unwrap();
        assert!(summary.contains("0 thumbnail(s)"), "{summary}");
        assert!(summary.contains("2 skipped"), "{summary}");
        let manifest =
            std::fs::read_to_string(PathBuf::from(&dir).join(MANIFEST_FILE_NAME)).unwrap();
        assert_eq!(manifest.matches(SKIPPED_MUTE_CHECK_FAILED).count(), 2);
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1, "only the manifest may exist");
    }

    /// `muted` / `mute_check_failed` flags on input rows are honored without
    /// consulting the mute list at all.
    #[test]
    fn row_flags_mark_muted_without_list() {
        let rows_text = format!(
            "{}\n{}",
            bookmark_line_extra("f1", "plain synthetic post", photo_json(), "\"muted\":true"),
            bookmark_line_extra(
                "f2",
                "another plain post",
                photo_json(),
                "\"mute_check_failed\":true"
            )
        );
        let (_rows_dir, rows) = rows_file(&rows_text);
        let (_dir_guard, dir) = out_dir();
        let no_fetch =
            |_: &str| -> Result<Vec<u8>, String> { panic!("flagged rows must not download") };
        let no_frame = |_: &[u8]| -> Result<Vec<u8>, String> {
            panic!("flagged rows must not extract frames")
        };
        run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &no_fetch,
            &no_frame,
            &no_mutes,
        )
        .unwrap();
        let manifest =
            std::fs::read_to_string(PathBuf::from(&dir).join(MANIFEST_FILE_NAME)).unwrap();
        let mut lines = manifest.lines();
        assert!(lines.next().unwrap().contains(r#""skipped":"muted""#));
        assert!(lines
            .next()
            .unwrap()
            .contains(r#""skipped":"mute_check_failed""#));
    }

    /// Sources smaller than 240px keep their pixels; the thumbnail never
    /// grows them.
    #[test]
    fn small_sources_are_never_upscaled() {
        let (_rows_dir, rows) = rows_file(&bookmark_line("tiny7", "synthetic icon", photo_json()));
        let (_dir_guard, dir) = out_dir();
        let shot = png_bytes(100, 60);
        let fetch = |_: &str| Ok(shot.clone());
        let no_frame =
            |_: &[u8]| -> Result<Vec<u8>, String> { panic!("photos never extract frames") };
        run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &fetch,
            &no_frame,
            &no_mutes,
        )
        .unwrap();
        let thumb = PathBuf::from(&dir).join("tiny7_0.jpg");
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (100, 60));
    }

    /// A second run reuses the files on disk, downloads nothing new, and
    /// rewrites an identical manifest.
    #[test]
    fn rerun_reuses_existing_thumbnails() {
        let (_rows_dir, rows) =
            rows_file(&bookmark_line("idem5", "synthetic repeat", photo_json()));
        let (_dir_guard, dir) = out_dir();
        let calls = Cell::new(0usize);
        let shot = png_bytes(500, 500);
        let fetch = |_: &str| {
            calls.set(calls.get() + 1);
            Ok(shot.clone())
        };
        let no_frame =
            |_: &[u8]| -> Result<Vec<u8>, String> { panic!("photos never extract frames") };
        let args = argv(&["--rows", &rows, "--out-dir", &dir]);
        let first = run(&args, &no_env, &fetch, &no_frame, &no_mutes).unwrap();
        assert!(first.contains("1 thumbnail(s), 0 cached"), "{first}");
        let manifest_path = PathBuf::from(&dir).join(MANIFEST_FILE_NAME);
        let before = std::fs::read_to_string(&manifest_path).unwrap();
        let second = run(&args, &no_env, &fetch, &no_frame, &no_mutes).unwrap();
        assert!(second.contains("0 thumbnail(s), 1 cached"), "{second}");
        assert_eq!(calls.get(), 1, "the rerun must not download");
        assert_eq!(std::fs::read_to_string(&manifest_path).unwrap(), before);
    }

    /// Sources with alpha encode as PNG so transparency survives the
    /// thumbnail.
    #[test]
    fn alpha_sources_encode_png() {
        let (_rows_dir, rows) = rows_file(&bookmark_line("glass1", "synthetic logo", photo_json()));
        let (_dir_guard, dir) = out_dir();
        let shot = rgba_png_bytes(500, 500);
        let fetch = |_: &str| Ok(shot.clone());
        let no_frame =
            |_: &[u8]| -> Result<Vec<u8>, String> { panic!("photos never extract frames") };
        run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &fetch,
            &no_frame,
            &no_mutes,
        )
        .unwrap();
        let thumb = PathBuf::from(&dir).join("glass1_0.png");
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (240, 240));
        let head = std::fs::read(&thumb).unwrap();
        assert_eq!(
            &head[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    /// Media URLs off the X media hosts are skipped before any download
    /// starts.
    #[test]
    fn unsafe_urls_are_skipped() {
        let evil = "{\"kind\":\"photo\",\"url\":\"https://evil.example/x.jpg\"}";
        let (_rows_dir, rows) = rows_file(&bookmark_line("evil1", "synthetic post", evil));
        let (_dir_guard, dir) = out_dir();
        let no_fetch =
            |_: &str| -> Result<Vec<u8>, String> { panic!("unsafe URLs must not be fetched") };
        let no_frame =
            |_: &[u8]| -> Result<Vec<u8>, String> { panic!("unsafe URLs must not reach ffmpeg") };
        let summary = run(
            &argv(&["--rows", &rows, "--out-dir", &dir]),
            &no_env,
            &no_fetch,
            &no_frame,
            &no_mutes,
        )
        .unwrap();
        assert!(summary.contains("unsafe media URL"), "{summary}");
    }

    /// First-frame extraction without the ffmpeg binary fails closed, and
    /// empty input is refused without spawning anything.
    #[test]
    fn missing_ffmpeg_fails_closed() {
        let err = extract_first_frame(&[1, 2, 3], Path::new("/nonexistent/ffmpeg")).unwrap_err();
        assert_eq!(err, "ffmpeg not available");
        assert_eq!(
            extract_first_frame(&[], Path::new(FFMPEG_BIN)).unwrap_err(),
            "empty video"
        );
    }

    /// Unknown flags and empty values are usage errors, never silent
    /// defaults; both `--flag value` and `--flag=value` forms parse.
    #[test]
    fn rejects_unknown_args() {
        let no_fetch = |_: &str| Err::<Vec<u8>, String>("unreachable".to_string());
        let no_frame = |_: &[u8]| Err::<Vec<u8>, String>("unreachable".to_string());
        assert!(run(
            &argv(&["--rows", "x", "--bogus"]),
            &no_env,
            &no_fetch,
            &no_frame,
            &no_mutes
        )
        .is_err());
        assert!(parse_args(&argv(&["--rows="])).is_err());
        let want = ThumbArgs {
            rows: Some("a".to_string()),
            out_dir: Some("b".to_string()),
        };
        assert_eq!(
            parse_args(&argv(&["--rows", "a", "--out-dir", "b"])).unwrap(),
            want
        );
        assert_eq!(
            parse_args(&argv(&["--rows=a", "--out-dir=b"])).unwrap(),
            want
        );
    }

    /// The output directory prefers the flag over the env var over the
    /// eval-private default, and fails without any home.
    #[test]
    fn out_dir_prefers_flag_over_env_over_home() {
        let env = |k: &str| match k {
            "HOME" => Some("/home/synth".to_string()),
            "ANCIENTOS_THUMBS_DIR" => Some("/tmp/synth-thumbs".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_out_dir(Some("/flag/dir"), &env).unwrap(),
            PathBuf::from("/flag/dir")
        );
        assert_eq!(
            resolve_out_dir(None, &env).unwrap(),
            PathBuf::from("/tmp/synth-thumbs")
        );
        let home_only = |k: &str| match k {
            "HOME" => Some("/home/synth".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_out_dir(None, &home_only).unwrap(),
            PathBuf::from("/home/synth/.ancientos-hackathon/eval-private/thumbs")
        );
        assert!(resolve_out_dir(None, &no_env).is_err());
    }

    /// Thumbnail stems keep only safe characters so hostile post ids cannot
    /// escape the output directory or collide with flags.
    #[test]
    fn stem_sanitizes_post_ids() {
        assert_eq!(safe_stem("abc123", 0), "abc123_0");
        let hostile = safe_stem("../../etc/x", 2);
        assert!(!hostile.contains(['/', '.', ':']), "{hostile}");
        assert!(hostile.ends_with("_2"), "{hostile}");
        assert_eq!(safe_stem("", 0), "post_0");
    }
}
