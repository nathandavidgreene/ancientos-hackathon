//! Compounding wiki: idea cards and research bookmarks distilled into
//! concept pages with cited claims.
//!
//! Layout under the app data dir (`wiki/`, dirs 0700, files 0600):
//! - `raw/<post_id>.md`: immutable sanitized source (post text, unrolled
//!   thread, delegate notes). Never overwritten: a changed source gets a new
//!   `raw/<post_id>-<n>.md` (`n` from 2 up).
//! - `pages/<slug>.md`: one page per idea card plus concept/entity pages.
//! - `index.md`: catalog, one line per page (link, one-line summary,
//!   source count).
//! - `log.md`: append-only human-readable lines
//!   (`## [YYYY-MM-DD] <op> | <title>`) rendered from `wiki.*` decision-log
//!   entries. The decision log stays the source of truth; `log.md` only ever
//!   grows to match it and any divergence is an error (fail closed, never
//!   rewrite).
//!
//! Ingest writes raw first, then updates pages. The paid path needs
//! `ANCIENTOS_ALLOW_PAID=1` and [`DataClass::Public`] sources only (the
//! router ceiling is enforced again inside the Muse call); non-public
//! sources compile via the local LLM when one is configured, otherwise via
//! the deterministic local fallback (same fallback when paid is off or the
//! model reply is unusable). Every claim must cite at least one raw source
//! id; edits without citations are rejected. A page's data class is the max
//! of its sources, and existing page text is never sent to a hosted model:
//! paid prompts carry only the current ingest's sanitized Public sources.
//!
//! Query picks relevant pages via `index.md` plus simple term overlap (no
//! embeddings) and answers with citations to pages and raw sources.
//! Lint reports hygiene findings and proposes them as `wiki.edit` approvals
//! ([`Risk::High`](ancientos_core::gate::Risk)); findings are never
//! auto-applied.

use ancientos_router::data_class::{check_egress, max_of, DataClass, Route};
use ancientos_router::local_llm::LocalLlmClient;
use ancientos_router::muse::MuseClient;
use ancientos_router::ModelClient;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Wiki root below the app data dir.
pub const WIKI_DIR_NAME: &str = "wiki";
/// Immutable sanitized sources below the wiki root.
pub const RAW_DIR_NAME: &str = "raw";
/// Concept/entity pages below the wiki root.
pub const PAGES_DIR_NAME: &str = "pages";
/// Catalog file name below the wiki root.
pub const INDEX_FILE_NAME: &str = "index.md";
/// Human-readable log file name below the wiki root.
pub const LOG_FILE_NAME: &str = "log.md";

/// Log kind appended when an ingest is queued: `{reason, ref_id, post_id,
/// title, date, at}` (metadata only).
pub const WIKI_QUEUED_KIND: &str = "wiki.queued";
/// Log kind appended by an ingest: `{post_ids, raw, pages, mode, rejected,
/// title, date, at}` (metadata only, never page text).
pub const WIKI_INGEST_KIND: &str = "wiki.ingest";
/// Log kind appended when a query answer is filed as a page: `{slug, title,
/// pages_cited, raw_cited, date, at}` (metadata only).
pub const WIKI_FILEBACK_KIND: &str = "wiki.fileback";
/// Log kind appended by lint: `{findings, approvals, title, date, at}`.
pub const WIKI_LINT_KIND: &str = "wiki.lint";

/// Queue reason for a `delegate` vote verdict.
pub const QUEUE_REASON_DELEGATE: &str = "delegate_verdict";
/// Queue reason for an adopted idea card.
pub const QUEUE_REASON_ADOPT: &str = "idea_adopt";
/// Queue reason for a manual CLI ingest.
pub const QUEUE_REASON_MANUAL: &str = "manual";

/// Ingest mode recorded when page edits came from Muse Spark.
pub const MODE_PAID: &str = "paid";
/// Ingest mode recorded when page edits came from the local LLM.
pub const MODE_LOCAL: &str = "local";
/// Ingest mode recorded for deterministic local compilation (paid off,
/// no local model, or the model reply was unusable).
pub const MODE_FALLBACK: &str = "fallback";

/// Paid prompt cap: at most this many chars of sanitized source text per
/// source (fail closed on size, like the rest of the paid paths).
pub const MAX_SOURCE_CHARS: usize = 4000;
/// Completion cap for the single paid wiki call per ingest.
pub const WIKI_MAX_COMPLETION_TOKENS: u32 = 1500;
/// Reasoning effort for the paid wiki call.
pub const WIKI_REASONING_EFFORT: &str = "low";

/// Wiki root: `$ANCIENTOS_HOME/wiki` (or `~/.ancientos-hackathon/wiki`).
pub fn wiki_dir() -> Result<PathBuf, String> {
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(WIKI_DIR_NAME))
}

/// Wiki root alongside a decision log (keeps tests, custom data dirs and the
/// daemon isolated): `<log parent>/wiki`, or `wiki/` when the log has no
/// parent dir.
pub fn wiki_dir_for_log(log_path: &Path) -> PathBuf {
    match log_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(WIKI_DIR_NAME),
        _ => PathBuf::from(WIKI_DIR_NAME),
    }
}

/// True for safe raw/source ids (post ids, card ids): non-empty ASCII word
/// chars plus `-`, so the id is safe as a file name (no separators, no dot
/// segments, no traversal).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// True for safe page slugs: same file-name rule as [`valid_id`].
pub fn valid_slug(slug: &str) -> bool {
    valid_id(slug)
}

/// Slugify a title: lowercase, runs of other chars become one `-`,
/// trimmed, capped at 64 chars. Empty results fall back to `page`.
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let slug = out.trim_matches('-').to_string();
    let slug: String = slug.chars().take(64).collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "page".to_string()
    } else {
        slug
    }
}

/// Collapse whitespace (including newlines) to one line, trimmed.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to `max` chars (no ellipsis: byte-stable for logs and pages).
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn ensure_wiki_dirs(wiki: &Path) -> Result<(), String> {
    crate::token_store::ensure_dir_0700(&wiki.join(RAW_DIR_NAME))
        .map_err(|e| format!("wiki raw dir failed: {e}"))?;
    crate::token_store::ensure_dir_0700(&wiki.join(PAGES_DIR_NAME))
        .map_err(|e| format!("wiki pages dir failed: {e}"))?;
    Ok(())
}

fn write_0600(path: &Path, data: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("wiki dir failed: {e}"))?;
        }
    }
    crate::token_store::atomic_write_0600(path, data.as_bytes())
        .map_err(|e| format!("wiki write failed for {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn chmod_0600(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("wiki perms failed for {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn chmod_0600(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// One immutable raw source on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSource {
    /// Base source id (post id). Claims cite this, never the versioned name.
    pub id: String,
    /// File written or reused (`raw/<id>.md` or `raw/<id>-<n>.md`).
    pub path: PathBuf,
    /// True when the content landed in (or reused) a versioned `-<n>` file.
    pub versioned: bool,
    /// Data class of the original (pre-sanitization) text.
    pub class: DataClass,
    /// True when an existing identical file was reused (nothing written).
    pub reused: bool,
}

/// Render the immutable raw body: sanitized post text, unrolled thread and
/// delegate notes. `sanitized` carries the already-sanitized section texts;
/// the header records the source id, origin url, data class and ingest date.
pub fn render_raw(
    post_id: &str,
    url: &str,
    class: DataClass,
    date: &str,
    sanitized_post: &str,
    sanitized_thread: &[String],
    sanitized_notes: Option<&str>,
) -> String {
    let mut out = format!(
        "# raw {post_id}\nsource: {url}\nclass: {}\ndate: {date}\n\n## post\n{sanitized_post}\n",
        class.as_str()
    );
    out.push_str("\n## thread\n");
    if sanitized_thread.is_empty() {
        out.push_str("none\n");
    } else {
        for (i, t) in sanitized_thread.iter().enumerate() {
            out.push_str(&format!("### {}/{}\n{t}\n", i + 1, sanitized_thread.len()));
        }
    }
    out.push_str("\n## delegate_notes\n");
    match sanitized_notes.map(str::trim).filter(|s| !s.is_empty()) {
        Some(notes) => out.push_str(&format!("{notes}\n")),
        None => out.push_str("none\n"),
    }
    out
}

/// Body of a rendered raw file without the volatile `date:` line, so
/// re-ingesting unchanged content reuses the existing file instead of
/// minting a new version every day.
fn raw_body_for_compare(rendered: &str) -> String {
    rendered
        .lines()
        .filter(|l| !l.starts_with("date: "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Existing raw files for `post_id`: the base file plus `-<n>` versions in
/// version order (base first). Missing dir means none.
fn existing_raws(wiki: &Path, post_id: &str) -> Vec<PathBuf> {
    let raw_dir = wiki.join(RAW_DIR_NAME);
    let base = raw_dir.join(format!("{post_id}.md"));
    let mut out = Vec::new();
    if base.exists() {
        out.push(base);
    }
    let mut n = 2u32;
    loop {
        let p = raw_dir.join(format!("{post_id}-{n}.md"));
        if p.exists() {
            out.push(p);
            n += 1;
        } else {
            break;
        }
    }
    out
}

/// Next versioned path for `post_id` (`raw/<post_id>-<n>.md`).
fn next_versioned_raw(wiki: &Path, post_id: &str) -> PathBuf {
    let mut n = 2u32;
    loop {
        let p = wiki.join(RAW_DIR_NAME).join(format!("{post_id}-{n}.md"));
        if !p.exists() {
            return p;
        }
        n += 1;
    }
}

/// Write (or reuse) the immutable raw source for `post_id`.
///
/// The rendered content is compared by body (ignoring the `date:` line)
/// against every existing version: an identical body reuses that file
/// (nothing is written, the old bytes are untouched); a changed body mints
/// the next `raw/<post_id>-<n>.md`. Existing files are never overwritten —
/// the write uses create-new semantics and any pre-existing target is an
/// error (fail closed, never silently replace).
#[allow(clippy::too_many_arguments)]
pub fn write_raw(
    wiki: &Path,
    post_id: &str,
    url: &str,
    class: DataClass,
    date: &str,
    sanitized_post: &str,
    sanitized_thread: &[String],
    sanitized_notes: Option<&str>,
) -> Result<RawSource, String> {
    if !valid_id(post_id) {
        return Err(format!("invalid post id for raw: {post_id:?}"));
    }
    ensure_wiki_dirs(wiki)?;
    let rendered = render_raw(
        post_id,
        url,
        class,
        date,
        sanitized_post,
        sanitized_thread,
        sanitized_notes,
    );
    let want = raw_body_for_compare(&rendered);
    for path in existing_raws(wiki, post_id) {
        let current = std::fs::read_to_string(&path)
            .map_err(|e| format!("raw read failed for {}: {e}", path.display()))?;
        if raw_body_for_compare(&current) == want {
            let versioned = path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s != post_id);
            return Ok(RawSource {
                id: post_id.to_string(),
                path,
                versioned,
                class,
                reused: true,
            });
        }
    }
    let base = wiki.join(RAW_DIR_NAME).join(format!("{post_id}.md"));
    let (path, versioned) = if !base.exists() {
        (base, false)
    } else {
        (next_versioned_raw(wiki, post_id), true)
    };
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("raw create failed for {}: {e}", path.display()))?;
        f.write_all(rendered.as_bytes())
            .map_err(|e| format!("raw write failed for {}: {e}", path.display()))?;
        f.sync_all()
            .map_err(|e| format!("raw sync failed for {}: {e}", path.display()))?;
    }
    chmod_0600(&path)?;
    Ok(RawSource {
        id: post_id.to_string(),
        path,
        versioned,
        class,
        reused: false,
    })
}

/// Base source ids with at least one raw file (`<id>.md` or `<id>-<n>.md`).
/// Used to check that claims cite real raw sources.
///
/// A stem `<id>-<n>` counts as a version of `<id>` only when the base file
/// plus the full `2..=n` chain exist, so a base id like `demo-2001` is never
/// mistaken for version 2001 of `demo`.
pub fn known_raw_ids(wiki: &Path) -> BTreeSet<String> {
    let raw_dir = wiki.join(RAW_DIR_NAME);
    let mut stems = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(&raw_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".md") {
                if !stem.is_empty() && valid_id(stem) {
                    stems.insert(stem.to_string());
                }
            }
        }
    }
    let mut out = BTreeSet::new();
    for stem in &stems {
        out.insert(base_raw_id(stem, &stems));
    }
    out
}

/// Base id for one raw file stem: the version prefix when the stem is a
/// genuine `-<n>` version (base plus the full chain present), else the stem
/// itself.
fn base_raw_id(stem: &str, stems: &BTreeSet<String>) -> String {
    if let Some((id, n)) = stem.rsplit_once('-') {
        if !id.is_empty() && valid_id(id) {
            if let Ok(n) = n.parse::<u32>() {
                // Versions start at 2 and grow by one; cap the chain walk
                // so a huge numeric suffix stays a base id.
                if (2..=10_000).contains(&n) && stems.contains(id) {
                    let chained = (2..n).all(|k| stems.contains(&format!("{id}-{k}")));
                    if chained {
                        return id.to_string();
                    }
                }
            }
        }
    }
    stem.to_string()
}

/// True when at least one raw file exists for the base source id.
pub fn raw_exists(wiki: &Path, source_id: &str) -> bool {
    known_raw_ids(wiki).contains(source_id)
}

/// Data class of a raw source: the max `class:` header across all its
/// versions. Missing files, missing headers and unknown values read as
/// [`DataClass::Restricted`] (fail closed).
pub fn raw_class(wiki: &Path, source_id: &str) -> DataClass {
    let paths = existing_raws(wiki, source_id);
    if paths.is_empty() {
        return DataClass::Restricted;
    }
    let mut classes = Vec::new();
    for path in &paths {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let class = text
            .lines()
            .find_map(|line| line.strip_prefix("class: "))
            .map(|value| DataClass::parse(value.trim()))
            .unwrap_or(DataClass::Restricted);
        classes.push(class);
    }
    max_of(classes)
}

/// Class of a page's sources: the max over the cited raw ids. Unknown or
/// missing sources taint the whole page to [`DataClass::Restricted`].
pub fn sources_class(wiki: &Path, sources: &[String]) -> DataClass {
    max_of(sources.iter().map(|s| raw_class(wiki, s)))
}

/// One cited claim on a page. `sources` holds raw source ids; an empty list
/// means the claim is uncited (rejected on ingest, flagged by lint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageClaim {
    pub text: String,
    pub sources: Vec<String>,
}

/// One parsed wiki page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub slug: String,
    pub title: String,
    pub summary: String,
    /// Distinct raw source ids cited by this page, sorted.
    pub sources: Vec<String>,
    pub class: DataClass,
    pub claims: Vec<PageClaim>,
    /// Distinct `[[slug]]` backlinks, sorted.
    pub links: Vec<String>,
}

/// File for one page slug.
pub fn page_path(wiki: &Path, slug: &str) -> PathBuf {
    wiki.join(PAGES_DIR_NAME).join(format!("{slug}.md"))
}

/// Render a page: `# title` plus a small header (`slug:`, `sources:`,
/// `class:`, `summary:`), a `## claims` bullet list (every bullet ends with
/// `[sources: id, ...]`) and a `## links` bullet list of `[[slug]]` backlinks.
pub fn render_page(page: &Page) -> String {
    let mut out = format!(
        "# {}\nslug: {}\nsources: {}\nclass: {}\nsummary: {}\n\n## claims\n",
        page.title,
        page.slug,
        page.sources.join(", "),
        page.class.as_str(),
        page.summary
    );
    if page.claims.is_empty() {
        out.push_str("none\n");
    } else {
        for c in &page.claims {
            out.push_str(&format!(
                "- {} [sources: {}]\n",
                c.text,
                c.sources.join(", ")
            ));
        }
    }
    out.push_str("\n## links\n");
    if page.links.is_empty() {
        out.push_str("none\n");
    } else {
        for l in &page.links {
            out.push_str(&format!("- [[{l}]]\n"));
        }
    }
    out
}

/// `[[slug]]` targets mentioned anywhere in `text` (headers and claims).
pub fn wikilinks_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        rest = &rest[start + 2..];
        if let Some(end) = rest.find("]]") {
            let target = rest[..end].trim();
            if valid_slug(target) && !out.contains(&target.to_string()) {
                out.push(target.to_string());
            }
            rest = &rest[end + 2..];
        } else {
            break;
        }
    }
    out
}

/// Parse one claim bullet: `- <text> [sources: a, b]`. Bullets without the
/// marker (or with an empty list) parse as uncited claims so lint can flag
/// them; they are never silently dropped.
fn parse_claim_bullet(line: &str) -> Option<PageClaim> {
    let body = line.strip_prefix("- ")?;
    match body.rfind("[sources:") {
        Some(pos) => {
            let text = body[..pos].trim().to_string();
            let rest = body[pos + "[sources:".len()..].trim();
            let list = rest.strip_suffix(']').unwrap_or(rest);
            let sources: Vec<String> = list
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Some(PageClaim { text, sources })
        }
        None => Some(PageClaim {
            text: body.trim().to_string(),
            sources: Vec::new(),
        }),
    }
}

/// Parse a page file. Fails closed on a missing title/slug only; uncited
/// claims and missing sources parse fine so lint can report them.
pub fn parse_page(text: &str) -> Result<Page, String> {
    let mut title: Option<String> = None;
    let mut slug: Option<String> = None;
    let mut summary = String::new();
    let mut class = DataClass::Restricted;
    let mut claims = Vec::new();
    let mut links = BTreeSet::new();
    let mut section = "";
    for line in text.lines() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix("# ") {
            if title.is_none() {
                title = Some(h.trim().to_string());
            }
            continue;
        }
        if let Some(h) = t.strip_prefix("## ") {
            section = match h.trim() {
                "claims" => "claims",
                "links" => "links",
                _ => "",
            };
            continue;
        }
        if section == "claims" && t.starts_with("- ") {
            if let Some(claim) = parse_claim_bullet(t) {
                for target in wikilinks_in(&claim.text) {
                    links.insert(target);
                }
                claims.push(claim);
            }
            continue;
        }
        if section == "links" && t.starts_with("- ") {
            for target in wikilinks_in(t) {
                links.insert(target);
            }
            continue;
        }
        if section.is_empty() {
            if let Some(v) = t.strip_prefix("slug: ") {
                slug = Some(v.trim().to_string());
            } else if let Some(v) = t.strip_prefix("summary: ") {
                summary = v.trim().to_string();
            } else if let Some(v) = t.strip_prefix("class: ") {
                class = DataClass::parse(v.trim());
            }
        }
    }
    let title = title
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "wiki page has no `# title` line".to_string())?;
    let slug = slug
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "wiki page has no `slug:` header".to_string())?;
    if !valid_slug(&slug) {
        return Err(format!("wiki page has an invalid slug: {slug:?}"));
    }
    let mut sources = BTreeSet::new();
    for c in &claims {
        for s in &c.sources {
            sources.insert(s.clone());
        }
    }
    Ok(Page {
        slug,
        title,
        summary,
        sources: sources.into_iter().collect(),
        class,
        claims,
        links: links.into_iter().collect(),
    })
}

/// Every page in `pages/`, sorted by slug. Malformed files are skipped here
/// (lint reports them as `unparseable`); unreadable dirs read as empty.
pub fn list_pages(wiki: &Path) -> Vec<Page> {
    let mut out = Vec::new();
    let pages_dir = wiki.join(PAGES_DIR_NAME);
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".md") else {
            continue;
        };
        if !valid_slug(stem) {
            continue;
        }
        let text = match std::fs::read_to_string(entry.path()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Ok(mut page) = parse_page(&text) {
            // The file name wins over the header on disagreements.
            page.slug = stem.to_string();
            out.push(page);
        }
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

/// Raw markdown of one page (for `wiki show`). The slug is validated so it
/// is safe as a file name; unknown slugs fail closed.
pub fn read_page(wiki: &Path, slug: &str) -> Result<String, String> {
    if !valid_slug(slug) {
        return Err(format!("invalid wiki slug: {slug:?}"));
    }
    let path = page_path(wiki, slug);
    std::fs::read_to_string(&path).map_err(|_| format!("no wiki page for slug {slug:?}"))
}

/// Merge `edits` into `pages/`: new slugs are created, existing slugs gain
/// the new claims (exact-text dedupe) and links (union). Every applied page
/// recomputes its `sources:` header and its class as the max of its sources.
/// Returns the touched slugs in sorted order.
pub fn apply_edits(
    wiki: &Path,
    edits: &[PageEdit],
    fallback_class: DataClass,
) -> Result<Vec<String>, String> {
    ensure_wiki_dirs(wiki)?;
    let mut touched = BTreeSet::new();
    for edit in edits {
        let path = page_path(wiki, &edit.slug);
        let mut page = match std::fs::read_to_string(&path) {
            Ok(text) => parse_page(&text)
                .map_err(|e| format!("existing page [[{}]] is unparseable: {e}", edit.slug))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Page {
                slug: edit.slug.clone(),
                title: edit.title.clone(),
                summary: edit.summary.clone(),
                sources: Vec::new(),
                class: fallback_class,
                claims: Vec::new(),
                links: Vec::new(),
            },
            Err(e) => {
                return Err(format!("wiki page read failed for {}: {e}", path.display()));
            }
        };
        page.slug = edit.slug.clone();
        if page.title.trim().is_empty() {
            page.title = edit.title.clone();
        }
        if page.summary.trim().is_empty() {
            page.summary = edit.summary.clone();
        }
        for claim in &edit.claims {
            if claim.text.trim().is_empty() {
                continue;
            }
            let already = page
                .claims
                .iter()
                .any(|c| c.text == claim.text && c.sources == claim.sources);
            if !already {
                page.claims.push(PageClaim {
                    text: claim.text.clone(),
                    sources: claim.sources.clone(),
                });
            }
        }
        let mut links: BTreeSet<String> = page.links.into_iter().collect();
        for l in &edit.links {
            if valid_slug(l) && l != &edit.slug {
                links.insert(l.clone());
            }
        }
        // Inline [[targets]] in the new claims count as backlinks too.
        for target in edit.claims.iter().flat_map(|c| wikilinks_in(&c.text)) {
            if target != edit.slug {
                links.insert(target);
            }
        }
        page.links = links.into_iter().collect();
        let mut sources = BTreeSet::new();
        for c in &page.claims {
            for s in &c.sources {
                sources.insert(s.clone());
            }
        }
        page.sources = sources.into_iter().collect();
        page.class = max_of(
            std::iter::once(fallback_class).chain(page.sources.iter().map(|s| raw_class(wiki, s))),
        );
        write_0600(&path, &render_page(&page))?;
        touched.insert(edit.slug.clone());
    }
    Ok(touched.into_iter().collect())
}

/// Render the catalog: one line per page (`- [[slug]] — <summary>
/// `(sources: <n>)`), sorted by slug.
pub fn render_index(pages: &[Page]) -> String {
    let mut out = String::from("# wiki index\n\n");
    let mut sorted: Vec<&Page> = pages.iter().collect();
    sorted.sort_by(|a, b| a.slug.cmp(&b.slug));
    for p in sorted {
        let summary = one_line(&p.summary);
        let summary = if summary.is_empty() {
            "(no summary)".to_string()
        } else {
            truncate_chars(&summary, 140)
        };
        out.push_str(&format!(
            "- [[{}]] — {summary} (sources: {})\n",
            p.slug,
            p.sources.len()
        ));
    }
    out
}

/// Rewrite `index.md` from the current pages. Returns the page count.
pub fn sync_index(wiki: &Path) -> Result<usize, String> {
    ensure_wiki_dirs(wiki)?;
    let pages = list_pages(wiki);
    let path = wiki.join(INDEX_FILE_NAME);
    write_0600(&path, &render_index(&pages))?;
    Ok(pages.len())
}

/// One proposed claim edit: text plus the raw source ids it cites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimEdit {
    pub text: String,
    pub sources: Vec<String>,
}

/// One proposed page edit: new pages are created, existing slugs are merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageEdit {
    pub slug: String,
    pub title: String,
    pub summary: String,
    pub claims: Vec<ClaimEdit>,
    pub links: Vec<String>,
}

#[derive(serde::Deserialize)]
struct EditClaimJson {
    text: String,
    #[serde(default)]
    sources: OneOrMany,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl Default for OneOrMany {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
        }
    }
}

#[derive(serde::Deserialize)]
struct EditPageJson {
    slug: String,
    title: String,
    summary: String,
    #[serde(default)]
    claims: Vec<EditClaimJson>,
    #[serde(default)]
    links: Vec<String>,
}

#[derive(serde::Deserialize)]
struct EditsReplyJson {
    pages: Vec<EditPageJson>,
}

/// Parse structured page edits from a model reply: the JSON object between
/// the first `{` and the last `}` must be
/// `{"pages":[{"slug","title","summary","claims":[{"text","sources"}],"links"}]}`.
/// Anything else is an error (fail closed, never guess edits).
pub fn parse_edits(reply: &str) -> Result<Vec<PageEdit>, String> {
    let start = reply
        .find('{')
        .ok_or_else(|| "wiki reply has no JSON object".to_string())?;
    let end = reply
        .rfind('}')
        .ok_or_else(|| "wiki reply has no JSON object".to_string())?;
    if end < start {
        return Err("wiki reply has no JSON object".to_string());
    }
    let parsed: EditsReplyJson = serde_json::from_str(&reply[start..=end])
        .map_err(|e| format!("wiki reply is not valid page edits JSON: {e}"))?;
    Ok(parsed
        .pages
        .into_iter()
        .map(|p| PageEdit {
            slug: p.slug,
            title: p.title,
            summary: p.summary,
            claims: p
                .claims
                .into_iter()
                .map(|c| ClaimEdit {
                    text: c.text,
                    sources: c.sources.into_vec(),
                })
                .collect(),
            links: p.links,
        })
        .collect())
}

/// Validate proposed edits against the known raw ids. Returns the valid
/// edits plus one rejection reason per dropped page. Rules (fail closed):
/// the slug must normalize to a safe non-empty slug, the title and summary
/// must be non-empty, at least one claim is required, and every claim must
/// cite at least one known raw source id. Invalid `[[links]]` are dropped
/// (auxiliary, never load-bearing); everything else rejects the whole page.
pub fn validate_edits(
    edits: &[PageEdit],
    known_raw_ids: &BTreeSet<String>,
) -> (Vec<PageEdit>, Vec<String>) {
    let mut valid = Vec::new();
    let mut rejected = Vec::new();
    for edit in edits {
        let slug = edit.slug.trim().to_lowercase();
        if slug.is_empty() || !valid_slug(&slug) {
            rejected.push(format!("rejected page with invalid slug {:?}", edit.slug));
            continue;
        }
        let title = one_line(&edit.title);
        if title.is_empty() {
            rejected.push(format!("rejected page [[{slug}]]: empty title"));
            continue;
        }
        let summary = one_line(&edit.summary);
        if summary.is_empty() {
            rejected.push(format!("rejected page [[{slug}]]: empty summary"));
            continue;
        }
        if edit.claims.is_empty() {
            rejected.push(format!("rejected page [[{slug}]]: no claims"));
            continue;
        }
        let mut claims = Vec::new();
        let mut bad: Option<String> = None;
        for claim in &edit.claims {
            let text = one_line(&claim.text);
            if text.is_empty() {
                bad = Some(format!("rejected page [[{slug}]]: empty claim text"));
                break;
            }
            let sources: Vec<String> = {
                let mut set = BTreeSet::new();
                for s in &claim.sources {
                    let s = s.trim().to_string();
                    if s.is_empty() {
                        continue;
                    }
                    if !valid_id(&s) {
                        bad = Some(format!("rejected page [[{slug}]]: invalid source id {s:?}"));
                        break;
                    }
                    if !known_raw_ids.contains(&s) {
                        bad = Some(format!(
                            "rejected page [[{slug}]]: unknown raw source {s:?}"
                        ));
                        break;
                    }
                    set.insert(s);
                }
                set.into_iter().collect()
            };
            if bad.is_some() {
                break;
            }
            if sources.is_empty() {
                bad = Some(format!(
                    "rejected page [[{slug}]]: claim without cited sources"
                ));
                break;
            }
            claims.push(ClaimEdit {
                text: truncate_chars(&text, 500),
                sources,
            });
        }
        if let Some(reason) = bad {
            rejected.push(reason);
            continue;
        }
        let mut links = BTreeSet::new();
        for l in &edit.links {
            let l = l.trim().to_lowercase();
            if valid_slug(&l) && l != slug {
                links.insert(l);
            }
        }
        valid.push(PageEdit {
            slug,
            title: truncate_chars(&title, 140),
            summary: truncate_chars(&summary, 300),
            claims,
            links: links.into_iter().collect(),
        });
    }
    (valid, rejected)
}

/// Muse caller for the paid path: a prompt -> the reply text.
pub type MuseCaller<'a> = dyn Fn(&str) -> Result<String, String> + 'a;
/// Local-model caller for non-public sources: a prompt -> the reply text.
pub type LocalCaller<'a> = dyn Fn(&str) -> Result<String, String> + 'a;

/// True when paid calls are enabled (`ANCIENTOS_ALLOW_PAID=1`).
pub fn is_paid_allowed() -> bool {
    std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn cap(s: &str) -> String {
    truncate_chars(s, MAX_SOURCE_CHARS)
}

/// Paid prompt: sanitized Public source text only, plus the strict-JSON
/// instruction. Callers must only build this for [`DataClass::Public`]
/// sources; existing page text is never included.
pub fn build_paid_prompt(
    raw_id: &str,
    sanitized_post: &str,
    sanitized_thread: &[String],
    sanitized_notes: Option<&str>,
    cards: &[crate::idea_card::IdeaCard],
) -> String {
    let mut prompt = format!(
        "Instructions: compile the sanitized public research source below into wiki page edits. \
         Reply with strict JSON only, no other text: \
         {{\"pages\":[{{\"slug\":\"kebab-case\",\"title\":\"...\",\"summary\":\"one line\",\
         \"claims\":[{{\"text\":\"...\",\"sources\":[\"{raw_id}\"]}}],\"links\":[\"other-slug\"]}}]}}. \
         Rules: every claim must cite at least one source id from [{raw_id}] (claims without \
         citations are rejected); keep slugs lowercase kebab-case; 1-3 pages; 1-5 claims per page.\n\n\
         Source {raw_id} (sanitized post):\n{}\n",
        cap(sanitized_post)
    );
    if !sanitized_thread.is_empty() {
        prompt.push_str("\nThread (sanitized):\n");
        for (i, t) in sanitized_thread.iter().enumerate() {
            prompt.push_str(&format!("{}. {}\n", i + 1, cap(t)));
        }
    }
    if let Some(notes) = sanitized_notes.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str(&format!("\nDelegate notes (sanitized):\n{}\n", cap(notes)));
    }
    if !cards.is_empty() {
        prompt.push_str("\nRelated idea cards (cite the source id for their claims):\n");
        for c in cards {
            prompt.push_str(&format!(
                "- {} (maps to: {})\n",
                truncate_chars(&one_line(&c.idea), 200),
                c.ancientos_mapping.join(", ")
            ));
        }
    }
    prompt
}

/// Local-model prompt: same shape as [`build_paid_prompt`] but over the
/// original (unsanitized) text. Only ever sent to the on-device model.
pub fn build_local_prompt(
    raw_id: &str,
    post: &str,
    thread: &[String],
    notes: Option<&str>,
    cards: &[crate::idea_card::IdeaCard],
) -> String {
    let mut prompt = format!(
        "Instructions: compile the private research source below into wiki page edits. \
         This prompt stays on the device. Reply with strict JSON only, no other text: \
         {{\"pages\":[{{\"slug\":\"kebab-case\",\"title\":\"...\",\"summary\":\"one line\",\
         \"claims\":[{{\"text\":\"...\",\"sources\":[\"{raw_id}\"]}}],\"links\":[\"other-slug\"]}}]}}. \
         Rules: every claim must cite at least one source id from [{raw_id}] (claims without \
         citations are rejected); keep slugs lowercase kebab-case; 1-3 pages; 1-5 claims per page.\n\n\
         Source {raw_id} (private, on-device only):\n{}\n",
        cap(post)
    );
    if !thread.is_empty() {
        prompt.push_str("\nThread:\n");
        for (i, t) in thread.iter().enumerate() {
            prompt.push_str(&format!("{}. {}\n", i + 1, cap(t)));
        }
    }
    if let Some(notes) = notes.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str(&format!("\nDelegate notes:\n{}\n", cap(notes)));
    }
    if !cards.is_empty() {
        prompt.push_str("\nRelated idea cards (cite the source id for their claims):\n");
        for c in cards {
            prompt.push_str(&format!(
                "- {} (maps to: {})\n",
                truncate_chars(&one_line(&c.idea), 200),
                c.ancientos_mapping.join(", ")
            ));
        }
    }
    prompt
}

/// Split sanitized text into quoted claims (sentence-ish, capped).
fn split_claims(text: &str) -> Vec<String> {
    let mut claims = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        current.push(chars[i]);
        let end = matches!(chars[i], '.' | '?' | '!')
            && (i + 1 >= chars.len() || chars[i + 1].is_whitespace());
        if end {
            let claim = one_line(&current);
            if !claim.is_empty() {
                claims.push(truncate_chars(&claim, 200));
            }
            current.clear();
        }
        i += 1;
    }
    let tail = one_line(&current);
    if !tail.is_empty() {
        claims.push(truncate_chars(&tail, 200));
    }
    claims.truncate(3);
    claims
}

/// Deterministic local compilation: one page for the post, one page per
/// related idea card, plus concept pages from the cards' AncientOS mapping.
/// Claims quote the sanitized source text and every claim cites `raw_id`.
pub fn deterministic_edits(
    post_id: &str,
    sanitized_post: &str,
    cards: &[crate::idea_card::IdeaCard],
    raw_id: &str,
) -> Vec<PageEdit> {
    let mut edits = Vec::new();
    let mut claims = split_claims(sanitized_post);
    if claims.is_empty() {
        claims.push("Source archived with no extractable text.".to_string());
    }
    let mut modules = BTreeSet::new();
    for c in cards {
        for m in &c.ancientos_mapping {
            let m = m.trim().to_lowercase();
            if valid_slug(&m) {
                modules.insert(m);
            }
        }
    }
    let post_slug = format!("post-{}", slugify(post_id));
    edits.push(PageEdit {
        slug: post_slug,
        title: format!("Post {post_id}"),
        summary: truncate_chars(
            &format!(
                "Research note compounded from source {post_id}. {}",
                one_line(sanitized_post)
            ),
            300,
        ),
        claims: claims
            .iter()
            .map(|t| ClaimEdit {
                text: t.clone(),
                sources: vec![raw_id.to_string()],
            })
            .collect(),
        links: modules.iter().cloned().collect(),
    });
    for card in cards {
        let short: String = card.id.chars().take(8).collect();
        let idea = one_line(&card.idea);
        let mut card_claims = vec![ClaimEdit {
            text: truncate_chars(&format!("Idea: {idea}"), 500),
            sources: vec![raw_id.to_string()],
        }];
        if let Some(first) = claims.first() {
            card_claims.push(ClaimEdit {
                text: truncate_chars(&format!("Source excerpt: {first}"), 500),
                sources: vec![raw_id.to_string()],
            });
        }
        let mut card_links: Vec<String> = card
            .ancientos_mapping
            .iter()
            .map(|m| m.trim().to_lowercase())
            .filter(|m| valid_slug(m))
            .collect();
        card_links.sort();
        card_links.dedup();
        edits.push(PageEdit {
            slug: format!("idea-{}", slugify(&card.id)),
            title: truncate_chars(&format!("Idea {short}: {idea}"), 140),
            summary: truncate_chars(&idea, 300),
            claims: card_claims,
            links: card_links,
        });
    }
    let mut by_module: BTreeMap<String, Vec<&crate::idea_card::IdeaCard>> = BTreeMap::new();
    for card in cards {
        for m in &card.ancientos_mapping {
            let m = m.trim().to_lowercase();
            if valid_slug(&m) {
                by_module.entry(m).or_default().push(card);
            }
        }
    }
    for (module, grouped) in &by_module {
        let mut concept_claims = Vec::new();
        for card in grouped {
            concept_claims.push(ClaimEdit {
                text: truncate_chars(&format!("Idea: {}", one_line(&card.idea)), 500),
                sources: vec![raw_id.to_string()],
            });
        }
        concept_claims.truncate(5);
        let mut concept_links: Vec<String> = grouped
            .iter()
            .map(|c| format!("idea-{}", slugify(&c.id)))
            .collect();
        concept_links.sort();
        concept_links.dedup();
        edits.push(PageEdit {
            slug: module.clone(),
            title: format!("Concept: {module}"),
            summary: truncate_chars(
                &format!("Notes on {module} compounded from research bookmarks."),
                300,
            ),
            claims: concept_claims,
            links: concept_links,
        });
    }
    edits
}

/// One ingest source: the original (pre-sanitization) texts plus the related
/// idea cards. Sanitization happens inside [`ingest_with`].
#[derive(Debug, Clone)]
pub struct IngestInput {
    pub post_id: String,
    pub url: String,
    pub text: String,
    /// Unrolled thread replies (original texts, root excluded).
    pub thread: Vec<String>,
    /// Delegate notes for this post, when the inspiration file has any.
    pub delegate_notes: Option<String>,
    /// Idea cards sourced from this post.
    pub cards: Vec<crate::idea_card::IdeaCard>,
}

impl IngestInput {
    /// Data class of the whole input: the max over every original text.
    pub fn class(&self) -> DataClass {
        let mut classes = vec![crate::translate::class_of(&self.text)];
        for t in &self.thread {
            classes.push(crate::translate::class_of(t));
        }
        if let Some(notes) = &self.delegate_notes {
            classes.push(crate::translate::class_of(notes));
        }
        for c in &self.cards {
            classes.push(crate::translate::class_of(&c.idea));
        }
        max_of(classes)
    }
}

/// Ingest context: paths, clock, gates and model callers.
pub struct IngestCtx<'a> {
    pub wiki: &'a Path,
    pub log_path: &'a Path,
    pub date: &'a str,
    pub now: u64,
    pub allow_paid: bool,
    pub muse: Option<&'a MuseCaller<'a>>,
    pub local: Option<&'a LocalCaller<'a>>,
}

/// Outcome of one ingest.
#[derive(Debug, Clone)]
pub struct IngestReport {
    pub raw: RawSource,
    pub pages: Vec<String>,
    pub mode: String,
    /// Rejection reasons for paid/local edits (empty on clean runs).
    pub rejected: Vec<String>,
}

/// Log one egress decision for the wiki paid path. Returns true when the
/// class is allowed (and the decision was recorded); a log failure fails
/// closed to false.
fn egress_allowed(log_path: &Path, class: DataClass) -> bool {
    match ancientos_core::log::DecisionLog::open(log_path) {
        Ok(mut log) => check_egress(class, Route::MuseSpark, "wiki", Some(&mut log)).is_ok(),
        Err(_) => false,
    }
}

/// Record the refused hosted egress for a non-public ingest (best-effort:
/// the ingest itself stays fail-open to the local model or the fallback).
fn log_refused_egress(log_path: &Path, class: DataClass) {
    if let Ok(mut log) = ancientos_core::log::DecisionLog::open(log_path) {
        let _ = check_egress(class, Route::MuseSpark, "wiki", Some(&mut log));
    }
}

/// Write raw, then update pages. Mode selection:
/// - Public + paid allowed + Muse caller + fresh raw: one paid Muse Spark
///   call over the sanitized sources (existing page text is never sent).
/// - Non-public + local caller + fresh raw: one local-model call over the
///   original texts (the refused hosted egress is logged first).
/// - Anything else (paid off, no local model, reused raw, model errors or
///   edits without citations): the deterministic local fallback.
///
/// Model edits are validated ([`validate_edits`]) and pages citing unknown
/// raw ids are rejected; when the model yields nothing usable the fallback
/// compiles instead, so an ingest always produces cited pages. Paid calls
/// are incremental: an unchanged (reused) raw never triggers a model call.
pub fn ingest_with(input: &IngestInput, ctx: &IngestCtx<'_>) -> Result<IngestReport, String> {
    if !valid_id(&input.post_id) {
        return Err(format!("invalid post id for ingest: {:?}", input.post_id));
    }
    let sanitized_post = crate::sanitize::sanitize_uncapped(&input.text);
    let sanitized_thread: Vec<String> = input
        .thread
        .iter()
        .map(|t| crate::sanitize::sanitize_uncapped(t))
        .collect();
    let sanitized_notes = input
        .delegate_notes
        .as_deref()
        .map(crate::sanitize::sanitize_uncapped);
    let class = input.class();
    let raw = write_raw(
        ctx.wiki,
        &input.post_id,
        &input.url,
        class,
        ctx.date,
        &sanitized_post,
        &sanitized_thread,
        sanitized_notes.as_deref(),
    )?;
    let known = known_raw_ids(ctx.wiki);
    let mut rejected = Vec::new();
    let mut mode = MODE_FALLBACK.to_string();
    let mut edits: Option<Vec<PageEdit>> = None;
    // Incremental paid calls: only a fresh raw may trigger a model call.
    let model_eligible = !raw.reused;
    if class == DataClass::Public && ctx.allow_paid && model_eligible {
        if let Some(muse) = ctx.muse {
            if egress_allowed(ctx.log_path, class) {
                let prompt = build_paid_prompt(
                    &raw.id,
                    &sanitized_post,
                    &sanitized_thread,
                    sanitized_notes.as_deref(),
                    &input.cards,
                );
                match muse(&prompt) {
                    Ok(reply) => match parse_edits(&reply) {
                        Ok(parsed) => {
                            let (valid_edits, reasons) = validate_edits(&parsed, &known);
                            rejected.extend(reasons);
                            if valid_edits.is_empty() {
                                rejected.push(
                                    "paid edits yielded no valid pages; used fallback".to_string(),
                                );
                            } else {
                                mode = MODE_PAID.to_string();
                                edits = Some(valid_edits);
                            }
                        }
                        Err(e) => {
                            rejected.push(format!("paid reply unusable ({e}); used fallback"))
                        }
                    },
                    Err(e) => rejected.push(format!("paid call failed ({e}); used fallback")),
                }
            } else {
                rejected.push("paid egress refused; used fallback".to_string());
            }
        }
    } else if class > DataClass::Public {
        log_refused_egress(ctx.log_path, class);
        if model_eligible {
            if let Some(local) = ctx.local {
                let prompt = build_local_prompt(
                    &raw.id,
                    &input.text,
                    &input.thread,
                    input.delegate_notes.as_deref(),
                    &input.cards,
                );
                match local(&prompt) {
                    Ok(reply) => match parse_edits(&reply) {
                        Ok(parsed) => {
                            let (valid_edits, reasons) = validate_edits(&parsed, &known);
                            rejected.extend(reasons);
                            if valid_edits.is_empty() {
                                rejected.push(
                                    "local edits yielded no valid pages; used fallback".to_string(),
                                );
                            } else {
                                mode = MODE_LOCAL.to_string();
                                edits = Some(valid_edits);
                            }
                        }
                        Err(e) => {
                            rejected.push(format!("local reply unusable ({e}); used fallback"))
                        }
                    },
                    Err(e) => rejected.push(format!("local call failed ({e}); used fallback")),
                }
            } else {
                rejected.push(
                    "non-public source with no local model configured; used fallback".to_string(),
                );
            }
        }
    }
    let edits = edits.unwrap_or_else(|| {
        deterministic_edits(&input.post_id, &sanitized_post, &input.cards, &raw.id)
    });
    let (valid_edits, fallback_rejected) = validate_edits(&edits, &known);
    rejected.extend(fallback_rejected);
    if valid_edits.is_empty() {
        return Err("wiki ingest produced no valid cited pages".to_string());
    }
    let pages = apply_edits(ctx.wiki, &valid_edits, class)?;
    sync_index(ctx.wiki)?;
    let title = format!("{}: {} pages via {mode}", input.post_id, pages.len(),);
    append_ingest(
        ctx.log_path,
        std::slice::from_ref(&input.post_id),
        &raw,
        &pages,
        &mode,
        &rejected,
        &title,
        ctx.date,
        ctx.now,
    )?;
    sync_log_md(ctx.log_path, ctx.wiki)?;
    Ok(IngestReport {
        raw,
        pages,
        mode,
        rejected,
    })
}

/// Append a [`WIKI_INGEST_KIND`] entry (metadata only, never page text).
#[allow(clippy::too_many_arguments)]
fn append_ingest(
    log_path: &Path,
    post_ids: &[String],
    raw: &RawSource,
    pages: &[String],
    mode: &str,
    rejected: &[String],
    title: &str,
    date: &str,
    now: u64,
) -> Result<(), String> {
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let raw_name = raw
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("raw.md")
        .to_string();
    let mut log = ancientos_core::log::DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    log.append(
        WIKI_INGEST_KIND,
        serde_json::json!({
            "post_ids": post_ids,
            "raw": raw_name,
            "reused_raw": raw.reused,
            "class": raw.class.as_str(),
            "pages": pages,
            "mode": mode,
            "rejected": rejected,
            "title": title,
            "date": date,
            "at": now,
        }),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Payload for a queued ingest: `{reason, ref_id, post_id, title, date, at}`.
pub fn queued_payload(reason: &str, ref_id: &str, post_id: &str, now: u64) -> serde_json::Value {
    let date = crate::brief::date_from_days((now / 86400) as i64);
    serde_json::json!({
        "reason": reason,
        "ref_id": ref_id,
        "post_id": post_id,
        "title": format!("queued ingest for {post_id} ({reason})"),
        "date": date,
        "at": now,
    })
}

/// Append a [`WIKI_QUEUED_KIND`] entry to an open log (for callers that
/// already hold the handle, e.g. the vote flow).
pub fn append_queued(
    log: &mut ancientos_core::log::DecisionLog,
    reason: &str,
    ref_id: &str,
    post_id: &str,
    now: u64,
) -> Result<(), String> {
    log.append(
        WIKI_QUEUED_KIND,
        queued_payload(reason, ref_id, post_id, now),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Queue an ingest in the decision log at `log_path` (creating parent dirs).
pub fn queue_ingest(
    log_path: &Path,
    reason: &str,
    ref_id: &str,
    post_id: &str,
    now: u64,
) -> Result<(), String> {
    if reason.trim().is_empty() || post_id.trim().is_empty() {
        return Err("wiki queue needs a reason and a post id".to_string());
    }
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = ancientos_core::log::DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    append_queued(&mut log, reason, ref_id, post_id, now)
}

/// Post id from an `https://x.com/.../status/<id>` url, if it parses.
pub fn post_id_from_url(url: &str) -> Option<String> {
    let after = url.split("/status/").nth(1)?;
    let id: String = after
        .chars()
        .take_while(|c| *c != '/' && *c != '?' && *c != '#' && !c.is_whitespace())
        .collect();
    if valid_id(&id) {
        Some(id)
    } else {
        None
    }
}

/// Post ids queued for delegation (`delegate.queued` / `delegate.done`
/// entries) plus every idea card's source post, sorted and deduped.
pub fn delegated_post_ids(
    entries: &[ancientos_core::log::Entry],
    cards: &[crate::idea_card::IdeaCard],
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for e in entries {
        if e.kind == crate::vote::DELEGATE_QUEUED_KIND
            || e.kind == crate::delegate::DELEGATE_DONE_KIND
        {
            if let Some(id) = e.payload.get("post_id").and_then(|v| v.as_str()) {
                if valid_id(id) {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    for c in cards {
        if let Some(id) = post_id_from_url(&c.source_url) {
            ids.insert(id);
        }
    }
    ids.into_iter().collect()
}

/// Delegate notes for `post_id` from the inspiration file: the bodies of the
/// `### <date> — <url>` sections whose url carries the post id. Missing or
/// unreadable files mean no notes (enrichment only, never load-bearing).
pub fn read_inspiration_notes(post_id: &str) -> Option<String> {
    let path = crate::delegate::inspiration_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let mut bodies = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("### ") {
            if let Some(body) = current.take() {
                if !body.trim().is_empty() {
                    bodies.push(body);
                }
            }
            current = rest.contains(post_id).then(String::new);
        } else if let Some(body) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(body) = current {
        if !body.trim().is_empty() {
            bodies.push(body);
        }
    }
    let joined = bodies.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// One bookmark as a thread post (same shape `unroll` consumes for X data).
fn thread_post_of(b: &crate::fixture::Bookmark) -> crate::unroll::ThreadPost {
    crate::unroll::ThreadPost {
        id: b.post_id.clone(),
        author_id: b
            .author_username
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| b.post_id.clone()),
        text: b.text.clone(),
        created_at: b.created_at.clone(),
        conversation_id: None,
        referenced_tweets: Vec::new(),
        quoted: None,
    }
}

/// Production ingest of one post: the merged store supplies the post text
/// (plus whatever thread the store can unroll), the inspiration file the
/// delegate notes, and the idea-card store the related cards. Paid calls
/// need `ANCIENTOS_ALLOW_PAID=1` and Public sources; non-public sources use
/// the local LLM only when `ANCIENTOS_LOCAL_LLM_URL` is configured.
pub fn ingest_post(post_id: &str) -> Result<IngestReport, String> {
    if !valid_id(post_id) {
        return Err(format!("invalid post id for ingest: {post_id:?}"));
    }
    let merged = crate::merge::load_merged(None)?;
    let bookmark = merged
        .rows
        .iter()
        .find(|b| b.post_id == post_id)
        .ok_or_else(|| format!("post {post_id} is not in the merged store"))?;
    let root = thread_post_of(bookmark);
    let candidates: Vec<crate::unroll::ThreadPost> = merged
        .rows
        .iter()
        .filter(|b| b.post_id != post_id)
        .map(thread_post_of)
        .collect();
    let chain = crate::unroll::unroll(&root, &candidates);
    let thread: Vec<String> = chain.iter().skip(1).map(|p| p.text.clone()).collect();
    let cards = crate::idea_card::load(&crate::idea_card::idea_cards_path()?)?;
    let related: Vec<crate::idea_card::IdeaCard> = cards
        .into_iter()
        .filter(|c| post_id_from_url(&c.source_url).as_deref() == Some(post_id))
        .collect();
    let input = IngestInput {
        post_id: post_id.to_string(),
        url: bookmark.url.clone(),
        text: bookmark.text.clone(),
        thread,
        delegate_notes: read_inspiration_notes(post_id),
        cards: related,
    };
    let wiki = wiki_dir()?;
    let log_path = crate::brief::decision_log_path()?;
    let date = crate::brief::current_date();
    let now = crate::idea_card::now_secs();
    let muse_client = MuseClient::from_env()
        .with_max_completion_tokens(WIKI_MAX_COMPLETION_TOKENS)
        .with_reasoning_effort(WIKI_REASONING_EFFORT);
    let muse = |prompt: &str| {
        muse_client
            .chat_with_class(DataClass::Public, "wiki", None, prompt)
            .map_err(|e| e.to_string())
            .map(|r| r.content)
    };
    let local_client = LocalLlmClient::from_env();
    let local_configured = local_client.base_url().is_some();
    let local = |prompt: &str| local_client.complete(prompt).map_err(|e| e.to_string());
    ingest_with(
        &input,
        &IngestCtx {
            wiki: &wiki,
            log_path: &log_path,
            date: &date,
            now,
            allow_paid: is_paid_allowed(),
            muse: Some(&muse),
            local: if local_configured { Some(&local) } else { None },
        },
    )
}

/// Production ingest of everything delegated: every `delegate` verdict post,
/// every delegated thread, and every idea card's source post. Each post runs
/// the full ingest (raw first, then pages); failures are collected and
/// returned together so one bad post never stops the rest.
pub fn ingest_delegated() -> Result<Vec<IngestReport>, String> {
    let log_path = crate::brief::decision_log_path()?;
    let log = ancientos_core::log::DecisionLog::open(&log_path).map_err(|e| e.to_string())?;
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    let cards = crate::idea_card::load(&crate::idea_card::idea_cards_path()?)?;
    let ids = delegated_post_ids(&entries, &cards);
    if ids.is_empty() {
        return Err("nothing delegated yet: no delegate verdicts or idea cards".to_string());
    }
    let mut reports = Vec::new();
    let mut errors = Vec::new();
    for id in ids {
        match ingest_post(&id) {
            Ok(report) => reports.push(report),
            Err(e) => errors.push(format!("{id}: {e}")),
        }
    }
    if errors.is_empty() {
        Ok(reports)
    } else {
        Err(format!("wiki ingest failures: {}", errors.join("; ")))
    }
}

/// Render one human-readable log line: `## [YYYY-MM-DD] <op> | <title>`.
pub fn render_log_line(op: &str, date: &str, title: &str) -> String {
    format!(
        "## [{date}] {op} | {}",
        truncate_chars(&one_line(title), 120)
    )
}

/// Title and date for one `wiki.*` decision-log entry. Payloads written by
/// this module carry explicit `title`/`date`; older or foreign entries fall
/// back to their metadata (never to page text).
fn log_line_for(entry: &ancientos_core::log::Entry) -> Option<String> {
    let op = entry.kind.strip_prefix("wiki.")?;
    if op.is_empty() || op.contains(char::is_whitespace) {
        return None;
    }
    let date = entry
        .payload
        .get("date")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            entry
                .payload
                .get("at")
                .and_then(|v| v.as_u64())
                .map(|at| crate::brief::date_from_days((at / 86400) as i64))
        })
        .unwrap_or_else(|| "unknown".to_string());
    let title = entry
        .payload
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| match op {
            "ingest" => {
                let pages = entry
                    .payload
                    .get("pages")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                format!("ingested {pages} pages")
            }
            "queued" => {
                let post = entry
                    .payload
                    .get("post_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                format!("queued ingest for {post}")
            }
            "fileback" => {
                let slug = entry
                    .payload
                    .get("slug")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                format!("filed answer as [[{slug}]]")
            }
            "lint" => {
                let n = entry
                    .payload
                    .get("findings")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                format!("lint: {n} findings")
            }
            _ => format!("{op} update"),
        });
    Some(render_log_line(op, &date, &title))
}

/// Grow `log.md` to match the `wiki.*` decision-log entries. Append-only:
/// existing lines must be an exact prefix of the expected lines, and only
/// the missing suffix is appended. Any divergence is an error (fail closed,
/// never rewrite history). Returns the appended line count.
pub fn sync_log_md(log_path: &Path, wiki: &Path) -> Result<usize, String> {
    let log = ancientos_core::log::DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    let expected: Vec<String> = entries.iter().filter_map(log_line_for).collect();
    let path = wiki.join(LOG_FILE_NAME);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("wiki dir failed: {e}"))?;
        }
    }
    let current = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("wiki log read failed: {e}")),
    };
    let existing: Vec<&str> = current.lines().filter(|l| l.starts_with("## [")).collect();
    if existing.len() > expected.len() {
        return Err(
            "wiki log.md has more entries than the decision log; refusing to rewrite".to_string(),
        );
    }
    for (i, line) in existing.iter().enumerate() {
        if *line != expected[i].as_str() {
            return Err(format!(
                "wiki log.md diverged from the decision log at line {}; refusing to rewrite",
                i + 1
            ));
        }
    }
    let missing = &expected[existing.len()..];
    if missing.is_empty() && !current.is_empty() {
        return Ok(0);
    }
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("wiki log write failed: {e}"))?;
        if current.is_empty() {
            writeln!(f, "# wiki log\n").map_err(|e| format!("wiki log write failed: {e}"))?;
        }
        for line in missing {
            writeln!(f, "{line}").map_err(|e| format!("wiki log write failed: {e}"))?;
        }
        f.sync_all()
            .map_err(|e| format!("wiki log sync failed: {e}"))?;
    }
    chmod_0600(&path)?;
    Ok(missing.len())
}

/// Short tokens dropped from term-overlap scoring.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "can", "had", "her", "was", "one",
    "our", "out", "has", "have", "with", "from", "that", "this", "what", "when", "where", "which",
    "who", "why", "how", "about", "into", "does",
];

/// Lowercase alphanumeric tokens (`len >= 3`, stopwords dropped).
fn tokens(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut current = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            current.push(c);
        } else if current.len() >= 3 && !STOPWORDS.contains(&current.as_str()) {
            out.insert(std::mem::take(&mut current));
        } else {
            current.clear();
        }
    }
    if current.len() >= 3 && !STOPWORDS.contains(&current.as_str()) {
        out.insert(current);
    }
    out
}

/// Answer to a wiki query, with citations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAnswer {
    /// Rendered answer markdown (cites `[[pages]]` and raw source ids).
    pub text: String,
    /// Cited page slugs, best match first.
    pub pages: Vec<String>,
    /// Cited raw source ids, sorted.
    pub raw: Vec<String>,
    /// Slug of the filed page, when the answer was filed back.
    pub filed: Option<String>,
}

/// Answer `question` from the wiki: pick up to 3 pages by term overlap over
/// the catalog and page text (no embeddings), then render the top evidence
/// with citations to pages and raw sources. Pure read: no logs, no writes.
/// Empty questions and questions with no term overlap fail closed.
pub fn query(question: &str, wiki: &Path) -> Result<QueryAnswer, String> {
    if question.trim().is_empty() {
        return Err("wiki query needs a non-empty question".to_string());
    }
    let pages = list_pages(wiki);
    if pages.is_empty() {
        return Err("the wiki has no pages yet; ingest a post first".to_string());
    }
    let asking = tokens(question);
    if asking.is_empty() {
        return Err("the question has no searchable terms".to_string());
    }
    let mut scored: Vec<(usize, &Page)> = Vec::new();
    for page in &pages {
        let mut hay = format!("{} {} {}", page.title, page.title, page.summary);
        for c in &page.claims {
            hay.push(' ');
            hay.push_str(&c.text);
        }
        let overlap = tokens(&hay).intersection(&asking).count();
        if overlap > 0 {
            scored.push((overlap, page));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.slug.cmp(&b.1.slug)));
    scored.truncate(3);
    if scored.is_empty() {
        return Err("no wiki pages match the question (no term overlap)".to_string());
    }
    let mut text = format!("## wiki answer\n\nQuestion: {}\n", one_line(question));
    let mut cited_pages = Vec::new();
    let mut cited_raw = BTreeSet::new();
    for (score, page) in &scored {
        cited_pages.push(page.slug.clone());
        text.push_str(&format!(
            "\n### {} ([[{}]], overlap {score})\n",
            page.title, page.slug
        ));
        let mut shown = 0;
        for claim in &page.claims {
            if shown >= 2 {
                break;
            }
            if tokens(&claim.text).intersection(&asking).count() == 0 && shown > 0 {
                continue;
            }
            text.push_str(&format!(
                "- {} (raw: {})\n",
                claim.text,
                claim.sources.join(", ")
            ));
            for s in &claim.sources {
                cited_raw.insert(s.clone());
            }
            shown += 1;
        }
        if shown == 0 {
            text.push_str("(no cited claims on this page)\n");
        }
    }
    let raw: Vec<String> = cited_raw.into_iter().collect();
    text.push_str(&format!(
        "\nPages cited: {}\nRaw sources: {}\n",
        cited_pages
            .iter()
            .map(|s| format!("[[{s}]]"))
            .collect::<Vec<_>>()
            .join(", "),
        raw.join(", ")
    ));
    Ok(QueryAnswer {
        text,
        pages: cited_pages,
        raw,
        filed: None,
    })
}

/// File a query `answer` back as a new page (`answer-<hash>`, suffixed on
/// collision), refresh `index.md`, and log a [`WIKI_FILEBACK_KIND`] entry.
/// Returns the new slug.
pub fn file_back(
    question: &str,
    answer: &QueryAnswer,
    wiki: &Path,
    log_path: &Path,
    now: u64,
) -> Result<String, String> {
    if answer.pages.is_empty() {
        return Err("cannot file back an answer with no cited pages".to_string());
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(question.as_bytes());
    let digest = h.finalize();
    let hash: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    let mut slug = format!("answer-{hash}");
    let mut n = 2u32;
    while page_path(wiki, &slug).exists() {
        slug = format!("answer-{hash}-{n}");
        n += 1;
    }
    let pages = list_pages(wiki);
    let mut claims = Vec::new();
    for cited in &answer.pages {
        if let Some(page) = pages.iter().find(|p| &p.slug == cited) {
            if let Some(claim) = page.claims.first() {
                claims.push(ClaimEdit {
                    text: truncate_chars(&claim.text, 500),
                    sources: claim.sources.clone(),
                });
            }
        }
    }
    if claims.is_empty() {
        return Err("cannot file back: cited pages have no claims to carry".to_string());
    }
    let title = truncate_chars(&format!("Answer: {}", one_line(question)), 140);
    let summary = truncate_chars(
        &format!(
            "Answer citing {} (raw: {}).",
            answer
                .pages
                .iter()
                .map(|s| format!("[[{s}]]"))
                .collect::<Vec<_>>()
                .join(", "),
            answer.raw.join(", ")
        ),
        300,
    );
    let edit = PageEdit {
        slug: slug.clone(),
        title: title.clone(),
        summary,
        claims,
        links: answer.pages.clone(),
    };
    let known = known_raw_ids(wiki);
    let (valid_edits, rejected) = validate_edits(std::slice::from_ref(&edit), &known);
    if valid_edits.is_empty() {
        return Err(format!("file-back edits invalid: {}", rejected.join("; ")));
    }
    let class = sources_class(wiki, &answer.raw);
    apply_edits(wiki, &valid_edits, class)?;
    sync_index(wiki)?;
    let date = crate::brief::date_from_days((now / 86400) as i64);
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = ancientos_core::log::DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    log.append(
        WIKI_FILEBACK_KIND,
        serde_json::json!({
            "slug": slug,
            "title": title,
            "question_chars": question.chars().count(),
            "pages_cited": answer.pages,
            "raw_cited": answer.raw,
            "date": date,
            "at": now,
        }),
    )
    .map_err(|e| e.to_string())?;
    sync_log_md(log_path, wiki)?;
    Ok(slug)
}

/// Answer `question` and file the answer back as a new page (logged).
/// Convenience for `wiki query --file-back`.
pub fn query_file_back(
    question: &str,
    wiki: &Path,
    log_path: &Path,
    now: u64,
) -> Result<QueryAnswer, String> {
    let mut answer = query(question, wiki)?;
    let slug = file_back(question, &answer, wiki, log_path, now)?;
    answer.filed = Some(slug);
    Ok(answer)
}

/// Lint finding kinds.
pub const FINDING_ORPHAN: &str = "orphan";
pub const FINDING_MISSING_RAW: &str = "missing_raw";
pub const FINDING_DUPLICATE_SLUG: &str = "duplicate_slug";
pub const FINDING_NEAR_DUPLICATE_TITLE: &str = "near_duplicate_title";
pub const FINDING_UNCITED_CLAIM: &str = "uncited_claim";
pub const FINDING_MISSING_CONCEPT: &str = "missing_concept";
/// Defensive extra: a page file that does not parse (lint never crashes on
/// malformed input; the finding proposes a human look).
pub const FINDING_UNPARSEABLE: &str = "unparseable";

/// One lint finding: what is wrong with which page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: String,
    pub slug: String,
    pub detail: String,
}

/// How many pages must mention a concept before it deserves its own page.
pub const CONCEPT_MENTION_THRESHOLD: usize = 3;

/// Normalized title for near-duplicate comparison: lowercase alphanumeric
/// words, single-spaced.
fn normalized_title(title: &str) -> String {
    one_line(&title.to_lowercase())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Word-set Jaccard similarity of two normalized titles.
fn title_jaccard(a: &str, b: &str) -> f64 {
    let set_a: BTreeSet<&str> = a.split_whitespace().collect();
    let set_b: BTreeSet<&str> = b.split_whitespace().collect();
    if set_a.is_empty() || set_b.is_empty() {
        return 0.0;
    }
    let inter = set_a.intersection(&set_b).count() as f64;
    let union = set_a.union(&set_b).count() as f64;
    inter / union
}

/// True when `concept` appears in `text` as a whole token: the chars around
/// the match are not `[a-z0-9_]` (so `gate` matches "the approval gate."
/// but not "aggregate", and `decision_log` matches exactly).
fn mentions_concept(text: &str, concept: &str) -> bool {
    if concept.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(concept) {
        let abs = from + pos;
        let before_ok = abs == 0 || {
            let c = lower.as_bytes()[abs - 1] as char;
            !(c.is_ascii_alphanumeric() || c == '_')
        };
        let after = abs + concept.len();
        let after_ok = after >= lower.len() || {
            let c = lower.as_bytes()[after] as char;
            !(c.is_ascii_alphanumeric() || c == '_')
        };
        if before_ok && after_ok {
            return true;
        }
        from = abs + 1;
        if from >= lower.len() {
            break;
        }
    }
    false
}

/// Check the wiki for orphans (no inbound `[[links]]`), pages citing missing
/// raw sources, duplicate slugs, near-duplicate titles, claims without
/// sources, and concepts mentioned in 3+ pages without their own page. Pure
/// read: never writes anything. Findings sort by (kind, slug, detail).
pub fn lint(wiki: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    // Raw page files: stems plus parse results (unparseable files are
    // findings, not crashes).
    let pages_dir = wiki.join(PAGES_DIR_NAME);
    let mut stems: Vec<String> = Vec::new();
    let mut parsed: Vec<Page> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&pages_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            if !valid_slug(stem) {
                continue;
            }
            stems.push(stem.to_string());
            match std::fs::read_to_string(entry.path()) {
                Ok(text) => match parse_page(&text) {
                    Ok(mut page) => {
                        page.slug = stem.to_string();
                        parsed.push(page);
                    }
                    Err(e) => findings.push(Finding {
                        kind: FINDING_UNPARSEABLE.to_string(),
                        slug: stem.to_string(),
                        detail: format!("page does not parse: {e}"),
                    }),
                },
                Err(e) => findings.push(Finding {
                    kind: FINDING_UNPARSEABLE.to_string(),
                    slug: stem.to_string(),
                    detail: format!("page is unreadable: {e}"),
                }),
            }
        }
    }
    stems.sort();
    parsed.sort_by(|a, b| a.slug.cmp(&b.slug));
    // Duplicate slugs: same slug modulo case across files.
    let mut by_lower: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for stem in &stems {
        by_lower
            .entry(stem.to_lowercase())
            .or_default()
            .push(stem.clone());
    }
    for (lower, files) in &by_lower {
        if files.len() > 1 {
            for stem in files {
                findings.push(Finding {
                    kind: FINDING_DUPLICATE_SLUG.to_string(),
                    slug: stem.clone(),
                    detail: format!(
                        "slug collides case-insensitively: {lower} in {}",
                        files.join(", ")
                    ),
                });
            }
        }
    }
    // Inbound links per page (self-links do not count).
    let slugs: BTreeSet<&str> = parsed.iter().map(|p| p.slug.as_str()).collect();
    let mut inbound: BTreeMap<&str, usize> = BTreeMap::new();
    for page in &parsed {
        for link in &page.links {
            if link != &page.slug && slugs.contains(link.as_str()) {
                *inbound.entry(link.as_str()).or_default() += 1;
            }
        }
    }
    for page in &parsed {
        if inbound.get(page.slug.as_str()).copied().unwrap_or(0) == 0 {
            findings.push(Finding {
                kind: FINDING_ORPHAN.to_string(),
                slug: page.slug.clone(),
                detail: "no other page links here".to_string(),
            });
        }
    }
    // Claims without sources, and citations of missing raw.
    for page in &parsed {
        for claim in &page.claims {
            if claim.sources.is_empty() {
                findings.push(Finding {
                    kind: FINDING_UNCITED_CLAIM.to_string(),
                    slug: page.slug.clone(),
                    detail: format!(
                        "claim without sources: {:?}",
                        truncate_chars(&claim.text, 80)
                    ),
                });
            } else {
                for source in &claim.sources {
                    if !raw_exists(wiki, source) {
                        findings.push(Finding {
                            kind: FINDING_MISSING_RAW.to_string(),
                            slug: page.slug.clone(),
                            detail: format!("cites missing raw source {source:?}"),
                        });
                    }
                }
            }
        }
    }
    // Near-duplicate titles: identical normalized forms or word Jaccard >= 0.8.
    let titles: Vec<(&Page, String)> = parsed
        .iter()
        .map(|p| (p, normalized_title(&p.title)))
        .collect();
    for i in 0..titles.len() {
        for j in (i + 1)..titles.len() {
            let (a, na) = &titles[i];
            let (b, nb) = &titles[j];
            if na == nb || title_jaccard(na, nb) >= 0.8 {
                findings.push(Finding {
                    kind: FINDING_NEAR_DUPLICATE_TITLE.to_string(),
                    slug: b.slug.clone(),
                    detail: format!(
                        "title {:?} near-duplicates [[{}]] {:?}",
                        b.title, a.slug, a.title
                    ),
                });
            }
        }
    }
    // Concepts mentioned in 3+ pages without their own page: the known
    // AncientOS modules plus every [[link]] target.
    let mut candidates = BTreeSet::new();
    for m in crate::idea_card::known_modules() {
        candidates.insert(m.to_lowercase());
    }
    for page in &parsed {
        for link in &page.links {
            candidates.insert(link.to_lowercase());
        }
    }
    let have: BTreeSet<String> = parsed.iter().map(|p| p.slug.to_lowercase()).collect();
    for candidate in &candidates {
        if have.contains(candidate) {
            continue;
        }
        let mut mentioning = Vec::new();
        for page in &parsed {
            let mut hay = format!("{} {} {}", page.title, page.summary, page.links.join(" "));
            for c in &page.claims {
                hay.push(' ');
                hay.push_str(&c.text);
            }
            let linked = page.links.iter().any(|l| l.to_lowercase() == *candidate);
            if linked || mentions_concept(&hay, candidate) {
                mentioning.push(page.slug.clone());
            }
        }
        if mentioning.len() >= CONCEPT_MENTION_THRESHOLD {
            mentioning.sort();
            findings.push(Finding {
                kind: FINDING_MISSING_CONCEPT.to_string(),
                slug: candidate.clone(),
                detail: format!(
                    "mentioned in {} pages ({}) but has no page",
                    mentioning.len(),
                    mentioning.join(", ")
                ),
            });
        }
    }
    findings.sort_by(|a, b| (&a.kind, &a.slug, &a.detail).cmp(&(&b.kind, &b.slug, &b.detail)));
    findings
}

/// Outcome of lint plus proposals.
#[derive(Debug, Clone)]
pub struct LintReport {
    pub findings: Vec<Finding>,
    pub approvals: Vec<String>,
}

/// Run [`lint`] and propose every finding as a `wiki.edit` approval
/// ([`Risk::High`](ancientos_core::gate::Risk)); findings are never
/// auto-applied — pages are untouched. A [`WIKI_LINT_KIND`] entry records
/// the run. Returns the findings plus the approval ids.
pub fn lint_and_propose(wiki: &Path, log_path: &Path, now: u64) -> Result<LintReport, String> {
    let findings = lint(wiki);
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = ancientos_core::log::DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    let mut approvals = Vec::new();
    for f in &findings {
        let req = ancientos_core::approvals::request(
            &mut log,
            crate::brief::AGENT_NAME,
            ancientos_core::gate::WIKI_EDIT_ACTION,
            &format!(
                "wiki lint [{}] {}: {}",
                f.kind,
                f.slug,
                truncate_chars(&f.detail, 140)
            ),
            serde_json::json!({"kind": f.kind, "slug": f.slug, "detail": f.detail}),
            now,
            None,
        )
        .map_err(|e| e.to_string())?;
        approvals.push(req.id);
    }
    let date = crate::brief::date_from_days((now / 86400) as i64);
    let title = format!(
        "lint: {} findings, {} approvals",
        findings.len(),
        approvals.len()
    );
    log.append(
        WIKI_LINT_KIND,
        serde_json::json!({
            "findings": findings.len(),
            "approvals": approvals,
            "title": title,
            "date": date,
            "at": now,
        }),
    )
    .map_err(|e| e.to_string())?;
    drop(log);
    sync_log_md(log_path, wiki)?;
    Ok(LintReport {
        findings,
        approvals,
    })
}

/// Render a lint report for the CLI: one line per finding plus the proposed
/// approval ids.
pub fn render_lint_report(report: &LintReport) -> String {
    let mut out = format!("wiki lint: {} findings\n", report.findings.len());
    for f in &report.findings {
        out.push_str(&format!("[{}] {}: {}\n", f.kind, f.slug, f.detail));
    }
    if report.approvals.is_empty() {
        out.push_str("no approvals proposed\n");
    } else {
        out.push_str(&format!(
            "proposed {} approvals (action wiki.edit, nothing applied):\n",
            report.approvals.len()
        ));
        for id in &report.approvals {
            out.push_str(&format!("  {id}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const DATE: &str = "2026-10-27";
    const NOW: u64 = 1_793_577_600;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join(WIKI_DIR_NAME);
        let log = dir.path().join("decision_log.jsonl");
        (dir, wiki, log)
    }

    fn public_input(post_id: &str) -> IngestInput {
        IngestInput {
            post_id: post_id.to_string(),
            url: format!("https://x.com/i/status/{post_id}"),
            text: format!(
                "[synthetic] Local file tooling note for {post_id}. It keeps an append-only log of approvals."
            ),
            thread: vec![format!(
                "[synthetic] Follow-up on {post_id} about scheduler reminders."
            )],
            delegate_notes: Some("[synthetic] Delegate notes about the approval gate.".to_string()),
            cards: Vec::new(),
        }
    }

    fn card(id: &str, post_id: &str, idea: &str, mapping: &[&str]) -> crate::idea_card::IdeaCard {
        crate::idea_card::IdeaCard {
            id: id.to_string(),
            source_url: format!("https://x.com/i/status/{post_id}"),
            idea: idea.to_string(),
            ancientos_mapping: mapping.iter().map(|s| s.to_string()).collect(),
            effort: crate::idea_card::Effort::S,
            status: crate::idea_card::Status::New,
            created_at: NOW,
            park_until: None,
        }
    }

    fn ctx<'a>(wiki: &'a Path, log: &'a Path, allow_paid: bool) -> IngestCtx<'a> {
        IngestCtx {
            wiki,
            log_path: log,
            date: DATE,
            now: NOW,
            allow_paid,
            muse: None,
            local: None,
        }
    }

    #[test]
    fn raw_is_immutable_and_versions_on_change() {
        let (_dir, wiki, _log) = setup();
        let first = write_raw(
            &wiki,
            "p1",
            "https://x.com/i/status/p1",
            DataClass::Public,
            DATE,
            "synthetic post one",
            &[],
            None,
        )
        .unwrap();
        assert_eq!(
            first.path,
            wiki.join(RAW_DIR_NAME).join("p1.md"),
            "first write lands in the base file"
        );
        assert!(!first.versioned && !first.reused);
        let bytes = std::fs::read(&first.path).unwrap();
        // Same content (even on another date) reuses the file untouched.
        let again = write_raw(
            &wiki,
            "p1",
            "https://x.com/i/status/p1",
            DataClass::Public,
            "2026-10-28",
            "synthetic post one",
            &[],
            None,
        )
        .unwrap();
        assert!(again.reused);
        assert_eq!(again.path, first.path);
        assert_eq!(std::fs::read(&first.path).unwrap(), bytes);
        // Changed content mints a versioned file; the original never changes.
        let changed = write_raw(
            &wiki,
            "p1",
            "https://x.com/i/status/p1",
            DataClass::Public,
            DATE,
            "synthetic post one, edited",
            &[],
            None,
        )
        .unwrap();
        assert!(!changed.reused && changed.versioned);
        assert_eq!(changed.path, wiki.join(RAW_DIR_NAME).join("p1-2.md"));
        assert_eq!(std::fs::read(&first.path).unwrap(), bytes);
        assert!(known_raw_ids(&wiki).contains("p1"));
    }

    #[cfg(unix)]
    #[test]
    fn wiki_files_are_0600_and_dirs_are_0700() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, wiki, log) = setup();
        let input = public_input("p1");
        ingest_with(&input, &ctx(&wiki, &log, false)).unwrap();
        for path in [
            wiki.join(RAW_DIR_NAME).join("p1.md"),
            wiki.join(PAGES_DIR_NAME).join("post-p1.md"),
            wiki.join(INDEX_FILE_NAME),
            wiki.join(LOG_FILE_NAME),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} is {mode:o}", path.display());
        }
        for dir in [wiki.join(RAW_DIR_NAME), wiki.join(PAGES_DIR_NAME)] {
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} is {mode:o}", dir.display());
        }
    }

    #[test]
    fn slugify_and_id_validation_reject_traversal() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  ---  "), "page");
        assert_eq!(slugify("Gate"), "gate");
        for bad in ["", "../x", "a/b", "a b", "x.md\0", ".", ".."] {
            assert!(!valid_id(bad), "{bad:?} must be invalid");
            assert!(!valid_slug(bad), "{bad:?} must be invalid");
        }
        assert!(write_raw(
            &setup().1,
            "../evil",
            "https://x.com/i/status/1",
            DataClass::Public,
            DATE,
            "x",
            &[],
            None,
        )
        .is_err());
    }

    #[test]
    fn fallback_ingest_compounds_cards_and_concepts() {
        let (_dir, wiki, log) = setup();
        let mut input = public_input("p1");
        input.cards = vec![card(
            "abc123",
            "p1",
            "Drag a card to approve via the gate",
            &["gate"],
        )];
        let report = ingest_with(&input, &ctx(&wiki, &log, false)).unwrap();
        assert_eq!(report.mode, MODE_FALLBACK);
        // One post page, one idea-card page, one concept page.
        assert_eq!(report.pages, vec!["gate", "idea-abc123", "post-p1"]);
        // Every claim cites the raw source.
        for page in list_pages(&wiki) {
            assert!(!page.claims.is_empty(), "[[{}]] has claims", page.slug);
            for claim in &page.claims {
                assert_eq!(claim.sources, vec!["p1"], "cited: {}", claim.text);
            }
            assert_eq!(page.class, DataClass::Public);
        }
        // The concept page links back to the idea page and vice versa.
        let gate = parse_page(&std::fs::read_to_string(page_path(&wiki, "gate")).unwrap()).unwrap();
        assert!(gate.links.contains(&"idea-abc123".to_string()), "{gate:?}");
        // index.md: one line per page with summary and source count.
        let index = std::fs::read_to_string(wiki.join(INDEX_FILE_NAME)).unwrap();
        for slug in &report.pages {
            let line = index
                .lines()
                .find(|l| l.contains(&format!("[[{slug}]]")))
                .unwrap_or_else(|| panic!("index misses {slug}:\n{index}"));
            assert!(line.contains("(sources: 1)"), "{line}");
        }
        // log.md: one human-readable ingest line from the decision log.
        let log_md = std::fs::read_to_string(wiki.join(LOG_FILE_NAME)).unwrap();
        assert!(
            log_md.contains("## [2026-10-27] ingest | p1: 3 pages via fallback"),
            "{log_md}"
        );
        // The decision log carries metadata only, never post text.
        let entries = ancientos_core::log::DecisionLog::open(&log)
            .unwrap()
            .verify()
            .unwrap();
        let ingest = entries
            .iter()
            .find(|e| e.kind == WIKI_INGEST_KIND)
            .expect("wiki.ingest entry");
        assert_eq!(ingest.payload["mode"], MODE_FALLBACK);
        let logged = ingest.payload.to_string();
        assert!(!logged.contains("append-only log of approvals"), "{logged}");
    }

    #[test]
    fn paid_ingest_uses_mock_muse_over_sanitized_public_text() {
        let (_dir, wiki, log) = setup();
        let seen = RefCell::new(Vec::new());
        let muse = |prompt: &str| -> Result<String, String> {
            seen.borrow_mut().push(prompt.to_string());
            Ok(r#"{"pages":[{"slug":"spark-notes","title":"Spark notes","summary":"Paid test page","claims":[{"text":"Tools keep logs","sources":["p1"]}],"links":[]}]}"#.to_string())
        };
        let input = public_input("p1");
        let report = ingest_with(
            &input,
            &IngestCtx {
                wiki: &wiki,
                log_path: &log,
                date: DATE,
                now: NOW,
                allow_paid: true,
                muse: Some(&muse),
                local: None,
            },
        )
        .unwrap();
        assert_eq!(report.mode, MODE_PAID);
        assert_eq!(seen.borrow().len(), 1, "exactly one paid call");
        assert_eq!(report.pages, vec!["spark-notes"]);
        // The prompt carried only sanitized Public text for source p1.
        let prompt = &seen.borrow()[0];
        assert!(
            prompt.contains("[\"p1\"]") || prompt.contains("[p1]"),
            "{prompt}"
        );
        assert!(prompt.contains("append-only log of approvals"), "{prompt}");
        // The router allowed the Public egress (logged).
        let entries = ancientos_core::log::DecisionLog::open(&log)
            .unwrap()
            .verify()
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.kind == ancientos_router::data_class::EGRESS_ALLOWED_KIND),
            "egress allowed entry missing"
        );
        // Re-ingesting the unchanged source makes no second paid call.
        let report = ingest_with(
            &input,
            &IngestCtx {
                wiki: &wiki,
                log_path: &log,
                date: DATE,
                now: NOW,
                allow_paid: true,
                muse: Some(&muse),
                local: None,
            },
        )
        .unwrap();
        assert_eq!(report.mode, MODE_FALLBACK);
        assert!(report.raw.reused);
        assert_eq!(seen.borrow().len(), 1, "reused raw triggers no paid call");
    }

    #[test]
    fn edits_without_citations_are_rejected() {
        let known = BTreeSet::from(["p1".to_string()]);
        let good = PageEdit {
            slug: "ok-page".to_string(),
            title: "Ok".to_string(),
            summary: "A cited page.".to_string(),
            claims: vec![ClaimEdit {
                text: "Cited claim.".to_string(),
                sources: vec!["p1".to_string()],
            }],
            links: vec!["other".to_string()],
        };
        let cases: Vec<(PageEdit, &str)> = vec![
            (
                PageEdit {
                    slug: "uncited".to_string(),
                    claims: vec![ClaimEdit {
                        text: "No sources.".to_string(),
                        sources: Vec::new(),
                    }],
                    ..good.clone()
                },
                "without cited sources",
            ),
            (
                PageEdit {
                    slug: "unknown-source".to_string(),
                    claims: vec![ClaimEdit {
                        text: "Ghost cite.".to_string(),
                        sources: vec!["ghost".to_string()],
                    }],
                    ..good.clone()
                },
                "unknown raw source",
            ),
            (
                PageEdit {
                    slug: "../evil".to_string(),
                    ..good.clone()
                },
                "invalid slug",
            ),
            (
                PageEdit {
                    slug: "no-title".to_string(),
                    title: String::new(),
                    ..good.clone()
                },
                "empty title",
            ),
            (
                PageEdit {
                    slug: "no-claims".to_string(),
                    claims: Vec::new(),
                    ..good.clone()
                },
                "no claims",
            ),
        ];
        for (edit, want) in cases {
            let (valid, rejected) = validate_edits(&[edit], &known);
            assert!(valid.is_empty(), "must reject: {want}");
            assert_eq!(rejected.len(), 1);
            assert!(rejected[0].contains(want), "{}", rejected[0]);
        }
        let (valid, rejected) = validate_edits(&[good], &known);
        assert!(rejected.is_empty());
        assert_eq!(valid.len(), 1);
        // End to end: a paid reply without citations falls back, still cited.
        let (_dir, wiki, log) = setup();
        let muse = |_: &str| -> Result<String, String> {
            Ok(r#"{"pages":[{"slug":"bad","title":"Bad","summary":"Uncited","claims":[{"text":"No cite","sources":[]}],"links":[]}]}"#.to_string())
        };
        let input = public_input("p1");
        let report = ingest_with(
            &input,
            &IngestCtx {
                wiki: &wiki,
                log_path: &log,
                date: DATE,
                now: NOW,
                allow_paid: true,
                muse: Some(&muse),
                local: None,
            },
        )
        .unwrap();
        assert_eq!(report.mode, MODE_FALLBACK);
        assert!(!report.rejected.is_empty());
        for page in list_pages(&wiki) {
            for claim in &page.claims {
                assert!(!claim.sources.is_empty(), "fallback stays cited");
            }
        }
    }

    #[test]
    fn private_sources_never_reach_the_paid_prompt() {
        let (_dir, wiki, log) = setup();
        let muse_calls = RefCell::new(Vec::new());
        let muse = |prompt: &str| -> Result<String, String> {
            muse_calls.borrow_mut().push(prompt.to_string());
            Ok(r#"{"pages":[]}"#.to_string())
        };
        let mut input = public_input("priv1");
        // An email address makes the source Confidential.
        input.text =
            "[synthetic] Note for case review, contact syn.reviewer@example.org soon.".to_string();
        assert_eq!(input.class(), DataClass::Confidential);
        // No local model: deterministic fallback, Muse never called.
        let report = ingest_with(
            &input,
            &IngestCtx {
                wiki: &wiki,
                log_path: &log,
                date: DATE,
                now: NOW,
                allow_paid: true,
                muse: Some(&muse),
                local: None,
            },
        )
        .unwrap();
        assert_eq!(report.mode, MODE_FALLBACK);
        assert!(muse_calls.borrow().is_empty(), "Muse must not be called");
        // The raw source records the Confidential class (sanitized text).
        let raw_text = std::fs::read_to_string(&report.raw.path).unwrap();
        assert!(raw_text.contains("class: confidential"), "{raw_text}");
        assert!(!raw_text.contains("syn.reviewer@example.org"), "{raw_text}");
        // The refused hosted egress is logged.
        let entries = ancientos_core::log::DecisionLog::open(&log)
            .unwrap()
            .verify()
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.kind == ancientos_router::data_class::EGRESS_DENIED_KIND),
            "egress denied entry missing"
        );
        // With a local model configured, the private source compiles locally.
        let local_seen = RefCell::new(Vec::new());
        let local = |prompt: &str| -> Result<String, String> {
            local_seen.borrow_mut().push(prompt.to_string());
            Ok(r#"{"pages":[{"slug":"private-notes","title":"Private notes","summary":"Local test page","claims":[{"text":"Review is pending","sources":["priv1"]}],"links":[]}]}"#.to_string())
        };
        let (_dir2, wiki2, log2) = setup();
        let report = ingest_with(
            &input,
            &IngestCtx {
                wiki: &wiki2,
                log_path: &log2,
                date: DATE,
                now: NOW,
                allow_paid: true,
                muse: Some(&muse),
                local: Some(&local),
            },
        )
        .unwrap();
        assert_eq!(report.mode, MODE_LOCAL);
        assert!(muse_calls.borrow().is_empty(), "Muse still never called");
        assert_eq!(local_seen.borrow().len(), 1);
        assert!(
            local_seen.borrow()[0].contains("syn.reviewer@example.org"),
            "local prompt keeps original text on-device"
        );
    }

    #[test]
    fn log_md_is_append_only_and_refuses_divergence() {
        let (_dir, wiki, log) = setup();
        let day = crate::brief::date_from_days((NOW / 86400) as i64);
        queue_ingest(&log, QUEUE_REASON_MANUAL, "p1", "p1", NOW).unwrap();
        assert_eq!(sync_log_md(&log, &wiki).unwrap(), 1);
        let first = std::fs::read_to_string(wiki.join(LOG_FILE_NAME)).unwrap();
        assert!(
            first.contains(&format!(
                "## [{day}] queued | queued ingest for p1 (manual)"
            )),
            "{first}"
        );
        // Syncing again appends nothing.
        assert_eq!(sync_log_md(&log, &wiki).unwrap(), 0);
        // A new entry appends one line; old lines are untouched.
        queue_ingest(&log, QUEUE_REASON_MANUAL, "p2", "p2", NOW + 60).unwrap();
        assert_eq!(sync_log_md(&log, &wiki).unwrap(), 1);
        let second = std::fs::read_to_string(wiki.join(LOG_FILE_NAME)).unwrap();
        assert!(second.starts_with(&first), "prefix kept:\n{second}");
        // A hand-edited log.md diverges: fail closed, never rewrite.
        let tampered = second.replacen("queued ingest for p1", "edited history", 1);
        std::fs::write(wiki.join(LOG_FILE_NAME), tampered).unwrap();
        assert!(sync_log_md(&log, &wiki).is_err());
    }

    #[test]
    fn query_answers_with_page_and_raw_citations() {
        let (_dir, wiki, log) = setup();
        let mut sched = public_input("sched1");
        sched.text = "[synthetic] The weekly scheduler sends reminders every morning.".to_string();
        sched.thread = Vec::new();
        sched.delegate_notes = None;
        ingest_with(&sched, &ctx(&wiki, &log, false)).unwrap();
        let mut gate = public_input("gate1");
        gate.text = "[synthetic] The approval gate drags cards into review.".to_string();
        gate.thread = Vec::new();
        gate.delegate_notes = None;
        ingest_with(&gate, &ctx(&wiki, &log, false)).unwrap();
        let answer = query("how do scheduler reminders work", &wiki).unwrap();
        assert!(
            answer.pages.contains(&"post-sched1".to_string()),
            "best match cited: {:?}",
            answer.pages
        );
        assert!(answer.raw.contains(&"sched1".to_string()), "{answer:?}");
        assert!(answer.text.contains("[[post-sched1]]"), "{}", answer.text);
        assert!(answer.text.contains("sched1"), "{}", answer.text);
        assert!(answer.filed.is_none(), "plain query files nothing");
        // Fail closed: empty questions and no-overlap questions.
        assert!(query("   ", &wiki).is_err());
        assert!(query("quantum entanglement zyzzyva", &wiki).is_err());
    }

    #[test]
    fn file_back_creates_a_logged_page() {
        let (_dir, wiki, log) = setup();
        ingest_with(&public_input("p1"), &ctx(&wiki, &log, false)).unwrap();
        let before: Vec<String> = list_pages(&wiki).iter().map(|p| p.slug.clone()).collect();
        let answer = query_file_back("what does the tooling note say", &wiki, &log, NOW).unwrap();
        let slug = answer.filed.clone().expect("filed slug");
        assert!(slug.starts_with("answer-"), "{slug}");
        // The filed page cites the same pages and raw sources.
        let filed = parse_page(&std::fs::read_to_string(page_path(&wiki, &slug)).unwrap()).unwrap();
        assert_eq!(filed.links, answer.pages);
        assert!(!filed.claims.is_empty());
        for claim in &filed.claims {
            assert!(!claim.sources.is_empty());
        }
        // The catalog grew by one and the file-back is logged both ways.
        let after: Vec<String> = list_pages(&wiki).iter().map(|p| p.slug.clone()).collect();
        assert_eq!(after.len(), before.len() + 1);
        let index = std::fs::read_to_string(wiki.join(INDEX_FILE_NAME)).unwrap();
        assert!(index.contains(&format!("[[{slug}]]")), "{index}");
        let entries = ancientos_core::log::DecisionLog::open(&log)
            .unwrap()
            .verify()
            .unwrap();
        assert!(entries.iter().any(|e| e.kind == WIKI_FILEBACK_KIND));
        let log_md = std::fs::read_to_string(wiki.join(LOG_FILE_NAME)).unwrap();
        assert!(log_md.contains("] fileback | "), "{log_md}");
    }

    /// Seed helper: one raw plus one page file with the given markdown.
    fn seed_page(wiki: &Path, slug: &str, md: &str) {
        let pages = wiki.join(PAGES_DIR_NAME);
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(pages.join(format!("{slug}.md")), md).unwrap();
    }

    fn seed_raw(wiki: &Path, id: &str) {
        write_raw(
            wiki,
            id,
            &format!("https://x.com/i/status/{id}"),
            DataClass::Public,
            DATE,
            &format!("synthetic source {id}"),
            &[],
            None,
        )
        .unwrap();
    }

    #[test]
    fn lint_detects_each_finding_class() {
        let (_dir, wiki, _log) = setup();
        seed_raw(&wiki, "p1");
        // Orphan: nobody links here (and it links nowhere).
        seed_page(
            &wiki,
            "lonely",
            "# Lonely\nslug: lonely\nsources: p1\nclass: public\nsummary: Alone.\n\n## claims\n- Alone claim. [sources: p1]\n\n## links\nnone\n",
        );
        // Missing raw: cites a ghost source (linked, so not an orphan).
        seed_page(
            &wiki,
            "ghost-cite",
            "# Ghost cite\nslug: ghost-cite\nsources: ghost\nclass: public\nsummary: Ghost.\n\n## claims\n- Ghost claim. [sources: ghost]\n\n## links\n- [[lonely]]\n",
        );
        // Duplicate slugs: same slug modulo case.
        seed_page(
            &wiki,
            "Case",
            "# Case upper\nslug: Case\nsources: p1\nclass: public\nsummary: Upper.\n\n## claims\n- Upper claim. [sources: p1]\n\n## links\n- [[lonely]]\n",
        );
        seed_page(
            &wiki,
            "case",
            "# Case lower\nslug: case\nsources: p1\nclass: public\nsummary: Lower.\n\n## claims\n- Lower claim. [sources: p1]\n\n## links\n- [[lonely]]\n",
        );
        // Near-duplicate titles.
        seed_page(
            &wiki,
            "dup-a",
            "# Weekly scheduler reminders\nslug: dup-a\nsources: p1\nclass: public\nsummary: A.\n\n## claims\n- Claim A. [sources: p1]\n\n## links\n- [[lonely]]\n",
        );
        seed_page(
            &wiki,
            "dup-b",
            "# Weekly scheduler reminders!\nslug: dup-b\nsources: p1\nclass: public\nsummary: B.\n\n## claims\n- Claim B. [sources: p1]\n\n## links\n- [[lonely]]\n",
        );
        // Uncited claim.
        seed_page(
            &wiki,
            "uncited",
            "# Uncited\nslug: uncited\nsources: \nclass: public\nsummary: U.\n\n## claims\n- A claim with no marker.\n\n## links\n- [[lonely]]\n",
        );
        // Missing concept: three pages mention the scheduler, none owns it.
        for (slug, title) in [("m1", "Alpha"), ("m2", "Beta"), ("m3", "Gamma")] {
            seed_page(
                &wiki,
                slug,
                &format!(
                    "# {title}\nslug: {slug}\nsources: p1\nclass: public\nsummary: {title}.\n\n## claims\n- The scheduler sends reminders. [sources: p1]\n\n## links\n- [[lonely]]\n"
                ),
            );
        }
        let findings = lint(&wiki);
        for kind in [
            FINDING_ORPHAN,
            FINDING_MISSING_RAW,
            FINDING_DUPLICATE_SLUG,
            FINDING_NEAR_DUPLICATE_TITLE,
            FINDING_UNCITED_CLAIM,
            FINDING_MISSING_CONCEPT,
        ] {
            assert!(
                findings.iter().any(|f| f.kind == kind),
                "missing {kind} in {findings:?}"
            );
        }
        // Spot-check the details point at the right pages.
        assert!(
            findings
                .iter()
                .any(|f| f.kind == FINDING_ORPHAN && f.slug == "ghost-cite"),
            "{findings:?}"
        );
        assert!(
            findings
                .iter()
                .any(|f| f.kind == FINDING_MISSING_RAW && f.detail.contains("ghost")),
            "{findings:?}"
        );
        assert!(
            findings
                .iter()
                .any(|f| f.kind == FINDING_MISSING_CONCEPT && f.slug == "scheduler"),
            "{findings:?}"
        );
    }

    #[test]
    fn lint_proposes_approvals_and_never_edits() {
        let (_dir, wiki, log) = setup();
        seed_raw(&wiki, "p1");
        seed_page(
            &wiki,
            "lonely",
            "# Lonely\nslug: lonely\nsources: p1\nclass: public\nsummary: Alone.\n\n## claims\n- Alone claim. [sources: p1]\n\n## links\nnone\n",
        );
        let before = std::fs::read_to_string(page_path(&wiki, "lonely")).unwrap();
        let report = lint_and_propose(&wiki, &log, NOW).unwrap();
        assert!(!report.findings.is_empty());
        assert_eq!(report.findings.len(), report.approvals.len());
        // Pages are byte-identical: nothing was auto-applied.
        assert_eq!(
            std::fs::read_to_string(page_path(&wiki, "lonely")).unwrap(),
            before
        );
        // Every proposal is a pending wiki.edit approval (High risk gate).
        let clog = ancientos_core::log::DecisionLog::open(&log).unwrap();
        let pending = ancientos_core::approvals::pending(&clog, NOW).unwrap();
        assert_eq!(pending.len(), report.approvals.len());
        for a in &pending {
            assert_eq!(a.request.action, ancientos_core::gate::WIKI_EDIT_ACTION);
        }
        let gate = ancientos_core::gate::default_gate();
        assert_eq!(
            gate.evaluate(&ancientos_core::gate::Proposal {
                agent: "t".to_string(),
                action: ancientos_core::gate::WIKI_EDIT_ACTION.to_string(),
            }),
            ancientos_core::gate::Verdict::NeedsHuman
        );
        // The lint run itself is logged.
        let entries = clog.verify().unwrap();
        assert!(entries.iter().any(|e| e.kind == WIKI_LINT_KIND));
        let rendered = render_lint_report(&report);
        assert!(rendered.contains("wiki.edit"), "{rendered}");
    }

    #[test]
    fn queue_and_delegated_ids_fold_vote_and_card_sources() {
        let (_dir, _wiki, log) = setup();
        queue_ingest(&log, QUEUE_REASON_ADOPT, "card1", "p9", NOW).unwrap();
        let entries = ancientos_core::log::DecisionLog::open(&log)
            .unwrap()
            .verify()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, WIKI_QUEUED_KIND);
        assert_eq!(entries[0].payload["reason"], QUEUE_REASON_ADOPT);
        assert_eq!(
            queue_ingest(&log, "", "card1", "p9", NOW).unwrap_err(),
            "wiki queue needs a reason and a post id"
        );
        // URL parsing.
        assert_eq!(
            post_id_from_url("https://x.com/syn_user/status/12345"),
            Some("12345".to_string())
        );
        assert_eq!(post_id_from_url("https://x.com/i/status/"), None);
        assert_eq!(post_id_from_url("not a url"), None);
        // Delegated ids fold delegate entries plus card sources.
        let mut log2 =
            ancientos_core::log::DecisionLog::open(log.with_file_name("d2.jsonl")).unwrap();
        log2.append(
            crate::vote::DELEGATE_QUEUED_KIND,
            serde_json::json!({"post_id": "p1", "category": "Rust"}),
        )
        .unwrap();
        log2.append(
            crate::delegate::DELEGATE_DONE_KIND,
            serde_json::json!({"post_id": "p2", "thread_posts": 1}),
        )
        .unwrap();
        let entries2 = log2.verify_or_empty().unwrap();
        let cards = vec![card("c1", "p3", "Synthetic idea", &["gate"])];
        assert_eq!(
            delegated_post_ids(&entries2, &cards),
            vec!["p1".to_string(), "p2".to_string(), "p3".to_string()]
        );
    }

    #[test]
    fn page_class_is_the_max_of_its_sources() {
        let (_dir, wiki, _log) = setup();
        write_raw(
            &wiki,
            "pub1",
            "https://x.com/i/status/pub1",
            DataClass::Public,
            DATE,
            "synthetic public",
            &[],
            None,
        )
        .unwrap();
        write_raw(
            &wiki,
            "priv1",
            "https://x.com/i/status/priv1",
            DataClass::Confidential,
            DATE,
            "synthetic private",
            &[],
            None,
        )
        .unwrap();
        // Unknown sources taint to Restricted.
        assert_eq!(raw_class(&wiki, "ghost"), DataClass::Restricted);
        assert_eq!(
            sources_class(&wiki, &["pub1".to_string(), "priv1".to_string()]),
            DataClass::Confidential
        );
        let edits = vec![PageEdit {
            slug: "mixed".to_string(),
            title: "Mixed".to_string(),
            summary: "Mixed sources.".to_string(),
            claims: vec![
                ClaimEdit {
                    text: "Public claim.".to_string(),
                    sources: vec!["pub1".to_string()],
                },
                ClaimEdit {
                    text: "Private claim.".to_string(),
                    sources: vec!["priv1".to_string()],
                },
            ],
            links: Vec::new(),
        }];
        let known = known_raw_ids(&wiki);
        let (valid, _) = validate_edits(&edits, &known);
        apply_edits(&wiki, &valid, DataClass::Public).unwrap();
        let page =
            parse_page(&std::fs::read_to_string(page_path(&wiki, "mixed")).unwrap()).unwrap();
        assert_eq!(page.class, DataClass::Confidential);
    }

    #[test]
    fn known_raw_ids_keep_version_chains_straight() {
        let (_dir, wiki, _log) = setup();
        // A base id with a numeric suffix is a base, not a version.
        seed_raw(&wiki, "demo-2001");
        assert_eq!(
            known_raw_ids(&wiki),
            BTreeSet::from(["demo-2001".to_string()])
        );
        // A genuine base + version chain folds to the base.
        seed_raw(&wiki, "p1");
        write_raw(
            &wiki,
            "p1",
            "https://x.com/i/status/p1",
            DataClass::Public,
            DATE,
            "synthetic source p1, edited",
            &[],
            None,
        )
        .unwrap();
        assert!(wiki.join(RAW_DIR_NAME).join("p1-2.md").exists());
        assert_eq!(
            known_raw_ids(&wiki),
            BTreeSet::from(["demo-2001".to_string(), "p1".to_string()])
        );
        assert!(raw_exists(&wiki, "p1"));
        assert!(!raw_exists(&wiki, "ghost"));
    }
}
