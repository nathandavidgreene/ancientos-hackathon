//! "Try it" tag for hands-on content: a practice you could physically try
//! (exercise, breathing, mobility, meditation, recipes) or a hands-on
//! tool/demo you can do yourself (setup steps, software tutorials, prompts
//! you can run, config tweaks).
//!
//! Deterministic local classifier (no network): the topic from
//! [`brief::topic_of`](crate::brief::topic_of) plus keyword/imperative cues
//! in the full text (see [`classify_with_media`]). Follow-ups
//! (`tried yes|no|later`) and the pending queue live in the append-only
//! decision log as metadata only.
use crate::fixture::Bookmark;
use ancientos_core::log::{DecisionLog, Entry};
use std::collections::BTreeSet;

/// Log kind appended when the brief renders a tagged pick.
pub const SHOWN_KIND: &str = "try_it.shown";
/// Log kind appended by [`record_followup`].
pub const FOLLOWUP_KIND: &str = "try_it.followup";
/// Payload field carrying the unix-secs timestamp of a [`SHOWN_KIND`] record.
/// Older records lack it; readers treat a missing field as unknown age.
pub const SHOWN_AT_FIELD: &str = "shown_at";
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
/// A video/GIF attachment counts as a demo cue (see [`classify_with_media`]).
pub fn classify(b: &Bookmark) -> Option<TryItTag> {
    let has_video = b
        .media
        .iter()
        .any(|m| m.kind == "video" || m.kind == "animated_gif");
    classify_with_media(&crate::brief::topic_of(b), &b.text, has_video)
}

/// Classify from a topic and full text (used by the brief, which renders
/// hydrated/expanded text rather than the stored excerpt).
pub fn classify_text(topic: &str, text: &str) -> Option<TryItTag> {
    classify_with_media(topic, text, false)
}

/// Deterministic rule set:
/// - strong practice cues (breathing compounds, hold, reps, each side,
///   mindfulness, ingredients, hands-on practice words) tag on a hands-on
///   topic, with an explicit "try it", with a second strong cue, or with
///   numbered steps or an imperative;
/// - weak practice words (practice, exercise, workout, ...) need an explicit
///   "try it" or numbered steps: bare mentions stay untagged;
/// - how-to guides ("step by step", "complete guide", "field guide", ...),
///   paste-this-prompt cues, shell commands, runnable prompts, agent
///   instructions, and config tweaks tag on their own;
/// - how-to pointers ("here's how to", "here's the setup", cheatsheets)
///   need steps, imperatives, pasted prompts, or commands;
/// - two imperative sentences with distinct verbs, numbered steps starting
///   with an imperative, or two imperative bullets tag;
/// - setup/download words with numbered steps, dosages with a stack or
///   steps, teach-a-skill pointers with a link, and tool pointers tag;
/// - a video demo (video media, or a short post with a link) tags when it
///   carries a strong practice cue or a hands-on action verb.
///
/// Structural cues (lists, durations, lone imperatives) never tag alone.
pub fn classify_with_media(topic: &str, text: &str, has_video: bool) -> Option<TryItTag> {
    let det = detect(text);
    let hands_on = topic_is_hands_on(topic);
    let strong = det.strong > 0;
    let video_demo = (has_video || det.short) && (strong || det.tight);
    let tagged = video_demo
        || (strong
            && (hands_on || det.explicit || det.strong >= 2 || det.numbered || det.imperative))
        || (det.weak && (det.explicit || det.numbered))
        || (hands_on && det.numbered && det.step_num_imp >= 1)
        || det.howto_strong
        || (det.howto_ptr
            && ((det.numbered && det.step_num_imp >= 1)
                || det.step_bul_imp >= 1
                || det.paste
                || det.cmd))
        || (det.cheat
            && (det.numbered_any || det.bullets || det.imperative || det.paste || det.cmd))
        || (det.numbered && det.step_num_imp >= 1)
        || det.step_bul_imp >= 2
        || det.multi_imp
        || det.paste
        || (det.prompt_hdr && det.link)
        || det.cmd
        || (det.dosage && (det.stack || det.bullets || det.numbered_any))
        || det.day_cycle
        || (det.teach && det.link)
        || det.in_config
        || (det.cfg_gated && (det.imperative || det.numbered_any || det.bullets || det.link))
        || det.howto_can
        || det.tell_agent
        || (det.send_link && det.link)
        || (det.setup && det.numbered_any)
        || (det.download && det.numbered_any && det.link)
        || (det.all_need && det.link)
        || (det.prac_skill && det.link)
        || (det.affirm && det.link)
        || (det.tools_for && hands_on)
        || (det.hacks_to && hands_on)
        || (det.things_do && det.numbered_any)
        || (det.here_works && hands_on);
    if !tagged {
        return None;
    }
    let mut labels = det.labels;
    if video_demo {
        labels.push(if has_video { CUE_VIDEO } else { CUE_SHORT_DEMO });
    }
    Some(TryItTag {
        reason: format!("{}; topic '{topic}'", labels.join(", ")),
    })
}

/// Every detector output for one text, plus reason labels in fixed order.
#[derive(Debug)]
struct Det {
    labels: Vec<&'static str>,
    explicit: bool,
    strong: usize,
    weak: bool,
    numbered: bool,
    numbered_any: bool,
    bullets: bool,
    imperative: bool,
    multi_imp: bool,
    step_num_imp: usize,
    step_bul_imp: usize,
    howto_strong: bool,
    howto_ptr: bool,
    cheat: bool,
    paste: bool,
    prompt_hdr: bool,
    link: bool,
    short: bool,
    cmd: bool,
    dosage: bool,
    stack: bool,
    day_cycle: bool,
    teach: bool,
    setup: bool,
    download: bool,
    here_works: bool,
    affirm: bool,
    tell_agent: bool,
    send_link: bool,
    all_need: bool,
    prac_skill: bool,
    tools_for: bool,
    hacks_to: bool,
    things_do: bool,
    in_config: bool,
    cfg_gated: bool,
    howto_can: bool,
    tight: bool,
}

const CUE_VIDEO: &str = "video demo";
const CUE_SHORT_DEMO: &str = "short demo";
const CUE_PRACTICE: &str = "practice cue";
const CUE_WEAK: &str = "weak practice cue";
const CUE_MULTI_IMP: &str = "multi-imperative";
const CUE_STEP_IMP: &str = "step imperatives";
const CUE_HOWTO: &str = "how-to guide";
const CUE_PTR: &str = "how-to pointer";
const CUE_CHEAT: &str = "cheatsheet";
const CUE_PASTE: &str = "paste prompt";
const CUE_PROMPT_HDR: &str = "prompt header";
const CUE_CMD: &str = "shell command";
const CUE_DOSAGE: &str = "dosage";
const CUE_STACK: &str = "supplement stack";
const CUE_DAY_CYCLE: &str = "cycling protocol";
const CUE_TEACH: &str = "teach cue";
const CUE_SETUP: &str = "setup steps";
const CUE_DOWNLOAD: &str = "install steps";
const CUE_WORKS: &str = "works pointer";
const CUE_AFFIRM: &str = "affirmation";
const CUE_TELL_AGENT: &str = "tell agent";
const CUE_SEND_LINK: &str = "send link";
const CUE_ALL_NEED: &str = "all you need";
const CUE_PRAC_SKILL: &str = "practical skills";
const CUE_TOOLS_FOR: &str = "tools-for";
const CUE_HACKS_TO: &str = "hacks-to";
const CUE_THINGS_DO: &str = "things i do";
const CUE_CONFIG: &str = "config tweak";
const CUE_PLACE: &str = "place instruction";

/// Specific words naming something you physically do or practise.
const PRACTICE_STRONG: &[&str] = &[
    "mix",
    "whisk",
    "bake",
    "refrigerate",
    "simmer",
    "preheat",
    "chop",
    "stir",
    "drill",
    "drills",
    "technique",
    "routine",
    "with me",
    "isometric",
    "plank",
    "squat",
    "squats",
    "push-up",
    "push-ups",
    "pushup",
    "pushups",
    "stretch",
    "stretches",
    "stretching",
    "journal",
    "answer these",
    "how you do it",
    "this is how you",
    "follow along",
    "tutorial",
    "pose",
    "asana",
    "warmup",
    "warm-up",
    "cook",
    "cooking",
    "season",
    "marinad",
];

/// Generic words that need an explicit "try it" or numbered steps.
const PRACTICE_WEAK: &[&str] = &[
    "practice",
    "practise",
    "exercise",
    "workout",
    "work out",
    "working out",
];

/// Hands-on verbs that make a short clip a demo rather than commentary.
const TIGHT_VERBS: &[&str] = &[
    "tie",
    "tying",
    "knot",
    "fold",
    "folding",
    "cook",
    "cooking",
    "stretch",
    "stretching",
    "lift",
    "lifting",
    "throw",
    "swing",
    "build",
    "fix",
    "fixing",
    "drill",
];

fn has_tight_verb(lower: &str) -> bool {
    has_any_phrase(lower, TIGHT_VERBS)
}

/// Explicit invitations to try the practice yourself.
const EXPLICIT_TRY: &[&str] = &[
    "try it",
    "try this",
    "give it a try",
    "give this a try",
    "try it out",
    "try it yourself",
    "you can try",
    "go try",
    "you can do right now",
    "do it right now",
    "give this one a try",
];

/// Guide phrases that tag on their own.
const HOWTO_STRONG: &[&str] = &[
    "step by step",
    "step-by-step",
    "complete guide",
    "setup guide",
    "setup steps",
    "prompts i used",
    "prompt in replies",
    "here is exactly what",
    "here's how you can",
    "here is how you can",
    "here's the trick",
    "here is the trick",
    "put a guide together",
    "field guide",
    "operator guide",
    "taught me how to",
    "one line to install",
    "one-line install",
];

/// Guide pointers that need steps, imperatives, pasted prompts, or commands.
const HOWTO_PTR: &[&str] = &[
    "here's the setup",
    "here is the setup",
    "here's my setup",
    "here is my setup",
    "here's how to",
    "here is how to",
    "here's how you",
    "here is how you",
    "here's the exact setup",
    "here is the exact setup",
    "here's what i have and recommend",
];

/// Paste-or-run prompt cues (prompts you can run yourself).
const PASTE_CUES: &[&str] = &[
    "paste this",
    "paste this prompt",
    "send this prompt",
    "prompt in replies",
    "prompts i used",
    "paste into",
    "paste it",
    "copy paste",
    "copy-paste",
    "copy and paste",
];

/// Pointers to a training you can follow along with (need a link).
const TEACH_CUES: &[&str] = &[
    "will teach you",
    "training video",
    "tutorial video",
    "walkthrough",
];

/// "What works" pointers on a hands-on topic.
const HERE_WORKS: &[&str] = &[
    "here's what works",
    "here is what works",
    "what actually works",
];

/// Affirmation exercises (need a link).
const AFFIRM_CUES: &[&str] = &["affirmation", "say to yourself", "repeat to yourself"];

/// Direct instructions to an agent.
const TELL_AGENT: &[&str] = &[
    "tell the agent",
    "ask the agent",
    "tell your agent",
    "ask your agent",
    "tell it to",
    "ask it to",
];

/// "You put it here" setup instructions.
const HOWTO_CAN: &[&str] = &[
    "you place it",
    "you put it",
    "you add it",
    "you paste it",
    "you copy it",
    "you save it",
    "you drop it",
    "you run it",
];

/// Supplement-stack context words.
const STACK_WORDS: &[&str] = &[
    "stack",
    "cycling",
    "upon waking",
    "before bed",
    "timing",
    "days on",
    "days off",
];

/// Setup words that need numbered steps.
const SETUP_WORDS: &[&str] = &["setup", "set up", "configure"];

/// Install words that need numbered steps plus a link.
const DOWNLOAD_WORDS: &[&str] = &["download", "install", "installs", "installed"];

/// Installer commands that tag on their own.
const INSTALL_CMDS: &[&str] = &[
    "npm install",
    "pip install",
    "brew install",
    "apt install",
    "git clone",
    "docker run",
    "npx",
];

/// A short post (under 120 chars once links are removed) that carries a
/// link: usually a video clip whose caption names the activity.
fn is_short_demo(text: &str) -> bool {
    let has_link = text.contains("https://t.co/");
    let body: String = text
        .split_whitespace()
        .filter(|w| !w.starts_with("http"))
        .collect::<Vec<_>>()
        .join(" ");
    has_link && body.chars().count() < 120
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

/// Append a [`SHOWN_KIND`] record with its timestamp. Metadata only:
/// post id plus unix secs, never text.
pub fn record_shown(log: &mut DecisionLog, post_id: &str, shown_at: u64) -> Result<Entry, String> {
    if post_id.trim().is_empty() {
        return Err("post id must be non-empty".to_string());
    }
    log.append(
        SHOWN_KIND,
        serde_json::json!({"post_id": post_id, "shown_at": shown_at}),
    )
    .map_err(|e| e.to_string())
}

/// Timestamp of a [`SHOWN_KIND`] payload, if present and numeric.
/// Records written before timestamps return `None` (compatibly ignored).
pub fn shown_at_of(payload: &serde_json::Value) -> Option<u64> {
    payload.get(SHOWN_AT_FIELD)?.as_u64()
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

/// Breathing compounds (bare "breathe"/"breathing" is too loose alone).
const BREATH_WORDS: &[&str] = &[
    "breathe in",
    "breathe out",
    "breathwork",
    "box breathing",
    "inhale",
    "exhale",
    "deep breath",
];

/// Mindfulness practice phrases.
const MINDFUL_WORDS: &[&str] = &[
    "close your eyes",
    "focus on your breath",
    "body scan",
    "mindful",
];

/// All detector outputs for the text, in fixed label order.
fn detect(text: &str) -> Det {
    let lower = text.to_lowercase();
    let link = text.contains("https://t.co/") || text.contains("http");
    let short = is_short_demo(text);
    let tight = has_tight_verb(&lower);
    let explicit = has_any_phrase(&lower, EXPLICIT_TRY);
    let numbered_marks = numbered_spans(&text.chars().collect::<Vec<_>>()).len();
    let numbered_any = numbered_marks >= 1;
    let numbered = numbered_marks >= 2;
    let bullets = has_bullets(text);
    let breath = has_any_phrase(&lower, BREATH_WORDS);
    let hold = has_hold_cue(&lower);
    let reps = has_reps_cue(&lower);
    let duration = has_duration_cue(&lower);
    let side = contains_phrase(&lower, "each side");
    let mindful = has_any_phrase(&lower, MINDFUL_WORDS);
    let ingredients = contains_phrase(&lower, "ingredients");
    let practice = has_any_phrase(&lower, PRACTICE_STRONG);
    let weak = has_any_phrase(&lower, PRACTICE_WEAK);
    let strong = [breath, hold, reps, side, mindful, ingredients, practice]
        .into_iter()
        .filter(|hit| *hit)
        .count();
    let frag_verbs = imperative_verbs(text);
    let step_num_imp = split_numbered(text)
        .iter()
        .filter(|s| is_imperative(s))
        .count();
    let step_bul_imp = split_bullets(text)
        .iter()
        .filter(|s| is_imperative(s))
        .count();
    let imperative = has_imperative(text);
    let distinct: BTreeSet<&str> = frag_verbs.iter().map(String::as_str).collect();
    let multi_imp = frag_verbs.len() >= 2 && distinct.len() >= 2;
    let howto_strong = has_any_phrase(&lower, HOWTO_STRONG);
    let howto_ptr = has_any_phrase(&lower, HOWTO_PTR);
    let cheat = contains_phrase(&lower, "cheatsheet") || contains_phrase(&lower, "cheat sheet");
    let paste = has_any_phrase(&lower, PASTE_CUES);
    let prompt_hdr = has_prompt_header(&lower);
    let cmd = has_cmd(text, &lower, link);
    let dosage = has_dosage(&lower);
    let stack = has_any_phrase(&lower, STACK_WORDS);
    let day_cycle = contains_phrase(&lower, "days on") && contains_phrase(&lower, "days off");
    let teach = has_any_phrase(&lower, TEACH_CUES);
    let setup = has_any_phrase(&lower, SETUP_WORDS);
    let download = has_any_phrase(&lower, DOWNLOAD_WORDS);
    let here_works = has_any_phrase(&lower, HERE_WORKS);
    let affirm = has_any_phrase(&lower, AFFIRM_CUES);
    let tell_agent = has_any_phrase(&lower, TELL_AGENT);
    let send_link = contains_phrase(&lower, "send the link");
    let all_need = contains_phrase(&lower, "all you need");
    let prac_skill = contains_phrase(&lower, "practical") && contains_phrase(&lower, "skills");
    let tools_for = contains_phrase(&lower, "tools for");
    let hacks_to = contains_phrase(&lower, "hacks to") || contains_phrase(&lower, "hack to");
    let things_do = contains_phrase(&lower, "things i do");
    let in_config =
        contains_phrase(&lower, "in your config") || contains_phrase(&lower, "to your config");
    let cfg_gated = contains_phrase(&lower, "add this to your")
        || (contains_phrase(&lower, "changes to")
            && (lower.contains(".md") || lower.contains("config")));
    let howto_can = has_any_phrase(&lower, HOWTO_CAN);
    let mut labels = Vec::new();
    if explicit {
        labels.push(CUE_EXPLICIT);
    }
    if numbered {
        labels.push(CUE_NUMBERED);
    }
    if bullets {
        labels.push(CUE_BULLETS);
    }
    if breath {
        labels.push(CUE_BREATH);
    }
    if hold {
        labels.push(CUE_HOLD);
    }
    if reps {
        labels.push(CUE_REPS);
    }
    if duration {
        labels.push(CUE_DURATION);
    }
    if side {
        labels.push(CUE_SIDE);
    }
    if mindful {
        labels.push(CUE_MINDFUL);
    }
    if ingredients {
        labels.push(CUE_INGREDIENTS);
    }
    if practice {
        labels.push(CUE_PRACTICE);
    }
    if weak {
        labels.push(CUE_WEAK);
    }
    if imperative {
        labels.push(CUE_IMPERATIVE);
    }
    if multi_imp {
        labels.push(CUE_MULTI_IMP);
    }
    if step_num_imp + step_bul_imp > 0 {
        labels.push(CUE_STEP_IMP);
    }
    if howto_strong {
        labels.push(CUE_HOWTO);
    }
    if howto_ptr {
        labels.push(CUE_PTR);
    }
    if cheat {
        labels.push(CUE_CHEAT);
    }
    if paste {
        labels.push(CUE_PASTE);
    }
    if prompt_hdr {
        labels.push(CUE_PROMPT_HDR);
    }
    if cmd {
        labels.push(CUE_CMD);
    }
    if dosage {
        labels.push(CUE_DOSAGE);
    }
    if stack {
        labels.push(CUE_STACK);
    }
    if day_cycle {
        labels.push(CUE_DAY_CYCLE);
    }
    if teach {
        labels.push(CUE_TEACH);
    }
    if setup {
        labels.push(CUE_SETUP);
    }
    if download {
        labels.push(CUE_DOWNLOAD);
    }
    if here_works {
        labels.push(CUE_WORKS);
    }
    if affirm {
        labels.push(CUE_AFFIRM);
    }
    if tell_agent {
        labels.push(CUE_TELL_AGENT);
    }
    if send_link {
        labels.push(CUE_SEND_LINK);
    }
    if all_need {
        labels.push(CUE_ALL_NEED);
    }
    if prac_skill {
        labels.push(CUE_PRAC_SKILL);
    }
    if tools_for {
        labels.push(CUE_TOOLS_FOR);
    }
    if hacks_to {
        labels.push(CUE_HACKS_TO);
    }
    if things_do {
        labels.push(CUE_THINGS_DO);
    }
    if in_config || cfg_gated {
        labels.push(CUE_CONFIG);
    }
    if howto_can {
        labels.push(CUE_PLACE);
    }
    Det {
        labels,
        explicit,
        strong,
        weak,
        numbered,
        numbered_any,
        bullets,
        imperative,
        multi_imp,
        step_num_imp,
        step_bul_imp,
        howto_strong,
        howto_ptr,
        cheat,
        paste,
        prompt_hdr,
        link,
        short,
        cmd,
        dosage,
        stack,
        day_cycle,
        teach,
        setup,
        download,
        here_works,
        affirm,
        tell_agent,
        send_link,
        all_need,
        prac_skill,
        tools_for,
        hacks_to,
        things_do,
        in_config,
        cfg_gated,
        howto_can,
        tight,
    }
}

/// `prompt` followed by optional whitespace and `:` or `;` ("PROMPT:", "Prompt;").
fn has_prompt_header(lower: &str) -> bool {
    let h = lower.as_bytes();
    let mut i = 0;
    while i + 6 <= h.len() {
        if &h[i..i + 6] == b"prompt" {
            let mut j = i + 6;
            while j < h.len() && h[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < h.len() && (h[j] == b':' || h[j] == b';') {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// A pasted shell line (`$ cmd`), a piped installer (`| bash`), an installer
/// command, or curl with a link.
fn has_cmd(text: &str, lower: &str, link: bool) -> bool {
    if text.lines().any(|line| {
        let rest = match line.trim_start().strip_prefix("$ ") {
            Some(rest) => rest,
            None => return false,
        };
        rest.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }) {
        return true;
    }
    text.contains("| bash")
        || has_any_phrase(lower, INSTALL_CMDS)
        || (contains_phrase(lower, "curl") && link)
}

/// `<number>[ ]mg|mcg|µg` ("150 mg", "500mg", "6mg").
fn has_dosage(lower: &str) -> bool {
    const UNITS: &[&str] = &["mcg", "\u{b5}g", "mg"];
    let h = lower.as_bytes();
    let mut i = 0;
    while i < h.len() {
        if h[i].is_ascii_digit() {
            let mut j = i;
            while j < h.len() && h[j].is_ascii_digit() {
                j += 1;
            }
            let mut k = j;
            if k < h.len() && h[k] == b' ' {
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

/// Verbs that start an instruction ("Roll your shoulders", "Install the app").
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
    "install",
    "download",
    "paste",
    "copy",
    "type",
    "click",
    "tap",
    "select",
    "choose",
    "search",
    "enter",
    "set",
    "create",
    "connect",
    "enable",
    "restart",
    "update",
    "upgrade",
    "launch",
    "save",
    "upload",
    "import",
    "clone",
    "deploy",
    "write",
    "use",
    "send",
    "grab",
    "get",
    "check",
    "make",
    "move",
    "edit",
    "configure",
    "sync",
    "read",
    "fast",
    "sleep",
    "wake",
    "train",
    "meditate",
    "cut",
    "eat",
    "tie",
    "give",
    "ask",
    "define",
    "design",
    "build",
    "test",
    "delete",
    "disable",
];

/// Filler words skipped before the first content word ("just paste this").
const SKIP_WORDS: &[&str] = &["please", "just", "simply", "then", "and", "also"];

fn word_key(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// True when the sentence starts with an instruction verb ("Roll 10x").
/// Exposed for the deeper local fallback's step extraction.
pub(crate) fn is_imperative_sentence(sentence: &str) -> bool {
    is_imperative(sentence)
}

/// First instruction verb of the fragment, if any: skips empty tokens,
/// slash commands, shell prompts, mentions and tags (`/fast mode` is not
/// the verb "fast"), then filler words. "Open source" and "check out"
/// are not instructions.
fn first_verb(fragment: &str) -> Option<String> {
    let mut words: Vec<String> = fragment
        .split_whitespace()
        .filter(|w| !matches!(w.chars().next(), Some('/' | '$' | '@' | '#')))
        .map(word_key)
        .filter(|w| !w.is_empty())
        .collect();
    for _ in 0..3 {
        if words
            .first()
            .is_some_and(|w| SKIP_WORDS.contains(&w.as_str()))
        {
            words.remove(0);
        } else {
            break;
        }
    }
    let first = words.first()?;
    if first == "open" && matches!(words.get(1).map(String::as_str), Some("source" | "sourced")) {
        return None;
    }
    if first == "check" && words.get(1).is_some_and(|w| w == "out") {
        return None;
    }
    if IMPERATIVE_VERBS.contains(&first.as_str()) {
        Some(first.clone())
    } else {
        None
    }
}

/// True when the fragment starts with an imperative verb.
fn is_imperative(fragment: &str) -> bool {
    first_verb(fragment).is_some()
}

/// Instruction verb of each sentence-like fragment, in order.
fn imperative_verbs(text: &str) -> Vec<String> {
    text.split(&['.', '!', '?', ';', ':', '\n'][..])
        .filter_map(first_verb)
        .collect()
}

/// True when any sentence-like fragment, numbered step, or bullet starts
/// imperative.
fn has_imperative(text: &str) -> bool {
    if !imperative_verbs(text).is_empty() {
        return true;
    }
    split_numbered(text).iter().any(|s| is_imperative(s))
        || split_bullets(text).iter().any(|s| is_imperative(s))
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

/// `(marker_begin, content_start)` char indices for `1)`, `2.`, `3:`, `1-`, `(4)`.
/// Markers are at most two digits: longer runs are quantities (`past 100.`).
/// Excludes `1/3` thread counters, decimals (`v2.0`), clock times, and
/// numeric ranges (`1-2`).
fn numbered_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let closed = j > i + 1 && j - (i + 1) <= 2 && j < chars.len() && chars[j] == ')';
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
            if j - i > 2 {
                i = j;
                continue;
            }
            if j < chars.len() && matches!(chars[j], ')' | '.' | ':' | '-') {
                let after = j + 1;
                let is_decimal = matches!(chars[j], '.' | '-')
                    && after < chars.len()
                    && chars[after].is_ascii_digit();
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
    fn shown_records_carry_timestamps_compatibly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        record_shown(&mut log, "p1", 1_700_000_000).unwrap();
        // Legacy record without a timestamp still counts as shown.
        log.append(SHOWN_KIND, serde_json::json!({"post_id": "p0"}))
            .unwrap();
        assert!(record_shown(&mut log, "  ", 0).is_err());
        let log = DecisionLog::open(&path).unwrap();
        assert_eq!(pending_followups(&log), ["p1", "p0"].map(str::to_string));
        let entries = log.verify().unwrap();
        assert_eq!(
            shown_at_of(&entries[0].payload),
            Some(1_700_000_000),
            "timestamped record"
        );
        assert_eq!(shown_at_of(&entries[1].payload), None, "legacy record");
        assert_eq!(
            shown_at_of(&serde_json::json!({"shown_at": "yesterday"})),
            None,
            "non-numeric timestamp"
        );
        for e in entries.iter().take(2) {
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
        // Hermetic: paid off so brief liveness skips with no network.
        let _env = crate::test_support::EnvHold::paid_off();
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

    #[test]
    fn list_style_advice_is_not_tagged() {
        let t =
            "5 things for your marriage:\n1. Weekly date night, 30 minutes.\n2. Say thanks daily.";
        assert!(classify_with_media("Relationships", t, false).is_none());
        let b = "- Anger means a boundary.\n- Anxiety is preparation.";
        assert!(classify_with_media("Mental Health", b, false).is_none());
    }

    #[test]
    fn short_video_demo_is_tagged() {
        let t = "Practical knot-tying skills https://t.co/abc";
        assert!(classify_with_media("Misc", t, false).is_some());
        let v = "Watch how to fold a fitted sheet";
        let tag = classify_with_media("Misc", v, true).unwrap();
        assert!(tag.reason.contains("video demo"), "{}", tag.reason);
        assert!(classify_with_media("Misc", "Funny cat", true).is_none());
    }

    #[test]
    fn explicit_try_needs_practice_cue() {
        assert!(classify_with_media(
            "Programming",
            "Try this debugging trick. Explain the bug out loud.",
            false
        )
        .is_none());
    }

    #[test]
    fn tags_hands_on_tool_setup_and_prompts() {
        let positives = [
            // Numbered setup steps with step-initial imperatives.
            "[synthetic] DemoBot setup in 3 steps: 1. Install demobot 2. Run demobot init 3. Open the dashboard.",
            // Dashed numbered steps.
            "[synthetic] Cash deposits: 1- Search for a store nearby 2- Show the barcode at checkout.",
            // Paste-this-prompt cue.
            "[synthetic] Paste this prompt into DemoBot to plan the week: list my three goals and schedule them.",
            // Several imperatives with distinct verbs.
            "[synthetic] Type a city name. Save the draft. Download the poster.",
            // Config tweak.
            "[synthetic] DemoBot pro tip: in your config, set session-timeout to 10m.",
            // Agent instruction.
            "[synthetic] Ask your agent to update to beta, then restart the gateway.",
            // Shell installer.
            "[synthetic] One-line install: curl -fsSL https://example.invalid/i.sh | bash",
            // Dosage plus stack context.
            "[synthetic] Night stack for deep-work recovery: 200mg moonroot, timing 30m before bed.",
            // How-to pointer with imperative steps.
            "[synthetic] Here is how to upgrade DemoBot safely: 1. Run demobot validate 2. Check the plugins.",
            // Standalone guide phrase.
            "[synthetic] A friend wrote a step by step guide for a locked-down DemoBot install.",
            // Runnable prompt header with a link.
            "[synthetic] DemoBot prompt: summarize this thread as five bullets https://example.invalid/p",
        ];
        for (i, text) in positives.into_iter().enumerate() {
            let tag = classify_with_media("Tech & Tools", text, false);
            assert!(tag.is_some(), "should tag positive {i}: {text}");
        }
    }

    #[test]
    fn ignores_link_bait_advice_and_bare_mentions() {
        let negatives = [
            // Bare how-to link bait without hands-on verbs.
            "[synthetic] How to win every debate forever https://t.co/synth01",
            // Watch/bookmark pointer without a teach cue.
            "[synthetic] Watch the workshop. Bookmark it. https://t.co/synth02",
            // Single AMA invitation, no practice.
            "[synthetic] I maintain DemoBot. Ask me anything in the thread.",
            // "Check out" is not an instruction.
            "[synthetic] Check out my new post on focus and sleep.",
            // Bare workout mention on a hands-on topic.
            "[synthetic] This workout looks amazing, saving for later.",
            // "You can run" story without steps or commands.
            "[synthetic] 10 repos that distill smart people into AI you can run on a laptop. https://t.co/synth03",
        ];
        for (i, text) in negatives.into_iter().enumerate() {
            let topic = if i == 4 { "Health & Wellness" } else { "Misc" };
            assert!(
                classify_with_media(topic, text, false).is_none(),
                "should not tag negative {i}: {text}"
            );
        }
    }
}
