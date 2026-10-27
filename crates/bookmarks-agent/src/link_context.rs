//! Linked-page and media enrichment for the try-it classifiers.
//!
//! Often the thing to try is in the linked page or the video, not the post
//! text. [`enrich`] builds a text block from a stored bookmark's expanded
//! link URLs plus its media alt text and video captions, so the classifiers
//! can decide on the post text plus that context (see [`cascade_input`]).
//!
//! Linked pages: plain HTTP GET with the repo's existing client (`ureq`),
//! timeout [`TIMEOUT_SECS`], at most [`MAX_LINKS_PER_POST`] links per post,
//! at most [`MAX_BODY_BYTES`] read per body, `text/html` or `text/plain`
//! only, at most [`MAX_REDIRECTS`] redirects. Extraction keeps the title,
//! meta description / `og:description`, and readable body text (script,
//! style, and nav stripped, whitespace collapsed), capped at
//! [`MAX_LINK_CHARS`] chars per link.
//!
//! Media: stored alt text plus, for videos, a caption/subtitle track URL
//! when the stored data carries one. Caption URLs on X API hosts would need
//! a paid API call and are skipped unless `ANCIENTOS_ALLOW_PAID=1`. This
//! module makes no X API calls at all, and never writes anything.
//!
//! Gates: network fetches need the opt-in `ANCIENTOS_FETCH_LINKS=1`
//! (default off means no network and empty enrichment). Everything fetched
//! is sanitized with [`crate::sanitize`] before it could reach a paid model,
//! and private bookmarks (the same rule [`crate::translate`] uses: the
//! post text is not [`Sensitivity::Sanitized`](ancientos_router::Sensitivity))
//! are enriched for local use only — [`cascade_input`] withholds their
//! enrichment from the cascade text that paid models may see.
//!
//! Cache: fetched text lives on disk as JSON keyed by SHA-256 of the URL
//! (same pattern as the translate cache), TTL [`CACHE_TTL_SECS`], file mode
//! 0600. A cache hit makes no network call.
//!
//! Fail closed everywhere: any fetch or parse error skips that source with
//! a recorded reason in [`Enrichment::notes`]. [`enrich`] never panics and
//! never blocks the decision.
use crate::fixture::Bookmark;
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Opt-in for network fetches: must equal `"1"`, default off.
pub const FETCH_ENV_VAR: &str = "ANCIENTOS_FETCH_LINKS";
/// Cache file name in the data dir (JSON, no secrets).
pub const CACHE_FILE_NAME: &str = "link_context_cache.json";
/// Env override for the cache path (tests, custom data dirs).
pub const CACHE_ENV_VAR: &str = "LINK_CONTEXT_CACHE_PATH";
/// Cache time-to-live: 7 days.
pub const CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
/// Max links fetched per post.
pub const MAX_LINKS_PER_POST: usize = 3;
/// Max enrichment chars kept per link (after sanitizing).
pub const MAX_LINK_CHARS: usize = 2000;
/// Max caption chars kept per video (after sanitizing).
pub const MAX_CAPTION_CHARS: usize = 2000;
/// Max body bytes read per fetch.
pub const MAX_BODY_BYTES: usize = 512 * 1024;
/// Whole-request timeout, in seconds.
pub const TIMEOUT_SECS: u64 = 8;
/// Max redirects followed per fetch.
pub const MAX_REDIRECTS: u32 = 3;
/// Header separating post text from enrichment in cascade input.
pub const CONTEXT_HEADER: &str = "Linked/Media context:";

/// One cached fetch: sanitized, capped text plus when it was fetched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    pub text: String,
    /// Unix seconds when the fetch ran.
    pub fetched_at: u64,
}

/// Enrichment for one bookmark: the context block, whether it is local-only,
/// and one recorded reason per skipped source.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Enrichment {
    /// Sanitized context block, `""` when there is nothing (or fetching is
    /// off). Safe to show locally; see [`cascade_input`] for paid-model use.
    pub text: String,
    /// True for private bookmarks: local use only, never sent to a paid
    /// model. Same rule [`crate::translate`] uses.
    pub local_only: bool,
    /// Skip reasons (`link 2 skipped: timeout`, `captions unavailable`,
    /// ...). Index-based, never carrying URLs or page text.
    pub notes: Vec<String>,
}

/// True when link fetching is opted in (`ANCIENTOS_FETCH_LINKS=1`).
pub fn is_fetch_enabled() -> bool {
    std::env::var(FETCH_ENV_VAR).is_ok_and(|v| v == "1")
}

/// Default cache path: `$LINK_CONTEXT_CACHE_PATH` or
/// `$ANCIENTOS_HOME/link_context_cache.json`.
pub fn default_cache_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var(CACHE_ENV_VAR) {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(CACHE_FILE_NAME))
}

/// Cache path alongside `log_path` (keeps tests and custom data dirs
/// isolated), falling back to [`default_cache_path`].
pub fn cache_path_for_log(log_path: &Path) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var(CACHE_ENV_VAR) {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            return Ok(parent.join(CACHE_FILE_NAME));
        }
    }
    default_cache_path()
}

/// Load the disk cache. A missing or corrupt file means empty (fail closed:
/// refetch rather than trust it).
pub fn load_cache(path: &Path) -> HashMap<String, CacheEntry> {
    match std::fs::read_to_string(path) {
        Err(_) => HashMap::new(),
        Ok(text) => {
            if text.trim().is_empty() {
                return HashMap::new();
            }
            serde_json::from_str(&text).unwrap_or_default()
        }
    }
}

/// Save the disk cache atomically (parents created 0700, file 0600).
pub fn save_cache(path: &Path, cache: &HashMap<String, CacheEntry>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("link context cache dir failed: {e}"))?;
        }
    }
    let data =
        serde_json::to_string(cache).map_err(|e| format!("link context cache encode: {e}"))?;
    crate::token_store::atomic_write_0600(path, data.as_bytes())
        .map_err(|e| format!("link context cache write failed: {e}"))?;
    Ok(())
}

/// True when the entry is still inside the 7-day window.
pub fn is_fresh(entry: &CacheEntry, now: u64) -> bool {
    now.saturating_sub(entry.fetched_at) < CACHE_TTL_SECS
}

/// Host part of a URL authority: strips a `:port` (IPv6 literals in
/// brackets keep their brackets; they never match the skip lists).
fn host_of(authority: &str) -> &str {
    if authority.starts_with('[') {
        match authority.find(']') {
            Some(i) => &authority[..i + 1],
            None => authority,
        }
    } else {
        authority.split(':').next().unwrap_or(authority)
    }
}

/// True for X hosts (any case, optional trailing dot or subdomain).
fn is_x_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "x.com" || h.ends_with(".x.com") || h == "twitter.com" || h.ends_with(".twitter.com")
}

/// True for the `t.co` short-link host.
fn is_tco_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "t.co" || h.ends_with(".t.co")
}

/// Accept one stored URL for fetching: trimmed http(s), length-capped, no
/// whitespace or controls, no userinfo. Skips unexpanded `t.co` links and
/// x.com/twitter.com status links (fail closed: `None` means skip).
fn accept_link_url(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 2048 {
        return None;
    }
    let rest = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))?;
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let host = host_of(authority);
    if is_tco_host(host) {
        return None;
    }
    if is_x_host(host) && rest[end..].to_ascii_lowercase().contains("/status/") {
        return None;
    }
    Some(s.to_string())
}

/// Links to fetch for a bookmark: the stored expanded URLs in order,
/// deduped, skipping unexpanded `t.co`, X status links, and non-http(s),
/// at most [`MAX_LINKS_PER_POST`].
pub fn link_urls(b: &Bookmark) -> Vec<String> {
    let mut out = Vec::new();
    for raw in &b.urls {
        if out.len() >= MAX_LINKS_PER_POST {
            break;
        }
        if let Some(url) = accept_link_url(raw) {
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

/// True when fetching `url` would need a paid X API call (caption URLs on
/// X API hosts). Such URLs are skipped unless `ANCIENTOS_ALLOW_PAID=1`.
/// Garbage input is not a paid host (fetch validation rejects it anyway).
pub fn caption_needs_paid(url: &str) -> bool {
    let s = url.trim();
    let Some(rest) = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
    else {
        return false;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = host_of(&rest[..end])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    host == "api.x.com" || host == "api.twitter.com"
}

/// True when a transport error was caused by a timeout: ureq wraps the
/// deadline expiry as an `io::Error` of kind `TimedOut` in the source
/// chain (the message alone does not name it).
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

/// Map a fetch error to a short skip reason. Static strings plus the HTTP
/// status only: never URLs, hostnames, or body text.
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

/// Plain HTTP GET with `timeout`, at most [`MAX_REDIRECTS`] redirects,
/// reading at most [`MAX_BODY_BYTES`]. Returns the body plus the lowercase
/// media type (`text/html`, ...). Any error is a skip reason, never a panic.
fn fetch_body(url: &str, timeout: Duration) -> Result<(String, String), String> {
    if accept_link_url(url).is_none() && !is_captions_fetchable_url(url) {
        return Err("invalid url".to_string());
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(MAX_REDIRECTS)
        .build();
    let resp = agent.get(url).call().map_err(map_fetch_error)?;
    let media_type = resp
        .header("content-type")
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_BODY_BYTES as u64)
        .read_to_end(&mut buf)
        .map_err(|_| "unreadable body".to_string())?;
    Ok((String::from_utf8_lossy(&buf).into_owned(), media_type))
}

/// Caption URLs accept the same http(s) shape as links but without the
/// `t.co` / X-status skip list (tracks live on media hosts).
fn is_captions_fetchable_url(raw: &str) -> bool {
    let s = raw.trim();
    if s.is_empty() || s.len() > 2048 {
        return false;
    }
    let Some(rest) = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
    else {
        return false;
    };
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    !authority.is_empty() && !authority.contains('@')
}

/// Fetch one linked page: `text/html` or `text/plain` only, extracted to
/// title plus meta description plus body text, sanitized and capped at
/// [`MAX_LINK_CHARS`] chars. Any failure is a skip reason.
pub fn fetch_page(url: &str, timeout: Duration) -> Result<String, String> {
    let (body, media_type) = fetch_body(url, timeout)?;
    let text = match media_type.as_str() {
        "text/html" => page_snippet(&extract_page(&body)),
        "text/plain" => collapse_whitespace(&body),
        _ => return Err("unsupported content type".to_string()),
    };
    let clean = truncate_chars(&crate::sanitize::sanitize_uncapped(&text), MAX_LINK_CHARS);
    if clean.trim().is_empty() {
        return Err("no readable text".to_string());
    }
    Ok(clean)
}

/// Fetch one caption/subtitle track: text tracks only (`text/*` plus
/// `application/x-subrip`), reduced to cue text, sanitized and capped at
/// [`MAX_CAPTION_CHARS`] chars. Any failure is a skip reason.
pub fn fetch_caption(url: &str, timeout: Duration) -> Result<String, String> {
    let (body, media_type) = fetch_body(url, timeout)?;
    if !(media_type.starts_with("text/") || media_type == "application/x-subrip") {
        return Err("unsupported content type".to_string());
    }
    let clean = truncate_chars(
        &crate::sanitize::sanitize_uncapped(&caption_text(&body)),
        MAX_CAPTION_CHARS,
    );
    if clean.trim().is_empty() {
        return Err("no readable text".to_string());
    }
    Ok(clean)
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn find_bytes(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Remove `<!-- ... -->` comments. An unterminated comment drops the rest
/// (fail closed: commented-out markup must not leak into the text).
fn strip_comments(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"<!--") {
            match find_bytes(bytes, i + 4, b"-->") {
                Some(end) => i = end + 3,
                None => break,
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Lowercase ASCII tag name after `<` (skipping an optional `/`), plus
/// whether it was a closing tag. `None` when `<` starts no tag.
fn tag_name_at(bytes: &[u8], lt: usize) -> Option<(String, bool)> {
    let mut i = lt + 1;
    let mut closing = false;
    if bytes.get(i) == Some(&b'/') {
        closing = true;
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((
        String::from_utf8_lossy(&bytes[start..i]).to_ascii_lowercase(),
        closing,
    ))
}

/// Remove whole elements (`script`, `style`, `nav`) with their content.
/// Nesting of the same element is depth-counted; an unterminated element
/// drops the rest. Other markup passes through untouched.
fn strip_elements(html: &str, names: &[&str]) -> String {
    let bytes = html.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let Some((name, closing)) = tag_name_at(bytes, i) else {
            out.push(bytes[i]);
            i += 1;
            continue;
        };
        if closing || !names.contains(&name.as_str()) {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let Some(tag_end) = find_bytes(bytes, i, b">") else {
            break;
        };
        if bytes[i..tag_end].ends_with(b"/") {
            i = tag_end + 1;
            continue;
        }
        // Skip to the matching close tag, counting nested opens.
        let mut depth = 1;
        let mut j = tag_end + 1;
        let mut skipped_to = None;
        while j < bytes.len() {
            if bytes[j] != b'<' {
                j += 1;
                continue;
            }
            let Some((inner, inner_closing)) = tag_name_at(bytes, j) else {
                j += 1;
                continue;
            };
            if inner != name {
                j += 1;
                continue;
            }
            let Some(inner_end) = find_bytes(bytes, j, b">") else {
                break;
            };
            let self_closing = !inner_closing && bytes[j..inner_end].ends_with(b"/");
            if inner_closing {
                depth -= 1;
                if depth == 0 {
                    skipped_to = Some(inner_end + 1);
                    break;
                }
            } else if !self_closing {
                depth += 1;
            }
            j = inner_end + 1;
        }
        match skipped_to {
            Some(end) => i = end,
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Remove every `<...>` tag (each replaced by a space so words cannot join).
/// A `<` with no later `>` is kept as text.
fn strip_tags(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match find_bytes(bytes, i, b">") {
            Some(end) => {
                out.push(b' ');
                i = end + 1;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode common HTML entities plus numeric character references. Unknown
/// or malformed entities are kept literally (never dropped, never guessed).
fn decode_entities(text: &str) -> String {
    // Byte output: `&` and `;` are ASCII, so every slice below lands on a
    // char boundary, and non-ASCII bytes pass through untouched.
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let rest = &text[i..];
        let Some(semi) = rest.find(';').filter(|s| *s <= 12) else {
            out.push(b'&');
            i += 1;
            continue;
        };
        let entity = &rest[1..semi];
        let decoded: Option<String> = match entity {
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" | "#39" | "#x27" | "#X27" => Some("'".to_string()),
            "nbsp" => Some(" ".to_string()),
            _ if entity.len() > 1 && entity.starts_with('#') => decode_numeric(entity),
            _ => None,
        };
        match decoded {
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                i += semi + 1;
            }
            None => {
                out.push(b'&');
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode `&#123;` / `&#x1F;` references. Control characters and invalid
/// values are kept literally by the caller (`None`).
fn decode_numeric(entity: &str) -> Option<String> {
    let digits = entity.strip_prefix('#')?;
    let code = if let Some(hex) = digits
        .strip_prefix('x')
        .or_else(|| digits.strip_prefix('X'))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        digits.parse::<u32>().ok()?
    };
    let c = char::from_u32(code)?;
    if c.is_control() {
        return None;
    }
    Some(c.to_string())
}

/// Text of the first `<title>...</title>` (case-insensitive), if any.
fn extract_title(html: &str) -> Option<String> {
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let lt = find_bytes(bytes, i, b"<")?;
        match tag_name_at(bytes, lt) {
            Some((name, false)) if name == "title" => {
                let content_start = find_bytes(bytes, lt, b">")? + 1;
                let close = find_bytes(&bytes.to_ascii_lowercase(), content_start, b"</title")?;
                let end = find_bytes(bytes, close, b">")? + 1;
                let raw = &html[content_start..close];
                let text = collapse_whitespace(&decode_entities(&strip_tags(raw)));
                if text.trim().is_empty() {
                    i = end;
                    continue;
                }
                return Some(text);
            }
            _ => i = lt + 1,
        }
    }
    None
}

/// Attribute pairs of one `<meta ...>` tag span: lowercase names, quoted
/// values only (double or single quotes). Malformed attributes are skipped,
/// never aborting the tag.
fn meta_attrs(tag: &str) -> Vec<(String, String)> {
    let bytes = tag.as_bytes();
    let mut attrs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric()
                || bytes[i] == b'-'
                || bytes[i] == b'_'
                || bytes[i] == b':')
        {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let name = String::from_utf8_lossy(&bytes[start..i]).to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let quote = bytes.get(i).copied();
        if quote != Some(b'"') && quote != Some(b'\'') {
            continue;
        }
        i += 1;
        let value_start = i;
        while i < bytes.len() && Some(bytes[i]) != quote {
            i += 1;
        }
        let value = String::from_utf8_lossy(&bytes[value_start..i]).into_owned();
        if i < bytes.len() {
            i += 1;
        }
        attrs.push((name, value));
    }
    attrs
}

/// First usable meta description: `name="description"` wins, else
/// `property="og:description"`. Attribute order is free.
fn extract_meta_description(html: &str) -> Option<String> {
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut og = None;
    while i < bytes.len() {
        let Some(lt) = find_bytes(bytes, i, b"<") else {
            break;
        };
        match tag_name_at(bytes, lt) {
            Some((name, false)) if name == "meta" => {
                let Some(tag_end) = find_bytes(bytes, lt, b">") else {
                    break;
                };
                let tag = &html[lt + 5..tag_end];
                let attrs = meta_attrs(tag);
                let get = |key: &str| {
                    attrs
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                };
                let content = get("content");
                if get("name").as_deref() == Some("description") {
                    if let Some(c) = content {
                        return Some(collapse_whitespace(&decode_entities(&c)));
                    }
                } else if get("property").as_deref() == Some("og:description") && og.is_none() {
                    og = content.map(|c| collapse_whitespace(&decode_entities(&c)));
                }
                i = tag_end + 1;
            }
            _ => i = lt + 1,
        }
    }
    og
}

/// Extracted page text: title, meta description, and readable body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageText {
    pub title: Option<String>,
    pub description: Option<String>,
    pub body: String,
}

/// Extract title, meta description / `og:description`, and readable body
/// text (comments plus script/style/nav removed, tags stripped, entities
/// decoded, whitespace collapsed). Never panics; garbage in means empty out.
pub fn extract_page(html: &str) -> PageText {
    let no_comments = strip_comments(html);
    let title = extract_title(&no_comments);
    let description = extract_meta_description(&no_comments);
    let no_blocks = strip_elements(&no_comments, &["script", "style", "nav"]);
    let body = collapse_whitespace(&decode_entities(&strip_tags(&no_blocks)));
    PageText {
        title,
        description,
        body,
    }
}

/// Title, description, and body joined to one context string (non-empty
/// parts only). The caller caps and sanitizes.
pub fn page_snippet(page: &PageText) -> String {
    let mut parts = Vec::new();
    if let Some(t) = page
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(t.to_string());
    }
    if let Some(d) = page
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(d.to_string());
    }
    if !page.body.trim().is_empty() {
        parts.push(page.body.trim().to_string());
    }
    collapse_whitespace(&parts.join(" "))
}

/// Reduce a caption/subtitle track (WebVTT or SRT) to spoken cue text:
/// drops the `WEBVTT` header, `NOTE` lines, timestamp lines, and SRT cue
/// numbers, strips inline tags, decodes entities, and joins cues with
/// spaces. Garbage in means empty out.
pub fn caption_text(raw: &str) -> String {
    let mut cues = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("WEBVTT") || line.starts_with("NOTE") {
            continue;
        }
        if line.contains("-->") {
            continue;
        }
        if line.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let clean = collapse_whitespace(&decode_entities(&strip_tags(line)));
        if !clean.is_empty() {
            cues.push(clean);
        }
    }
    cues.join(" ")
}

/// Build the enrichment for one bookmark, consulting `cache` first (a hit
/// makes no network call) and inserting successful fetches (the caller
/// saves). See the module docs for gates, caps, and fail-closed rules.
pub fn enrich(b: &Bookmark, cache: &mut HashMap<String, CacheEntry>) -> Enrichment {
    enrich_at(
        b,
        cache,
        crate::liveness::now_secs(),
        Duration::from_secs(TIMEOUT_SECS),
    )
}

/// [`enrich`] with an injectable clock and timeout, for tests.
pub fn enrich_at(
    b: &Bookmark,
    cache: &mut HashMap<String, CacheEntry>,
    now: u64,
    timeout: Duration,
) -> Enrichment {
    let local_only =
        crate::translate::sensitivity_of(&b.text) != ancientos_router::Sensitivity::Sanitized;
    if !is_fetch_enabled() {
        return Enrichment {
            text: String::new(),
            local_only,
            notes: vec!["fetch disabled (ANCIENTOS_FETCH_LINKS!=1)".to_string()],
        };
    }
    let mut notes = Vec::new();
    let mut parts = Vec::new();
    for (i, url) in link_urls(b).iter().enumerate() {
        let key = crate::translate::sha256_hex(url);
        if let Some(entry) = cache.get(&key) {
            if is_fresh(entry, now) && !entry.text.trim().is_empty() {
                parts.push(format!("Link: {}", entry.text));
                continue;
            }
        }
        match fetch_page(url, timeout) {
            Ok(text) => {
                cache.insert(
                    key,
                    CacheEntry {
                        text: text.clone(),
                        fetched_at: now,
                    },
                );
                parts.push(format!("Link: {text}"));
            }
            Err(reason) => notes.push(format!("link {} skipped: {reason}", i + 1)),
        }
    }
    let mut alts = Vec::new();
    for m in &b.media {
        if let Some(alt) = m.alt_text.as_deref() {
            let clean = crate::sanitize::sanitize_uncapped(alt);
            if !clean.trim().is_empty() {
                alts.push(clean);
            }
        }
    }
    if !alts.is_empty() {
        parts.push(format!("Media alt text: {}", alts.join("; ")));
    }
    let mut captions = Vec::new();
    let paid = crate::translate::is_paid_allowed();
    for m in &b.media {
        let Some(raw) = m.caption_url.as_deref() else {
            continue;
        };
        let url = raw.trim();
        if caption_needs_paid(url) && !paid {
            notes.push("captions skipped: paid API required (ANCIENTOS_ALLOW_PAID!=1)".to_string());
            continue;
        }
        let key = crate::translate::sha256_hex(url);
        if let Some(entry) = cache.get(&key) {
            if is_fresh(entry, now)
                && !entry.text.trim().is_empty()
                && !captions.contains(&entry.text)
            {
                captions.push(entry.text.clone());
                continue;
            }
        }
        match fetch_caption(url, timeout) {
            Ok(text) => {
                cache.insert(
                    key,
                    CacheEntry {
                        text: text.clone(),
                        fetched_at: now,
                    },
                );
                if !captions.contains(&text) {
                    captions.push(text);
                }
            }
            Err(reason) => notes.push(format!("captions skipped: {reason}")),
        }
    }
    if !captions.is_empty() {
        parts.push(format!("Video captions: {}", captions.join(" ")));
    }
    let has_video = b
        .media
        .iter()
        .any(|m| m.kind == "video" || m.kind == "animated_gif");
    if has_video && captions.is_empty() {
        notes.push("captions unavailable".to_string());
    }
    Enrichment {
        text: parts.join("\n\n"),
        local_only,
        notes,
    }
}

/// True when the enrichment may join cascade input: non-empty and not
/// local-only (private enrichment never reaches a paid model).
pub fn will_append(e: &Enrichment) -> bool {
    !e.local_only && !e.text.trim().is_empty()
}

/// Cascade input text: post text plus `"\n\nLinked/Media context:\n"` plus
/// the enrichment when [`will_append`] holds, else the post text unchanged.
/// Topic filtering and the paid stages all see this one text.
pub fn cascade_input(post_text: &str, e: &Enrichment) -> String {
    if will_append(e) {
        format!("{post_text}\n\n{CONTEXT_HEADER}\n{}", e.text)
    } else {
        post_text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn bm(id: &str, text: &str) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: text.into(),
            author_username: Some(format!("syn_{id}")),
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: Some("en".into()),
            urls: Vec::new(),
        }
    }

    /// Post text with no strippable tokens: [`crate::translate`] treats it as
    /// sanitized (may reach paid models once enriched).
    const SANITIZED_TEXT: &str =
        "[synthetic] A short post about a morning stretch routine with a link.";
    /// Post text carrying a URL: sensitive, so local-only enrichment.
    const PRIVATE_TEXT: &str =
        "[synthetic] See https://example.invalid/guide for the full routine.";

    type Hits = Arc<Mutex<usize>>;

    /// Read one request head (through the blank line); returns the raw head.
    fn read_head(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut head = Vec::new();
        let mut buf = [0u8; 1];
        while head.len() < 65536 {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    head.push(buf[0]);
                    if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
                        break;
                    }
                }
            }
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    struct Route {
        path: String,
        status: u16,
        content_type: Option<String>,
        extra_headers: Vec<(String, String)>,
        body: Vec<u8>,
        delay: Duration,
    }

    /// Mock HTTP server on 127.0.0.1: replies by request path (first line),
    /// counts hits, serves until `max_hits`, then exits (or after a few
    /// seconds idle, so zero-request tests never hang).
    fn mock_server(routes: Vec<Route>, max_hits: usize) -> (String, Hits) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let url = format!("http://{}", listener.local_addr().expect("mock addr"));
        listener.set_nonblocking(true).expect("nonblocking");
        let hits: Hits = Arc::new(Mutex::new(0));
        let hits_thread = Arc::clone(&hits);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if *hits_thread.lock().expect("hits lock") >= max_hits || Instant::now() > deadline
                {
                    break;
                }
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                *hits_thread.lock().expect("hits lock") += 1;
                let head = read_head(&mut stream);
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let route = routes.iter().find(|r| r.path == path);
                if let Some(r) = route {
                    if !r.delay.is_zero() {
                        std::thread::sleep(r.delay);
                    }
                }
                let (status, content_type, extra, body) = match route {
                    Some(r) => (
                        r.status,
                        r.content_type.clone(),
                        r.extra_headers.clone(),
                        r.body.clone(),
                    ),
                    None => (
                        404,
                        Some("text/plain".to_string()),
                        Vec::new(),
                        b"nope".to_vec(),
                    ),
                };
                let reason = match status {
                    200 => "OK",
                    302 => "Found",
                    404 => "Not Found",
                    500 => "Internal Server Error",
                    _ => "Error",
                };
                let mut resp = format!("HTTP/1.1 {status} {reason}\r\n");
                if let Some(ct) = content_type {
                    resp.push_str(&format!("Content-Type: {ct}\r\n"));
                }
                for (k, v) in &extra {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ));
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (url, hits)
    }

    fn page_route(path: &str, body: &str) -> Route {
        Route {
            path: path.to_string(),
            status: 200,
            content_type: Some("text/html; charset=utf-8".to_string()),
            extra_headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            delay: Duration::ZERO,
        }
    }

    fn hits_of(hits: &Hits) -> usize {
        *hits.lock().expect("hits lock")
    }

    /// A base URL with nothing listening: any attempted call fails fast.
    fn closed_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let url = format!("http://{}", listener.local_addr().expect("probe addr"));
        drop(listener);
        url
    }

    const SAMPLE_HTML: &str = r#"<!doctype html>
<html><head><title>Synthetic Stretch Guide</title>
<meta name="description" content="A short routine with breathing cues.">
<meta property="og:description" content="OG fallback must lose.">
<style>body { color: red; }</style>
<script>var x = "secret script text";</script>
</head><body>
<!-- a comment that must not leak -->
<nav><a href="/home">Home</a> <a href="/about">About</a></nav>
<h1>Morning stretch</h1>
<p>Roll your shoulders &amp; breathe slowly. 5&#37; effort.</p>
</body></html>"#;

    #[test]
    fn extracts_title_description_and_body() {
        let page = extract_page(SAMPLE_HTML);
        assert_eq!(page.title.as_deref(), Some("Synthetic Stretch Guide"));
        assert_eq!(
            page.description.as_deref(),
            Some("A short routine with breathing cues.")
        );
        assert!(page.body.contains("Morning stretch"), "body: {}", page.body);
        assert!(
            page.body
                .contains("Roll your shoulders & breathe slowly. 5% effort."),
            "body: {}",
            page.body
        );
        let snippet = page_snippet(&page);
        assert!(snippet.contains("Synthetic Stretch Guide"), "{snippet}");
        assert!(snippet.contains("breathing cues"), "{snippet}");
        assert!(snippet.contains("Roll your shoulders"), "{snippet}");
    }

    #[test]
    fn strips_script_style_nav_and_comments() {
        let page = extract_page(SAMPLE_HTML);
        for banned in [
            "secret script text",
            "color: red",
            "Home",
            "About",
            "comment that must not leak",
        ] {
            assert!(
                !page.body.contains(banned),
                "{banned:?} leaked into {:?}",
                page.body
            );
        }
        // Case-insensitive tags with attributes, plus nesting.
        let page = extract_page(
            "<SCRIPT type=\"x\">a<SCRIPT>b</SCRIPT>c</SCRIPT><P>kept</P><NAV id=n>x</NAV>",
        );
        assert_eq!(page.body, "kept");
        assert!(page.title.is_none());
        assert!(page.description.is_none());
    }

    #[test]
    fn meta_description_wins_over_og_with_free_attribute_order() {
        // Reversed order, single quotes: og first, description second.
        let page = extract_page(
            "<meta content='OG text here' property='og:description'>\
             <meta content = \"Real description here\" name = \"description\">",
        );
        assert_eq!(page.description.as_deref(), Some("Real description here"));
        // og alone is used when no description exists.
        let page = extract_page("<meta property=\"og:description\" content=\"OG only here\">");
        assert_eq!(page.description.as_deref(), Some("OG only here"));
        // Empty content is ignored.
        let page = extract_page("<meta name=\"description\" content=\"  \">");
        assert!(page.description.is_none());
    }

    #[test]
    fn extraction_never_panics_on_garbage() {
        for html in [
            "",
            "   ",
            "<<<not html>>>",
            "<title>unterminated",
            "<title></title>",
            "<meta name=description content=noquotes>",
            "<meta name=\"description\" content=\"unterminated>",
            "<script>unterminated script",
            "<nav><nav>nested unterminated",
            "<!-- unterminated comment",
            "5 < 6 and 7 > 6",
            "emoji kept: \u{1F600} caf\u{E9}",
            "&bogus; &; &#xZZ; &#99999999; &amp",
        ] {
            let page = extract_page(html);
            let _ = page_snippet(&page);
        }
        assert_eq!(extract_page("").body, "");
        assert_eq!(extract_page("5 < 6").body, "5 < 6");
        assert_eq!(
            extract_page("caf\u{E9} \u{1F600}").body,
            "caf\u{E9} \u{1F600}"
        );
    }

    #[test]
    fn caption_text_reduces_vtt_and_srt_to_cues() {
        let vtt = "WEBVTT\n\nNOTE a comment\n\n00:00:00.000 --> 00:00:02.000\nRoll your <b>shoulders</b>\n\n00:00:02.000 --> 00:00:04.000 align:center\nand breathe &amp; relax\n";
        assert_eq!(caption_text(vtt), "Roll your shoulders and breathe & relax");
        let srt = "1\n00:00:00,000 --> 00:00:02,000\nFirst cue\n\n2\n00:00:02,000 --> 00:00:04,000\nSecond cue\n";
        assert_eq!(caption_text(srt), "First cue Second cue");
        assert_eq!(caption_text(""), "");
        assert_eq!(caption_text("WEBVTT\n"), "");
    }

    #[test]
    fn link_selection_skips_and_caps() {
        let mut b = bm("u1", SANITIZED_TEXT);
        b.urls = vec![
            "https://example.invalid/guide".into(),
            "http://example.invalid/plain".into(),
            "https://t.co/abc123".into(),
            "https://www.t.co/abc123".into(),
            "https://x.com/syn_user/status/123".into(),
            "https://www.twitter.com/syn_user/status/456?x=1".into(),
            "https://mobile.twitter.com/i/status/789".into(),
            "ftp://example.invalid/nope".into(),
            "javascript:alert(1)".into(),
            "https://user@example.invalid/creds".into(),
            "https://example.invalid/guide".into(),
            "not a url".into(),
            String::new(),
        ];
        assert_eq!(
            link_urls(&b),
            vec![
                "https://example.invalid/guide".to_string(),
                "http://example.invalid/plain".to_string(),
            ]
        );
        // Non-status X links are kept (only status links are skipped).
        let mut b = bm("u2", SANITIZED_TEXT);
        b.urls = vec!["https://x.com/i/articles/synthetic".into()];
        assert_eq!(
            link_urls(&b),
            vec!["https://x.com/i/articles/synthetic".to_string()]
        );
        // At most three links per post.
        let mut b = bm("u3", SANITIZED_TEXT);
        b.urls = (1..=5)
            .map(|i| format!("https://example.invalid/p{i}"))
            .collect();
        assert_eq!(link_urls(&b).len(), MAX_LINKS_PER_POST);
        assert_eq!(MAX_LINKS_PER_POST, 3);
    }

    #[test]
    fn caption_paid_gate_matches_x_api_hosts_only() {
        assert!(caption_needs_paid("https://api.x.com/2/tweets/1"));
        assert!(caption_needs_paid("http://api.twitter.com/2/x"));
        assert!(!caption_needs_paid("https://video.twimg.com/x.vtt"));
        assert!(!caption_needs_paid("https://example.invalid/x.vtt"));
        assert!(!caption_needs_paid("https://x.com/i/status/1"));
        assert!(!caption_needs_paid("not a url"));
        assert!(!caption_needs_paid(""));
    }

    #[test]
    fn fetch_page_reads_html_and_plain_but_skips_other_types() {
        let (base, hits) = mock_server(
            vec![
                page_route("/page", SAMPLE_HTML),
                Route {
                    path: "/plain".to_string(),
                    status: 200,
                    content_type: Some("text/plain".to_string()),
                    extra_headers: Vec::new(),
                    body: b"plain body   with  spacing".to_vec(),
                    delay: Duration::ZERO,
                },
                Route {
                    path: "/data".to_string(),
                    status: 200,
                    content_type: Some("application/json".to_string()),
                    extra_headers: Vec::new(),
                    body: b"{}".to_vec(),
                    delay: Duration::ZERO,
                },
                Route {
                    path: "/notype".to_string(),
                    status: 200,
                    content_type: None,
                    extra_headers: Vec::new(),
                    body: b"body".to_vec(),
                    delay: Duration::ZERO,
                },
            ],
            10,
        );
        let html = fetch_page(&format!("{base}/page"), Duration::from_secs(5)).unwrap();
        assert!(html.contains("Synthetic Stretch Guide"), "{html}");
        assert!(html.contains("Roll your shoulders"), "{html}");
        assert!(!html.contains("secret script text"), "{html}");
        let plain = fetch_page(&format!("{base}/plain"), Duration::from_secs(5)).unwrap();
        assert_eq!(plain, "plain body with spacing");
        assert_eq!(
            fetch_page(&format!("{base}/data"), Duration::from_secs(5)),
            Err("unsupported content type".to_string())
        );
        assert_eq!(
            fetch_page(&format!("{base}/notype"), Duration::from_secs(5)),
            Err("unsupported content type".to_string())
        );
        assert_eq!(hits_of(&hits), 4);
    }

    #[test]
    fn fetch_page_caps_long_pages_and_huge_bodies() {
        let long_body = format!("<title>T</title><p>{}</p>", "word ".repeat(5000));
        let huge_body = "z".repeat(600 * 1024);
        let (base, _) = mock_server(
            vec![
                page_route("/long", &long_body),
                Route {
                    path: "/huge".to_string(),
                    status: 200,
                    content_type: Some("text/plain".to_string()),
                    extra_headers: Vec::new(),
                    body: huge_body.into_bytes(),
                    delay: Duration::ZERO,
                },
            ],
            10,
        );
        let capped = fetch_page(&format!("{base}/long"), Duration::from_secs(5)).unwrap();
        assert!(capped.chars().count() <= MAX_LINK_CHARS, "{}", capped.len());
        assert_eq!(MAX_LINK_CHARS, 2000);
        assert!(capped.starts_with('T'), "{capped}");
        // A 600 KiB body is truncated at read time, never an error.
        let huge = fetch_page(&format!("{base}/huge"), Duration::from_secs(5)).unwrap();
        assert!(huge.chars().count() <= MAX_LINK_CHARS, "{}", huge.len());
        assert_eq!(MAX_BODY_BYTES, 512 * 1024);
    }

    #[test]
    fn fetch_page_follows_a_few_redirects_but_caps_them() {
        let redirect = |path: &str, to: &str| Route {
            path: path.to_string(),
            status: 302,
            content_type: None,
            extra_headers: vec![("Location".to_string(), to.to_string())],
            body: Vec::new(),
            delay: Duration::ZERO,
        };
        let (base, _) = mock_server(
            vec![
                redirect("/short", "/final"),
                page_route("/final", "<title>Final</title><p>reached</p>"),
                redirect("/l1", "/l2"),
                redirect("/l2", "/l3"),
                redirect("/l3", "/l4"),
                redirect("/l4", "/final"),
            ],
            20,
        );
        // One redirect is followed.
        let ok = fetch_page(&format!("{base}/short"), Duration::from_secs(5)).unwrap();
        assert!(ok.contains("Final"), "{ok}");
        // Four redirects exceed the cap of three: skipped, not followed.
        assert_eq!(MAX_REDIRECTS, 3);
        assert_eq!(
            fetch_page(&format!("{base}/l1"), Duration::from_secs(5)),
            Err("too many redirects".to_string())
        );
    }

    #[test]
    fn fetch_errors_are_skip_reasons_not_panics() {
        let (base, _) = mock_server(
            vec![
                Route {
                    path: "/boom".to_string(),
                    status: 500,
                    content_type: Some("text/plain".to_string()),
                    extra_headers: Vec::new(),
                    body: b"down".to_vec(),
                    delay: Duration::ZERO,
                },
                Route {
                    path: "/slow".to_string(),
                    status: 200,
                    content_type: Some("text/plain".to_string()),
                    extra_headers: Vec::new(),
                    body: b"late".to_vec(),
                    delay: Duration::from_secs(2),
                },
                page_route("/empty", "<script>only script</script>"),
            ],
            10,
        );
        assert_eq!(
            fetch_page(&format!("{base}/boom"), Duration::from_secs(5)),
            Err("http status 500".to_string())
        );
        assert_eq!(
            fetch_page(&format!("{base}/missing"), Duration::from_secs(5)),
            Err("http status 404".to_string())
        );
        assert_eq!(
            fetch_page(&format!("{base}/slow"), Duration::from_millis(150)),
            Err("timeout".to_string())
        );
        assert_eq!(
            fetch_page(&format!("{base}/empty"), Duration::from_secs(5)),
            Err("no readable text".to_string())
        );
        // Refused and invalid URLs are skip reasons too.
        assert!(fetch_page(&closed_url(), Duration::from_secs(5)).is_err());
        assert_eq!(
            fetch_page("ftp://example.invalid/x", Duration::from_secs(5)),
            Err("invalid url".to_string())
        );
    }

    #[test]
    fn enrich_combines_links_media_and_captions() {
        let _env = crate::test_support::EnvHold::fetch_on();
        let (base, hits) = mock_server(
            vec![
                page_route("/page", SAMPLE_HTML),
                Route {
                    path: "/track.vtt".to_string(),
                    status: 200,
                    content_type: Some("text/vtt".to_string()),
                    extra_headers: Vec::new(),
                    body: b"WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nBend gently left\n".to_vec(),
                    delay: Duration::ZERO,
                },
            ],
            10,
        );
        let mut b = bm("e1", SANITIZED_TEXT);
        b.urls = vec![format!("{base}/page")];
        b.media = vec![crate::media::Media {
            kind: "video".into(),
            url: None,
            preview_url: Some("https://pbs.twimg.com/thumb/synthetic.jpg".into()),
            video_url: Some("https://video.twimg.com/synthetic.mp4".into()),
            alt_text: Some("a person stretching".into()),
            caption_url: Some(format!("{base}/track.vtt")),
        }];
        let mut cache = HashMap::new();
        let e = enrich_at(&b, &mut cache, 5_000_000, Duration::from_secs(5));
        assert!(!e.local_only);
        assert!(e.notes.is_empty(), "{:?}", e.notes);
        assert!(e.text.contains("Link: "), "{}", e.text);
        assert!(e.text.contains("Synthetic Stretch Guide"), "{}", e.text);
        assert!(
            e.text.contains("Media alt text: a person stretching"),
            "{}",
            e.text
        );
        assert!(
            e.text.contains("Video captions: Bend gently left"),
            "{}",
            e.text
        );
        assert_eq!(hits_of(&hits), 2);
        // Both fetches are cached under sha256(url).
        assert_eq!(cache.len(), 2);
        assert!(cache.contains_key(&crate::translate::sha256_hex(&format!("{base}/page"))));
        // Second run is all cache hits: zero new requests.
        let e2 = enrich_at(&b, &mut cache, 5_000_100, Duration::from_secs(5));
        assert_eq!(e2.text, e.text);
        assert_eq!(hits_of(&hits), 2);
    }

    #[test]
    fn enrich_opt_in_off_means_empty_and_zero_requests() {
        let _env = crate::test_support::EnvHold::paid_off();
        let (base, hits) = mock_server(vec![page_route("/page", SAMPLE_HTML)], 5);
        let mut b = bm("e2", SANITIZED_TEXT);
        b.urls = vec![format!("{base}/page")];
        b.media = vec![crate::media::Media {
            kind: "photo".into(),
            url: Some("https://pbs.twimg.com/media/synthetic.jpg".into()),
            preview_url: None,
            video_url: None,
            alt_text: Some("stored alt".into()),
            caption_url: None,
        }];
        let mut cache = HashMap::new();
        let e = enrich_at(&b, &mut cache, 5_000_000, Duration::from_secs(5));
        assert_eq!(e.text, "");
        assert!(!e.local_only);
        assert_eq!(
            e.notes,
            vec!["fetch disabled (ANCIENTOS_FETCH_LINKS!=1)".to_string()]
        );
        assert!(cache.is_empty());
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(hits_of(&hits), 0);
    }

    #[test]
    fn enrich_skips_failed_links_with_reasons_and_keeps_going() {
        let _env = crate::test_support::EnvHold::fetch_on();
        let (base, hits) = mock_server(
            vec![
                page_route("/good", "<title>Good</title><p>usable text here</p>"),
                Route {
                    path: "/bad".to_string(),
                    status: 500,
                    content_type: Some("text/plain".to_string()),
                    extra_headers: Vec::new(),
                    body: b"down".to_vec(),
                    delay: Duration::ZERO,
                },
            ],
            10,
        );
        let mut b = bm("e3", SANITIZED_TEXT);
        b.urls = vec![format!("{base}/bad"), format!("{base}/good")];
        let mut cache = HashMap::new();
        let e = enrich_at(&b, &mut cache, 5_000_000, Duration::from_secs(5));
        assert_eq!(e.notes, vec!["link 1 skipped: http status 500".to_string()]);
        assert!(e.text.contains("Good"), "{}", e.text);
        assert!(e.text.contains("usable text here"), "{}", e.text);
        assert_eq!(hits_of(&hits), 2);
    }

    #[test]
    fn enrich_records_captions_unavailable_for_videos_without_tracks() {
        let _env = crate::test_support::EnvHold::fetch_on();
        let mut b = bm("e4", SANITIZED_TEXT);
        b.media = vec![crate::media::Media {
            kind: "video".into(),
            url: None,
            preview_url: Some("https://pbs.twimg.com/thumb/synthetic.jpg".into()),
            video_url: Some("https://video.twimg.com/synthetic.mp4".into()),
            alt_text: None,
            caption_url: None,
        }];
        let mut cache = HashMap::new();
        let e = enrich_at(&b, &mut cache, 5_000_000, Duration::from_secs(5));
        assert_eq!(e.text, "");
        assert_eq!(e.notes, vec!["captions unavailable".to_string()]);
        // Photos never need captions: no note.
        let mut b = bm("e5", SANITIZED_TEXT);
        b.media = vec![crate::media::Media {
            kind: "photo".into(),
            url: Some("https://pbs.twimg.com/media/synthetic.jpg".into()),
            preview_url: None,
            video_url: None,
            alt_text: None,
            caption_url: None,
        }];
        let e = enrich_at(&b, &mut cache, 5_000_000, Duration::from_secs(5));
        assert!(e.notes.is_empty(), "{:?}", e.notes);
    }

    #[test]
    fn enrich_skips_paid_caption_hosts_unless_paid_is_allowed() {
        let _env = crate::test_support::EnvHold::paid_off();
        std::env::set_var(FETCH_ENV_VAR, "1");
        let mut b = bm("e6", SANITIZED_TEXT);
        b.media = vec![crate::media::Media {
            kind: "video".into(),
            url: None,
            preview_url: Some("https://pbs.twimg.com/thumb/synthetic.jpg".into()),
            video_url: Some("https://video.twimg.com/synthetic.mp4".into()),
            alt_text: None,
            caption_url: Some("https://api.x.com/2/synthetic/track".into()),
        }];
        let mut cache = HashMap::new();
        // No mock server at all: the paid host is skipped before any fetch.
        let e = enrich_at(&b, &mut cache, 5_000_000, Duration::from_secs(5));
        assert_eq!(e.text, "");
        assert!(
            e.notes.iter().any(|n| n.contains("paid API required")),
            "{:?}",
            e.notes
        );
        assert!(
            e.notes.contains(&"captions unavailable".to_string()),
            "{:?}",
            e.notes
        );
    }

    #[test]
    fn enrich_sanitizes_everything_before_paid_models_could_see_it() {
        let _env = crate::test_support::EnvHold::fetch_on();
        let (base, _) = mock_server(
            vec![page_route(
                "/dirty",
                "<title>Guide</title><p>Hi @syn_handle, see https://example.invalid/x or \
                 mail syn.user@example.invalid or call 555-123-4567 for steps.</p>",
            )],
            5,
        );
        let mut b = bm("e7", SANITIZED_TEXT);
        b.urls = vec![format!("{base}/dirty")];
        let mut cache = HashMap::new();
        let e = enrich_at(&b, &mut cache, 5_000_000, Duration::from_secs(5));
        for banned in [
            "@syn_handle",
            "https://example.invalid/x",
            "syn.user@example.invalid",
            "555-123-4567",
        ] {
            assert!(
                !e.text.contains(banned),
                "{banned:?} leaked into {:?}",
                e.text
            );
        }
        assert!(e.text.contains("Guide"), "{}", e.text);
        assert!(e.text.contains("for steps"), "{}", e.text);
    }

    #[test]
    fn private_bookmarks_are_enriched_locally_but_never_in_paid_prompts() {
        let _env = crate::test_support::EnvHold::fetch_on();
        let (base, _) = mock_server(
            vec![page_route(
                "/page",
                "<title>MarkerSnippet</title><p>usable text</p>",
            )],
            5,
        );
        let mut cache = HashMap::new();
        // Private post: enrichment exists for local use ...
        let mut priv_b = bm("priv1", PRIVATE_TEXT);
        priv_b.urls = vec![format!("{base}/page")];
        let e = enrich_at(&priv_b, &mut cache, 5_000_000, Duration::from_secs(5));
        assert!(e.local_only);
        assert!(e.text.contains("MarkerSnippet"), "{}", e.text);
        // ... but the cascade input (which paid models may see) is unchanged.
        assert!(!will_append(&e));
        assert_eq!(cascade_input(PRIVATE_TEXT, &e), PRIVATE_TEXT);
        let paid_prompt =
            ancientos_router::try_it_cascade::muse_prompt(&cascade_input(PRIVATE_TEXT, &e));
        assert!(!paid_prompt.contains("MarkerSnippet"), "{paid_prompt}");
        // Sanitized post: the same enrichment is appended and may be sent.
        let mut pub_b = bm("pub1", SANITIZED_TEXT);
        pub_b.urls = vec![format!("{base}/page")];
        let e = enrich_at(&pub_b, &mut cache, 5_000_100, Duration::from_secs(5));
        assert!(!e.local_only);
        assert!(will_append(&e));
        let input = cascade_input(SANITIZED_TEXT, &e);
        assert!(input.contains("MarkerSnippet"), "{input}");
        let paid_prompt = ancientos_router::try_it_cascade::muse_prompt(&input);
        assert!(paid_prompt.contains("MarkerSnippet"), "{paid_prompt}");
    }

    #[test]
    fn cascade_input_appends_only_when_allowed() {
        let post = "[synthetic] Post text here.";
        let e = Enrichment {
            text: "Link: some context".to_string(),
            local_only: false,
            notes: Vec::new(),
        };
        assert!(will_append(&e));
        assert_eq!(
            cascade_input(post, &e),
            "[synthetic] Post text here.\n\nLinked/Media context:\nLink: some context"
        );
        // Empty or whitespace-only enrichment: unchanged.
        for text in ["", "   ", "\n\t "] {
            let e = Enrichment {
                text: text.to_string(),
                local_only: false,
                notes: Vec::new(),
            };
            assert!(!will_append(&e));
            assert_eq!(cascade_input(post, &e), post);
        }
        // Local-only enrichment: unchanged.
        let e = Enrichment {
            text: "Link: private context".to_string(),
            local_only: true,
            notes: Vec::new(),
        };
        assert!(!will_append(&e));
        assert_eq!(cascade_input(post, &e), post);
    }

    #[test]
    fn stale_cache_entries_are_refetched() {
        let _env = crate::test_support::EnvHold::fetch_on();
        let (base, hits) = mock_server(
            vec![page_route("/page", "<title>Fresh</title><p>new text</p>")],
            5,
        );
        let url = format!("{base}/page");
        let key = crate::translate::sha256_hex(&url);
        let mut cache = HashMap::new();
        cache.insert(
            key.clone(),
            CacheEntry {
                text: "Link: stale text".to_string(),
                fetched_at: 1_000,
            },
        );
        let mut b = bm("e8", SANITIZED_TEXT);
        b.urls = vec![url];
        // 7 days plus one second later: stale, refetched.
        assert_eq!(CACHE_TTL_SECS, 7 * 24 * 60 * 60);
        let e = enrich_at(
            &b,
            &mut cache,
            1_000 + CACHE_TTL_SECS + 1,
            Duration::from_secs(5),
        );
        assert!(e.text.contains("Fresh"), "{}", e.text);
        assert_eq!(hits_of(&hits), 1);
        assert_eq!(cache[&key].fetched_at, 1_000 + CACHE_TTL_SECS + 1);
        // One second before the TTL ends: fresh, no refetch.
        let e = enrich_at(
            &b,
            &mut cache,
            1_000 + 2 * CACHE_TTL_SECS,
            Duration::from_secs(5),
        );
        assert!(e.text.contains("Fresh"), "{}", e.text);
        assert_eq!(hits_of(&hits), 1);
    }

    #[test]
    fn cache_round_trips_on_disk_with_strict_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(CACHE_FILE_NAME);
        assert!(load_cache(&path).is_empty());
        let mut cache = HashMap::new();
        cache.insert(
            "abc123".to_string(),
            CacheEntry {
                text: "Link: cached".to_string(),
                fetched_at: 9_000_000,
            },
        );
        save_cache(&path, &cache).unwrap();
        assert_eq!(load_cache(&path), cache);
        // Corrupt files load as empty (fail closed).
        std::fs::write(&path, "not json").unwrap();
        assert!(load_cache(&path).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            save_cache(&path, &cache).unwrap();
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "file mode is {file_mode:o}");
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "dir mode is {dir_mode:o}");
        }
    }

    #[test]
    fn cache_paths_follow_env_then_log_dir() {
        let _env = crate::test_support::EnvHold::fetch_on();
        let dir = tempfile::tempdir().unwrap();
        let via_env = dir.path().join("custom.json");
        std::env::set_var(CACHE_ENV_VAR, &via_env);
        assert_eq!(default_cache_path().unwrap(), via_env);
        assert_eq!(
            cache_path_for_log(&dir.path().join("decision_log.jsonl")).unwrap(),
            via_env
        );
        std::env::remove_var(CACHE_ENV_VAR);
        assert_eq!(
            cache_path_for_log(&dir.path().join("decision_log.jsonl")).unwrap(),
            dir.path().join(CACHE_FILE_NAME)
        );
    }
}
