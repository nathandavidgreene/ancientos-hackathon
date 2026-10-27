//! Personal feed ranking learned from verdicts.
//!
//! Trains locally and deterministically on the feedback records already in
//! the decision log ([`ancientos_core::feedback::records_from_log`]) plus
//! `tried yes` follow-ups: approve, delegate and tried-yes are positive;
//! skip, never_post, never_category and never_author are negative; anything
//! else (unhide_author, tried no/later) is ignored. No network, no paid
//! calls, no new crates.
//!
//! Model: smoothed naive Bayes over category, normalized author, has_media,
//! has_link, thread-length bucket, and lowercase word unigrams from sanitized
//! post text (stopwords dropped, the [`VOCAB_CAP`] most frequent kept),
//! blended with a zoxide-style frecency affinity per author and category:
//! approve and tried-yes add, skip subtracts, weighted by recency (the idea
//! only; written here from scratch).
//!
//! Fail closed: with fewer than `min_records` labeled records (env var
//! [`MIN_RECORDS_ENV_VAR`], default [`MIN_RECORDS_DEFAULT`]) the ranker is
//! inactive and callers keep today's ordering unchanged.
//!
//! Decision-log verdicts carry no timestamps, so log-trained events use a
//! neutral recency weight; [`LabeledEvent::at`] carries explicit vote times
//! for time-aware training (the eval harness uses synthetic vote times).
use crate::fixture::Bookmark;
use ancientos_core::feedback::{self, FeedbackRecord, Verdict};
use ancientos_core::log::DecisionLog;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Log kind appended once per brief when the ranker stays inactive.
pub const INACTIVE_KIND: &str = "ranker.inactive";
/// Log kind appended once per brief when the ranker re-ranks candidates.
pub const APPLIED_KIND: &str = "ranker.applied";
/// Env var overriding the minimum labeled records (default [`MIN_RECORDS_DEFAULT`]).
pub const MIN_RECORDS_ENV_VAR: &str = "ANCIENTOS_RANKER_MIN_RECORDS";
/// Default minimum labeled records before the ranker activates.
pub const MIN_RECORDS_DEFAULT: usize = 10;
/// Cap on word-unigram vocabulary (most frequent win, ties alphabetical).
pub const VOCAB_CAP: usize = 500;
/// Weight blending frecency affinity into the naive Bayes log-odds score.
pub const FRECENCY_BLEND: f64 = 0.25;

/// One labeled training signal: a verdict (or tried-yes) on one post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Approve,
    Delegate,
    TriedYes,
    Skip,
    NeverPost,
    NeverCategory,
    NeverAuthor,
}

impl Signal {
    /// Positive class: approve, delegate, tried-yes.
    pub fn positive(self) -> bool {
        matches!(self, Self::Approve | Self::Delegate | Self::TriedYes)
    }

    /// Signed frecency step: approve and tried-yes boost, skip decays,
    /// everything else is neutral (delegate still counts as positive for
    /// naive Bayes; never verdicts hide posts elsewhere).
    pub fn frecency_delta(self) -> f64 {
        match self {
            Self::Approve | Self::TriedYes => 1.0,
            Self::Skip => -1.0,
            Self::Delegate | Self::NeverPost | Self::NeverCategory | Self::NeverAuthor => 0.0,
        }
    }
}

/// One labeled post for training.
#[derive(Debug, Clone)]
pub struct LabeledEvent {
    pub bookmark: Bookmark,
    pub signal: Signal,
    /// Vote time in unix secs when known. `None` (every decision-log
    /// verdict, which carries no timestamp) trains with neutral recency.
    pub at: Option<u64>,
}

/// Score for one post: higher means more likely worth surfacing.
#[derive(Debug, Clone, PartialEq)]
pub struct RankScore {
    pub score: f64,
    /// The 3 strongest contributing features in plain words. Empty when
    /// the ranker is inactive.
    pub top_reasons: Vec<String>,
}

/// Recency multiplier for one frecency step: x4 under an hour, x2 under a
/// day, x0.5 under a week, x0.25 older. Unknown age is neutral (x1).
pub fn recency_weight(age_secs: Option<u64>) -> f64 {
    match age_secs {
        None => 1.0,
        Some(a) if a < 3600 => 4.0,
        Some(a) if a < 86_400 => 2.0,
        Some(a) if a < 7 * 86_400 => 0.5,
        Some(_) => 0.25,
    }
}

/// Common English words dropped from unigrams (lowercase, ASCII).
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "did", "do", "does",
    "for", "from", "had", "has", "have", "he", "her", "his", "i", "if", "in", "into", "is", "it",
    "its", "just", "like", "me", "more", "most", "my", "no", "not", "now", "of", "off", "on",
    "once", "only", "or", "other", "our", "out", "over", "so", "some", "such", "than", "that",
    "the", "their", "them", "then", "there", "these", "this", "to", "today", "too", "up", "us",
    "was", "we", "were", "what", "when", "where", "which", "who", "will", "with", "you", "your",
];

/// Lowercase word unigrams from sanitized text: ASCII alphanumeric runs of
/// length 2+, stopwords dropped. Deterministic.
pub fn tokenize(sanitized: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in sanitized.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_alphanumeric() {
            cur.push(c.to_ascii_lowercase());
        } else if !cur.is_empty() {
            if cur.len() >= 2 && !STOPWORDS.contains(&cur.as_str()) {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    out
}

/// Estimate thread length from post text: 1 plus one per numbered
/// continuation marker (`2/`, `2.`, `2)` at a line start after the first
/// line). Bookmarks carry no thread linkage, so this is a text heuristic;
/// plain posts estimate 1.
pub fn thread_len_estimate(text: &str) -> usize {
    let mut n = 1usize;
    for line in text.lines().skip(1) {
        let t = line.trim_start();
        let digits = t.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            if let Some(sep) = t.as_bytes().get(digits) {
                if matches!(sep, b'/' | b'.' | b')') {
                    n += 1;
                }
            }
        }
    }
    n
}

/// Thread-length bucket: single posts, short threads, long threads.
pub fn thread_bucket(len: usize) -> &'static str {
    match len {
        0 | 1 => "1",
        2 | 3 => "2-3",
        _ => "4+",
    }
}

/// True when the post links out: stored expanded URLs or a link in the text.
pub fn has_link(b: &Bookmark) -> bool {
    !b.urls.is_empty() || b.text.contains("http") || b.text.contains("www.")
}

struct Features {
    category: String,
    author: String,
    has_media: bool,
    has_link: bool,
    thread: String,
    words: Vec<String>,
}

fn features_of(b: &Bookmark) -> Features {
    let sanitized = crate::sanitize::sanitize_uncapped(&b.text);
    let words: BTreeSet<String> = tokenize(&sanitized).into_iter().collect();
    Features {
        category: crate::brief::topic_of(b),
        author: feedback::normalize_author(b.author_username.as_deref().unwrap_or("")),
        has_media: !b.media.is_empty(),
        has_link: has_link(b),
        thread: thread_bucket(thread_len_estimate(&b.text)).to_string(),
        words: words.into_iter().collect(),
    }
}

/// Minimum labeled records from the environment: [`MIN_RECORDS_DEFAULT`]
/// when unset, empty, or unparseable (fail closed to the default).
pub fn min_records_from_env() -> usize {
    std::env::var(MIN_RECORDS_ENV_VAR)
        .ok()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(MIN_RECORDS_DEFAULT)
}

/// Smoothed log-odds for one categorical value: Laplace (+1) over `k`
/// distinct values.
fn log_odds(p: u32, n_pos: usize, n: u32, n_neg: usize, k: f64) -> f64 {
    let a = (f64::from(p) + 1.0) / (n_pos as f64 + k);
    let b = (f64::from(n) + 1.0) / (n_neg as f64 + k);
    (a / b).ln()
}

/// Smoothed log-odds for one boolean feature value.
fn bool_log_odds(yes: bool, yes_counts: (u32, u32), n_pos: usize, n_neg: usize) -> f64 {
    let (p, n) = if yes {
        yes_counts
    } else {
        (
            (n_pos as u32).saturating_sub(yes_counts.0),
            (n_neg as u32).saturating_sub(yes_counts.1),
        )
    };
    log_odds(p, n_pos, n, n_neg, 2.0)
}

fn category_reason(cat: &str, p: u32, n: u32, contrib: f64) -> String {
    let t = p + n;
    if t == 0 {
        format!("no votes yet in {cat}")
    } else if contrib >= 0.0 {
        format!("you approved {p} of {t} in {cat}")
    } else {
        format!("you usually skip {cat}")
    }
}

fn author_reason(p: u32, n: u32, contrib: f64) -> String {
    if p + n == 0 {
        "new author for you".to_string()
    } else if contrib >= 0.0 {
        "author you often approve".to_string()
    } else {
        "author you often skip".to_string()
    }
}

fn media_reason(has: bool, contrib: f64) -> String {
    match (has, contrib >= 0.0) {
        (true, true) => "has media like posts you approve".to_string(),
        (true, false) => "has media like posts you skip".to_string(),
        (false, true) => "no media, like posts you approve".to_string(),
        (false, false) => "no media, like posts you skip".to_string(),
    }
}

fn link_reason(has: bool, contrib: f64) -> String {
    match (has, contrib >= 0.0) {
        (true, true) => "has a link like posts you approve".to_string(),
        (true, false) => "has a link like posts you skip".to_string(),
        (false, true) => "no link, like posts you approve".to_string(),
        (false, false) => "no link, like posts you skip".to_string(),
    }
}

fn thread_reason(bucket: &str, contrib: f64) -> String {
    let desc = match bucket {
        "1" => "single post",
        "2-3" => "short thread",
        _ => "long thread",
    };
    if contrib >= 0.0 {
        format!("{desc} like ones you approve")
    } else {
        format!("{desc} like ones you skip")
    }
}

fn word_reason(word: &str, contrib: f64) -> String {
    if contrib >= 0.0 {
        format!("mentions '{word}' which you often approve")
    } else {
        format!("mentions '{word}' which you often skip")
    }
}

/// Personal ranker: smoothed naive Bayes blended with frecency affinity.
/// Train with [`Ranker::train`] or [`Ranker::train_from_log`]; inactive
/// rankers score 0.0 with no reasons so callers keep today's order.
pub struct Ranker {
    active: bool,
    n_labeled: usize,
    min_records: usize,
    n_pos: usize,
    n_neg: usize,
    cats: BTreeMap<String, (u32, u32)>,
    authors: BTreeMap<String, (u32, u32)>,
    media_yes: (u32, u32),
    link_yes: (u32, u32),
    threads: BTreeMap<String, (u32, u32)>,
    word_pos: BTreeMap<String, u32>,
    word_neg: BTreeMap<String, u32>,
    total_pos_words: u32,
    total_neg_words: u32,
    vocab: BTreeSet<String>,
    author_aff: BTreeMap<String, f64>,
    cat_aff: BTreeMap<String, f64>,
}

impl Ranker {
    /// Train on explicit events. Deterministic: same events and clock give
    /// the same scores. Active only with at least `min_records` events (and
    /// at least one event no matter what).
    pub fn train(events: &[LabeledEvent], now_secs: u64, min_records: usize) -> Self {
        let n_labeled = events.len();
        let mut r = Self {
            active: n_labeled >= min_records && n_labeled > 0,
            n_labeled,
            min_records,
            n_pos: 0,
            n_neg: 0,
            cats: BTreeMap::new(),
            authors: BTreeMap::new(),
            media_yes: (0, 0),
            link_yes: (0, 0),
            threads: BTreeMap::new(),
            word_pos: BTreeMap::new(),
            word_neg: BTreeMap::new(),
            total_pos_words: 0,
            total_neg_words: 0,
            vocab: BTreeSet::new(),
            author_aff: BTreeMap::new(),
            cat_aff: BTreeMap::new(),
        };
        for e in events {
            let f = features_of(&e.bookmark);
            let pos = e.signal.positive();
            if pos {
                r.n_pos += 1;
            } else {
                r.n_neg += 1;
            }
            for (map, key) in [
                (&mut r.cats, f.category.clone()),
                (&mut r.authors, f.author.clone()),
                (&mut r.threads, f.thread.clone()),
            ] {
                let c = map.entry(key).or_insert((0, 0));
                if pos {
                    c.0 += 1;
                } else {
                    c.1 += 1;
                }
            }
            if f.has_media {
                if pos {
                    r.media_yes.0 += 1;
                } else {
                    r.media_yes.1 += 1;
                }
            }
            if f.has_link {
                if pos {
                    r.link_yes.0 += 1;
                } else {
                    r.link_yes.1 += 1;
                }
            }
            for w in &f.words {
                let m = if pos {
                    &mut r.word_pos
                } else {
                    &mut r.word_neg
                };
                *m.entry(w.clone()).or_insert(0) += 1;
            }
            let delta = e.signal.frecency_delta();
            if delta != 0.0 {
                let age = e.at.map(|t| now_secs.saturating_sub(t));
                let w = delta * recency_weight(age);
                *r.author_aff.entry(f.author.clone()).or_insert(0.0) += w;
                *r.cat_aff.entry(f.category.clone()).or_insert(0.0) += w;
            }
        }
        // Vocabulary: most frequent overall, ties alphabetical.
        let mut totals: BTreeMap<&str, u32> = BTreeMap::new();
        for (w, c) in r.word_pos.iter().chain(r.word_neg.iter()) {
            *totals.entry(w.as_str()).or_insert(0) += c;
        }
        let mut items: Vec<(&str, u32)> = totals.into_iter().collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        r.vocab = items
            .into_iter()
            .take(VOCAB_CAP)
            .map(|(w, _)| w.to_string())
            .collect();
        for w in &r.vocab {
            r.total_pos_words += r.word_pos.get(w).copied().unwrap_or(0);
            r.total_neg_words += r.word_neg.get(w).copied().unwrap_or(0);
        }
        r
    }

    /// Train on the verdicts in a decision log, resolving voted post ids
    /// against `bookmarks`. Records that resolve to no post (unknown ids,
    /// author-hide verdicts whose id is a username) contribute nothing; the
    /// minimum comes from the environment (see [`min_records_from_env`]).
    pub fn train_from_log(
        log: &DecisionLog,
        bookmarks: &[Bookmark],
        now_secs: u64,
    ) -> Result<Self, String> {
        let events = events_from_log(log, bookmarks)?;
        Ok(Self::train(&events, now_secs, min_records_from_env()))
    }

    /// True when trained on enough labeled records to re-rank.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Labeled events actually trained on.
    pub fn n_labeled(&self) -> usize {
        self.n_labeled
    }

    /// Minimum labeled records this ranker was trained with.
    pub fn min_records(&self) -> usize {
        self.min_records
    }

    /// Score one post. Inactive rankers return 0.0 with no reasons.
    pub fn score(&self, bookmark: &Bookmark) -> RankScore {
        if !self.active {
            return RankScore {
                score: 0.0,
                top_reasons: Vec::new(),
            };
        }
        let f = features_of(bookmark);
        let mut total = ((self.n_pos + 1) as f64 / (self.n_neg + 1) as f64).ln();
        let mut parts: Vec<(f64, String)> = Vec::new();
        let (cp, cn) = self.cats.get(&f.category).copied().unwrap_or((0, 0));
        let c = log_odds(
            cp,
            self.n_pos,
            cn,
            self.n_neg,
            self.cats.len().max(1) as f64,
        );
        total += c;
        parts.push((c, category_reason(&f.category, cp, cn, c)));
        let (ap, an) = self.authors.get(&f.author).copied().unwrap_or((0, 0));
        let c = log_odds(
            ap,
            self.n_pos,
            an,
            self.n_neg,
            self.authors.len().max(1) as f64,
        );
        total += c;
        parts.push((c, author_reason(ap, an, c)));
        let c = bool_log_odds(f.has_media, self.media_yes, self.n_pos, self.n_neg);
        total += c;
        parts.push((c, media_reason(f.has_media, c)));
        let c = bool_log_odds(f.has_link, self.link_yes, self.n_pos, self.n_neg);
        total += c;
        parts.push((c, link_reason(f.has_link, c)));
        let (tp, tn) = self.threads.get(&f.thread).copied().unwrap_or((0, 0));
        let c = log_odds(
            tp,
            self.n_pos,
            tn,
            self.n_neg,
            self.threads.len().max(1) as f64,
        );
        total += c;
        parts.push((c, thread_reason(&f.thread, c)));
        let v = self.vocab.len().max(1) as f64;
        let denom_pos = self.total_pos_words as f64 + v;
        let denom_neg = self.total_neg_words as f64 + v;
        let mut best_word: Option<(f64, &str)> = None;
        for w in &f.words {
            if !self.vocab.contains(w) {
                continue;
            }
            let cp = f64::from(self.word_pos.get(w).copied().unwrap_or(0));
            let cn = f64::from(self.word_neg.get(w).copied().unwrap_or(0));
            let c = (((cp + 1.0) / denom_pos) / ((cn + 1.0) / denom_neg)).ln();
            total += c;
            if best_word.is_none_or(|(b, _)| c > b) {
                best_word = Some((c, w));
            }
        }
        if let Some((c, w)) = best_word {
            parts.push((c, word_reason(w, c)));
        }
        let fa = self.author_aff.get(&f.author).copied().unwrap_or(0.0);
        if fa != 0.0 {
            let c = FRECENCY_BLEND * fa;
            total += c;
            parts.push((
                c,
                if fa > 0.0 {
                    "you recently approved more by this author".to_string()
                } else {
                    "you recently skipped more by this author".to_string()
                },
            ));
        }
        let fc = self.cat_aff.get(&f.category).copied().unwrap_or(0.0);
        if fc != 0.0 {
            let c = FRECENCY_BLEND * fc;
            total += c;
            parts.push((
                c,
                if fc > 0.0 {
                    format!("you recently approved more in {}", f.category)
                } else {
                    format!("you recently skipped more in {}", f.category)
                },
            ));
        }
        parts.sort_by(|a, b| b.0.total_cmp(&a.0));
        RankScore {
            score: total,
            top_reasons: parts.into_iter().take(3).map(|(_, s)| s).collect(),
        }
    }

    /// Score several posts into a `post_id -> score` map.
    pub fn score_all<'a, I>(&self, bookmarks: I) -> HashMap<String, f64>
    where
        I: IntoIterator<Item = &'a Bookmark>,
    {
        bookmarks
            .into_iter()
            .map(|b| (b.post_id.clone(), self.score(b).score))
            .collect()
    }
}

/// Read labeled events out of a decision log: feedback verdicts plus
/// `tried yes` follow-ups, resolved against `bookmarks` by post id.
/// `UnhideAuthor`, tried no/later, malformed follow-ups, and ids missing
/// from `bookmarks` are ignored. Log order is preserved.
pub fn events_from_log(
    log: &DecisionLog,
    bookmarks: &[Bookmark],
) -> Result<Vec<LabeledEvent>, String> {
    let entries = log.verify_or_empty().map_err(|e| e.to_string())?;
    let by_id: HashMap<&str, &Bookmark> =
        bookmarks.iter().map(|b| (b.post_id.as_str(), b)).collect();
    let mut out = Vec::new();
    for e in &entries {
        if e.kind == feedback::FEEDBACK_KIND {
            let rec: FeedbackRecord =
                serde_json::from_value(e.payload.clone()).map_err(|err| err.to_string())?;
            let signal = match rec.verdict {
                Verdict::Approve => Some(Signal::Approve),
                Verdict::Delegate => Some(Signal::Delegate),
                Verdict::Skip => Some(Signal::Skip),
                Verdict::NeverPost => Some(Signal::NeverPost),
                Verdict::NeverCategory => Some(Signal::NeverCategory),
                Verdict::NeverAuthor => Some(Signal::NeverAuthor),
                Verdict::UnhideAuthor => None,
            };
            if let (Some(s), Some(b)) = (signal, by_id.get(rec.pick_id.as_str())) {
                out.push(LabeledEvent {
                    bookmark: (*b).clone(),
                    signal: s,
                    at: None,
                });
            }
        } else if e.kind == crate::try_it::FOLLOWUP_KIND {
            let id = e
                .payload
                .get("post_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let tried = e
                .payload
                .get("tried")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if tried == crate::try_it::Tried::Yes.as_str() {
                if let Some(b) = by_id.get(id) {
                    out.push(LabeledEvent {
                        bookmark: (*b).clone(),
                        signal: Signal::TriedYes,
                        at: None,
                    });
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(id: &str, topic: Option<&str>, author: Option<&str>, text: &str) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: text.into(),
            author_username: author.map(str::to_string),
            created_at: None,
            folders: None,
            label: topic.map(|t| crate::fixture::Label {
                parent_label: Some(t.into()),
                confidence: Some(0.9),
                method: None,
                is_foldered: None,
            }),
            had_note_tweet: false,
            media: Vec::new(),
            lang: Some("en".to_string()),
            urls: Vec::new(),
        }
    }

    fn ev(b: Bookmark, signal: Signal) -> LabeledEvent {
        LabeledEvent {
            bookmark: b,
            signal,
            at: None,
        }
    }

    fn ev_at(b: Bookmark, signal: Signal, at: u64) -> LabeledEvent {
        LabeledEvent {
            bookmark: b,
            signal,
            at: Some(at),
        }
    }

    /// 12 events: 5 Rust approvals, 5 Markets skips, 2 delegate approvals.
    fn training_set() -> Vec<LabeledEvent> {
        let mut out = Vec::new();
        for i in 0..5 {
            out.push(ev(
                bm(
                    &format!("rust-{i}"),
                    Some("Rust & Systems"),
                    Some("syn_rustacean"),
                    "synthetic async borrow checker ownership post",
                ),
                Signal::Approve,
            ));
        }
        for i in 0..5 {
            out.push(ev(
                bm(
                    &format!("mkt-{i}"),
                    Some("Markets & Finance"),
                    Some("syn_ticker"),
                    "synthetic quarterly earnings revenue guidance post",
                ),
                Signal::Skip,
            ));
        }
        for i in 0..2 {
            out.push(ev(
                bm(
                    &format!("dlg-{i}"),
                    Some("Rust & Systems"),
                    Some("syn_other"),
                    "synthetic ownership lifetimes post",
                ),
                Signal::Delegate,
            ));
        }
        out
    }

    #[test]
    fn scores_are_deterministic() {
        let events = training_set();
        let a = Ranker::train(&events, 0, 10);
        let b = Ranker::train(&events, 0, 10);
        assert!(a.is_active());
        let probe = bm(
            "probe",
            Some("Rust & Systems"),
            Some("syn_rustacean"),
            "synthetic async ownership post",
        );
        let sa = a.score(&probe);
        let sb = b.score(&probe);
        assert_eq!(sa, sb);
        assert_eq!(a.score(&probe), sa, "repeat scoring is identical");
        assert!(sa.score.is_finite());
    }

    #[test]
    fn inactive_under_min_records_scores_zero_without_reasons() {
        let events = training_set();
        let r = Ranker::train(&events[..9], 0, 10);
        assert!(!r.is_active());
        assert_eq!(r.n_labeled(), 9);
        let probe = bm(
            "probe",
            Some("Rust & Systems"),
            Some("syn_rustacean"),
            "synthetic async ownership post",
        );
        assert_eq!(
            r.score(&probe),
            RankScore {
                score: 0.0,
                top_reasons: Vec::new(),
            }
        );
        // At the boundary the ranker activates.
        let r = Ranker::train(&events[..10], 0, 10);
        assert!(r.is_active());
    }

    #[test]
    fn min_records_needs_at_least_one_event() {
        let r = Ranker::train(&[], 0, 0);
        assert!(!r.is_active(), "no data means inactive even with min 0");
    }

    #[test]
    fn ordering_changes_after_votes() {
        let rust = bm(
            "new-rust",
            Some("Rust & Systems"),
            Some("syn_new"),
            "synthetic async borrow checker post",
        );
        let mkt = bm(
            "new-mkt",
            Some("Markets & Finance"),
            Some("syn_new"),
            "synthetic quarterly earnings post",
        );
        // Trained to like Rust: Rust outranks Markets.
        let r = Ranker::train(&training_set(), 0, 10);
        assert!(r.score(&rust).score > r.score(&mkt).score);
        // Trained the other way round: Markets outranks Rust.
        let mut flipped = Vec::new();
        for e in training_set() {
            let signal = if e.signal.positive() {
                Signal::Skip
            } else {
                Signal::Approve
            };
            flipped.push(LabeledEvent {
                bookmark: e.bookmark,
                signal,
                at: e.at,
            });
        }
        let r2 = Ranker::train(&flipped, 0, 10);
        assert!(r2.score(&mkt).score > r2.score(&rust).score);
    }

    #[test]
    fn reasons_are_human_readable_plain_words() {
        let r = Ranker::train(&training_set(), 0, 10);
        let probe = bm(
            "probe",
            Some("Rust & Systems"),
            Some("syn_rustacean"),
            "synthetic async ownership post",
        );
        let reasons = r.score(&probe).top_reasons;
        assert_eq!(reasons.len(), 3);
        assert!(
            reasons
                .iter()
                .any(|s| s == "you approved 7 of 7 in Rust & Systems"),
            "{reasons:?}"
        );
        assert!(
            reasons.iter().any(|s| s == "author you often approve"),
            "{reasons:?}"
        );
        for s in &reasons {
            assert!(!s.is_empty());
            assert!(
                !s.contains(['{', '}', '"', '[', ']']),
                "no debug formatting: {s}"
            );
            assert!(s.contains(' '), "plain words: {s}");
        }
    }

    #[test]
    fn recency_weight_buckets() {
        assert_eq!(recency_weight(None), 1.0);
        assert_eq!(recency_weight(Some(0)), 4.0);
        assert_eq!(recency_weight(Some(3599)), 4.0);
        assert_eq!(recency_weight(Some(3600)), 2.0);
        assert_eq!(recency_weight(Some(86_399)), 2.0);
        assert_eq!(recency_weight(Some(86_400)), 0.5);
        assert_eq!(recency_weight(Some(7 * 86_400 - 1)), 0.5);
        assert_eq!(recency_weight(Some(7 * 86_400)), 0.25);
        assert_eq!(recency_weight(Some(u64::MAX)), 0.25);
    }

    #[test]
    fn recent_approve_outranks_old_approve_via_frecency() {
        let mk = |id: &str, author: &str| {
            bm(
                id,
                Some("Same Topic"),
                Some(author),
                "synthetic identical neutral wording here",
            )
        };
        // Same topic and text shape; only the vote age differs.
        let mut events = Vec::new();
        for i in 0..5 {
            events.push(ev_at(
                mk(&format!("old-{i}"), "syn_old"),
                Signal::Approve,
                1000,
            ));
            events.push(ev_at(
                mk(&format!("new-{i}"), "syn_new"),
                Signal::Approve,
                200_000,
            ));
        }
        let r = Ranker::train(&events, 200_100, 10);
        assert!(r.is_active());
        let old_post = mk("probe-old", "syn_old");
        let new_post = mk("probe-new", "syn_new");
        assert!(
            r.score(&new_post).score > r.score(&old_post).score,
            "recent author affinity must win: {} vs {}",
            r.score(&new_post).score,
            r.score(&old_post).score
        );
    }

    #[test]
    fn skip_decays_frecency_for_author() {
        let mk = |id: &str, author: &str| {
            bm(
                id,
                Some("Same Topic"),
                Some(author),
                "synthetic identical neutral wording here",
            )
        };
        let mut events = Vec::new();
        for i in 0..5 {
            events.push(ev(mk(&format!("good-{i}"), "syn_good"), Signal::Approve));
            events.push(ev(mk(&format!("bad-{i}"), "syn_bad"), Signal::Skip));
        }
        let r = Ranker::train(&events, 0, 10);
        assert!(r.score(&mk("p-good", "syn_good")).score > r.score(&mk("p-bad", "syn_bad")).score);
    }

    #[test]
    fn tokenize_lowercases_drops_stopwords_and_shorts() {
        assert_eq!(
            tokenize("The Borrow Checker and async RUNTIME 7 x"),
            vec!["borrow", "checker", "async", "runtime"]
        );
        assert!(tokenize("the a an of to").is_empty());
        assert_eq!(tokenize("don't stop"), vec!["don", "stop"]);
    }

    #[test]
    fn thread_buckets_cover_singles_and_threads() {
        assert_eq!(thread_bucket(thread_len_estimate("just one post")), "1");
        assert_eq!(
            thread_bucket(thread_len_estimate("first post\n2/ second post")),
            "2-3"
        );
        assert_eq!(
            thread_bucket(thread_len_estimate("a\n2. b\n3) c\n4/ d")),
            "4+"
        );
        // Numbers mid-line are not thread markers.
        assert_eq!(
            thread_bucket(thread_len_estimate("top 10 tips\nsave 15%")),
            "1"
        );
    }

    #[test]
    fn min_records_from_env_defaults_and_overrides() {
        let _guard = crate::test_support::lock_env();
        let saved = std::env::var(MIN_RECORDS_ENV_VAR).ok();
        std::env::remove_var(MIN_RECORDS_ENV_VAR);
        assert_eq!(min_records_from_env(), MIN_RECORDS_DEFAULT);
        std::env::set_var(MIN_RECORDS_ENV_VAR, "");
        assert_eq!(min_records_from_env(), MIN_RECORDS_DEFAULT);
        std::env::set_var(MIN_RECORDS_ENV_VAR, "3");
        assert_eq!(min_records_from_env(), 3);
        std::env::set_var(MIN_RECORDS_ENV_VAR, "many");
        assert_eq!(min_records_from_env(), MIN_RECORDS_DEFAULT);
        match saved {
            Some(v) => std::env::set_var(MIN_RECORDS_ENV_VAR, v),
            None => std::env::remove_var(MIN_RECORDS_ENV_VAR),
        }
    }

    #[test]
    fn events_from_log_maps_verdicts_and_tried_yes() {
        let _guard = crate::test_support::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&log_path).unwrap();
        let mut bookmarks = Vec::new();
        for (id, topic) in [
            ("p-approve", "Rust & Systems"),
            ("p-delegate", "Rust & Systems"),
            ("p-tried", "Rust & Systems"),
            ("p-skip", "Markets & Finance"),
            ("p-never-post", "Markets & Finance"),
            ("p-never-cat", "Markets & Finance"),
        ] {
            bookmarks.push(bm(id, Some(topic), Some("syn_user"), "synthetic post"));
        }
        let policy = ancientos_core::feedback::ThresholdPolicy::default();
        for (id, cat, verdict) in [
            ("p-approve", "Rust & Systems", Verdict::Approve),
            ("p-delegate", "Rust & Systems", Verdict::Delegate),
            ("p-skip", "Markets & Finance", Verdict::Skip),
            ("p-never-post", "Markets & Finance", Verdict::NeverPost),
            ("p-never-cat", "Markets & Finance", Verdict::NeverCategory),
            // Author-hide verdicts carry a username, not a post id.
            ("syn_user", "Markets & Finance", Verdict::NeverAuthor),
            ("syn_user", "unknown", Verdict::UnhideAuthor),
            // Unknown post ids train nothing.
            ("ghost", "Rust & Systems", Verdict::Approve),
        ] {
            ancientos_core::feedback::record(
                &mut log,
                FeedbackRecord {
                    pick_id: id.into(),
                    category: cat.into(),
                    verdict,
                    tags: Vec::new(),
                },
                policy,
            )
            .unwrap();
        }
        crate::try_it::record_followup(&mut log, "p-tried", crate::try_it::Tried::Yes, None)
            .unwrap();
        crate::try_it::record_followup(&mut log, "p-tried", crate::try_it::Tried::No, None)
            .unwrap();
        crate::try_it::record_followup(&mut log, "ghost", crate::try_it::Tried::Yes, None).unwrap();
        drop(log);
        let log = DecisionLog::open(&log_path).unwrap();
        let events = events_from_log(&log, &bookmarks).unwrap();
        let got: Vec<(String, Signal)> = events
            .iter()
            .map(|e| (e.bookmark.post_id.clone(), e.signal))
            .collect();
        assert_eq!(
            got,
            vec![
                ("p-approve".to_string(), Signal::Approve),
                ("p-delegate".to_string(), Signal::Delegate),
                ("p-skip".to_string(), Signal::Skip),
                ("p-never-post".to_string(), Signal::NeverPost),
                ("p-never-cat".to_string(), Signal::NeverCategory),
                ("p-tried".to_string(), Signal::TriedYes),
            ]
        );
        assert!(events.iter().all(|e| e.at.is_none()));
        let pos = events.iter().filter(|e| e.signal.positive()).count();
        assert_eq!(pos, 3);
        // Trains end to end with a low minimum.
        let saved = std::env::var(MIN_RECORDS_ENV_VAR).ok();
        std::env::set_var(MIN_RECORDS_ENV_VAR, "6");
        let r = Ranker::train_from_log(&log, &bookmarks, 0).unwrap();
        assert!(r.is_active());
        match saved {
            Some(v) => std::env::set_var(MIN_RECORDS_ENV_VAR, v),
            None => std::env::remove_var(MIN_RECORDS_ENV_VAR),
        }
    }
}
