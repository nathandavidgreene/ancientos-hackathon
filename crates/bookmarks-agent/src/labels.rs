//! Per-post relevance labels for the eval queries (Task B labels page).
//!
//! This is the bookmarks app endpoint behind the web-ui labels page: the
//! page renders what this module loads and POSTs back through it. Layout
//! under the eval-private dir:
//!
//! - `nathan/label-queries.json`: queries `q1..q5` (id, text, post keys)
//!   plus a `posts` map (key -> author, text, X url). Required.
//! - `nathan/labels-latest.txt`: seed text
//!   (`q1:29=1,32=d,...;q2:...;new:1=1`, `1` = relevant, `0` = not,
//!   `d` = delete; numbers index the posts map). Optional; applied once
//!   at server startup by [`seed_labels`], idempotently.
//! - `labels.jsonl`: persisted labels, one row per labeling
//!   (`post_key`, `post_id`, `label`, `source`, `at`), read last-wins per
//!   `post_key`, always mode 600. Created by seeding or the first save.
//! - `bookmark-removals.jsonl`: one row per `remove_bookmark` attempt
//!   (`post_id`, `post_key`, `at`, `outcome`, optional `detail`).
//! - `thumbs/manifest.jsonl` + files: thumbnail rows (`post_id`, `path`,
//!   `kind`, ...); the web route only serves files listed here.
//! - `media-findings.jsonl` (optional): `analyze-media --out` rows;
//!   accepted findings join posts by `finding.provenance.post_id`.
//! - `classify.jsonl` (optional): rows
//!   (`post_id`, `category`) shown as extra tags; `post_id` may be an X
//!   status id or a label-queries post key.
//!
//! Strict inputs (queries, seed, saved labels) fail closed: any malformed
//! row aborts the call and writes nothing. Display-only inputs (thumbs
//! manifest, findings, classify) skip malformed rows so one bad line never
//! bricks labeling. Errors name relative files only, never absolute paths.
//!
//! Removal shells out to Social's single-post CLI as
//! `remove_bookmark --post-id <id>` (one post per call, never batched),
//! resolved from [`REMOVE_BIN_ENV`] else `PATH`; this module never calls X
//! directly. Removal has no phone approval: each click is an explicit,
//! logged human action. An already-removed post id is never called again,
//! and a missing CLI or non-zero exit keeps the bookmark and its label.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Env var overriding the eval-private dir for the labels page.
pub const EVAL_DIR_ENV: &str = "ANCIENTOS_EVAL_PRIVATE_DIR";
/// Env var holding the `remove_bookmark` binary path.
pub const REMOVE_BIN_ENV: &str = "ANCIENTOS_REMOVE_BOOKMARK_CMD";
/// CLI binary name used in safe error messages.
pub const REMOVE_BIN_NAME: &str = "bookmark remover";
/// Queries + posts file, relative to the eval-private dir.
pub const QUERIES_REL: &str = "nathan/label-queries.json";
/// Seed text file, relative to the eval-private dir.
pub const SEED_REL: &str = "nathan/labels-latest.txt";
/// Persisted labels file name.
pub const LABELS_FILE: &str = "labels.jsonl";
/// Removal-attempt log file name.
pub const REMOVALS_FILE: &str = "bookmark-removals.jsonl";
/// Thumbnails dir, relative to the eval-private dir.
pub const THUMBS_DIR: &str = "thumbs";
/// Thumbnail manifest, relative to the eval-private dir.
pub const THUMB_MANIFEST_REL: &str = "thumbs/manifest.jsonl";
/// `analyze-media --out` findings file name (optional).
pub const FINDINGS_FILE: &str = "media-findings.jsonl";
/// Classifier tags file name (optional).
pub const CLASSIFY_FILE: &str = "classify.jsonl";
/// Seconds before a `remove_bookmark` call is killed (fail closed).
pub const REMOVE_TIMEOUT_SECS: u64 = 60;
/// Longest stderr snippet kept in a removal-log `detail`.
pub const REMOVAL_DETAIL_CHARS: usize = 500;
/// Outcome recorded for a `remove_bookmark` exit 0.
pub const OUTCOME_OK: &str = "ok";

/// One relevance label for a post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    /// Relevant (`1` in the seed file).
    Relevant,
    /// Not relevant (`0` in the seed file).
    Not,
    /// Delete (`d` in the seed file, or a successful remove click).
    Delete,
}

impl Label {
    /// Stored word for this label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relevant => "relevant",
            Self::Not => "not",
            Self::Delete => "delete",
        }
    }

    /// Parse a stored word; `None` for anything else (fail closed).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "relevant" => Some(Self::Relevant),
            "not" => Some(Self::Not),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    /// Parse a seed-file value (`1`, `0`, `d`); `None` for anything else.
    pub fn parse_seed(s: &str) -> Option<Self> {
        match s {
            "1" => Some(Self::Relevant),
            "0" => Some(Self::Not),
            "d" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// One post in the label-queries posts map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPost {
    /// Author handle (`""` when the row omits it).
    pub author: String,
    /// Post text (truncated in the source file).
    pub text: String,
    /// Canonical X url; the trailing segment is the status id.
    pub url: String,
}

/// One eval query: an id, its text, and its candidate post keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelQuery {
    /// Query id (`q1`..`q5`).
    pub id: String,
    /// Query text.
    pub q: String,
    /// Candidate post keys, in file order.
    pub posts: Vec<String>,
}

/// Parsed label-queries file: queries plus the shared posts map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelQueries {
    /// Queries in file order.
    pub queries: Vec<LabelQuery>,
    /// Posts map keyed by label key (`"29"`).
    pub posts: BTreeMap<String, QueryPost>,
}

impl LabelQueries {
    /// Post keys referenced by any query, each once, numeric first.
    pub fn ordered_keys(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        for q in &self.queries {
            for k in &q.posts {
                if self.posts.contains_key(k) {
                    seen.insert(k.clone());
                }
            }
        }
        let mut keys: Vec<String> = seen.into_iter().collect();
        sort_keys(&mut keys);
        keys
    }

    /// Query ids containing `key`, in query file order.
    pub fn member_of(&self, key: &str) -> Vec<String> {
        self.queries
            .iter()
            .filter(|q| q.posts.iter().any(|k| k == key))
            .map(|q| q.id.clone())
            .collect()
    }
}

/// Sort label keys numerically, non-numeric keys last lexically.
fn sort_keys(keys: &mut [String]) {
    keys.sort_by(|a, b| match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y).then_with(|| a.cmp(b)),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    });
}

/// Eval-private dir: [`EVAL_DIR_ENV`] when set and non-empty, else
/// `$HOME/.ancientos-hackathon/eval-private`; `None` when the queries file
/// is absent (labels off) or no home is known.
pub fn eval_dir_from_env() -> Option<PathBuf> {
    let dir = match std::env::var(EVAL_DIR_ENV).ok().filter(|s| !s.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => {
            let home = std::env::var("HOME").ok().filter(|s| !s.is_empty())?;
            PathBuf::from(home).join(".ancientos-hackathon/eval-private")
        }
    };
    dir.join(QUERIES_REL).is_file().then_some(dir)
}

/// Parse the label-queries file; any malformed row fails the whole load.
pub fn load_queries(dir: &Path) -> Result<LabelQueries, String> {
    let text = std::fs::read_to_string(dir.join(QUERIES_REL))
        .map_err(|e| format!("{QUERIES_REL}: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{QUERIES_REL}: {e}"))?;
    let qv = v
        .get("queries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{QUERIES_REL}: missing queries array"))?;
    let mut queries = Vec::with_capacity(qv.len());
    for (i, q) in qv.iter().enumerate() {
        let at = format!("{QUERIES_REL}: queries[{i}]");
        let id = q
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{at}: missing id"))?;
        let text = q
            .get("q")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{at}: missing q"))?;
        let posts = q
            .get("posts")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("{at}: missing posts"))?;
        let mut keys = Vec::with_capacity(posts.len());
        for k in posts {
            match k {
                serde_json::Value::Number(n) => keys.push(n.to_string()),
                serde_json::Value::String(s) if !s.is_empty() => keys.push(s.clone()),
                _ => return Err(format!("{at}: bad post key")),
            }
        }
        queries.push(LabelQuery {
            id: id.to_string(),
            q: text.to_string(),
            posts: keys,
        });
    }
    let pv = v
        .get("posts")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("{QUERIES_REL}: missing posts map"))?;
    let mut posts = BTreeMap::new();
    for (key, p) in pv {
        let at = format!("{QUERIES_REL}: posts[{key}]");
        let text = p
            .get("t")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{at}: missing t"))?;
        let url = p
            .get("u")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{at}: missing u"))?;
        let author = p
            .get("a")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        posts.insert(
            key.clone(),
            QueryPost {
                author,
                text: text.to_string(),
                url: url.to_string(),
            },
        );
    }
    Ok(LabelQueries { queries, posts })
}

/// X status id from a post url (trailing `/`-segment); `None` unless it is
/// non-empty and all digits.
pub fn status_id_of_url(url: &str) -> Option<String> {
    let id = url.rsplit('/').next().unwrap_or("").trim();
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

/// One persisted label row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRow {
    /// Label-queries post key.
    pub post_key: String,
    /// X status id (`""` when the url did not parse at write time).
    pub post_id: String,
    /// The label.
    pub label: Label,
    /// Where the label came from (`seed:<section>` or `ui`).
    pub source: String,
    /// Unix time of the write.
    pub at: u64,
}

impl LabelRow {
    /// Render this row as JSONL-ready JSON.
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "post_key": self.post_key,
            "post_id": self.post_id,
            "label": self.label.as_str(),
            "source": self.source,
            "at": self.at,
        })
    }

    /// Parse one labels row; `None` names nothing (caller adds the line).
    fn parse(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            post_key: v
                .get("post_key")?
                .as_str()
                .filter(|s| !s.is_empty())?
                .to_string(),
            post_id: v.get("post_id")?.as_str()?.to_string(),
            label: Label::parse(v.get("label")?.as_str()?)?,
            source: v.get("source")?.as_str()?.to_string(),
            at: v.get("at")?.as_u64()?,
        })
    }
}

/// Fold `labels.jsonl` last-wins per `post_key`; a missing file reads as
/// empty, any malformed row fails the load (fail closed).
pub fn load_labels(dir: &Path) -> Result<BTreeMap<String, LabelRow>, String> {
    let text = match std::fs::read_to_string(dir.join(LABELS_FILE)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("{LABELS_FILE}: {e}")),
        Ok(t) => t,
    };
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("{LABELS_FILE} line {}: {e}", n + 1))?;
        let row =
            LabelRow::parse(&v).ok_or_else(|| format!("{LABELS_FILE} line {}: bad row", n + 1))?;
        out.insert(row.post_key.clone(), row);
    }
    Ok(out)
}

/// Restrict `path` to owner-only access (mode 600) on unix; elsewhere the
/// file is left as created.
fn restrict_to_owner(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{LABELS_FILE}: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Append `rows` to `labels.jsonl` (creating it when missing) and force
/// mode 600; with no rows the file is still created and restricted.
fn append_labels(dir: &Path, rows: &[LabelRow]) -> Result<(), String> {
    use std::io::Write;
    let path = dir.join(LABELS_FILE);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{LABELS_FILE}: {e}"))?;
    for r in rows {
        writeln!(f, "{}", r.to_json()).map_err(|e| format!("{LABELS_FILE}: {e}"))?;
    }
    drop(f);
    restrict_to_owner(&path)
}

/// One parsed seed entry: post key, label, and section name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeedEntry {
    /// Label-queries post key.
    key: String,
    /// Label from the `1`/`0`/`d` value.
    label: Label,
    /// Seed section (`q1`.. or `new`).
    section: String,
}

/// True for a seed section name (letters, digits, `_`, `-`, `.`).
fn valid_section(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Parse seed text (`q1:29=1,32=d;...`); unknown keys, bad values, or bad
/// sections fail the whole parse and nothing is written.
fn parse_seed(text: &str, queries: &LabelQueries) -> Result<Vec<SeedEntry>, String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for part in text.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, items) = part
            .split_once(':')
            .ok_or_else(|| format!("{SEED_REL}: bad section {part:?}"))?;
        let name = name.trim();
        if !valid_section(name) {
            return Err(format!("{SEED_REL}: bad section {part:?}"));
        }
        if items.trim().is_empty() {
            continue;
        }
        for item in items.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (key, val) = item
                .split_once('=')
                .ok_or_else(|| format!("{SEED_REL}: bad entry {item:?}"))?;
            let (key, val) = (key.trim(), val.trim());
            if !queries.posts.contains_key(key) {
                return Err(format!("{SEED_REL}: unknown post {key:?}"));
            }
            let label =
                Label::parse_seed(val).ok_or_else(|| format!("{SEED_REL}: bad value {item:?}"))?;
            if seen.insert(key.to_string()) {
                out.push(SeedEntry {
                    key: key.to_string(),
                    label,
                    section: name.to_string(),
                });
            }
        }
    }
    Ok(out)
}

/// Seed counts from [`seed_labels`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedReport {
    /// Entries appended to `labels.jsonl`.
    pub added: usize,
    /// Seed entries skipped (already labeled).
    pub kept: usize,
}

/// Append seed labels missing from `labels.jsonl` (existing labels win);
/// idempotent: a second run adds nothing. A missing seed file seeds
/// nothing (the labels file is still created); a malformed one writes
/// nothing and errors.
pub fn seed_labels(dir: &Path, now: u64) -> Result<SeedReport, String> {
    let queries = load_queries(dir)?;
    let text = match std::fs::read_to_string(dir.join(SEED_REL)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            append_labels(dir, &[])?;
            return Ok(SeedReport { added: 0, kept: 0 });
        }
        Err(e) => return Err(format!("{SEED_REL}: {e}")),
        Ok(t) => t,
    };
    let entries = parse_seed(&text, &queries)?;
    let have = load_labels(dir)?;
    let mut rows = Vec::new();
    let mut kept = 0;
    for e in entries {
        if have.contains_key(&e.key) {
            kept += 1;
            continue;
        }
        let post_id = queries
            .posts
            .get(&e.key)
            .and_then(|p| status_id_of_url(&p.url))
            .unwrap_or_default();
        rows.push(LabelRow {
            post_key: e.key,
            post_id,
            label: e.label,
            source: format!("seed:{}", e.section),
            at: now,
        });
    }
    append_labels(dir, &rows)?;
    Ok(SeedReport {
        added: rows.len(),
        kept,
    })
}

/// Save counts from [`save_labels`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveReport {
    /// Rows appended (labels that actually changed).
    pub saved: usize,
    /// Submitted labels identical to the stored ones.
    pub unchanged: usize,
}

/// Persist UI labels (`source "ui"`), appending only changed ones; unknown
/// post keys fail the whole save and nothing is written.
pub fn save_labels(dir: &Path, items: &[(String, Label)], now: u64) -> Result<SaveReport, String> {
    let queries = load_queries(dir)?;
    for (key, _) in items {
        if !queries.posts.contains_key(key) {
            return Err(format!("unknown post {key:?}"));
        }
    }
    let have = load_labels(dir)?;
    let mut rows = Vec::new();
    let mut unchanged = 0;
    for (key, label) in items {
        if have.get(key).is_some_and(|r| r.label == *label) {
            unchanged += 1;
            continue;
        }
        let post_id = queries
            .posts
            .get(key)
            .and_then(|p| status_id_of_url(&p.url))
            .unwrap_or_default();
        rows.push(LabelRow {
            post_key: key.clone(),
            post_id,
            label: *label,
            source: "ui".to_string(),
            at: now,
        });
    }
    append_labels(dir, &rows)?;
    Ok(SaveReport {
        saved: rows.len(),
        unchanged,
    })
}

/// One thumbnail manifest row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbEntry {
    /// X status id.
    pub post_id: String,
    /// File name inside the thumbs dir.
    pub path: String,
    /// `photo` or `video` (poster).
    pub kind: String,
}

/// Load the thumbnail manifest keyed by status id; a missing file reads as
/// empty and malformed rows are skipped (display-only input).
pub fn load_thumb_manifest(dir: &Path) -> BTreeMap<String, Vec<ThumbEntry>> {
    let mut out: BTreeMap<String, Vec<ThumbEntry>> = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(dir.join(THUMB_MANIFEST_REL)) else {
        return out;
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let get = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
        let (post_id, path, kind) = (get("post_id"), get("path"), get("kind"));
        if post_id.is_empty() || path.is_empty() || kind.is_empty() {
            continue;
        }
        out.entry(post_id.to_string())
            .or_default()
            .push(ThumbEntry {
                post_id: post_id.to_string(),
                path: path.to_string(),
                kind: kind.to_string(),
            });
    }
    out
}

/// File names listed in the thumbnail manifest (the serving allow-list).
pub fn thumb_allow_list(dir: &Path) -> BTreeSet<String> {
    load_thumb_manifest(dir)
        .values()
        .flat_map(|v| v.iter().map(|e| e.path.clone()))
        .collect()
}

/// One accepted media finding shown on a post card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownFinding {
    /// Finding kind (`text`, `claim`, `description`).
    pub kind: String,
    /// Finding text.
    pub text: String,
    /// Quoted image text, when the finding carries one.
    pub quote: Option<String>,
}

/// Load accepted `analyze-media --out` findings keyed by status id (from
/// `finding.provenance.post_id`); a missing file reads as empty and
/// malformed rows are skipped (display-only input).
pub fn load_findings(dir: &Path) -> BTreeMap<String, Vec<ShownFinding>> {
    let mut out: BTreeMap<String, Vec<ShownFinding>> = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(dir.join(FINDINGS_FILE)) else {
        return out;
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("status").and_then(serde_json::Value::as_str) != Some("accepted") {
            continue;
        }
        let Some(f) = v.get("finding") else { continue };
        let kind = f
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let text = f
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if kind.is_empty() || text.is_empty() {
            continue;
        }
        let post_id = f
            .get("provenance")
            .and_then(|p| p.get("post_id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if post_id.is_empty() {
            continue;
        }
        out.entry(post_id.to_string())
            .or_default()
            .push(ShownFinding {
                kind: kind.to_string(),
                text: text.to_string(),
                quote: f
                    .get("quote")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            });
    }
    out
}

/// Load classifier tags (`post_id` -> categories); `post_id` may be an X
/// status id or a label-queries post key. A missing file reads as empty
/// and malformed rows are skipped (display-only input).
pub fn load_classify(dir: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(dir.join(CLASSIFY_FILE)) else {
        return out;
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let id = v
            .get("post_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let cat = v
            .get("category")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if id.is_empty() || cat.is_empty() {
            continue;
        }
        out.entry(id.to_string())
            .or_default()
            .insert(cat.to_string());
    }
    out
}

/// Resolve the `remove_bookmark` binary: the explicit path when given, else
/// the first `PATH` entry containing [`REMOVE_BIN_NAME`] (empty `PATH`
/// segments are skipped, never treated as the working dir).
pub fn resolve_remove_bin(
    explicit: Option<&Path>,
    path_env: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(p) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return Ok(p.to_path_buf());
    }
    let path_env = path_env.unwrap_or("");
    for dir in path_env.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = Path::new(dir).join(REMOVE_BIN_NAME);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(format!(
        "{REMOVE_BIN_NAME} not found (set {REMOVE_BIN_ENV} or put it on PATH)"
    ))
}

/// Outcome of one CLI run without retaining potentially sensitive output.
struct CliRun {
    /// `Some(code)` when the CLI exited, `None` on spawn failure/timeout.
    code: Option<i32>,
    /// True only when the CLI ran to completion (whatever it exited).
    ran: bool,
}

/// Run `bin --post-id <id>` with no shell, killing it after
/// [`REMOVE_TIMEOUT_SECS`] seconds.
fn run_remove_cli(bin: &Path, id: &str, request_id: &str) -> CliRun {
    use std::process::Stdio;
    let mut child = match std::process::Command::new(bin)
        .arg("--post-id")
        .arg(id)
        .arg("--request-id")
        .arg(request_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Err(_) => {
            return CliRun {
                code: None,
                ran: false,
            }
        }
        Ok(c) => c,
    };
    let step = std::time::Duration::from_millis(50);
    let mut waited = std::time::Duration::ZERO;
    let budget = std::time::Duration::from_secs(REMOVE_TIMEOUT_SECS);
    loop {
        match child.try_wait() {
            Err(_) => {
                return CliRun {
                    code: None,
                    ran: false,
                }
            }
            Ok(Some(status)) => {
                return CliRun {
                    code: status.code(),
                    ran: true,
                };
            }
            Ok(None) => {
                if waited >= budget {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CliRun {
                        code: None,
                        ran: false,
                    };
                }
                std::thread::sleep(step);
                waited += step;
            }
        }
    }
}

/// Check that a request id has the canonical UUID shape.
fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Check that the configured remover is executable without following PATH.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Return the saved result for a duplicate request UUID without invoking the CLI.
fn prior_request(dir: &Path, request_id: &str) -> Option<Result<RemoveOutcome, String>> {
    let text = std::fs::read_to_string(dir.join(REMOVALS_FILE)).ok()?;
    for line in text.lines().rev() {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v["request_id"].as_str() == Some(request_id) {
            return Some(match v["outcome"].as_str()? {
                "removed" => Ok(RemoveOutcome::Removed),
                "already_absent" => Ok(RemoveOutcome::AlreadyRemoved),
                "refused_or_not_found" => {
                    Err("refused or not found on X (previous request result)".to_string())
                }
                _ => Err("removal failed (previous request result)".to_string()),
            });
        }
    }
    None
}

/// Append the required sanitized audit record for one remover attempt.
fn log_attempt(
    dir: &Path,
    post_id: &str,
    request_id: &str,
    outcome: &str,
    exit_code: Option<i32>,
    now: u64,
) -> Result<(), String> {
    use std::io::Write;
    let row = serde_json::json!({"timestamp": now, "post_id": post_id, "request_id": request_id, "outcome": outcome, "exit_code": exit_code});
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(REMOVALS_FILE))
        .map_err(|_| "removal audit log unavailable".to_string())?;
    writeln!(f, "{row}").map_err(|_| "removal audit log unavailable".to_string())
}

/// Outcome of [`remove_one`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The CLI exited 0; the removal was logged.
    Removed,
    /// The post id already had an `ok` row; the CLI was not called.
    AlreadyRemoved,
}

/// Remove exactly one post via Social's configured CLI and audit the result.
/// Request UUIDs replay a prior result without invoking the CLI again;
/// failures leave labels unchanged and return sanitized messages.
pub fn remove_one(
    dir: &Path,
    post_key: &str,
    explicit_bin: Option<&Path>,
    path_env: Option<&str>,
    now: u64,
    request_id: &str,
) -> Result<RemoveOutcome, String> {
    let queries = load_queries(dir)?;
    let post = queries
        .posts
        .get(post_key)
        .ok_or_else(|| format!("unknown post {post_key:?}"))?;
    let id = status_id_of_url(&post.url)
        .ok_or_else(|| format!("post {post_key:?} has no numeric X id"))?;
    if !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("post id must contain digits only".to_string());
    }
    if !is_uuid(request_id) {
        return Err("invalid request id".to_string());
    }
    if let Some(prior) = prior_request(dir, request_id) {
        return prior;
    }
    let bin = match explicit_bin {
        Some(path) if !path.as_os_str().is_empty() => path.to_path_buf(),
        Some(_) => return Err("remover is not configured".to_string()),
        None => PathBuf::from(
            path_env
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "remover is not configured".to_string())?,
        ),
    };
    if !bin.is_file() || !is_executable(&bin) {
        return Err("remover is not executable".to_string());
    }
    let run = run_remove_cli(&bin, &id, request_id);
    if !run.ran {
        let _ = log_attempt(dir, &id, request_id, "failed", None, now);
        return Err(format!("{REMOVE_BIN_NAME} failed"));
    }
    match run.code {
        Some(0) => {
            log_attempt(dir, &id, request_id, "removed", Some(0), now)?;
            Ok(RemoveOutcome::Removed)
        }
        Some(2) => {
            log_attempt(dir, &id, request_id, "refused_or_not_found", Some(2), now)?;
            Err("refused or not found on X".to_string())
        }
        Some(code) => {
            log_attempt(dir, &id, request_id, "failed", Some(code), now)?;
            Err(format!("removal failed (exit code {code})"))
        }
        None => {
            let _ = log_attempt(dir, &id, request_id, "failed", None, now);
            Err(format!("{REMOVE_BIN_NAME} died by signal"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Synthetic label-queries file: two queries sharing post 7, one post
    /// with a non-numeric url, all text invented for tests.
    fn queries_fixture() -> String {
        serde_json::json!({
            "source": "synthetic",
            "queries": [
                {"id": "q1", "q": "synthetic sleep tips", "posts": [7, 9]},
                {"id": "q2", "q": "synthetic local agents", "posts": [7]},
            ],
            "posts": {
                "7": {"a": "syn_sleeper", "t": "synthetic post about naps",
                      "u": "https://x.com/syn_sleeper/status/9000000000000000007"},
                "9": {"a": "syn_cook", "t": "synthetic post about soup",
                      "u": "https://x.com/syn_cook/status/9000000000000000009"},
                "11": {"a": "syn_broken", "t": "synthetic post with bad url",
                       "u": "not-a-status-url"},
            },
        })
        .to_string()
    }

    /// Temp eval-private dir holding the synthetic queries file.
    fn eval_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("nathan")).unwrap();
        std::fs::write(dir.path().join(QUERIES_REL), queries_fixture()).unwrap();
        dir
    }

    /// Temp eval-private dir with queries plus the given seed text.
    fn seeded_dir(seed: &str) -> tempfile::TempDir {
        let dir = eval_dir();
        std::fs::write(dir.path().join(SEED_REL), seed).unwrap();
        dir
    }

    /// Write `body` to `name` under `dir` and make it executable (unix).
    #[cfg(unix)]
    fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Fake `remove_bookmark`: appends its args to `calls_file`, exits
    /// `code`, prints `stderr` to stderr.
    #[cfg(unix)]
    fn fake_remove_bin(dir: &Path, calls_file: &Path, code: i32, stderr: &str) -> PathBuf {
        write_executable(
            dir,
            "remove_bookmark",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n[ -n '{}' ] && printf '%s' '{}' >&2\nexit {code}\n",
                calls_file.display(),
                stderr,
                stderr,
            ),
        )
    }

    /// Calls recorded by the fake binary, one line per invocation.
    #[cfg(unix)]
    fn recorded_calls(calls_file: &Path) -> Vec<String> {
        match std::fs::read_to_string(calls_file) {
            Err(_) => Vec::new(),
            Ok(t) => t.lines().map(str::to_string).collect(),
        }
    }

    /// Read the removal log rows as JSON values.
    fn removal_rows(dir: &Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(dir.join(REMOVALS_FILE)).unwrap_or_default();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Queries load with shared posts deduplicated and ordered.
    #[test]
    fn queries_load_orders_shared_posts_once() {
        let dir = eval_dir();
        let q = load_queries(dir.path()).unwrap();
        assert_eq!(q.ordered_keys(), vec!["7", "9"]);
        assert_eq!(q.member_of("7"), vec!["q1", "q2"]);
        assert_eq!(q.member_of("9"), vec!["q1"]);
        // Post 11 is in the map but in no query, so it never surfaces.
        assert!(q.posts.contains_key("11"));
        assert!(q.member_of("11").is_empty());
    }

    /// Malformed queries files fail the whole load.
    #[test]
    fn queries_malformed_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("nathan")).unwrap();
        for bad in [
            "not json",
            r#"{"queries": []}"#,
            r#"{"queries": [{"id": "q1", "q": "x", "posts": [{}]}], "posts": {}}"#,
            r#"{"queries": [{"id": "q1", "q": "x", "posts": []}], "posts": {"7": {"a": "u", "t": "x"}}}"#,
        ] {
            std::fs::write(dir.path().join(QUERIES_REL), bad).unwrap();
            assert!(load_queries(dir.path()).is_err(), "{bad}");
        }
        assert!(load_queries(dir.path().join("missing").as_path()).is_err());
    }

    /// Status ids come from the url tail and must be all digits.
    #[test]
    fn status_id_parsing() {
        assert_eq!(
            status_id_of_url("https://x.com/u/status/9000000000000000007").as_deref(),
            Some("9000000000000000007")
        );
        for bad in [
            "",
            "not-a-status-url",
            "https://x.com/u/status/",
            "12a45",
            "  ",
        ] {
            assert_eq!(status_id_of_url(bad), None, "{bad}");
        }
    }

    /// Seed values map 1/0/d and a second run adds nothing.
    #[test]
    fn seed_applies_once_and_is_idempotent() {
        let dir = seeded_dir("q1:7=1,9=0;q2:7=d;new:11=1\n");
        let first = seed_labels(dir.path(), 100).unwrap();
        // Post 7 keeps its first label (q1 relevant); q2's delete loses.
        assert_eq!(first, SeedReport { added: 3, kept: 0 });
        let labels = load_labels(dir.path()).unwrap();
        assert_eq!(labels["7"].label, Label::Relevant);
        assert_eq!(labels["7"].source, "seed:q1");
        assert_eq!(labels["7"].post_id, "9000000000000000007");
        assert_eq!(labels["9"].label, Label::Not);
        assert_eq!(labels["11"].label, Label::Relevant);
        assert_eq!(labels["11"].post_id, "");
        let before = std::fs::read(dir.path().join(LABELS_FILE)).unwrap();
        let second = seed_labels(dir.path(), 200).unwrap();
        assert_eq!(second, SeedReport { added: 0, kept: 3 });
        assert_eq!(std::fs::read(dir.path().join(LABELS_FILE)).unwrap(), before);
    }

    /// Empty sections and a missing seed file seed nothing but still
    /// create the labels file.
    #[test]
    fn seed_empty_sections_and_missing_file() {
        let dir = seeded_dir("q1:;q2:7=1;\n");
        let rep = seed_labels(dir.path(), 100).unwrap();
        assert_eq!(rep, SeedReport { added: 1, kept: 0 });
        let dir = eval_dir();
        let rep = seed_labels(dir.path(), 100).unwrap();
        assert_eq!(rep, SeedReport { added: 0, kept: 0 });
        assert!(dir.path().join(LABELS_FILE).is_file());
    }

    /// A malformed seed writes nothing, even when labels already exist.
    #[test]
    fn seed_malformed_writes_nothing() {
        for bad in ["q1:7=x", "q1:nope=1", "q1 7=1", "q 1:7=1", "q1:7", "q1:=1"] {
            let dir = seeded_dir(bad);
            save_labels(dir.path(), &[("7".to_string(), Label::Not)], 50).unwrap();
            let before = std::fs::read(dir.path().join(LABELS_FILE)).unwrap();
            assert!(seed_labels(dir.path(), 100).is_err(), "{bad}");
            assert_eq!(std::fs::read(dir.path().join(LABELS_FILE)).unwrap(), before);
        }
        // Fresh dir: no labels file appears at all.
        let dir = seeded_dir("q1:7=x");
        assert!(seed_labels(dir.path(), 100).is_err());
        assert!(!dir.path().join(LABELS_FILE).exists());
    }

    /// Seeding never overwrites labels already stored.
    #[test]
    fn seed_keeps_existing_labels() {
        let dir = seeded_dir("q1:7=1,9=1");
        save_labels(dir.path(), &[("7".to_string(), Label::Not)], 50).unwrap();
        let rep = seed_labels(dir.path(), 100).unwrap();
        assert_eq!(rep, SeedReport { added: 1, kept: 1 });
        let labels = load_labels(dir.path()).unwrap();
        assert_eq!(labels["7"].label, Label::Not);
        assert_eq!(labels["7"].source, "ui");
        assert_eq!(labels["9"].label, Label::Relevant);
    }

    /// Saves append only changed labels; unknown keys fail with no write.
    #[test]
    fn save_appends_changed_only_and_rejects_unknown_keys() {
        let dir = eval_dir();
        let rep = save_labels(
            dir.path(),
            &[
                ("7".to_string(), Label::Relevant),
                ("9".to_string(), Label::Not),
            ],
            100,
        )
        .unwrap();
        assert_eq!(
            rep,
            SaveReport {
                saved: 2,
                unchanged: 0
            }
        );
        let rep = save_labels(
            dir.path(),
            &[
                ("7".to_string(), Label::Relevant),
                ("9".to_string(), Label::Relevant),
            ],
            200,
        )
        .unwrap();
        assert_eq!(
            rep,
            SaveReport {
                saved: 1,
                unchanged: 1
            }
        );
        let labels = load_labels(dir.path()).unwrap();
        assert_eq!(labels["9"].label, Label::Relevant);
        assert_eq!(labels["9"].at, 200);
        let before = std::fs::read(dir.path().join(LABELS_FILE)).unwrap();
        assert!(save_labels(dir.path(), &[("zzz".to_string(), Label::Not)], 300).is_err());
        assert_eq!(std::fs::read(dir.path().join(LABELS_FILE)).unwrap(), before);
    }

    /// Stored labels fold last-wins; malformed rows fail the load.
    #[test]
    fn labels_fold_last_wins_and_reject_bad_rows() {
        let dir = eval_dir();
        std::fs::write(
            dir.path().join(LABELS_FILE),
            "{\"post_key\":\"7\",\"post_id\":\"1\",\"label\":\"relevant\",\"source\":\"ui\",\"at\":1}\n\
             \n\
             {\"post_key\":\"7\",\"post_id\":\"1\",\"label\":\"not\",\"source\":\"ui\",\"at\":2}\n",
        )
        .unwrap();
        let labels = load_labels(dir.path()).unwrap();
        assert_eq!(labels["7"].label, Label::Not);
        std::fs::write(dir.path().join(LABELS_FILE), "{\"post_key\":\"7\"}\n").unwrap();
        assert!(load_labels(dir.path()).is_err());
    }

    /// The labels file is created mode 600.
    #[test]
    #[cfg(unix)]
    fn labels_file_is_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = eval_dir();
        save_labels(dir.path(), &[("7".to_string(), Label::Relevant)], 100).unwrap();
        let mode = std::fs::metadata(dir.path().join(LABELS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The thumbnail manifest loads keyed by post id; bad rows are skipped.
    #[test]
    fn thumbs_load_and_skip_bad_rows() {
        let dir = eval_dir();
        assert!(load_thumb_manifest(dir.path()).is_empty());
        std::fs::create_dir_all(dir.path().join(THUMBS_DIR)).unwrap();
        std::fs::write(
            dir.path().join(THUMB_MANIFEST_REL),
            "{\"post_id\":\"9000000000000000007\",\"index\":0,\"kind\":\"photo\",\"path\":\"a_0.jpg\"}\n\
             not json\n\
             {\"post_id\":\"\",\"kind\":\"photo\",\"path\":\"b_0.jpg\"}\n\
             {\"post_id\":\"9000000000000000007\",\"index\":0,\"kind\":\"video\",\"path\":\"a_1.jpg\"}\n",
        )
        .unwrap();
        let man = load_thumb_manifest(dir.path());
        assert_eq!(man.len(), 1);
        assert_eq!(man["9000000000000000007"].len(), 2);
        assert_eq!(
            thumb_allow_list(dir.path()),
            BTreeSet::from(["a_0.jpg".to_string(), "a_1.jpg".to_string()])
        );
    }

    /// Only accepted findings load, joined by provenance post id.
    #[test]
    fn findings_load_accepted_only() {
        let dir = eval_dir();
        assert!(load_findings(dir.path()).is_empty());
        std::fs::write(
            dir.path().join(FINDINGS_FILE),
            "{\"status\":\"accepted\",\"finding\":{\"kind\":\"text\",\"text\":\"synthetic ocr\",\"quote\":\"hi\",\"provenance\":{\"post_id\":\"9000000000000000007\",\"media_key\":\"k\",\"media_url\":\"u\"}}}\n\
             {\"status\":\"rejected\",\"finding\":{\"kind\":\"text\",\"text\":\"nope\",\"provenance\":{\"post_id\":\"9000000000000000007\",\"media_key\":\"k\",\"media_url\":\"u\"}}}\n\
             {\"status\":\"mute_check\",\"post_id\":\"9000000000000000007\",\"muted\":false,\"muted_findings\":0,\"mute_check_failed\":false}\n\
             {\"status\":\"accepted\",\"finding\":{\"kind\":\"text\"}}\n",
        )
        .unwrap();
        let found = load_findings(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(
            found["9000000000000000007"],
            vec![ShownFinding {
                kind: "text".to_string(),
                text: "synthetic ocr".to_string(),
                quote: Some("hi".to_string()),
            }]
        );
    }

    /// Classifier tags load keyed as written; bad rows are skipped.
    #[test]
    fn classify_loads_and_skips_bad_rows() {
        let dir = eval_dir();
        assert!(load_classify(dir.path()).is_empty());
        std::fs::write(
            dir.path().join(CLASSIFY_FILE),
            "{\"post_id\":\"9000000000000000007\",\"category\":\"sleep\"}\n\
             {\"post_id\":\"7\",\"category\":\"rest\"}\n\
             not json\n\
             {\"post_id\":\"7\"}\n",
        )
        .unwrap();
        let tags = load_classify(dir.path());
        assert_eq!(
            tags["9000000000000000007"],
            BTreeSet::from(["sleep".to_string()])
        );
        assert_eq!(tags["7"], BTreeSet::from(["rest".to_string()]));
    }

    /// The explicit binary wins; PATH lookup skips empty segments.
    #[test]
    fn bin_resolution_prefers_explicit_then_path() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("bin");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join(REMOVE_BIN_NAME), "x").unwrap();
        let path_env = format!("::{}", sub.display());
        let found = resolve_remove_bin(None, Some(&path_env)).unwrap();
        assert_eq!(found, sub.join(REMOVE_BIN_NAME));
        let custom = dir.path().join("custom");
        assert_eq!(
            resolve_remove_bin(Some(&custom), Some(&path_env)).unwrap(),
            custom
        );
        assert!(resolve_remove_bin(None, Some("/nonexistent-dir-xyz")).is_err());
        assert!(resolve_remove_bin(None, None).is_err());
    }

    /// A successful remove calls once with exactly `--post-id <id>` and
    /// logs one `ok` row; labels are untouched (the caller sets them).
    #[test]
    #[cfg(unix)]
    fn remove_calls_cli_once_with_single_post_id() {
        let dir = eval_dir();
        let calls = dir.path().join("calls.txt");
        let bin = fake_remove_bin(dir.path(), &calls, 0, "");
        let out = remove_one(
            dir.path(),
            "7",
            Some(&bin),
            None,
            100,
            "00000000-0000-4000-8000-000000000001",
        )
        .unwrap();
        assert_eq!(out, RemoveOutcome::Removed);
        assert_eq!(
            recorded_calls(&calls),
            vec!["--post-id 9000000000000000007 --request-id 00000000-0000-4000-8000-000000000001"]
        );
        let rows = removal_rows(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["post_id"], "9000000000000000007");
        assert_eq!(
            rows[0]["request_id"],
            "00000000-0000-4000-8000-000000000001"
        );
        assert_eq!(rows[0]["timestamp"], 100);
        assert_eq!(rows[0]["outcome"], "removed");
        assert_eq!(rows[0]["exit_code"], 0);
        assert!(rows[0].get("detail").is_none());
        assert!(load_labels(dir.path()).unwrap().is_empty());
    }

    /// An already-removed post id is never called again and logs nothing.
    #[test]
    #[cfg(unix)]
    fn remove_is_idempotent_without_new_calls_or_rows() {
        let dir = eval_dir();
        let calls = dir.path().join("calls.txt");
        let bin = fake_remove_bin(dir.path(), &calls, 0, "");
        assert_eq!(
            remove_one(
                dir.path(),
                "7",
                Some(&bin),
                None,
                100,
                "00000000-0000-4000-8000-000000000001"
            )
            .unwrap(),
            RemoveOutcome::Removed
        );
        assert_eq!(
            remove_one(
                dir.path(),
                "7",
                Some(&bin),
                None,
                200,
                "00000000-0000-4000-8000-000000000001"
            )
            .unwrap(),
            RemoveOutcome::Removed
        );
        assert_eq!(recorded_calls(&calls).len(), 1);
        assert_eq!(removal_rows(dir.path()).len(), 1);
    }

    /// A non-zero exit keeps the bookmark, hides child output, and allows retry.
    #[test]
    #[cfg(unix)]
    fn remove_nonzero_exit_logs_error_and_retries() {
        let dir = eval_dir();
        let calls = dir.path().join("calls.txt");
        let failing = fake_remove_bin(dir.path(), &calls, 1, "synthetic boom");
        let err = remove_one(
            dir.path(),
            "7",
            Some(&failing),
            None,
            100,
            "00000000-0000-4000-8000-000000000002",
        )
        .unwrap_err();
        assert!(err.contains("exit code 1"), "{err}");
        let rows = removal_rows(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], "failed");
        assert_eq!(rows[0]["exit_code"], 1);
        // A failed attempt does not suppress the next one.
        let calls2 = dir.path().join("calls2.txt");
        let succeeding = write_executable(
            dir.path(),
            "ok_bin",
            &format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 0\n", calls2.display()),
        );
        let out = remove_one(
            dir.path(),
            "7",
            Some(&succeeding),
            None,
            200,
            "00000000-0000-4000-8000-000000000003",
        )
        .unwrap();
        assert_eq!(out, RemoveOutcome::Removed);
        assert_eq!(removal_rows(dir.path()).len(), 2);
    }

    /// A missing CLI fails closed with a logged attempt and no label change.
    #[test]
    #[cfg(unix)]
    fn remove_missing_cli_fails_closed_and_logs() {
        let dir = eval_dir();
        let missing = dir.path().join("no-such-bin");
        let err = remove_one(
            dir.path(),
            "7",
            Some(&missing),
            None,
            100,
            "00000000-0000-4000-8000-000000000004",
        )
        .unwrap_err();
        assert!(err.contains("not executable"), "{err}");
        let rows = removal_rows(dir.path());
        assert_eq!(rows.len(), 0);
        assert!(load_labels(dir.path()).unwrap().is_empty());
        // Nothing on PATH either: same fail-closed shape.
        let err = remove_one(
            dir.path(),
            "9",
            None,
            Some("/nonexistent-dir-xyz"),
            200,
            "00000000-0000-4000-8000-000000000005",
        )
        .unwrap_err();
        assert!(err.contains("not executable"), "{err}");
        assert!(removal_rows(dir.path()).is_empty());
    }

    /// Unset, empty, and non-executable configuration never invokes a remover.
    #[test]
    #[cfg(unix)]
    fn remove_configuration_fails_closed() {
        let dir = eval_dir();
        let calls = dir.path().join("calls.txt");
        let valid = fake_remove_bin(dir.path(), &calls, 0, "");
        let id = "00000000-0000-4000-8000-000000000008";
        assert!(remove_one(dir.path(), "7", None, None, 100, id).is_err());
        assert!(remove_one(dir.path(), "7", Some(Path::new("")), None, 100, id).is_err());
        let nonexec = dir.path().join("nonexec");
        std::fs::write(&nonexec, "not executable").unwrap();
        assert!(remove_one(dir.path(), "7", Some(&nonexec), None, 100, id).is_err());
        assert!(remove_one(dir.path(), "7", None, Some(""), 100, id).is_err());
        assert!(recorded_calls(&calls).is_empty());
        assert!(valid.is_file());
    }

    /// Social exit 2 logs refusal/not-found and returns an error without changing labels.
    #[test]
    #[cfg(unix)]
    fn exit_two_is_refused_or_not_found() {
        let dir = eval_dir();
        let calls = dir.path().join("calls.txt");
        let bin = fake_remove_bin(dir.path(), &calls, 2, "private output");
        let err = remove_one(
            dir.path(),
            "7",
            Some(&bin),
            None,
            100,
            "00000000-0000-4000-8000-000000000009",
        )
        .unwrap_err();
        assert!(err.contains("refused or not found on X"), "{err}");
        let rows = removal_rows(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], "refused_or_not_found");
        assert_eq!(rows[0]["exit_code"], 2);
        assert!(load_labels(dir.path()).unwrap().is_empty());
        let line = std::fs::read_to_string(dir.path().join(REMOVALS_FILE)).unwrap();
        assert!(!line.contains("private output"));
    }

    /// Replaying an exit-2 request id returns an error and does not call again.
    #[test]
    #[cfg(unix)]
    fn exit_two_request_id_dedupe_does_not_report_success() {
        let dir = eval_dir();
        let calls = dir.path().join("calls.txt");
        let bin = fake_remove_bin(dir.path(), &calls, 2, "");
        let id = "00000000-0000-4000-8000-000000000010";
        assert!(remove_one(dir.path(), "7", Some(&bin), None, 100, id).is_err());
        let replay = remove_one(dir.path(), "7", Some(&bin), None, 200, id).unwrap_err();
        assert!(replay.contains("refused or not found on X"), "{replay}");
        assert_eq!(recorded_calls(&calls).len(), 1);
        let rows = removal_rows(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], "refused_or_not_found");
        assert!(load_labels(dir.path()).unwrap().is_empty());
    }

    /// Unknown keys and non-numeric ids never reach the CLI or the log.
    #[test]
    #[cfg(unix)]
    fn remove_invalid_post_never_calls_or_logs() {
        let dir = eval_dir();
        let calls = dir.path().join("calls.txt");
        let bin = fake_remove_bin(dir.path(), &calls, 0, "");
        assert!(remove_one(
            dir.path(),
            "zzz",
            Some(&bin),
            None,
            100,
            "00000000-0000-4000-8000-000000000006"
        )
        .is_err());
        assert!(remove_one(
            dir.path(),
            "11",
            Some(&bin),
            None,
            100,
            "00000000-0000-4000-8000-000000000007"
        )
        .is_err());
        assert!(recorded_calls(&calls).is_empty());
        assert!(!dir.path().join(REMOVALS_FILE).exists());
    }

    /// The eval dir resolves from env, else home; absence means labels off.
    #[test]
    fn eval_dir_from_env_prefers_env_then_home() {
        let _lock = crate::test_support::lock_env();
        let saved_env = std::env::var(EVAL_DIR_ENV).ok();
        let saved_home = std::env::var("HOME").ok();
        let dir = eval_dir();
        std::env::set_var(EVAL_DIR_ENV, dir.path());
        assert_eq!(eval_dir_from_env().as_deref(), Some(dir.path()));
        std::env::remove_var(EVAL_DIR_ENV);
        // HOME without a queries file means off, not an error.
        std::env::set_var("HOME", dir.path().join("nope"));
        assert_eq!(eval_dir_from_env(), None);
        match saved_env {
            Some(v) => std::env::set_var(EVAL_DIR_ENV, v),
            None => std::env::remove_var(EVAL_DIR_ENV),
        }
        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}
