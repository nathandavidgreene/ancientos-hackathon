//! "Try it" tag for hands-on content (stretching, physio, mobility,
//! meditation, breathing, exercise, recipes-to-practice).
//!
//! Deterministic local classifier (no network): the topic from
//! [`brief::topic_of`](crate::brief::topic_of) plus keyword/imperative cues
//! in the full text. A post is tagged when it carries an explicit try-it cue,
//! when a hands-on topic pairs with any cue, or when two distinct cue types
//! fire. Follow-ups (`tried yes|no|later`) and the pending queue live in the
//! append-only decision log as metadata only.
use crate::fixture::Bookmark;
use ancientos_core::log::{DecisionLog, Entry};
use std::collections::BTreeSet;

/// Log kind appended when the brief renders a tagged pick.
pub const SHOWN_KIND: &str = "try_it.shown";
/// Log kind appended by [`record_followup`].
pub const FOLLOWUP_KIND: &str = "try_it.followup";
/// Maximum steps returned by [`extract_steps`].
pub const MAX_STEPS: usize = 8;
/// Maximum note characters stored by [`record_followup`].
pub const MAX_NOTE_CHARS: usize = 280;
/// Maximum characters kept per extracted step.
pub const MAX_STEP_CHARS: usize = 140;

/// Tag attached to a hands-on post, with the human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TryItTag {
    pub reason: String,
}

/// Answer to "did you try it?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tried {
    Yes,
    No,
    Later,
}

impl Tried {
    /// Canonical lowercase spelling, matching the CLI.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Later => "later",
        }
    }
}

impl std::str::FromStr for Tried {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "yes" => Ok(Self::Yes),
            "no" => Ok(Self::No),
            "later" => Ok(Self::Later),
            other => Err(format!("unknown tried value: {other}")),
        }
    }
}

/// True when the bookmark is hands-on content. See [`classify`].
pub fn is_try_it(b: &Bookmark) -> bool {
    classify(b).is_some()
}

/// Classify a bookmark, returning the tag with its reason, or `None`.
pub fn classify(b: &Bookmark) -> Option<TryItTag> {
    classify_text(&crate::brief::topic_of(b), &b.text)
}

/// Classify from a topic and full text (used by the brief, which renders
/// hydrated/expanded text rather than the stored excerpt).
pub fn classify_text(topic: &str, text: &str) -> Option<TryItTag> {
    let cues = detect_cues(text);
    if cues.is_empty() {
        return None;
    }
    let explicit = cues.contains(&CUE_EXPLICIT);
    let tagged = explicit || topic_is_hands_on(topic) || cues.len() >= 2;
    if !tagged {
        return None;
    }
    Some(TryItTag {
        reason: format!("{}; topic '{topic}'", cues.join(", ")),
    })
}

/// Extract up to [`MAX_STEPS`] practice steps: numbered or bulleted steps
/// first, otherwise imperative sentences. Empty when nothing actionable.
pub fn extract_steps(text: &str) -> Vec<String> {
    let numbered = split_numbered(text);
    if !numbered.is_empty() {
        return numbered;
    }
    let bullets = split_bullets(text);
    if !bullets.is_empty() {
        return bullets;
    }
    let mut out = Vec::new();
    for fragment in text.split(&['.', '!', '?', ';', ':', '\n'][..]) {
        if is_imperative(fragment) {
            if let Some(step) = clean_step(fragment) {
                out.push(step);
                if out.len() >= MAX_STEPS {
                    break;
                }
            }
        }
    }
    out
}

/// Record a `tried yes|no|later` follow-up. Metadata only: the note is
/// sanitized (no URLs, handles, emails, phones) and length-capped.
pub fn record_followup(
    log: &mut DecisionLog,
    post_id: &str,
    tried: Tried,
    note: Option<&str>,
) -> Result<Entry, String> {
    if post_id.trim().is_empty() {
        return Err("post id must be non-empty".to_string());
    }
    let mut payload = serde_json::json!({
        "post_id": post_id,
        "tried": tried.as_str(),
    });
    if let Some(raw) = note {
        let clean = clean_note(raw);
        if !clean.is_empty() {
            payload["note"] = serde_json::json!(clean);
        }
    }
    log.append(FOLLOWUP_KIND, payload)
        .map_err(|e| e.to_string())
}

/// Record a follow-up in the decision log at `log_path` (creating parent
/// directories), for the `tried` CLI command.
pub fn run_tried(
    post_id: &str,
    tried: Tried,
    note: Option<&str>,
    log_path: &std::path::Path,
) -> Result<Entry, String> {
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent)
                .map_err(|e| format!("log dir failed: {e}"))?;
        }
    }
    let mut log = DecisionLog::open(log_path).map_err(|e| e.to_string())?;
    record_followup(&mut log, post_id, tried, note)
}

/// Posts shown with the tag ([`SHOWN_KIND`]) that have no `yes`/`no`
/// follow-up yet, in first-shown order. A `later` follow-up keeps the post
/// pending. Unreadable logs fail closed to an empty queue.
pub fn pending_followups(log: &DecisionLog) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut shown = Vec::new();
    let mut resolved = BTreeSet::new();
    for entry in log.verify_or_empty().unwrap_or_default() {
        if entry.kind == SHOWN_KIND {
            if let Some(id) = entry.payload.get("post_id").and_then(|v| v.as_str()) {
                if !id.is_empty() && seen.insert(id.to_string()) {
                    shown.push(id.to_string());
                }
            }
        } else if entry.kind == FOLLOWUP_KIND {
            let id = entry.payload.get("post_id").and_then(|v| v.as_str());
            let tried = entry.payload.get("tried").and_then(|v| v.as_str());
            if let (Some(id), Some(t)) = (id, tried) {
                if t == Tried::Yes.as_str() || t == Tried::No.as_str() {
                    resolved.insert(id.to_string());
                }
            }
        }
    }
    shown
        .into_iter()
        .filter(|id| !resolved.contains(id))
        .collect()
}

fn clean_note(note: &str) -> String {
    crate::sanitize::sanitize_uncapped(note)
        .chars()
        .take(MAX_NOTE_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

const CUE_EXPLICIT: &str = "explicit try-it";
const CUE_NUMBERED: &str = "numbered steps";
const CUE_BULLETS: &str = "bulleted steps";
const CUE_BREATH: &str = "breathing cue";
const CUE_HOLD: &str = "hold cue";
const CUE_REPS: &str = "reps/sets";
const CUE_DURATION: &str = "duration";
const CUE_SIDE: &str = "each side";
const CUE_MINDFUL: &str = "mindfulness cue";
const CUE_INGREDIENTS: &str = "ingredients";
const CUE_IMPERATIVE: &str = "imperative";

/// Hands-on topics (matched case-insensitively as substrings).
const HANDS_ON_TOPICS: &[&str] = &[
    "health",
    "wellness",
    "fitness",
    "exercise",
    "workout",
    "mobility",
    "physio",
    "stretch",
    "yoga",
    "pilat",
    "meditat",
    "mindful",
    "breath",
    "recipe",
    "nutrition",
    "cook",
];

fn topic_is_hands_on(topic: &str) -> bool {
    let lower = topic.to_lowercase();
    HANDS_ON_TOPICS.iter().any(|k| lower.contains(k))
}

/// All cue types present in the text, in fixed detector order.
fn detect_cues(text: &str) -> Vec<&'static str> {
    let lower = text.to_lowercase();
    let mut cues = Vec::new();
    if has_explicit_try(&lower) {
        cues.push(CUE_EXPLICIT);
    }
    if numbered_spans(&text.chars().collect::<Vec<_>>()).len() >= 2 {
        cues.push(CUE_NUMBERED);
    }
    if has_bullets(text) {
        cues.push(CUE_BULLETS);
    }
    if has_any_phrase(
        &lower,
        &[
            "breathe in",
            "breathe out",
            "breathe",
            "breathing",
            "breathwork",
            "box breathing",
            "inhale",
            "exhale",
            "deep breath",
        ],
    ) {
        cues.push(CUE_BREATH);
    }
    if has_hold_cue(&lower) {
        cues.push(CUE_HOLD);
    }
    if has_reps_cue(&lower) {
        cues.push(CUE_REPS);
    }
    if has_duration_cue(&lower) {
        cues.push(CUE_DURATION);
    }
    if contains_phrase(&lower, "each side") {
        cues.push(CUE_SIDE);
    }
    if has_any_phrase(
        &lower,
        &[
            "close your eyes",
            "focus on your breath",
            "body scan",
            "mindful",
        ],
    ) {
        cues.push(CUE_MINDFUL);
    }
    if contains_phrase(&lower, "ingredients") {
        cues.push(CUE_INGREDIENTS);
    }
    if has_imperative(text) {
        cues.push(CUE_IMPERATIVE);
    }
    cues
}

/// True when `phrase` appears with non-alphanumeric boundaries (so "pastry
/// item" does not match "try it"). Both sides must already be lowercased.
fn contains_phrase(hay: &str, phrase: &str) -> bool {
    let h = hay.as_bytes();
    let p = phrase.as_bytes();
    if p.is_empty() || h.len() < p.len() {
        return false;
    }
    for i in 0..=(h.len() - p.len()) {
        if h[i..i + p.len()] == p[..] {
            let left_ok = i == 0 || !h[i - 1].is_ascii_alphanumeric();
            let right_ok = i + p.len() == h.len() || !h[i + p.len()].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

fn has_any_phrase(hay: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|p| contains_phrase(hay, p))
}

fn has_explicit_try(lower: &str) -> bool {
    has_any_phrase(
        lower,
        &["try it", "try this", "give it a try", "give this a try"],
    )
}

fn has_bullets(text: &str) -> bool {
    text.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with("- ") || t.starts_with("* ") || t.starts_with("• ")
    })
}

/// "hold for ..." or "hold <number>" ("hold 4s", "hold 30 seconds").
fn has_hold_cue(lower: &str) -> bool {
    if contains_phrase(lower, "hold for") {
        return true;
    }
    let h = lower.as_bytes();
    let mut i = 0;
    while i + 4 <= h.len() {
        if &h[i..i + 4] == b"hold"
            && (i == 0 || !h[i - 1].is_ascii_alphanumeric())
            && (i + 4 == h.len() || !h[i + 4].is_ascii_alphanumeric())
        {
            let mut j = i + 4;
            while j < h.len() && h[j] == b' ' {
                j += 1;
            }
            if j < h.len() && h[j].is_ascii_digit() {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Words after `Nx` that mean a multiplier, never exercise reps.
const NX_COMPARISONS: &[&str] = &[
    "faster",
    "slower",
    "better",
    "best",
    "cheaper",
    "engineer",
    "engineers",
];

/// `10x`, `3 sets of 10`, `4 rounds`, `repeat 4`, `3 times`.
fn has_reps_cue(lower: &str) -> bool {
    has_nx_reps(lower) || has_reps_words(lower) || has_repeat_n(lower) || has_n_times(lower)
}

/// Digits directly followed by `x` ("10x"), excluding comparatives.
fn has_nx_reps(lower: &str) -> bool {
    let h = lower.as_bytes();
    let mut i = 0;
    while i < h.len() {
        if h[i].is_ascii_digit() {
            let mut j = i;
            while j < h.len() && h[j].is_ascii_digit() {
                j += 1;
            }
            if j < h.len()
                && h[j] == b'x'
                && (j + 1 == h.len() || !h[j + 1].is_ascii_alphanumeric())
            {
                let mut k = j + 1;
                while k < h.len() && h[k] == b' ' {
                    k += 1;
                }
                let mut end = k;
                while end < h.len() && h[end].is_ascii_alphanumeric() {
                    end += 1;
                }
                if !NX_COMPARISONS.contains(&&lower[k..end]) {
                    return true;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// `<number> reps|sets|rounds`.
fn has_reps_words(lower: &str) -> bool {
    let h = lower.as_bytes();
    let mut i = 0;
    while i < h.len() {
        if h[i].is_ascii_digit() {
            let mut j = i;
            while j < h.len() && h[j].is_ascii_digit() {
                j += 1;
            }
            let mut k = j;
            while k < h.len() && h[k] == b' ' {
                k += 1;
            }
            for word in ["reps", "rep", "sets", "set", "rounds", "round"] {
                if lower[k..].starts_with(word) {
                    let after = k + word.len();
                    if after == h.len() || !h[after].is_ascii_alphanumeric() {
                        return true;
                    }
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// `repeat <number>` ("Repeat 4 rounds").
fn has_repeat_n(lower: &str) -> bool {
    let h = lower.as_bytes();
    let mut i = 0;
    while i + 6 <= h.len() {
        if &h[i..i + 6] == b"repeat"
            && (i == 0 || !h[i - 1].is_ascii_alphanumeric())
            && (i + 6 == h.len() || !h[i + 6].is_ascii_alphanumeric())
        {
            let mut j = i + 6;
            while j < h.len() && h[j] == b' ' {
                j += 1;
            }
            if j < h.len() && h[j].is_ascii_digit() {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `<number> times` ("repeat 3 times a day" is caught by duration/repeat;
/// this covers bare counts).
fn has_n_times(lower: &str) -> bool {
    let h = lower.as_bytes();
    let mut i = 0;
    while i < h.len() {
        if h[i].is_ascii_digit() {
            let mut j = i;
            while j < h.len() && h[j].is_ascii_digit() {
                j += 1;
            }
            let mut k = j;
            while k < h.len() && h[k] == b' ' {
                k += 1;
            }
            if lower[k..].starts_with("times") {
                let after = k + "times".len();
                if after == h.len() || !h[after].is_ascii_alphanumeric() {
                    return true;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// `<number>[ ]<unit>` ("20s", "30 seconds", "10 min").
fn has_duration_cue(lower: &str) -> bool {
    const UNITS: &[&str] = &[
        "seconds", "second", "secs", "sec", "mins", "minutes", "minute", "min", "s",
    ];
    let h = lower.as_bytes();
    let mut i = 0;
    while i < h.len() {
        if h[i].is_ascii_digit() {
            let mut j = i;
            while j < h.len() && h[j].is_ascii_digit() {
                j += 1;
            }
            let mut k = j;
            while k < h.len() && h[k] == b' ' {
                k += 1;
            }
            for unit in UNITS {
                if lower[k..].starts_with(unit) {
                    let after = k + unit.len();
                    if after == h.len() || !h[after].is_ascii_alphanumeric() {
                        return true;
                    }
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// Verbs that start an instruction ("Roll your shoulders", "Mix and bake").
const IMPERATIVE_VERBS: &[&str] = &[
    "roll",
    "tilt",
    "hold",
    "breathe",
    "inhale",
    "exhale",
    "stretch",
    "repeat",
    "twist",
    "bend",
    "sit",
    "stand",
    "lie",
    "place",
    "put",
    "take",
    "start",
    "begin",
    "relax",
    "focus",
    "close",
    "open",
    "lift",
    "lower",
    "extend",
    "rotate",
    "press",
    "pull",
    "push",
    "walk",
    "run",
    "touch",
    "reach",
    "drop",
    "raise",
    "squeeze",
    "release",
    "try",
    "do",
    "follow",
    "practice",
    "step",
    "mix",
    "add",
    "chop",
    "stir",
    "bake",
    "cook",
    "simmer",
    "preheat",
    "serve",
    "combine",
    "pour",
    "slice",
    "whisk",
    "soak",
    "rinse",
    "drink",
    "rest",
    "alternate",
    "plank",
    "squat",
    "lunge",
    "kneel",
    "hinge",
    "tuck",
    "engage",
    "lengthen",
    "round",
    "arch",
    "fold",
];

fn word_key(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// True when the fragment starts with an imperative verb (a leading
/// "please" is skipped).
fn is_imperative(fragment: &str) -> bool {
    let mut words = fragment.split_whitespace();
    let Some(raw) = words.next() else {
        return false;
    };
    let mut first = word_key(raw);
    if first == "please" {
        let Some(raw) = words.next() else {
            return false;
        };
        first = word_key(raw);
    }
    IMPERATIVE_VERBS.contains(&first.as_str())
}

/// True when any sentence-like fragment or numbered step starts imperative.
fn has_imperative(text: &str) -> bool {
    if text
        .split(&['.', '!', '?', ';', ':', '\n'][..])
        .any(is_imperative)
    {
        return true;
    }
    split_numbered(text).iter().any(|s| is_imperative(s))
}

/// Extend a marker back over a leading "Step " so "Step 1: ..." splits
/// before "Step" rather than mid-phrase.
fn step_aware_begin(chars: &[char], at: usize) -> usize {
    if at >= 5 {
        let word: String = chars[at - 5..at].iter().collect();
        if word.eq_ignore_ascii_case("step ") {
            return at - 5;
        }
    }
    at
}

/// `(marker_begin, content_start)` char indices for `1)`, `2.`, `3:`, `(4)`.
/// Excludes `1/3` thread counters, decimals (`v2.0`), and clock times.
fn numbered_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let closed = j > i + 1 && j < chars.len() && chars[j] == ')';
            let right_ok = closed && (j + 1 >= chars.len() || chars[j + 1].is_whitespace());
            let left_ok = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
            if right_ok && left_ok {
                let mut k = j + 1;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                spans.push((step_aware_begin(chars, i), k));
                i = k;
                continue;
            }
            i += 1;
            continue;
        }
        let starts_number = chars[i].is_ascii_digit()
            && (i == 0 || (!chars[i - 1].is_ascii_alphanumeric() && chars[i - 1] != '/'));
        if starts_number {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j < chars.len() && matches!(chars[j], ')' | '.' | ':') {
                let after = j + 1;
                let is_decimal =
                    chars[j] == '.' && after < chars.len() && chars[after].is_ascii_digit();
                if !is_decimal && (after >= chars.len() || chars[after].is_whitespace()) {
                    let mut k = after;
                    while k < chars.len() && chars[k].is_whitespace() {
                        k += 1;
                    }
                    spans.push((step_aware_begin(chars, i), k));
                    i = k;
                    continue;
                }
            }
        }
        i += 1;
    }
    spans
}

fn clean_step(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.chars().count() < 2 {
        return None;
    }
    Some(
        trimmed
            .chars()
            .take(MAX_STEP_CHARS)
            .collect::<String>()
            .trim_end()
            .to_string(),
    )
}

fn split_numbered(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let spans = numbered_spans(&chars);
    if spans.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (k, &(_, start)) in spans.iter().enumerate() {
        let end = if k + 1 < spans.len() {
            spans[k + 1].0
        } else {
            chars.len()
        };
        if start < end {
            let raw: String = chars[start..end].iter().collect();
            if let Some(step) = clean_step(&raw) {
                out.push(step);
                if out.len() >= MAX_STEPS {
                    break;
                }
            }
        }
    }
    out
}

fn split_bullets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let rest = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("• "));
        if let Some(body) = rest {
            if let Some(step) = clean_step(body) {
                out.push(step);
                if out.len() >= MAX_STEPS {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Label;

    fn bm(id: &str, text: &str, topic: Option<&str>) -> Bookmark {
        Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: text.into(),
            author_username: Some(format!("syn_{id}")),
            created_at: None,
            folders: None,
            label: topic.map(|t| Label {
                parent_label: Some(t.into()),
                confidence: Some(0.9),
                method: None,
                is_foldered: None,
            }),
            had_note_tweet: false,
            media: Vec::new(),
        }
    }

    fn demo_fixture() -> Vec<Bookmark> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/demo/try_it_and_thread_bookmarks.jsonl"
        );
        crate::fixture::load(path).unwrap()
    }

    #[test]
    fn demo_fixture_tags_hands_on_only() {
        let rows = demo_fixture();
        assert_eq!(rows.len(), 5);
        for b in &rows {
            let tagged = is_try_it(b);
            match b.post_id.as_str() {
                "demo-2101" | "demo-2102" => {
                    assert!(tagged, "{} should be tagged", b.post_id);
                    let tag = classify(b).unwrap();
                    assert!(
                        tag.reason.contains("Health & Wellness"),
                        "reason names topic: {}",
                        tag.reason
                    );
                    assert!(!extract_steps(&b.text).is_empty(), "{}", b.post_id);
                }
                "demo-2001" | "demo-2002" | "demo-2003" => {
                    assert!(!tagged, "{} should not be tagged", b.post_id);
                    assert!(classify(b).is_none());
                }
                other => panic!("unexpected fixture post: {other}"),
            }
        }
        // The loader tolerates the fixture's thread and media fields.
        let stretch = rows.iter().find(|b| b.post_id == "demo-2101").unwrap();
        assert_eq!(stretch.media.len(), 1);
        assert_eq!(stretch.media[0].kind, "photo");
        let breath = rows.iter().find(|b| b.post_id == "demo-2102").unwrap();
        assert_eq!(breath.media[0].kind, "video");
    }

    #[test]
    fn tags_each_hands_on_category() {
        let positives = [
            (
                Some("Health & Wellness"),
                "[synthetic] Desk stretch: 1) roll shoulders 10x 2) neck tilt 20s each side 3) seated twist 30s each side. Try it now.",
            ),
            (
                Some("Health & Wellness"),
                "[synthetic] Box breathing: inhale 4s, hold 4s, exhale 4s, hold 4s. Repeat 4 rounds.",
            ),
            (
                None,
                "[synthetic] Physio for a stiff knee: 1) quad sets 10x 2) straight-leg raises 3 sets of 10 3) ice 10 min.",
            ),
            (
                None,
                "[synthetic] Morning mobility: touch your toes and hold for 20 seconds, then repeat 3 times.",
            ),
            (
                None,
                "[synthetic] Meditation: sit comfortably, close your eyes, and focus on your breath for 5 min.",
            ),
            (
                None,
                "[synthetic] Workout: 3 sets of 10 push-ups. Rest 60s between sets.",
            ),
            (
                None,
                "[synthetic] Overnight oats: 1) mix oats and milk 2) refrigerate overnight 3) top with berries in the morning.",
            ),
            (
                Some("Recipes"),
                "[synthetic] Ingredients:\n- rolled oats\n- milk\n- berries\nMix and refrigerate overnight.",
            ),
        ];
        for (i, (topic, text)) in positives.into_iter().enumerate() {
            let b = bm(&format!("pos{i}"), text, topic);
            let tag = classify(&b);
            assert!(tag.is_some(), "should tag: {text}");
            assert!(is_try_it(&b));
            assert!(!tag.unwrap().reason.is_empty());
        }
    }

    #[test]
    fn ignores_non_hands_on_posts() {
        let negatives = [
            (
                Some("AI Agents"),
                "[synthetic] 1/3 How I run my week with a kanban board that agents can pick cards from. Thread.",
            ),
            (
                Some("AI Agents"),
                "[synthetic] 2/3 Dragging a card to the 'Agent' column hands it off; the agent writes results back as a comment.",
            ),
            (
                Some("AI Agents"),
                "[synthetic] 3/3 Every handoff is logged, so I can review what was delegated each Friday.",
            ),
            (None, "[synthetic] Interesting study on meditation and sleep quality."),
            (
                Some("Health & Wellness"),
                "[synthetic] Interesting study on meditation and sleep quality.",
            ),
            (None, "[synthetic] This recipe looks amazing, saving for later."),
            (
                None,
                "Take notes on the synthetic demo. Ships 10x faster builds.",
            ),
            (None, "[synthetic] Back in the 90s we shipped software yearly."),
            (
                Some("AI Agents"),
                "[synthetic] Open source kanban board. Take a tour of the demo.",
            ),
        ];
        for (i, (topic, text)) in negatives.into_iter().enumerate() {
            let b = bm(&format!("neg{i}"), text, topic);
            assert!(!is_try_it(&b), "should not tag: {text}");
            assert!(classify(&b).is_none());
        }
    }

    #[test]
    fn extract_steps_finds_numbered_bulleted_and_imperative() {
        let numbered = "Desk stretch: 1) roll shoulders 10x 2) neck tilt 20s each side 3) seated twist 30s each side. Try it now.";
        let steps = extract_steps(numbered);
        assert_eq!(steps.len(), 3);
        assert!(steps[0].contains("roll shoulders"), "{steps:?}");
        assert!(steps[1].contains("neck tilt"), "{steps:?}");
        assert!(steps[2].contains("seated twist"), "{steps:?}");

        let colon = "Step 1: mix oats. Step 2: refrigerate.";
        let steps = extract_steps(colon);
        assert_eq!(steps.len(), 2);
        assert!(steps[0].contains("mix oats"), "{steps:?}");

        let bullets = "Ingredients:\n- rolled oats\n- milk\n- berries";
        let steps = extract_steps(bullets);
        assert_eq!(
            steps,
            ["rolled oats", "milk", "berries"].map(str::to_string)
        );

        let breath = "Box breathing: inhale 4s, hold 4s, exhale 4s, hold 4s. Repeat 4 rounds.";
        let steps = extract_steps(breath);
        assert_eq!(steps.len(), 2);
        assert!(steps[0].starts_with("inhale"), "{steps:?}");
        assert!(steps[1].contains("Repeat"), "{steps:?}");
    }

    #[test]
    fn extract_steps_caps_at_eight_and_skips_prose() {
        let many = (1..=10)
            .map(|n| format!("{n}) synthetic step {n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let steps = extract_steps(&many);
        assert_eq!(steps.len(), MAX_STEPS);
        assert!(steps[0].contains("synthetic step 1"));

        assert!(extract_steps("Interesting study on meditation and sleep quality.").is_empty());
        assert!(extract_steps("1/3 How I run my week with a kanban board.").is_empty());
        assert!(extract_steps("").is_empty());
    }

    #[test]
    fn tried_parses_three_values() {
        use std::str::FromStr;
        assert_eq!(Tried::from_str("yes").unwrap(), Tried::Yes);
        assert_eq!(Tried::from_str("no").unwrap(), Tried::No);
        assert_eq!(Tried::from_str("later").unwrap(), Tried::Later);
        assert_eq!(Tried::Yes.as_str(), "yes");
        assert_eq!(Tried::No.as_str(), "no");
        assert_eq!(Tried::Later.as_str(), "later");
        for bad in ["Yes", "maybe", "", "y"] {
            assert!(Tried::from_str(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn followup_round_trip_and_pending_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for id in ["p1", "p2", "p3"] {
            log.append(SHOWN_KIND, serde_json::json!({"post_id": id}))
                .unwrap();
        }
        assert_eq!(
            pending_followups(&log),
            ["p1", "p2", "p3"].map(str::to_string)
        );

        record_followup(&mut log, "p1", Tried::Yes, None).unwrap();
        assert_eq!(pending_followups(&log), ["p2", "p3"].map(str::to_string));

        // `later` keeps the post pending.
        record_followup(&mut log, "p2", Tried::Later, Some("will try Sunday")).unwrap();
        assert_eq!(pending_followups(&log), ["p2", "p3"].map(str::to_string));

        record_followup(&mut log, "p2", Tried::No, None).unwrap();
        assert_eq!(pending_followups(&log), ["p3"].map(str::to_string));

        // Follow-ups for never-shown posts never surface.
        record_followup(&mut log, "ghost", Tried::Yes, None).unwrap();
        assert_eq!(pending_followups(&log), ["p3"].map(str::to_string));

        let entries = log.verify().unwrap();
        let followups: Vec<_> = entries.iter().filter(|e| e.kind == FOLLOWUP_KIND).collect();
        assert_eq!(followups.len(), 4);
        assert_eq!(followups[0].payload["post_id"], "p1");
        assert_eq!(followups[0].payload["tried"], "yes");
        assert!(followups[0].payload.get("note").is_none());
        assert_eq!(followups[1].payload["note"], "will try Sunday");
        for e in &followups {
            assert!(e.payload.get("text").is_none(), "metadata only");
        }
    }

    #[test]
    fn followup_note_is_sanitized_capped_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = DecisionLog::open(dir.path().join("d.jsonl")).unwrap();
        record_followup(
            &mut log,
            "p1",
            Tried::Yes,
            Some("ping @syn_handle see https://t.co/x call 555-123-4567 ok"),
        )
        .unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries[0].payload["note"], "ping see call ok");

        let long = "w".repeat(500);
        record_followup(&mut log, "p2", Tried::Later, Some(&long)).unwrap();
        let entries = log.verify().unwrap();
        let note = entries[1].payload["note"].as_str().unwrap();
        assert_eq!(note.chars().count(), MAX_NOTE_CHARS);

        assert!(record_followup(&mut log, "", Tried::Yes, None).is_err());
        assert!(record_followup(&mut log, "   ", Tried::Yes, None).is_err());
    }

    struct FailingModel;

    impl crate::brief::BriefModel for FailingModel {
        fn model_id(&self) -> String {
            "test-failing".to_string()
        }

        fn call(
            &self,
            _prompt: &str,
        ) -> Result<crate::brief::ModelReply, ancientos_router::RouterError> {
            Err(ancientos_router::RouterError::PaidCallsDisabled)
        }
    }

    #[test]
    fn brief_marks_try_it_picks_and_logs_shown() {
        let mut stretch = bm(
            "h1",
            "[synthetic] Desk stretch: 1) roll shoulders 10x 2) neck tilt 20s each side. Try it now.",
            Some("Health & Wellness"),
        );
        stretch.media = vec![crate::media::Media {
            kind: "photo".into(),
            url: Some("https://pbs.twimg.com/media/synthetic.jpg".into()),
            preview_url: None,
            video_url: None,
            alt_text: Some("synthetic alt".into()),
        }];
        let breath = bm(
            "h2",
            "[synthetic] Box breathing: inhale 4s, hold 4s, exhale 4s, hold 4s. Repeat 4 rounds.",
            Some("Health & Wellness"),
        );
        let rows = vec![
            stretch,
            breath,
            bm(
                "n1",
                "[synthetic] 1/3 How I run my week with a kanban board. Thread.",
                Some("AI Agents"),
            ),
            bm(
                "n2",
                "[synthetic] Every handoff is logged for Friday review.",
                Some("AI Agents"),
            ),
        ];
        let merged = crate::merge::MergeOutput {
            rows: rows.clone(),
            live_ids: rows.iter().map(|b| b.post_id.clone()).collect(),
            counts: crate::merge::MergeCounts {
                live: 4,
                fixture: 0,
                overlap: 0,
                total: 4,
                high_confidence: 4,
            },
        };
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("d.jsonl");
        let res =
            crate::brief::run_with_hydrator(&merged, &FailingModel, &log_path, "2026-10-26", None)
                .unwrap();
        assert!(res.fallback, "failing model forces local picks");
        assert_eq!(res.picks.len(), 4);

        // Both hands-on picks carry the marker and steps; media is kept.
        assert_eq!(
            res.markdown.matches("[TRY IT]").count(),
            2,
            "{}",
            res.markdown
        );
        assert!(res.markdown.contains("**Steps**"), "{}", res.markdown);
        assert!(res.markdown.contains("roll shoulders"), "{}", res.markdown);
        assert!(res.markdown.contains("**Media**"), "{}", res.markdown);
        assert!(
            res.markdown
                .contains("https://pbs.twimg.com/media/synthetic.jpg"),
            "{}",
            res.markdown
        );

        // Exactly the tagged picks were logged as shown.
        let log = DecisionLog::open(&log_path).unwrap();
        let mut shown: Vec<String> = log
            .verify()
            .unwrap()
            .iter()
            .filter(|e| e.kind == SHOWN_KIND)
            .filter_map(|e| {
                e.payload
                    .get("post_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        shown.sort();
        assert_eq!(shown, vec!["h1".to_string(), "h2".to_string()]);
        assert_eq!(pending_followups(&log), ["h1", "h2"].map(str::to_string));
    }
}
