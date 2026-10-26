//! Idea cards: inspiration captured from delegated posts, triaged later.
//!
//! Append-only JSONL store (`$ANCIENTOS_HOME/idea_cards.jsonl`). Each line is
//! a full snapshot of a card; the latest line per `id` wins (fold).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const IDEA_CARDS_FILE_NAME: &str = "idea_cards.jsonl";
pub const PARK_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effort {
    S,
    M,
    L,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    New,
    Adopted,
    Parked,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdeaCard {
    pub id: String,
    pub source_url: String,
    pub idea: String,
    /// Plain module names, e.g. `gate`, `decision_log`. Accepts a string or list.
    #[serde(deserialize_with = "de_mapping")]
    pub ancientos_mapping: Vec<String>,
    pub effort: Effort,
    pub status: Status,
    /// Unix seconds (also accepts RFC 3339 strings on read).
    #[serde(deserialize_with = "de_time")]
    pub created_at: u64,
    /// Unix seconds; set when parked (also accepts RFC 3339 on read).
    #[serde(default, deserialize_with = "de_opt_time")]
    pub park_until: Option<u64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

fn de_mapping<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        OneOrMany::Many(v) => v,
    })
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TimeRepr {
    Secs(u64),
    Text(String),
}

fn time_from(t: TimeRepr) -> Result<u64, String> {
    match t {
        TimeRepr::Secs(n) => Ok(n),
        TimeRepr::Text(s) => parse_rfc3339(&s).ok_or_else(|| format!("bad timestamp: {s}")),
    }
}

fn de_time<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    time_from(TimeRepr::deserialize(d)?).map_err(serde::de::Error::custom)
}

fn de_opt_time<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    match Option::<TimeRepr>::deserialize(d)? {
        None => Ok(None),
        Some(t) => time_from(t).map(Some).map_err(serde::de::Error::custom),
    }
}

/// Minimal RFC 3339 parser (`YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`) -> unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let off = if rest == "Z" || rest == "z" {
        0
    } else {
        let sign = match rest.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let oh: i64 = rest.get(1..3)?.parse().ok()?;
        let om: i64 = rest.get(4..6)?.parse().ok()?;
        sign * (oh * 3600 + om * 60)
    };
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let t = days * 86400 + h * 3600 + mi * 60 + se - off;
    u64::try_from(t).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Triage {
    Adopt,
    Park,
    Drop,
}

/// Known AncientOS components used for keyword mapping.
const MODULES: &[(&str, &[&str])] = &[
    ("gate", &["gate", "approv", "card", "drag", "review"]),
    (
        "decision_log",
        &["log", "memory", "remember", "history", "audit"],
    ),
    (
        "scheduler",
        &[
            "schedul",
            "recurring",
            "daily",
            "weekly",
            "to-do",
            "todo",
            "wins",
        ],
    ),
    (
        "privacy_router",
        &[
            "privacy",
            "router",
            "private",
            "skills library",
            "permission",
        ],
    ),
    ("brief", &["brief", "summary", "digest"]),
    ("delegate", &["agent", "delegate", "hand"]),
];

pub fn idea_cards_path() -> Result<PathBuf, String> {
    Ok(crate::token_store::data_dir()
        .map_err(|e| e.to_string())?
        .join(IDEA_CARDS_FILE_NAME))
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Map idea text to plain module names by keyword. Unknown -> `["unmapped"]`.
pub fn map_modules(idea: &str) -> Vec<String> {
    let lower = idea.to_lowercase();
    let v: Vec<String> = MODULES
        .iter()
        .filter(|(_, kws)| kws.iter().any(|k| lower.contains(k)))
        .map(|(m, _)| m.to_string())
        .collect();
    if v.is_empty() {
        vec!["unmapped".to_string()]
    } else {
        v
    }
}

/// Rough effort heuristic: more touched modules -> bigger.
pub fn estimate_effort(mapping: &[String]) -> Effort {
    match mapping.len() {
        0 | 1 => Effort::S,
        2 => Effort::M,
        _ => Effort::L,
    }
}

/// Bullets under the "Worth borrowing for AncientOS" heading of delegate output.
pub fn extract_ideas(output: &str) -> Vec<String> {
    let mut in_section = false;
    let mut out = Vec::new();
    for line in output.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            in_section = t.to_lowercase().contains("worth borrowing");
            continue;
        }
        if in_section {
            if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
                let rest = rest.trim();
                if !rest.is_empty() {
                    out.push(rest.to_string());
                }
            }
        }
    }
    out
}

fn card_id(source_url: &str, idea: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(source_url.as_bytes());
    h.update(b"\n");
    h.update(idea.as_bytes());
    let d = h.finalize();
    d[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn new_card(source_url: &str, idea: &str, now: u64) -> IdeaCard {
    let mapping = map_modules(idea);
    IdeaCard {
        id: card_id(source_url, idea),
        source_url: source_url.to_string(),
        idea: idea.to_string(),
        effort: estimate_effort(&mapping),
        ancientos_mapping: mapping,
        status: Status::New,
        created_at: now,
        park_until: None,
    }
}

pub fn append(path: &Path, card: &IdeaCard) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            crate::token_store::ensure_dir_0700(parent).map_err(|e| e.to_string())?;
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let line = serde_json::to_string(card).map_err(|e| e.to_string())?;
    writeln!(f, "{line}").map_err(|e| e.to_string())
}

/// Latest state per id, in first-seen order. Missing file -> empty. Bad line -> error (fail closed).
pub fn load(path: &Path) -> Result<Vec<IdeaCard>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut order = Vec::new();
    let mut map: BTreeMap<String, IdeaCard> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let c: IdeaCard = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        if !map.contains_key(&c.id) {
            order.push(c.id.clone());
        }
        map.insert(c.id.clone(), c);
    }
    Ok(order.into_iter().filter_map(|id| map.remove(&id)).collect())
}

/// Create cards for every idea in delegate output; skips ids already stored.
pub fn capture_from_delegate(
    path: &Path,
    source_url: &str,
    output: &str,
    now: u64,
) -> Result<Vec<IdeaCard>, String> {
    let existing = load(path)?;
    let mut made = Vec::new();
    for idea in extract_ideas(output) {
        let c = new_card(source_url, &idea, now);
        if existing.iter().any(|e| e.id == c.id) || made.iter().any(|m: &IdeaCard| m.id == c.id) {
            continue;
        }
        append(path, &c)?;
        made.push(c);
    }
    Ok(made)
}

/// New cards, plus parked cards whose `park_until` has passed.
pub fn list_due_for_triage(path: &Path, now: u64) -> Result<Vec<IdeaCard>, String> {
    Ok(load(path)?
        .into_iter()
        .filter(|c| match c.status {
            Status::New => true,
            Status::Parked => c.park_until.is_none_or(|t| t <= now),
            _ => false,
        })
        .collect())
}

/// Backlog item text for Dev.
pub fn backlog_item(card: &IdeaCard) -> String {
    format!(
        "- [idea_card {}] {} (maps to: {}; effort {:?}; source: {})",
        card.id,
        card.idea,
        card.ancientos_mapping.join(", "),
        card.effort,
        card.source_url
    )
}

/// Apply a triage decision. Adopt appends a backlog line to `backlog_path`.
pub fn triage(
    path: &Path,
    backlog_path: &Path,
    id: &str,
    decision: Triage,
    now: u64,
) -> Result<IdeaCard, String> {
    let mut card = load(path)?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| format!("no idea card with id {id}"))?;
    match decision {
        Triage::Adopt => {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(backlog_path)
                .map_err(|e| format!("open {}: {e}", backlog_path.display()))?;
            writeln!(f, "{}", backlog_item(&card)).map_err(|e| e.to_string())?;
            card.status = Status::Adopted;
            card.park_until = None;
        }
        Triage::Park => {
            card.status = Status::Parked;
            card.park_until = Some(now.saturating_add(PARK_SECS));
        }
        Triage::Drop => {
            card.status = Status::Dropped;
            card.park_until = None;
        }
    }
    append(path, &card)?;
    Ok(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "## Setups\n- synthetic setup\n## Worth borrowing for AncientOS\n- Drag a card to approve\n- Weekly wins list via scheduler\n* Something novel\n\n## Other\n- not this";

    #[test]
    fn extracts_only_borrowing_bullets() {
        let v = extract_ideas(OUT);
        assert_eq!(
            v,
            vec![
                "Drag a card to approve",
                "Weekly wins list via scheduler",
                "Something novel"
            ]
        );
    }

    #[test]
    fn maps_and_estimates() {
        assert_eq!(map_modules("Drag a card"), vec!["gate"]);
        assert_eq!(map_modules("zzz"), vec!["unmapped"]);
        let m = map_modules("shared memory log with approval gate on schedule");
        assert!(m.len() >= 3);
        assert_eq!(estimate_effort(&m), Effort::L);
    }

    #[test]
    fn capture_is_idempotent_and_folds() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("idea_cards.jsonl");
        let made = capture_from_delegate(&p, "https://x.com/a/status/1", OUT, 100).unwrap();
        assert_eq!(made.len(), 3);
        assert!(made
            .iter()
            .all(|c| c.status == Status::New && c.created_at == 100));
        let again = capture_from_delegate(&p, "https://x.com/a/status/1", OUT, 200).unwrap();
        assert!(again.is_empty());
        assert_eq!(load(&p).unwrap().len(), 3);
    }

    #[test]
    fn triage_flow() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("idea_cards.jsonl");
        let b = d.path().join("backlog.md");
        let cards = capture_from_delegate(&p, "https://x.com/a/status/1", OUT, 0).unwrap();
        assert_eq!(list_due_for_triage(&p, 0).unwrap().len(), 3);

        let a = triage(&p, &b, &cards[0].id, Triage::Adopt, 10).unwrap();
        assert_eq!(a.status, Status::Adopted);
        assert!(std::fs::read_to_string(&b)
            .unwrap()
            .contains("Drag a card to approve"));

        let pk = triage(&p, &b, &cards[1].id, Triage::Park, 10).unwrap();
        assert_eq!(pk.park_until, Some(10 + PARK_SECS));
        triage(&p, &b, &cards[2].id, Triage::Drop, 10).unwrap();

        assert!(list_due_for_triage(&p, 11).unwrap().is_empty());
        let due = list_due_for_triage(&p, 10 + PARK_SECS).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, cards[1].id);
        // Append-only: 3 creates + 3 triage lines.
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 6);
        assert!(triage(&p, &b, "nope", Triage::Drop, 0).is_err());
    }

    #[test]
    fn park_at_max_time_saturates() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("idea_cards.jsonl");
        let b = d.path().join("backlog.md");
        let cards = capture_from_delegate(&p, "https://x.com/a/status/1", OUT, 0).unwrap();
        let pk = triage(&p, &b, &cards[0].id, Triage::Park, u64::MAX).unwrap();
        assert_eq!(pk.park_until, Some(u64::MAX));
    }

    #[test]
    fn loads_demo_fixture() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/demo/expected_idea_cards.jsonl");
        let cards = load(&p).unwrap();
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].ancientos_mapping, vec!["gate"]);
        assert_eq!(
            cards[0].created_at,
            parse_rfc3339("2026-10-26T21:30:00Z").unwrap()
        );
        let parked = cards.iter().find(|c| c.status == Status::Parked).unwrap();
        assert_eq!(
            parked.park_until,
            parse_rfc3339("2026-11-26T00:00:00+00:00")
        );
        assert_eq!(parked.effort, Effort::L);
        // Due: the new one now; the parked one only after park_until.
        let now = parse_rfc3339("2026-10-27T00:00:00Z").unwrap();
        assert_eq!(list_due_for_triage(&p, now).unwrap().len(), 1);
        assert_eq!(
            list_due_for_triage(&p, parked.park_until.unwrap())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-10-26T17:30:00-04:00"),
            parse_rfc3339("2026-10-26T21:30:00Z")
        );
        assert_eq!(parse_rfc3339("nope"), None);
    }

    #[test]
    fn bad_line_fails_closed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("idea_cards.jsonl");
        std::fs::write(&p, "{not json}\n").unwrap();
        assert!(load(&p).is_err());
        assert!(load(&d.path().join("missing.jsonl")).unwrap().is_empty());
    }
}
