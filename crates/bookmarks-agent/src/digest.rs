//! Weekly triage digest: due idea cards (one at a time) plus "did you try it?" nudges.
//!
//! Pure functions over an injectable `now` (unix secs) and existing data:
//! idea cards from [`crate::idea_card`], try-it shown/pending records from
//! [`crate::try_it`] log entries. Ordering is deterministic everywhere:
//! cards by `(created_at, id)`, nudges by `(first_shown_at, post_id)`.
//!
//! Fail-closed edges: a parked card with no `park_until` is still shown as
//! due (never hidden); a shown record without a timestamp never nudges (no
//! nagging without evidence of age); timestamps in the future never trigger.

use crate::idea_card::{IdeaCard, Status};
use ancientos_core::log::{DecisionLog, Entry};
use std::collections::{BTreeMap, BTreeSet};

/// Seconds in the weekly digest cadence.
pub const WEEK_SECS: u64 = 7 * 24 * 60 * 60;
/// A pending try-it post nudges only when first shown strictly more than this ago.
pub const NUDGE_SECS: u64 = 3 * 24 * 60 * 60;
/// Log kind recording that a weekly digest was shown; payload carries [`DIGEST_AT_FIELD`].
pub const DIGEST_KIND: &str = "digest.weekly";
/// Payload field carrying the unix-secs time a digest was shown.
pub const DIGEST_AT_FIELD: &str = "at";

/// True when a weekly digest is due: never shown, or the last showing is a
/// week old (`last + 7 days <= now`). A future `last` (clock skew) is not due.
pub fn digest_due(last_digest_at: Option<u64>, now: u64) -> bool {
    match last_digest_at {
        None => true,
        Some(last) => last.saturating_add(WEEK_SECS) <= now,
    }
}

/// True when a `digest` run records its showing: a real run only, with
/// no `--dry-run` and no `--now` override (a `--now` clock is a test/demo
/// clock, so it never records).
pub fn should_record(dry_run: bool, now_override: bool) -> bool {
    !dry_run && !now_override
}

/// Append a [`DIGEST_KIND`] record that the digest was shown at `now`
/// (unix secs), resetting the weekly-due check. Metadata only.
pub fn record_shown(log: &mut DecisionLog, now: u64) -> Result<Entry, String> {
    log.append(DIGEST_KIND, serde_json::json!({ DIGEST_AT_FIELD: now }))
        .map_err(|e| e.to_string())
}

/// Latest `at` timestamp across [`DIGEST_KIND`] entries; entries with a
/// missing or non-numeric `at` are ignored.
pub fn last_digest_at(entries: &[Entry]) -> Option<u64> {
    entries
        .iter()
        .filter(|e| e.kind == DIGEST_KIND)
        .filter_map(|e| e.payload.get(DIGEST_AT_FIELD)?.as_u64())
        .max()
}

/// An idea card due for triage, flagged when it resurfaced from parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueCard {
    pub card: IdeaCard,
    pub resurfaced: bool,
}

/// Cards due for triage: `New`, plus `Parked` whose `park_until` has passed
/// (`park_until <= now`, flagged `resurfaced`). A parked card with no
/// `park_until` (legacy) is due but not flagged; adopted/dropped cards never
/// are. Sorted by `(created_at, id)`.
pub fn due_cards(cards: &[IdeaCard], now: u64) -> Vec<DueCard> {
    let mut out: Vec<DueCard> = cards
        .iter()
        .filter_map(|c| match c.status {
            Status::New => Some(DueCard {
                card: c.clone(),
                resurfaced: false,
            }),
            Status::Parked => match c.park_until {
                Some(t) if t <= now => Some(DueCard {
                    card: c.clone(),
                    resurfaced: true,
                }),
                None => Some(DueCard {
                    card: c.clone(),
                    resurfaced: false,
                }),
                _ => None,
            },
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| (a.card.created_at, &a.card.id).cmp(&(b.card.created_at, &b.card.id)));
    out
}

/// A pending try-it post old enough to nudge about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TryNudge {
    pub post_id: String,
    pub first_shown_at: u64,
}

/// First timestamped showing per post from `try_it.shown` entries (minimum
/// `shown_at` wins). Entries without a timestamp are ignored.
pub fn first_shown(entries: &[Entry]) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    for e in entries
        .iter()
        .filter(|e| e.kind == crate::try_it::SHOWN_KIND)
    {
        let (Some(id), Some(at)) = (
            e.payload.get("post_id").and_then(|v| v.as_str()),
            crate::try_it::shown_at_of(&e.payload),
        ) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        out.entry(id.to_string())
            .and_modify(|t| *t = (*t).min(at))
            .or_insert(at);
    }
    out
}

/// Pending posts first shown strictly more than 3 days before `now`
/// (`now - first_shown > NUDGE_SECS`; exactly 3 days does not nudge).
/// Posts with no timestamped showing, or shown in the future, never nudge.
/// Sorted by `(first_shown_at, post_id)`.
pub fn try_nudges(pending: &[String], entries: &[Entry], now: u64) -> Vec<TryNudge> {
    let shown = first_shown(entries);
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for id in pending {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(at) = shown.get(id) {
            if now.saturating_sub(*at) > NUDGE_SECS {
                out.push(TryNudge {
                    post_id: id.clone(),
                    first_shown_at: *at,
                });
            }
        }
    }
    out.sort_by(|a, b| (a.first_shown_at, &a.post_id).cmp(&(b.first_shown_at, &b.post_id)));
    out
}

/// Everything the `digest` command prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeeklyDigest {
    pub now: u64,
    pub last_digest_at: Option<u64>,
    pub due: bool,
    pub cards: Vec<DueCard>,
    pub nudges: Vec<TryNudge>,
}

/// Assemble the digest from idea cards, the pending try-it list (see
/// [`crate::try_it::pending_followups`]), and decision-log entries.
pub fn weekly_digest(
    cards: &[IdeaCard],
    pending: &[String],
    entries: &[Entry],
    now: u64,
) -> WeeklyDigest {
    let last = last_digest_at(entries);
    WeeklyDigest {
        now,
        last_digest_at: last,
        due: digest_due(last, now),
        cards: due_cards(cards, now),
        nudges: try_nudges(pending, entries, now),
    }
}

/// Hidden authors from the `feedback` entries in `entries`
/// (malformed payloads are ignored: they prove no hide).
fn hidden_authors_from_entries(entries: &[Entry]) -> BTreeSet<String> {
    let records: Vec<crate::feedback::FeedbackRecord> = entries
        .iter()
        .filter(|e| e.kind == crate::feedback::FEEDBACK_KIND)
        .filter_map(|e| serde_json::from_value(e.payload.clone()).ok())
        .collect();
    crate::feedback::hidden_authors(&records)
}

/// Assemble the digest like [`weekly_digest`], skipping cards and nudges
/// by a hidden author (see [`crate::author_filter`]). Skipped silently: no
/// message, no placeholder. Cards and nudges are never hidden for language
/// (see [`crate::translate`]): these surfaces show ids and locally generated
/// idea text, never post text, so every card and nudge is kept and the
/// translation (with its `Translated from` / `Untranslated` marker plus the
/// original) renders wherever the post text itself is shown.
pub fn weekly_digest_for_bookmarks(
    cards: &[IdeaCard],
    pending: &[String],
    entries: &[Entry],
    now: u64,
    bookmarks: &[crate::fixture::Bookmark],
) -> WeeklyDigest {
    let hidden = hidden_authors_from_entries(entries);
    let cards: Vec<IdeaCard> = cards
        .iter()
        .filter(|c| {
            crate::author_filter::source_post_is_hidden(&c.source_url, bookmarks, &hidden)
                != Some(true)
        })
        .cloned()
        .collect();
    let pending = crate::author_filter::retain_not_hidden(pending, bookmarks, &hidden);
    weekly_digest(&cards, &pending, entries, now)
}

/// One due card, presented one at a time (`index/total`, 1-based) with the
/// adopt/park/drop triage commands.
pub fn render_card(card: &IdeaCard, resurfaced: bool, index: usize, total: usize) -> String {
    let flag = if resurfaced { "(resurfaced) " } else { "" };
    format!(
        "[{index}/{total}] {flag}{} {:?} {} {} {}\n  adopt: bookmarks-agent triage {} adopt\n  park:  bookmarks-agent triage {} park\n  drop:  bookmarks-agent triage {} drop\n",
        card.id,
        card.effort,
        card.ancientos_mapping.join(","),
        card.idea,
        card.source_url,
        card.id,
        card.id,
        card.id,
    )
}

/// Plain-text digest: due status, due cards one at a time, try-it nudges.
pub fn render(d: &WeeklyDigest) -> String {
    let mut out = String::new();
    out.push_str(&format!("Weekly triage digest (now {})\n", d.now));
    match d.last_digest_at {
        None => out.push_str("Status: DUE (no digest recorded yet)\n"),
        Some(last) if d.due => {
            out.push_str(&format!("Status: DUE (last digest {last})\n"));
        }
        Some(last) => {
            out.push_str(&format!("Status: not due (last digest {last})\n"));
        }
    }
    out.push_str(&format!("Idea cards due for triage ({})\n", d.cards.len()));
    if d.cards.is_empty() {
        out.push_str("  none\n");
    } else {
        let total = d.cards.len();
        for (i, due) in d.cards.iter().enumerate() {
            out.push_str(&render_card(&due.card, due.resurfaced, i + 1, total));
        }
    }
    out.push_str(&format!("Did you try it ({})\n", d.nudges.len()));
    if d.nudges.is_empty() {
        out.push_str("  none\n");
    } else {
        for n in &d.nudges {
            out.push_str(&format!(
                "  {} (first shown {})\n",
                n.post_id, n.first_shown_at
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idea_card::{Effort, Status};
    use ancientos_core::log::DecisionLog;

    const NOW: u64 = 1_800_000_000;

    fn card(id: &str, status: Status, created_at: u64, park_until: Option<u64>) -> IdeaCard {
        IdeaCard {
            id: id.into(),
            source_url: format!("https://x.com/i/status/{id}"),
            idea: format!("synthetic idea {id}"),
            ancientos_mapping: vec!["gate".to_string()],
            effort: Effort::S,
            status,
            created_at,
            park_until,
        }
    }

    fn digest_entries(lasts: &[u64]) -> Vec<Entry> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for at in lasts {
            log.append(DIGEST_KIND, serde_json::json!({"at": at}))
                .unwrap();
        }
        // Malformed digest records are ignored, not fatal.
        log.append(DIGEST_KIND, serde_json::json!({})).unwrap();
        log.append(DIGEST_KIND, serde_json::json!({"at": "last friday"}))
            .unwrap();
        DecisionLog::open(&path).unwrap().verify().unwrap()
    }

    fn shown_entries(shown: &[(&str, Option<u64>)], resolved: &[&str]) -> Vec<Entry> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for (id, at) in shown {
            match at {
                Some(t) => crate::try_it::record_shown(&mut log, id, *t).unwrap(),
                None => log
                    .append(
                        crate::try_it::SHOWN_KIND,
                        serde_json::json!({"post_id": id}),
                    )
                    .unwrap(),
            };
        }
        for id in resolved {
            crate::try_it::record_followup(&mut log, id, crate::try_it::Tried::Yes, None).unwrap();
        }
        DecisionLog::open(&path).unwrap().verify().unwrap()
    }

    fn pending_of(entries: &[Entry]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for e in entries {
            log.append(&e.kind, e.payload.clone()).unwrap();
        }
        crate::try_it::pending_followups(&log)
    }

    #[test]
    fn due_when_never_shown_or_week_old() {
        assert!(digest_due(None, NOW));
        assert!(digest_due(Some(NOW - WEEK_SECS), NOW), "exactly 7 days");
        assert!(digest_due(Some(NOW - WEEK_SECS - 1), NOW));
        assert!(!digest_due(Some(NOW - WEEK_SECS + 1), NOW));
        assert!(!digest_due(Some(NOW), NOW));
        assert!(!digest_due(Some(NOW + 100), NOW), "future last");
        assert!(digest_due(Some(0), u64::MAX), "saturating add");
    }

    #[test]
    fn last_digest_takes_max_and_ignores_malformed() {
        assert_eq!(last_digest_at(&[]), None);
        let entries = digest_entries(&[100, 300, 200]);
        assert_eq!(last_digest_at(&entries), Some(300));
    }

    #[test]
    fn should_record_truth_table() {
        assert!(should_record(false, false), "real run records");
        assert!(!should_record(true, false), "--dry-run never records");
        assert!(!should_record(false, true), "--now clock never records");
        assert!(!should_record(true, true));
    }

    #[test]
    fn record_shown_resets_weekly_due_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        let entry = record_shown(&mut log, NOW).unwrap();
        assert_eq!(entry.kind, DIGEST_KIND);
        assert_eq!(entry.payload, serde_json::json!({"at": NOW}));
        let entries = DecisionLog::open(&path).unwrap().verify().unwrap();
        let last = last_digest_at(&entries);
        assert_eq!(last, Some(NOW));
        assert!(!digest_due(last, NOW + WEEK_SECS - 1));
        assert!(digest_due(last, NOW + WEEK_SECS), "exactly 7 days is due");
    }

    #[test]
    fn dry_run_appends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        // Mirror cmd_digest's gate: only real runs append.
        for (dry_run, now_override) in [(true, false), (false, true), (true, true)] {
            if should_record(dry_run, now_override) {
                record_shown(&mut log, NOW).unwrap();
            }
        }
        assert!(log.verify_or_empty().unwrap().is_empty());
    }

    #[test]
    fn two_records_max_wins_either_order() {
        for order in [[NOW, NOW - 100], [NOW - 100, NOW]] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("d.jsonl");
            let mut log = DecisionLog::open(&path).unwrap();
            for at in order {
                record_shown(&mut log, at).unwrap();
            }
            let entries = log.verify_or_empty().unwrap();
            assert_eq!(last_digest_at(&entries), Some(NOW), "{order:?}");
        }
    }

    #[test]
    fn due_cards_cover_new_and_parked_boundary() {
        let cards = vec![
            card("new-b", Status::New, 20, None),
            card("new-a", Status::New, 10, None),
            card("park-due", Status::Parked, 5, Some(NOW)),
            card("park-old", Status::Parked, 6, Some(NOW - 1)),
            card("park-future", Status::Parked, 7, Some(NOW + 1)),
            card("park-legacy", Status::Parked, 8, None),
            card("adopted", Status::Adopted, 1, None),
            card("dropped", Status::Dropped, 2, None),
        ];
        let due = due_cards(&cards, NOW);
        let ids: Vec<(&str, bool)> = due
            .iter()
            .map(|d| (d.card.id.as_str(), d.resurfaced))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("park-due", true),
                ("park-old", true),
                ("park-legacy", false),
                ("new-a", false),
                ("new-b", false),
            ],
            "ordered by (created_at, id); exactly park_until is due"
        );
    }

    #[test]
    fn due_cards_tie_break_on_id() {
        let cards = vec![
            card("b", Status::New, 10, None),
            card("a", Status::New, 10, None),
        ];
        let due = due_cards(&cards, NOW);
        assert_eq!(due[0].card.id, "a");
        assert_eq!(due[1].card.id, "b");
    }

    #[test]
    fn nudges_fire_only_after_three_full_days() {
        let entries = shown_entries(
            &[
                ("old", Some(NOW - NUDGE_SECS - 1)),
                ("exact", Some(NOW - NUDGE_SECS)),
                ("fresh", Some(NOW - NUDGE_SECS + 1)),
                ("future", Some(NOW + 60)),
                ("legacy", None),
                ("repeat", Some(NOW - NUDGE_SECS - 10)),
                ("repeat", Some(NOW - 10)),
            ],
            &["done"],
        );
        // "done" was shown long ago but resolved; "ghost" was never shown.
        let mut entries = entries;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("extra.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        crate::try_it::record_shown(&mut log, "done", NOW - NUDGE_SECS - 100).unwrap();
        crate::try_it::record_followup(&mut log, "done", crate::try_it::Tried::No, None).unwrap();
        entries.extend(DecisionLog::open(&path).unwrap().verify().unwrap());
        let pending = pending_of(&entries);
        assert!(pending.contains(&"old".to_string()));
        assert!(!pending.contains(&"done".to_string()), "resolved");
        assert!(!pending.contains(&"ghost".to_string()), "never shown");
        let nudges = try_nudges(&pending, &entries, NOW);
        let ids: Vec<&str> = nudges.iter().map(|n| n.post_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["repeat", "old"],
            "oldest first; exact-3-days excluded"
        );
        assert_eq!(nudges[0].first_shown_at, NOW - NUDGE_SECS - 10);
        assert!(!ids.contains(&"exact"), "exactly 3 days does not nudge");
        assert!(!ids.contains(&"fresh"));
        assert!(!ids.contains(&"future"), "future showing never nudges");
        assert!(!ids.contains(&"legacy"), "untimestamped never nudges");
    }

    #[test]
    fn nudges_dedupe_pending_and_tie_break_on_id() {
        let entries = shown_entries(&[("b", Some(100)), ("a", Some(100))], &[]);
        let pending = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        let nudges = try_nudges(&pending, &entries, 100 + NUDGE_SECS + 1);
        let ids: Vec<&str> = nudges.iter().map(|n| n.post_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn render_shows_index_total_options_and_nudges() {
        let d = WeeklyDigest {
            now: NOW,
            last_digest_at: None,
            due: true,
            cards: vec![
                DueCard {
                    card: card("c1", Status::New, 1, None),
                    resurfaced: false,
                },
                DueCard {
                    card: card("c2", Status::Parked, 2, Some(NOW)),
                    resurfaced: true,
                },
            ],
            nudges: vec![TryNudge {
                post_id: "p9".to_string(),
                first_shown_at: NOW - NUDGE_SECS - 5,
            }],
        };
        let text = render(&d);
        assert!(text.contains("Status: DUE"), "{text}");
        assert!(text.contains("[1/2] c1"), "{text}");
        assert!(text.contains("[2/2] (resurfaced) c2"), "{text}");
        for verb in ["adopt", "park", "drop"] {
            assert!(text.contains(&format!("triage c1 {verb}")), "{text}");
            assert!(text.contains(&format!("triage c2 {verb}")), "{text}");
        }
        assert!(text.contains("p9 (first shown"), "{text}");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn render_empty_sections_say_none() {
        let d = WeeklyDigest {
            now: NOW,
            last_digest_at: Some(NOW),
            due: false,
            cards: Vec::new(),
            nudges: Vec::new(),
        };
        let text = render(&d);
        assert!(text.contains("Status: not due"), "{text}");
        assert!(text.contains("triage (0)"), "{text}");
        assert!(text.contains("Did you try it (0)"), "{text}");
    }

    #[test]
    fn weekly_digest_assembles_from_log_entries() {
        let cards = vec![card("n1", Status::New, 1, None)];
        let mut entries = digest_entries(&[NOW - WEEK_SECS]);
        entries.extend(shown_entries(&[("p1", Some(NOW - NUDGE_SECS - 1))], &[]));
        let pending = pending_of(&entries);
        let d = weekly_digest(&cards, &pending, &entries, NOW);
        assert!(d.due, "last digest exactly a week ago");
        assert_eq!(d.last_digest_at, Some(NOW - WEEK_SECS));
        assert_eq!(d.cards.len(), 1);
        assert_eq!(d.nudges.len(), 1);
        assert_eq!(d.nudges[0].post_id, "p1");
    }

    fn stored_post(id: &str, text: &str, lang: Option<&str>) -> crate::fixture::Bookmark {
        crate::fixture::Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: text.into(),
            author_username: Some(format!("syn_{id}")),
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: lang.map(str::to_string),
            urls: Vec::new(),
        }
    }

    fn sourced_card(id: &str, post_id: &str) -> IdeaCard {
        let mut c = card(id, Status::New, 1, None);
        c.source_url = format!("https://x.com/i/status/{post_id}");
        c
    }

    fn authored_post(id: &str, author: Option<&str>) -> crate::fixture::Bookmark {
        crate::fixture::Bookmark {
            post_id: id.into(),
            url: format!("https://x.com/i/status/{id}"),
            text: "[synthetic] The government presented an economic plan to support small local businesses.".into(),
            author_username: author.map(str::to_string),
            created_at: None,
            folders: None,
            label: None,
            had_note_tweet: false,
            media: Vec::new(),
            lang: Some("en".into()),
            urls: Vec::new(),
        }
    }

    fn feedback_entries(hides: &[&str], unhides: &[&str]) -> Vec<Entry> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for name in hides {
            log.append(
                crate::feedback::FEEDBACK_KIND,
                serde_json::json!({
                    "pick_id": name,
                    "category": "unknown",
                    "verdict": "never_author",
                }),
            )
            .unwrap();
        }
        for name in unhides {
            log.append(
                crate::feedback::FEEDBACK_KIND,
                serde_json::json!({
                    "pick_id": name,
                    "category": "unknown",
                    "verdict": "unhide_author",
                }),
            )
            .unwrap();
        }
        // Malformed feedback proves no hide and never breaks the digest.
        log.append(
            crate::feedback::FEEDBACK_KIND,
            serde_json::json!({"pick_id": 42}),
        )
        .unwrap();
        DecisionLog::open(&path).unwrap().verify().unwrap()
    }

    #[test]
    fn digest_for_bookmarks_skips_hidden_authors_and_keeps_unknown() {
        let bookmarks = vec![
            authored_post("good1", Some("syn_good")),
            authored_post("bad1", Some("@Syn_Bad")),
            authored_post("noauthor", None),
        ];
        let cards = vec![
            sourced_card("card-good", "good1"),
            sourced_card("card-bad", "bad1"),
            sourced_card("card-ghost", "ghost"),
        ];
        let mut entries = shown_entries(
            &[
                ("good1", Some(NOW - NUDGE_SECS - 1)),
                ("bad1", Some(NOW - NUDGE_SECS - 1)),
                ("ghost", Some(NOW - NUDGE_SECS - 1)),
                ("noauthor", Some(NOW - NUDGE_SECS - 1)),
            ],
            &[],
        );
        entries.extend(feedback_entries(&["syn_bad"], &[]));
        let pending = pending_of(&entries);
        assert_eq!(pending.len(), 4);
        let d = weekly_digest_for_bookmarks(&cards, &pending, &entries, NOW, &bookmarks);
        let card_ids: Vec<&str> = d.cards.iter().map(|c| c.card.id.as_str()).collect();
        assert_eq!(card_ids, vec!["card-ghost", "card-good"]);
        let nudge_ids: Vec<&str> = d.nudges.iter().map(|n| n.post_id.as_str()).collect();
        assert_eq!(nudge_ids, vec!["ghost", "good1", "noauthor"]);
        let text = render(&d);
        assert!(!text.contains("bad1"), "{text}");
        assert!(!text.contains("card-bad"), "{text}");
    }

    #[test]
    fn digest_unhide_author_restores_nudges() {
        let bookmarks = vec![authored_post("bad1", Some("syn_bad"))];
        let cards = vec![sourced_card("card-bad", "bad1")];
        let mut entries = shown_entries(&[("bad1", Some(NOW - NUDGE_SECS - 1))], &[]);
        entries.extend(feedback_entries(&["syn_bad"], &["@SYN_BAD"]));
        let pending = pending_of(&entries);
        let d = weekly_digest_for_bookmarks(&cards, &pending, &entries, NOW, &bookmarks);
        assert_eq!(d.cards.len(), 1);
        assert_eq!(d.nudges.len(), 1);
    }

    #[test]
    fn digest_for_bookmarks_keeps_non_english_and_shows_translation() {
        let es_text =
            "[synthetic] El gobierno presentó un plan económico para apoyar pequeñas empresas locales.";
        let bookmarks = vec![
            stored_post(
                "en1",
                "[synthetic] The government presented an economic plan to support small local businesses.",
                None,
            ),
            stored_post("es1", es_text, Some("es")),
        ];
        let cards = vec![
            sourced_card("card-en", "en1"),
            sourced_card("card-es", "es1"),
            sourced_card("card-ghost", "ghost"),
        ];
        let entries = shown_entries(
            &[
                ("en1", Some(NOW - NUDGE_SECS - 1)),
                ("es1", Some(NOW - NUDGE_SECS - 1)),
                ("ghost", Some(NOW - NUDGE_SECS - 1)),
            ],
            &[],
        );
        let pending = pending_of(&entries);
        assert_eq!(pending.len(), 3);
        // Nothing is hidden for language: all cards and nudges are kept.
        let d = weekly_digest_for_bookmarks(&cards, &pending, &entries, NOW, &bookmarks);
        let card_ids: Vec<&str> = d.cards.iter().map(|c| c.card.id.as_str()).collect();
        assert_eq!(card_ids, vec!["card-en", "card-es", "card-ghost"]);
        let nudge_ids: Vec<&str> = d.nudges.iter().map(|n| n.post_id.as_str()).collect();
        assert_eq!(nudge_ids, vec!["en1", "es1", "ghost"]);
        let text = render(&d);
        assert!(text.contains("es1"), "{text}");
        assert!(text.contains("card-es"), "{text}");
        // Wherever the post text renders, it shows the marker plus both
        // texts: cached translation plus the original...
        let es_english = "[synthetic] Translated English for es1.";
        let mut cache = std::collections::HashMap::new();
        cache.insert(
            crate::translate::cache_key("es1", es_text),
            crate::translate::CacheEntry {
                english: es_english.to_string(),
                source_lang: Some("es".to_string()),
            },
        );
        let es = bookmarks.iter().find(|b| b.post_id == "es1").unwrap();
        let t = crate::translate::cached_only(es, &cache);
        assert_eq!(
            crate::translate::marker_for(es, &t).as_deref(),
            Some("Translated from es")
        );
        assert_eq!(crate::translate::english_for(es, &t), es_english);
        assert_eq!(t.original, es_text);
        // ...or the original plus the untranslated marker on a cache miss.
        let t = crate::translate::cached_only(es, &std::collections::HashMap::new());
        assert_eq!(
            crate::translate::marker_for(es, &t).as_deref(),
            Some("Untranslated (es)")
        );
        assert_eq!(crate::translate::english_for(es, &t), es_text);
    }
}
