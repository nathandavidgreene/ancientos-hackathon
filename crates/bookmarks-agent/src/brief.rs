//! Daily brief: pick 3-5 bookmarks worth revisiting.
//!
//! Candidate excerpts are sanitized before anything else happens, and only
//! sanitized text is ever sent to the hosted model. One Muse Spark call at
//! most; when paid calls are disabled or the call fails, a deterministic
//! local fallback picks instead. Every pick passes the approval gate and the
//! candidate list, model call, and picks are appended to the decision log.
use crate::fixture::Bookmark;
use crate::merge::{MergeCounts, MergeOutput};
use ancientos_core::feedback;
use ancientos_core::gate::{Gate, Proposal, Risk, Verdict};
use ancientos_core::log::DecisionLog;
use ancientos_router::muse::{MuseClient, MuseUsage};
use ancientos_router::{route, Destination, RouterError, Sensitivity};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

pub const MAX_CANDIDATES: usize = 20;
pub const MIN_PICKS: usize = 3;
pub const MAX_PICKS: usize = 5;
pub const AGENT_NAME: &str = "bookmarks-agent";
pub const BRIEF_ACTION: &str = "brief.include";
pub const DECISION_LOG_FILE_NAME: &str = "decision_log.jsonl";
pub const DECISION_LOG_ENV_VAR: &str = "DECISION_LOG_PATH";

#[derive(Debug, Clone)]
pub struct Candidate {
    pub index: usize,
    pub post_id: String,
    pub author: Option<String>,
    pub url: String,
    pub topic: String,
    pub confidence: Option<f64>,
    pub sanitized: String,
}

#[derive(Debug, Clone)]
pub struct BriefPick {
    pub index: usize,
    pub post_id: String,
    pub author: Option<String>,
    pub url: String,
    pub topic: String,
    pub why: String,
    /// Full post text for display only (hydrated when truncated).
    /// Never sent to the model; the prompt uses sanitized excerpts.
    pub full_text: String,
    /// Attached media, rendered as links/thumbnails under the full text.
    pub media: Vec<crate::media::Media>,
}

#[derive(Debug, Clone)]
pub struct BriefResult {
    pub markdown: String,
    pub picks: Vec<BriefPick>,
    pub candidates: Vec<Candidate>,
    pub model: String,
    pub usage: Option<MuseUsage>,
    pub fallback: bool,
    pub counts: MergeCounts,
}

/// Reply from a brief model: text plus the model id and token usage.
#[derive(Debug, Clone)]
pub struct ModelReply {
    pub content: String,
    pub model: String,
    pub usage: Option<MuseUsage>,
}

/// The single model call the brief is allowed to make.
pub trait BriefModel {
    fn model_id(&self) -> String;
    fn call(&self, prompt: &str) -> Result<ModelReply, RouterError>;
}

/// Full-text hydrator: post ids -> post_id to full text. Called at most once
/// per brief; failures keep stored text.
pub type Hydrator<'a> = dyn Fn(&[String]) -> Result<HashMap<String, String>, String> + 'a;

/// Thread expander: a post id -> the unrolled thread when the post is a
/// thread root with self-replies, `None` otherwise. Called at most once per
/// brief (first pick only); failures and `None` keep the root text.
pub type ThreadExpander<'a> =
    dyn Fn(&str) -> Result<Option<Vec<crate::unroll::ThreadPost>>, String> + 'a;

impl BriefModel for MuseClient {
    fn model_id(&self) -> String {
        self.model().to_string()
    }

    fn call(&self, prompt: &str) -> Result<ModelReply, RouterError> {
        let resp = self.chat(prompt)?;
        let model = if resp.model.is_empty() {
            self.model().to_string()
        } else {
            resp.model
        };
        Ok(ModelReply {
            content: resp.content,
            model,
            usage: resp.usage,
        })
    }
}

pub fn topic_of(b: &Bookmark) -> String {
    if let Some(t) = b.label.as_ref().and_then(|l| l.parent_label.clone()) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    if let Some(f) = b.folders.as_ref().and_then(|f| f.first().cloned()) {
        let f = f.trim().to_string();
        if !f.is_empty() {
            return f;
        }
    }
    "Unlabeled".to_string()
}

fn confidence_of(b: &Bookmark) -> f64 {
    b.label.as_ref().and_then(|l| l.confidence).unwrap_or(-1.0)
}

/// Deterministic candidate selection: live rows first, newest `created_at`
/// first, then higher confidence, then `post_id`. Topics are interleaved
/// round-robin for diversity; at most [`MAX_CANDIDATES`] are kept.
pub fn select_candidates(rows: &[Bookmark], live_ids: &BTreeSet<String>) -> Vec<Candidate> {
    let mut ordered: Vec<&Bookmark> = rows.iter().collect();
    ordered.sort_by(|a, b| {
        let live = live_ids
            .contains(&b.post_id)
            .cmp(&live_ids.contains(&a.post_id));
        if live != Ordering::Equal {
            return live;
        }
        let ts = match (&b.created_at, &a.created_at) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        };
        if ts != Ordering::Equal {
            return ts;
        }
        let conf = confidence_of(b)
            .partial_cmp(&confidence_of(a))
            .unwrap_or(Ordering::Equal);
        if conf != Ordering::Equal {
            return conf;
        }
        a.post_id.cmp(&b.post_id)
    });

    // Group by topic in first-seen order, then interleave round-robin.
    let mut topic_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&Bookmark>> = HashMap::new();
    for b in ordered {
        let topic = topic_of(b);
        groups.entry(topic.clone()).or_default().push(b);
        if !topic_order.contains(&topic) {
            topic_order.push(topic);
        }
    }
    let mut interleaved: Vec<&Bookmark> = Vec::new();
    let mut round = 0;
    while interleaved.len() < MAX_CANDIDATES {
        let mut took = false;
        for topic in &topic_order {
            if let Some(list) = groups.get(topic) {
                if let Some(b) = list.get(round) {
                    interleaved.push(*b);
                    took = true;
                    if interleaved.len() >= MAX_CANDIDATES {
                        break;
                    }
                }
            }
        }
        if !took {
            break;
        }
        round += 1;
    }

    interleaved
        .into_iter()
        .enumerate()
        .map(|(i, b)| Candidate {
            index: i + 1,
            post_id: b.post_id.clone(),
            author: b.author_username.clone(),
            url: b.url.clone(),
            topic: topic_of(b),
            confidence: b.label.as_ref().and_then(|l| l.confidence),
            sanitized: crate::sanitize::sanitize(&b.text),
        })
        .collect()
}

/// Build the one prompt sent to the model. It contains sanitized excerpts
/// only — never raw bookmark text.
pub fn build_prompt(candidates: &[Candidate]) -> String {
    let mut prompt = String::from(
        "You curate a daily brief from saved bookmarks. Pick the 3 to 5 most interesting items.\n\nCandidates (sanitized excerpts, numbered):\n",
    );
    for c in candidates {
        prompt.push_str(&format!("{}. [{}] {}\n", c.index, c.topic, c.sanitized));
    }
    prompt.push_str(
        "\nReply with a strict JSON array only, no other text: [{\"n\": <candidate number>, \"why\": \"<one line, max 140 chars>\"}]. Use 3-5 distinct candidate numbers.",
    );
    prompt
}

#[derive(Debug, serde::Deserialize)]
struct RawPick {
    n: usize,
    why: String,
}

fn extract_json_array(reply: &str) -> Option<&str> {
    let start = reply.find('[')?;
    let end = reply.rfind(']')?;
    if end < start {
        return None;
    }
    Some(&reply[start..=end])
}

fn one_line(s: &str) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    let collapsed: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .chars()
        .take(200)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Parse the model reply robustly: find the JSON array anywhere in the text,
/// keep valid distinct 1-based indices with a non-empty one-line why.
pub fn parse_model_picks(reply: &str, n_candidates: usize) -> Vec<(usize, String)> {
    let Some(array) = extract_json_array(reply) else {
        return Vec::new();
    };
    let parsed: Vec<RawPick> = match serde_json::from_str(array) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in parsed {
        if p.n == 0 || p.n > n_candidates || !seen.insert(p.n) {
            continue;
        }
        let why = one_line(&p.why);
        if why.is_empty() {
            continue;
        }
        out.push((p.n, why));
    }
    out
}

/// Deterministic rule-based why for local picks.
pub fn rule_based_why(c: &Candidate) -> String {
    match c.confidence {
        Some(x) if x >= crate::merge::HIGH_CONFIDENCE_THRESHOLD => {
            format!("High-signal {} save worth revisiting", c.topic)
        }
        Some(x) => format!("Notable {} save (confidence {x:.2})", c.topic),
        None => format!("Recent {} save worth revisiting", c.topic),
    }
}

/// Clamp model picks to [`MIN_PICKS`]-[`MAX_PICKS`], padding with the top
/// remaining candidates when the model returned too few.
pub fn resolve_model_picks(
    candidates: &[Candidate],
    model_picks: &[(usize, String)],
) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = model_picks.iter().take(MAX_PICKS).cloned().collect();
    let want = MIN_PICKS.min(candidates.len());
    if out.len() < want {
        let taken: BTreeSet<usize> = out.iter().map(|(n, _)| *n).collect();
        for c in candidates {
            if out.len() >= want {
                break;
            }
            if !taken.contains(&c.index) {
                out.push((c.index, rule_based_why(c)));
            }
        }
    }
    out
}

/// Deterministic local fallback: the top candidates with rule-based whys.
pub fn local_picks(candidates: &[Candidate]) -> Vec<(usize, String)> {
    candidates
        .iter()
        .take(MAX_PICKS)
        .map(|c| (c.index, rule_based_why(c)))
        .collect()
}

fn verdict_label(v: &Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::NeedsHuman => "needs_human",
        Verdict::Deny { .. } => "deny",
    }
}

/// Decision log path: `$DECISION_LOG_PATH` or `$ANCIENTOS_HOME/decision_log.jsonl`
/// (defaulting to `~/.ancientos-hackathon/decision_log.jsonl`).
pub fn decision_log_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var(DECISION_LOG_ENV_VAR) {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(DECISION_LOG_FILE_NAME))
}

/// Current UTC date as `YYYY-MM-DD`, computed without extra dependencies.
pub fn current_date() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => date_from_days((d.as_secs() / 86400) as i64),
        Err(_) => "1970-01-01".to_string(),
    }
}

/// Convert days since 1970-01-01 to `YYYY-MM-DD` (Hinnant's civil date).
pub fn date_from_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}-{m:02}-{d:02}")
}

fn tokens_opt(usage: Option<&MuseUsage>) -> (Option<u64>, Option<u64>, Option<u64>) {
    match usage {
        Some(u) => (u.prompt_tokens, u.completion_tokens, u.total_tokens),
        None => (None, None, None),
    }
}

fn fmt_tok(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
}

/// True when stored `text` looks like a truncated X post: a t.co link at the
/// end, a trailing `…`, or at least 270 chars without a `note_tweet` source
/// (which would already be the full text).
pub fn looks_truncated(text: &str, had_note_tweet: bool) -> bool {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed
        .split_whitespace()
        .last()
        .is_some_and(|tok| tok.contains("t.co/"))
    {
        return true;
    }
    if trimmed.ends_with('…') {
        return true;
    }
    if !had_note_tweet && trimmed.chars().count() >= 270 {
        return true;
    }
    false
}

/// Render `text` as a Markdown blockquote: every line prefixed with `> `,
/// blank lines as `>`.
pub fn render_blockquote(text: &str) -> String {
    if text.is_empty() {
        return ">".to_string();
    }
    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_markdown(
    date: &str,
    picks: &[BriefPick],
    counts: &MergeCounts,
    model: &str,
    usage: Option<&MuseUsage>,
) -> String {
    let mut md = format!("# Daily brief — {date}\n\n");
    if picks.is_empty() {
        md.push_str("No approved picks today.\n\n");
    } else {
        for (i, p) in picks.iter().enumerate() {
            let author = p
                .author
                .as_deref()
                .map(|a| format!("@{a}"))
                .unwrap_or_else(|| "unknown".to_string());
            let tagged = crate::try_it::classify_text(&p.topic, &p.full_text).is_some();
            let marker = if tagged { " [TRY IT]" } else { "" };
            md.push_str(&format!(
                "## {}. **{}**{marker}\n- Author: {}\n- Topic: {}\n- Link: {}\n\n{}\n\n",
                i + 1,
                p.why,
                author,
                p.topic,
                p.url,
                render_blockquote(&p.full_text)
            ));
            if tagged {
                let steps = crate::try_it::extract_steps(&p.full_text);
                if !steps.is_empty() {
                    md.push_str("**Steps**\n");
                    for (n, step) in steps.iter().enumerate() {
                        md.push_str(&format!("{}. {step}\n", n + 1));
                    }
                    md.push('\n');
                }
            }
            let media_md = crate::media::render_markdown(&p.media);
            if !media_md.is_empty() {
                md.push_str(&media_md);
                md.push('\n');
            }
        }
    }
    let (pt, ct, tt) = tokens_opt(usage);
    md.push_str(&format!(
        "---\n*Sources: live={} fixture={} overlap={} total={} high_confidence={} · Model: {} · Tokens: prompt={} completion={} total={}*\n",
        counts.live,
        counts.fixture,
        counts.overlap,
        counts.total,
        counts.high_confidence,
        model,
        fmt_tok(pt),
        fmt_tok(ct),
        fmt_tok(tt)
    ));
    md
}

/// Run the brief: select candidates, make at most one model call, gate every
/// pick, hydrate truncated full text (display only), expand one thread root
/// (display only), append to the decision log, and render markdown.
pub fn run(
    merged: &MergeOutput,
    model: &dyn BriefModel,
    log_path: &Path,
    date: &str,
) -> Result<BriefResult, String> {
    run_full(
        merged,
        model,
        log_path,
        date,
        Some(&crate::x_api::hydrate),
        Some(&crate::x_api::fetch_thread_if_root),
    )
}

/// Same as [`run`] but with an injectable hydrator for tests. The hydrator is
/// called at most once per brief with the truncated pick ids; on failure the
/// stored text is kept. `None` skips hydration entirely. No thread expansion.
pub fn run_with_hydrator(
    merged: &MergeOutput,
    model: &dyn BriefModel,
    log_path: &Path,
    date: &str,
    hydrator: Option<&Hydrator<'_>>,
) -> Result<BriefResult, String> {
    run_full(merged, model, log_path, date, hydrator, None)
}

/// Same as [`run`] with injectable hydrator and thread expander for tests.
/// The expander is called at most once per brief with the first pick's id;
/// on failure or `None` the root text is kept. `None` skips expansion.
pub fn run_full(
    merged: &MergeOutput,
    model: &dyn BriefModel,
    log_path: &Path,
    date: &str,
    hydrator: Option<&Hydrator<'_>>,
    thread_expander: Option<&ThreadExpander<'_>>,
) -> Result<BriefResult, String> {
    if merged.rows.is_empty() {
        return Err("no bookmarks to brief".to_string());
    }
    // Posts hidden by a never_post verdict never become candidates.
    let hidden = {
        let log = DecisionLog::open(log_path).map_err(|e| e.to_string())?;
        feedback::hidden_posts(&feedback::records_from_log(&log).map_err(|e| e.to_string())?)
    };
    let rows: Vec<Bookmark> = merged
        .rows
        .iter()
        .filter(|b| !hidden.contains(&b.post_id))
        .cloned()
        .collect();
    if rows.is_empty() {
        return Err("no bookmarks to brief".to_string());
    }
    let candidates = select_candidates(&rows, &merged.live_ids);
    if candidates.is_empty() {
        return Err("no bookmarks to brief".to_string());
    }

    // Only sanitized text may leave the device. The prompt is built from
    // stored excerpts only; hydration happens after picks and is display only.
    let dest = route(Sensitivity::Sanitized, false);
    let (resolved, model_id, usage, fallback) = if dest == Destination::MuseSpark {
        match model.call(&build_prompt(&candidates)) {
            Ok(reply) => {
                let picks = parse_model_picks(&reply.content, candidates.len());
                let resolved = resolve_model_picks(&candidates, &picks);
                (resolved, reply.model, reply.usage, false)
            }
            Err(_) => (local_picks(&candidates), "local".to_string(), None, true),
        }
    } else {
        (local_picks(&candidates), "local".to_string(), None, true)
    };

    let stored: HashMap<&str, &Bookmark> = rows.iter().map(|b| (b.post_id.as_str(), b)).collect();
    let mut gate = Gate::new();
    gate.register(BRIEF_ACTION, Risk::Low);
    let mut evaluated: Vec<(BriefPick, Verdict, String)> = Vec::new();
    for (n, why) in &resolved {
        let Some(c) = candidates.iter().find(|c| c.index == *n) else {
            continue;
        };
        let verdict = gate.evaluate(&Proposal {
            agent: AGENT_NAME.to_string(),
            action: BRIEF_ACTION.to_string(),
        });
        let full_text = stored
            .get(c.post_id.as_str())
            .map(|b| b.text.clone())
            .unwrap_or_default();
        let media = stored
            .get(c.post_id.as_str())
            .map(|b| b.media.clone())
            .unwrap_or_default();
        let pick = BriefPick {
            index: c.index,
            post_id: c.post_id.clone(),
            author: c.author.clone(),
            url: c.url.clone(),
            topic: c.topic.clone(),
            why: why.clone(),
            full_text,
            media,
        };
        evaluated.push((pick, verdict, why.clone()));
    }
    let mut picks: Vec<BriefPick> = evaluated
        .iter()
        .filter(|(_, v, _)| *v == Verdict::Allow)
        .map(|(p, _, _)| p.clone())
        .collect();

    // Hydrate truncated picks with full text (display only). At most one call.
    let need: Vec<String> = picks
        .iter()
        .filter(|p| {
            let (text, had) = stored
                .get(p.post_id.as_str())
                .map(|b| (b.text.as_str(), b.had_note_tweet))
                .unwrap_or((p.full_text.as_str(), false));
            looks_truncated(text, had)
        })
        .map(|p| p.post_id.clone())
        .collect();
    let requested = need.len();
    let mut hydrated = 0usize;
    if !need.is_empty() {
        if let Some(h) = hydrator {
            if let Ok(map) = h(&need) {
                for p in &mut picks {
                    if let Some(full) = map.get(&p.post_id) {
                        if !full.is_empty() {
                            p.full_text = full.clone();
                            hydrated += 1;
                        }
                    }
                }
            }
        }
    }

    // Expand the first pick when it is a thread root (display only).
    // At most one expander call per brief; anything but a multi-post
    // thread keeps the root text. Quoted posts render one level deeper:
    // `> ...` here becomes `> > ...` in the blockquote.
    if let (Some(expand), Some(first_id)) =
        (thread_expander, picks.first().map(|p| p.post_id.clone()))
    {
        if let Ok(Some(posts)) = expand(&first_id) {
            if posts.len() > 1 {
                let mut texts: Vec<String> = Vec::new();
                for p in &posts {
                    let body = p.text.trim();
                    if !body.is_empty() {
                        texts.push(body.to_string());
                    }
                    if let Some((_, quoted_text)) = &p.quoted {
                        if !quoted_text.trim().is_empty() {
                            let nested = quoted_text
                                .lines()
                                .map(|l| {
                                    if l.trim().is_empty() {
                                        ">".to_string()
                                    } else {
                                        format!("> {l}")
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            texts.push(nested);
                        }
                    }
                }
                if !texts.is_empty() {
                    picks[0].full_text = texts.join("\n\n");
                }
            }
        }
    }

    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    let ids: Vec<&str> = candidates.iter().map(|c| c.post_id.as_str()).collect();
    log.append(
        "brief.candidates",
        serde_json::json!({"count": candidates.len(), "ids": ids}),
    )
    .map_err(|e| e.to_string())?;
    let (pt, ct, tt) = tokens_opt(usage.as_ref());
    // Never log prompt text here: model id and token counts only.
    log.append(
        "brief.model_call",
        serde_json::json!({
            "model": model_id,
            "prompt_tokens": pt,
            "completion_tokens": ct,
            "total_tokens": tt,
            "fallback": fallback,
            "destination": if fallback { "local" } else { "MuseSpark" },
        }),
    )
    .map_err(|e| e.to_string())?;
    for (pick, verdict, why) in &evaluated {
        let mut payload = serde_json::json!({
            "post_id": pick.post_id,
            "verdict": verdict_label(verdict),
            "why": why,
        });
        if let Verdict::Deny { reason } = verdict {
            payload["reason"] = serde_json::json!(reason);
        }
        log.append("brief.pick", payload)
            .map_err(|e| e.to_string())?;
    }
    // Never log post text here: counts only.
    log.append(
        "brief.hydrate",
        serde_json::json!({"requested": requested, "hydrated": hydrated}),
    )
    .map_err(|e| e.to_string())?;
    // Tag hands-on picks for follow-up: one `try_it.shown` entry per
    // rendered tagged pick (post id plus timestamp, never text).
    let shown_at = crate::idea_card::now_secs();
    for p in &picks {
        if crate::try_it::classify_text(&p.topic, &p.full_text).is_some() {
            crate::try_it::record_shown(&mut log, &p.post_id, shown_at)?;
        }
    }

    let markdown = render_markdown(date, &picks, &merged.counts, &model_id, usage.as_ref());
    Ok(BriefResult {
        markdown,
        picks,
        candidates,
        model: model_id,
        usage,
        fallback,
        counts: merged.counts.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn bm(id: &str, created: Option<&str>, topic: Option<&str>, conf: Option<f64>) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: format!("synthetic text for {id}"),
            author_username: Some(format!("syn_{id}")),
            created_at: created.map(str::to_string),
            folders: None,
            label: topic.map(|t| crate::fixture::Label {
                parent_label: Some(t.into()),
                confidence: conf,
                method: None,
                is_foldered: None,
            }),
            had_note_tweet: false,
            media: Vec::new(),
        }
    }

    fn merged_of(rows: Vec<Bookmark>, live_ids: &[&str]) -> MergeOutput {
        let counts = MergeCounts {
            live: live_ids.len(),
            fixture: 0,
            overlap: 0,
            total: rows.len(),
            high_confidence: 0,
        };
        MergeOutput {
            rows,
            live_ids: live_ids.iter().map(|s| (*s).to_string()).collect(),
            counts,
        }
    }

    struct FakeModel {
        content: Option<String>,
        model: String,
        usage: Option<MuseUsage>,
        seen: RefCell<Vec<String>>,
    }

    impl FakeModel {
        fn ok(content: &str) -> Self {
            Self {
                content: Some(content.into()),
                model: "fake-spark".into(),
                usage: Some(MuseUsage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    total_tokens: Some(15),
                }),
                seen: RefCell::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                content: None,
                model: "fake-spark".into(),
                usage: None,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl BriefModel for FakeModel {
        fn model_id(&self) -> String {
            self.model.clone()
        }

        fn call(&self, prompt: &str) -> Result<ModelReply, RouterError> {
            self.seen.borrow_mut().push(prompt.to_string());
            match &self.content {
                Some(c) => Ok(ModelReply {
                    content: c.clone(),
                    model: self.model.clone(),
                    usage: self.usage.clone(),
                }),
                None => Err(RouterError::PaidCallsDisabled),
            }
        }
    }

    #[test]
    fn selection_prefers_live_then_recency_then_confidence() {
        let rows = vec![
            bm(
                "fix-new",
                Some("2026-02-01T00:00:00+00:00"),
                Some("T"),
                Some(0.9),
            ),
            bm(
                "live-old",
                Some("2026-01-01T00:00:00+00:00"),
                Some("T"),
                Some(0.1),
            ),
            bm(
                "live-new",
                Some("2026-03-01T00:00:00+00:00"),
                Some("T"),
                Some(0.1),
            ),
            bm("live-nodate", None, Some("T"), Some(0.99)),
        ];
        let live: BTreeSet<String> = ["live-old", "live-new", "live-nodate"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let out = select_candidates(&rows, &live);
        let ids: Vec<&str> = out.iter().map(|c| c.post_id.as_str()).collect();
        assert_eq!(ids, vec!["live-new", "live-old", "live-nodate", "fix-new"]);
    }

    #[test]
    fn selection_interleaves_topics_and_caps_at_20() {
        let mut rows = Vec::new();
        for i in 0..25 {
            let topic = format!("Topic{}", i % 3);
            rows.push(bm(
                &format!("id{i:02}"),
                Some("2026-01-01T00:00:00+00:00"),
                Some(&topic),
                Some(0.5),
            ));
        }
        let live: BTreeSet<String> = rows.iter().map(|b| b.post_id.clone()).collect();
        let out = select_candidates(&rows, &live);
        assert_eq!(out.len(), MAX_CANDIDATES);
        let first_three: BTreeSet<&str> = out.iter().take(3).map(|c| c.topic.as_str()).collect();
        assert_eq!(first_three.len(), 3, "first round spans all topics");
        assert!(out.iter().enumerate().all(|(i, c)| c.index == i + 1));
    }

    #[test]
    fn selection_sanitizes_excerpts() {
        let mut b = bm("s1", None, Some("T"), None);
        b.text = "ping @syn_handle see https://t.co/abc call 555-123-4567".into();
        let live: BTreeSet<String> = ["s1".to_string()].into_iter().collect();
        let out = select_candidates(std::slice::from_ref(&b), &live);
        assert_eq!(out.len(), 1);
        assert!(!out[0].sanitized.contains("http"), "{}", out[0].sanitized);
        assert!(
            !out[0].sanitized.contains("@syn_handle"),
            "{}",
            out[0].sanitized
        );
        assert!(!out[0].sanitized.contains("555"), "{}", out[0].sanitized);
    }

    #[test]
    fn parses_strict_and_embedded_arrays() {
        let strict = r#"[{"n":2,"why":"good"},{"n":1,"why":"better"}]"#;
        assert_eq!(
            parse_model_picks(strict, 3),
            vec![(2, "good".into()), (1, "better".into())]
        );
        let prose = "Here are my picks:\n[{\"n\": 1, \"why\": \"  spaced out  \" }] thanks!";
        assert_eq!(parse_model_picks(prose, 3), vec![(1, "spaced out".into())]);
        let escaped = "[{\"n\": 2, \"why\": \"first\\nsecond\"}]";
        assert_eq!(parse_model_picks(escaped, 3), vec![(2, "first".into())]);
    }

    #[test]
    fn parse_drops_bad_indices_dupes_and_empty_why() {
        let reply = r#"[{"n":0,"why":"a"},{"n":9,"why":"b"},{"n":1,"why":"keep"},{"n":1,"why":"dupe"},{"n":2,"why":"   "}]"#;
        assert_eq!(parse_model_picks(reply, 3), vec![(1, "keep".into())]);
        assert!(parse_model_picks("no json here", 3).is_empty());
        assert!(parse_model_picks("[not json]", 3).is_empty());
    }

    #[test]
    fn resolve_clamps_and_pads() {
        let rows: Vec<Bookmark> = (0..6)
            .map(|i| bm(&format!("id{i}"), None, Some("T"), Some(0.5)))
            .collect();
        let live: BTreeSet<String> = rows.iter().map(|b| b.post_id.clone()).collect();
        let cands = select_candidates(&rows, &live);
        // Too many: clamp to 5.
        let many: Vec<(usize, String)> = (1..=6).map(|n| (n, format!("w{n}"))).collect();
        assert_eq!(resolve_model_picks(&cands, &many).len(), MAX_PICKS);
        // Too few: pad to 3 with rule-based whys.
        let one = vec![(6, "model why".into())];
        let resolved = resolve_model_picks(&cands, &one);
        assert_eq!(resolved.len(), MIN_PICKS);
        assert_eq!(resolved[0], (6, "model why".into()));
        assert!(resolved[1].1.contains('T'));
    }

    #[test]
    fn local_fallback_is_deterministic_top_five() {
        let rows: Vec<Bookmark> = (0..7)
            .map(|i| bm(&format!("id{i}"), None, Some("T"), Some(0.9)))
            .collect();
        let live: BTreeSet<String> = rows.iter().map(|b| b.post_id.clone()).collect();
        let cands = select_candidates(&rows, &live);
        let picks = local_picks(&cands);
        assert_eq!(picks.len(), 5);
        assert_eq!(picks[0].0, 1);
        assert!(picks[0].1.contains("High-signal"));
        assert_eq!(picks, local_picks(&cands));
    }

    #[test]
    fn run_success_logs_and_renders_markdown() {
        let rows: Vec<Bookmark> = (0..4)
            .map(|i| bm(&format!("id{i}"), None, Some("TopicA"), Some(0.9)))
            .collect();
        let merged = merged_of(rows, &["id0", "id1", "id2", "id3"]);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("decision_log.jsonl");
        let model = FakeModel::ok(
            r#"[{"n":2,"why":"model reason two"},{"n":4,"why":"model reason four"}]"#,
        );
        let res = run(&merged, &model, &log_path, "2026-10-26").unwrap();

        assert!(!res.fallback);
        assert_eq!(res.model, "fake-spark");
        assert_eq!(model.seen.borrow().len(), 1, "exactly one model call");
        // Two model picks padded to three.
        assert_eq!(res.picks.len(), 3);
        assert_eq!(res.picks[0].post_id, "id1");
        assert_eq!(res.picks[0].why, "model reason two");
        assert_eq!(res.picks[0].full_text, "synthetic text for id1");

        assert!(res.markdown.starts_with("# Daily brief — 2026-10-26"));
        assert!(res.markdown.contains("**model reason two**"));
        assert!(res.markdown.contains("@syn_id1"));
        assert!(res.markdown.contains("https://x.com/i/status/id1"));
        assert!(res.markdown.contains("TopicA"));
        assert!(res.markdown.contains("Model: fake-spark"));
        assert!(res.markdown.contains("prompt=10"));
        assert!(
            res.markdown.contains("> synthetic text for id1"),
            "{}",
            res.markdown
        );

        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1 + 1 + 3 + 1);
        assert_eq!(entries[0].kind, "brief.candidates");
        assert_eq!(entries[0].payload["count"], 4);
        assert_eq!(entries[1].kind, "brief.model_call");
        assert_eq!(entries[1].payload["model"], "fake-spark");
        assert_eq!(entries[1].payload["total_tokens"], 15);
        // No prompt text in the model-call entry.
        let call_json = entries[1].payload.to_string();
        assert!(!call_json.contains("synthetic text"), "{call_json}");
        assert_eq!(entries[2].kind, "brief.pick");
        assert_eq!(entries[2].payload["post_id"], "id1");
        assert_eq!(entries[2].payload["verdict"], "allow");
        assert_eq!(entries[5].kind, "brief.hydrate");
        assert_eq!(entries[5].payload["requested"], 0);
        assert_eq!(entries[5].payload["hydrated"], 0);
        assert!(
            !entries[5].payload.to_string().contains("synthetic text"),
            "no text in hydrate entry"
        );
    }

    #[test]
    fn run_falls_back_when_model_fails() {
        let rows: Vec<Bookmark> = (0..6)
            .map(|i| bm(&format!("id{i}"), None, Some("TopicB"), None))
            .collect();
        let merged = merged_of(rows, &["id0"]);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("decision_log.jsonl");
        let res = run(&merged, &FakeModel::failing(), &log_path, "2026-10-26").unwrap();
        assert!(res.fallback);
        assert_eq!(res.model, "local");
        assert!(res.usage.is_none());
        assert_eq!(res.picks.len(), 5);
        assert!(res.picks[0].why.contains("TopicB"));
        assert!(res.markdown.contains("Model: local"));
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries[1].payload["fallback"], true);
    }

    #[test]
    fn run_sends_only_sanitized_text() {
        let mut b = bm("s1", None, Some("T"), None);
        b.text = "secret-handle @syn_handle secret-link https://t.co/abc".into();
        let merged = merged_of(vec![b], &["s1"]);
        let dir = tempfile::tempdir().unwrap();
        let model = FakeModel::ok(r#"[{"n":1,"why":"ok"}]"#);
        run_with_hydrator(
            &merged,
            &model,
            &dir.path().join("d.jsonl"),
            "2026-10-26",
            None,
        )
        .unwrap();
        let prompt = &model.seen.borrow()[0];
        assert!(!prompt.contains("@syn_handle"), "{prompt}");
        assert!(!prompt.contains("https://t.co/abc"), "{prompt}");
        assert!(prompt.contains("secret-handle"), "{prompt}");
    }

    #[test]
    fn run_appends_without_rewriting_history() {
        let merged = merged_of(vec![bm("s1", None, Some("T"), None)], &["s1"]);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&log_path).unwrap();
        log.append("seed", serde_json::json!({"v": 1})).unwrap();
        drop(log);
        run(
            &merged,
            &FakeModel::ok(r#"[{"n":1,"why":"ok"}]"#),
            &log_path,
            "2026-10-26",
        )
        .unwrap();
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries[0].kind, "seed");
        assert!(entries.len() > 1);
    }

    #[test]
    fn run_rejects_empty_store() {
        let merged = merged_of(Vec::new(), &[]);
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            &merged,
            &FakeModel::failing(),
            &dir.path().join("d.jsonl"),
            "2026-10-26",
        )
        .unwrap_err();
        assert!(err.contains("no bookmarks"));
        assert!(
            !dir.path().join("d.jsonl").exists(),
            "no log without a brief"
        );
    }

    #[test]
    fn known_dates_convert() {
        assert_eq!(date_from_days(0), "1970-01-01");
        assert_eq!(date_from_days(20_484), "2026-01-31");
        assert_eq!(date_from_days(20_394), "2025-11-02");
    }

    #[test]
    fn current_date_is_shaped_like_yyyy_mm_dd() {
        let d = current_date();
        assert_eq!(d.len(), 10);
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
        assert!(d.chars().filter(|c| *c != '-').all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn detects_truncated_text() {
        // t.co link at the end.
        assert!(looks_truncated(
            "synthetic excerpt https://t.co/abc123",
            false
        ));
        assert!(looks_truncated(
            "synthetic excerpt https://t.co/abc123  ",
            false
        ));
        // t.co link not at the end is not truncation.
        assert!(!looks_truncated(
            "see https://t.co/abc123 for synthetic context here",
            false
        ));
        // Trailing ellipsis.
        assert!(looks_truncated("synthetic truncated…", false));
        assert!(looks_truncated("synthetic truncated…   ", false));
        assert!(!looks_truncated("synthetic complete.", false));
        // Long text without note_tweet source.
        let long = "x".repeat(270);
        assert!(looks_truncated(&long, false));
        assert!(looks_truncated(&"y".repeat(500), false));
        assert!(!looks_truncated(&"z".repeat(269), false));
        // Long text with note_tweet source is already full.
        assert!(!looks_truncated(&long, true));
        // Short non-truncated and empty.
        assert!(!looks_truncated("synthetic short", false));
        assert!(!looks_truncated("", false));
        assert!(!looks_truncated("   ", false));
    }

    #[test]
    fn renders_blockquote_with_blank_lines() {
        assert_eq!(render_blockquote("synthetic line"), "> synthetic line");
        assert_eq!(
            render_blockquote("synthetic one\nsynthetic two"),
            "> synthetic one\n> synthetic two"
        );
        assert_eq!(
            render_blockquote("synthetic one\n\nsynthetic three"),
            "> synthetic one\n>\n> synthetic three"
        );
        assert_eq!(render_blockquote(""), ">");
    }

    #[test]
    fn render_markdown_shows_media_block_only_for_picks_with_media() {
        let counts = MergeCounts {
            live: 2,
            fixture: 0,
            overlap: 0,
            total: 2,
            high_confidence: 0,
        };
        let with_media = BriefPick {
            index: 1,
            post_id: "m1".into(),
            author: Some("syn_user".into()),
            url: "https://x.com/i/status/m1".into(),
            topic: "Synthetic".into(),
            why: "synthetic why".into(),
            full_text: "synthetic full text".into(),
            media: vec![crate::media::Media {
                kind: "photo".into(),
                url: Some("https://pbs.twimg.com/media/synthetic.jpg".into()),
                preview_url: None,
                video_url: None,
                alt_text: Some("synthetic alt".into()),
            }],
        };
        let without_media = BriefPick {
            index: 2,
            post_id: "m2".into(),
            author: None,
            url: "https://x.com/i/status/m2".into(),
            topic: "Synthetic".into(),
            why: "synthetic why two".into(),
            full_text: "synthetic plain".into(),
            media: Vec::new(),
        };
        let md = render_markdown(
            "2026-10-26",
            std::slice::from_ref(&with_media),
            &counts,
            "local",
            None,
        );
        let quote_pos = md.find("> synthetic full text").expect("blockquote: {md}");
        let media_pos = md.find("**Media**").expect("media block: {md}");
        assert!(media_pos > quote_pos, "media under blockquote: {md}");
        assert!(
            md.contains("https://pbs.twimg.com/media/synthetic.jpg"),
            "{md}"
        );

        let plain = render_markdown(
            "2026-10-26",
            std::slice::from_ref(&without_media),
            &counts,
            "local",
            None,
        );
        assert!(!plain.contains("**Media**"), "{plain}");
    }

    #[test]
    fn hydrate_called_at_most_once_for_truncated_picks() {
        let mut t1 = bm("t1", None, Some("T"), None);
        t1.text = "synthetic truncated one https://t.co/aaa".into();
        let mut t2 = bm("t2", None, Some("T"), None);
        t2.text = "synthetic truncated two…".into();
        let t3 = bm("t3", None, Some("T"), None);
        let merged = merged_of(vec![t1, t2, t3], &["t1", "t2", "t3"]);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let calls = RefCell::new(0usize);
        let seen_ids = RefCell::new(Vec::new());
        let hydrator = |ids: &[String]| -> Result<HashMap<String, String>, String> {
            *calls.borrow_mut() += 1;
            *seen_ids.borrow_mut() = ids.to_vec();
            let mut map = HashMap::new();
            for id in ids {
                map.insert(id.clone(), format!("synthetic FULL for {id}\nsecond line"));
            }
            Ok(map)
        };
        let model = FakeModel::ok(r#"[{"n":1,"why":"ok one"},{"n":2,"why":"ok two"}]"#);
        let res =
            run_with_hydrator(&merged, &model, &log_path, "2026-10-26", Some(&hydrator)).unwrap();
        assert_eq!(*calls.borrow(), 1, "exactly one hydrate call");
        let mut got = seen_ids.borrow().clone();
        got.sort();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"t1".to_string()));
        assert!(got.contains(&"t2".to_string()));
        // Picks carry hydrated full text; markdown shows it as a blockquote.
        let p1 = res.picks.iter().find(|p| p.post_id == "t1").unwrap();
        assert_eq!(p1.full_text, "synthetic FULL for t1\nsecond line");
        assert!(res.markdown.contains("> synthetic FULL for t1"));
        assert!(res.markdown.contains("> second line"));
        // Non-truncated pick keeps stored text.
        let p3 = res.picks.iter().find(|p| p.post_id == "t3");
        if let Some(p) = p3 {
            assert_eq!(p.full_text, "synthetic text for t3");
        }
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        let h = entries.iter().find(|e| e.kind == "brief.hydrate").unwrap();
        assert_eq!(h.payload["requested"], 2);
        assert_eq!(h.payload["hydrated"], 2);
        assert!(!h.payload.to_string().contains("synthetic FULL"));
    }

    #[test]
    fn hydrate_failure_keeps_stored_text() {
        let mut t1 = bm("t1", None, Some("T"), None);
        t1.text = "synthetic truncated https://t.co/zzz".into();
        let merged = merged_of(vec![t1], &["t1"]);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let failing = |_ids: &[String]| -> Result<HashMap<String, String>, String> {
            Err("synthetic hydrate failure".into())
        };
        let res = run_with_hydrator(
            &merged,
            &FakeModel::ok(r#"[{"n":1,"why":"ok"}]"#),
            &log_path,
            "2026-10-26",
            Some(&failing),
        )
        .unwrap();
        assert_eq!(res.picks.len(), 1);
        assert_eq!(
            res.picks[0].full_text,
            "synthetic truncated https://t.co/zzz"
        );
        assert!(
            res.markdown
                .contains("> synthetic truncated https://t.co/zzz"),
            "{}",
            res.markdown
        );
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        let h = entries.iter().find(|e| e.kind == "brief.hydrate").unwrap();
        assert_eq!(h.payload["requested"], 1);
        assert_eq!(h.payload["hydrated"], 0);
    }

    #[test]
    fn hydrate_does_not_change_model_prompt() {
        let mut t1 = bm("t1", None, Some("T"), None);
        t1.text = "synthetic stored https://t.co/qqq".into();
        let merged = merged_of(vec![t1], &["t1"]);
        let dir = tempfile::tempdir().unwrap();
        let model = FakeModel::ok(r#"[{"n":1,"why":"ok"}]"#);
        let hydrator = |ids: &[String]| -> Result<HashMap<String, String>, String> {
            assert_eq!(ids, &["t1".to_string()]);
            let mut map = HashMap::new();
            map.insert(
                "t1".to_string(),
                "synthetic HYDRATED-ONLY full text".to_string(),
            );
            Ok(map)
        };
        let res = run_with_hydrator(
            &merged,
            &model,
            &dir.path().join("d.jsonl"),
            "2026-10-26",
            Some(&hydrator),
        )
        .unwrap();
        let prompt = &model.seen.borrow()[0];
        assert!(
            !prompt.contains("HYDRATED-ONLY"),
            "prompt must not see hydrated text: {prompt}"
        );
        assert!(prompt.contains("synthetic stored"), "{prompt}");
        assert_eq!(res.picks[0].full_text, "synthetic HYDRATED-ONLY full text");
    }

    fn seed_hidden(log_path: &Path, post_id: &str) {
        use ancientos_core::feedback::{
            record, FeedbackRecord, ThresholdPolicy, Verdict as FbVerdict,
        };
        let mut log = DecisionLog::open(log_path).unwrap();
        record(
            &mut log,
            FeedbackRecord {
                pick_id: post_id.into(),
                category: "TopicA".into(),
                verdict: FbVerdict::NeverPost,
                tags: Vec::new(),
            },
            ThresholdPolicy::default(),
        )
        .unwrap();
    }

    #[test]
    fn hidden_posts_are_excluded_from_candidates() {
        let rows: Vec<Bookmark> = (0..4)
            .map(|i| bm(&format!("id{i}"), None, Some("TopicA"), Some(0.9)))
            .collect();
        let merged = merged_of(rows, &["id0", "id1", "id2", "id3"]);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        seed_hidden(&log_path, "id1");
        let res = run_with_hydrator(
            &merged,
            &FakeModel::ok(r#"[{"n":1,"why":"ok"}]"#),
            &log_path,
            "2026-10-26",
            None,
        )
        .unwrap();
        assert!(!res.candidates.iter().any(|c| c.post_id == "id1"));
        assert!(!res.picks.iter().any(|p| p.post_id == "id1"));
        assert!(!res.markdown.contains("status/id1"), "{}", res.markdown);
        let log = DecisionLog::open(&log_path).unwrap();
        let entries = log.verify().unwrap();
        let cands = entries
            .iter()
            .find(|e| e.kind == "brief.candidates")
            .unwrap();
        assert_eq!(cands.payload["count"], 3);
    }

    #[test]
    fn all_hidden_is_no_brief() {
        let merged = merged_of(vec![bm("only", None, Some("T"), None)], &["only"]);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        seed_hidden(&log_path, "only");
        let err = run_with_hydrator(
            &merged,
            &FakeModel::failing(),
            &log_path,
            "2026-10-26",
            None,
        )
        .unwrap_err();
        assert!(err.contains("no bookmarks to brief"), "{err}");
    }

    fn thread_post(id: &str, text: &str) -> crate::unroll::ThreadPost {
        crate::unroll::ThreadPost {
            id: id.into(),
            author_id: "u1".into(),
            text: text.into(),
            created_at: None,
            conversation_id: Some("id0".into()),
            referenced_tweets: Vec::new(),
            quoted: None,
        }
    }

    #[test]
    fn thread_root_expands_first_pick_only() {
        let rows: Vec<Bookmark> = (0..3)
            .map(|i| bm(&format!("id{i}"), None, Some("T"), Some(0.9)))
            .collect();
        let merged = merged_of(rows, &["id0", "id1", "id2"]);
        let dir = tempfile::tempdir().unwrap();
        let calls = RefCell::new(Vec::new());
        let expander = |id: &str| -> Result<Option<Vec<crate::unroll::ThreadPost>>, String> {
            calls.borrow_mut().push(id.to_string());
            Ok(Some(vec![
                thread_post("id0", "synthetic root post"),
                thread_post("id0-r1", "synthetic reply one"),
                thread_post("id0-r2", "synthetic reply two"),
            ]))
        };
        let res = run_full(
            &merged,
            &FakeModel::ok(r#"[{"n":1,"why":"ok"}]"#),
            &dir.path().join("d.jsonl"),
            "2026-10-26",
            None,
            Some(&expander),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec!["id0".to_string()]);
        assert_eq!(
            res.picks[0].full_text,
            "synthetic root post\n\nsynthetic reply one\n\nsynthetic reply two"
        );
        assert!(
            res.markdown.contains("> synthetic reply two"),
            "{}",
            res.markdown
        );
        // Other picks keep stored text.
        assert_eq!(res.picks[1].full_text, "synthetic text for id1");
    }

    #[test]
    fn thread_expansion_nests_quoted_text_in_blockquote() {
        let merged = merged_of(vec![bm("id0", None, Some("T"), None)], &["id0"]);
        let dir = tempfile::tempdir().unwrap();
        let mut reply = thread_post("id0-r1", "synthetic reply one");
        reply.quoted = Some(("syn_quoted".into(), "synthetic quoted real content".into()));
        let expander = |_: &str| -> Result<Option<Vec<crate::unroll::ThreadPost>>, String> {
            Ok(Some(vec![
                thread_post("id0", "synthetic root post"),
                reply.clone(),
            ]))
        };
        let res = run_full(
            &merged,
            &FakeModel::ok(r#"[{"n":1,"why":"ok"}]"#),
            &dir.path().join("d.jsonl"),
            "2026-10-26",
            None,
            Some(&expander),
        )
        .unwrap();
        assert_eq!(
            res.picks[0].full_text,
            "synthetic root post\n\nsynthetic reply one\n\n> synthetic quoted real content"
        );
        assert!(
            res.markdown.contains("> > synthetic quoted real content"),
            "{}",
            res.markdown
        );
        assert!(
            res.markdown.contains("> synthetic reply one"),
            "{}",
            res.markdown
        );
    }

    #[test]
    fn thread_expansion_failures_keep_root_only() {
        let merged = merged_of(vec![bm("id0", None, Some("T"), None)], &["id0"]);
        let dir = tempfile::tempdir().unwrap();
        for (name, expander) in [
            (
                "none",
                &(|_: &str| Ok(None))
                    as &dyn Fn(&str) -> Result<Option<Vec<crate::unroll::ThreadPost>>, String>,
            ),
            (
                "err",
                &(|_: &str| Err("synthetic search failure".into()))
                    as &dyn Fn(&str) -> Result<Option<Vec<crate::unroll::ThreadPost>>, String>,
            ),
            (
                "single",
                &(|_: &str| Ok(Some(vec![thread_post("id0", "synthetic root post")])))
                    as &dyn Fn(&str) -> Result<Option<Vec<crate::unroll::ThreadPost>>, String>,
            ),
        ] {
            let res = run_full(
                &merged,
                &FakeModel::ok(r#"[{"n":1,"why":"ok"}]"#),
                &dir.path().join(format!("{name}.jsonl")),
                "2026-10-26",
                None,
                Some(expander),
            )
            .unwrap();
            assert_eq!(res.picks[0].full_text, "synthetic text for id0", "{name}");
        }
    }
}
