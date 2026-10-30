//! `ancientos-log` — read-only operator CLI for decision-log integrity and replay.
//!
//! - `verify PATH` checks the hash chain: exit 0 with the entry count, or
//!   exit 1 naming the first broken line and why it broke.
//! - `show PATH [--kind K] [--since RFC3339] [--limit N]` prints one
//!   redacted line per entry. `--since` keeps entries whose payload carries
//!   a unix-secs timestamp at/after the bound; entries without a recognized
//!   timestamp are skipped when `--since` is given.
//! - `replay-approvals PATH` folds the approval queue and prints one line
//!   per request lifecycle (status is folded from the log only, no clock).
//!
//! Feedback/verdict replay lives with the bookmarks app (its `feedback`
//! module), not here: core stays app-agnostic (principle 7).
//!
//! Read-only: the only file I/O is one read-only open per command. A missing
//! file is an error (fail closed), and any parse failure stops the command
//! with exit 1 before printing anything.
use ancientos_core::approvals::Approval;
use ancientos_core::log::{load_verified, Entry};
use ancientos_core::{approvals, redact};
use std::path::Path;

const USAGE: &str = "usage: ancientos-log verify PATH | show PATH [--kind KIND] [--since RFC3339] [--limit N] | replay-approvals PATH";

/// Payload keys holding unix-secs record times, first hit wins.
const TIME_KEYS: [&str; 8] = [
    "requested_at",
    "decided_at",
    "expired_at",
    "proposed_at",
    "queued_at",
    "shown_at",
    "done_at",
    "at",
];

/// Unix-secs record time of an entry, or `None` when the payload carries no
/// recognized timestamp (routes, feedback, threshold changes, ...).
fn entry_time(e: &Entry) -> Option<u64> {
    TIME_KEYS
        .iter()
        .filter_map(|k| e.payload.get(k)?.as_u64())
        .next()
}

/// Number of days in a month, leap-year aware.
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's days-from-civil).
fn days_from_civil(year: u32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse strict RFC3339 (`YYYY-MM-DDTHH:MM:SSZ`, optional fractional seconds,
/// `Z` or `±HH:MM` offset) to unix epoch seconds. Naive datetimes,
/// out-of-range fields and pre-1970 instants are errors (fail closed).
fn parse_rfc3339_to_epoch(s: &str) -> Result<u64, String> {
    let err = || format!("bad --since {s:?}: want RFC3339 like 2026-01-02T03:04:05Z");
    let b = s.as_bytes();
    if b.len() < 20 {
        return Err(err());
    }
    let num = |lo: usize, hi: usize| -> Option<u32> {
        if hi > b.len() {
            return None;
        }
        let mut v: u32 = 0;
        for &c in &b[lo..hi] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + u32::from(c - b'0');
        }
        Some(v)
    };
    let year = num(0, 4).ok_or_else(err)?;
    if b[4] != b'-' || b[7] != b'-' {
        return Err(err());
    }
    let month = num(5, 7).ok_or_else(err)?;
    let day = num(8, 10).ok_or_else(err)?;
    if b[10] != b'T' && b[10] != b't' {
        return Err(err());
    }
    let hour = num(11, 13).ok_or_else(err)?;
    if b[13] != b':' {
        return Err(err());
    }
    let minute = num(14, 16).ok_or_else(err)?;
    if b[16] != b':' {
        return Err(err());
    }
    let second = num(17, 19).ok_or_else(err)?;
    let mut i = 19;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return Err(err());
        }
    }
    let offset_secs: i64 = if i < b.len() && (b[i] == b'Z' || b[i] == b'z') {
        i += 1;
        0
    } else if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        let neg = b[i] == b'-';
        let oh = num(i + 1, i + 3).ok_or_else(err)?;
        if b.get(i + 3) != Some(&b':') {
            return Err(err());
        }
        let om = num(i + 4, i + 6).ok_or_else(err)?;
        if oh > 23 || om > 59 {
            return Err(err());
        }
        i += 6;
        let off = i64::from(oh) * 3600 + i64::from(om) * 60;
        if neg {
            -off
        } else {
            off
        }
    } else {
        return Err(err());
    };
    if i != b.len() {
        return Err(err());
    }
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) {
        return Err(err());
    }
    if day < 1 || day > days_in_month(year, month) {
        return Err(err());
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(err());
    }
    let days = days_from_civil(year, month, day);
    let epoch = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)
        - offset_secs;
    u64::try_from(epoch).map_err(|_| err())
}

struct ShowOpts {
    kind: Option<String>,
    since: Option<u64>,
    limit: Option<usize>,
}

/// Read a flag value, either inline (`--k=v`) or from the next argument.
fn show_flag_value(args: &[String], i: &mut usize, inline: Option<&str>) -> Result<String, String> {
    if let Some(v) = inline {
        return Ok(v.to_string());
    }
    *i += 1;
    args.get(*i).cloned().ok_or_else(|| USAGE.to_string())
}

/// Parse `show` flags (`--flag value` or `--flag=value`). Unknown, empty,
/// duplicated or malformed flags are usage errors (fail closed).
fn parse_show_args(args: &[String]) -> Result<ShowOpts, String> {
    let mut opts = ShowOpts {
        kind: None,
        since: None,
        limit: None,
    };
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (args[i].as_str(), None),
        };
        match flag {
            "--kind" => {
                if opts.kind.is_some() {
                    return Err(USAGE.into());
                }
                let v = show_flag_value(args, &mut i, inline)?;
                if v.is_empty() {
                    return Err(USAGE.into());
                }
                opts.kind = Some(v);
            }
            "--since" => {
                if opts.since.is_some() {
                    return Err(USAGE.into());
                }
                let raw = show_flag_value(args, &mut i, inline)?;
                opts.since = Some(parse_rfc3339_to_epoch(&raw)?);
            }
            "--limit" => {
                if opts.limit.is_some() {
                    return Err(USAGE.into());
                }
                let raw = show_flag_value(args, &mut i, inline)?;
                let n: usize = raw
                    .parse()
                    .map_err(|_| format!("bad --limit {raw:?}: want a positive integer"))?;
                if n == 0 {
                    return Err(format!("bad --limit {raw:?}: want a positive integer"));
                }
                opts.limit = Some(n);
            }
            _ => return Err(USAGE.into()),
        }
        i += 1;
    }
    Ok(opts)
}

/// One display line per entry; secret-looking keys and embedded secrets are
/// redacted again on display (old or hand-written entries may predate
/// write-time redaction).
fn format_entry(e: &Entry) -> String {
    let (scrubbed, _) = redact::redact_json(&e.payload);
    format!("#{} {:<20} {}", e.seq, e.kind, scrubbed)
}

/// Collapse newlines so one entry prints as one line.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// One line per request lifecycle: identity, folded status, decision when
/// present, payload hash and a redacted single-line summary.
fn format_approval(a: &Approval) -> String {
    let r = &a.request;
    let mut s = format!(
        "{} agent={} action={} status={} requested_at={} expires_at={}",
        r.id,
        r.agent,
        r.action,
        a.status.as_str(),
        r.requested_at,
        r.expires_at
    );
    if let Some(d) = &a.decision {
        s.push_str(&format!(
            " decided_by={} decided_at={}",
            d.decided_by, d.decided_at
        ));
        if let Some(reason) = &d.reason {
            s.push_str(&format!(" reason={}", one_line(reason)));
        }
    }
    s.push_str(&format!(" payload_hash={}", r.payload_hash));
    let summary = redact::redact_text(&r.summary).text;
    s.push_str(&format!(" summary={}", one_line(&summary)));
    s
}

/// `verify`: check the hash chain and print the entry count.
fn cmd_verify(path: &str) -> Result<(), String> {
    let entries = load_verified(Path::new(path)).map_err(|e| e.to_string())?;
    let n = entries.len();
    println!(
        "hash chain verified: {n} {}",
        if n == 1 { "entry" } else { "entries" }
    );
    Ok(())
}

/// `show`: print matching entries, redacted, one per line.
fn cmd_show(path: &str, opts: &ShowOpts) -> Result<(), String> {
    let entries = load_verified(Path::new(path)).map_err(|e| e.to_string())?;
    let mut shown = 0usize;
    for e in &entries {
        if let Some(k) = &opts.kind {
            if e.kind != *k {
                continue;
            }
        }
        if let Some(since) = opts.since {
            match entry_time(e) {
                Some(t) if t >= since => {}
                _ => continue,
            }
        }
        println!("{}", format_entry(e));
        shown += 1;
        if opts.limit.is_some_and(|n| shown >= n) {
            break;
        }
    }
    Ok(())
}

/// `replay-approvals`: print each folded approval lifecycle.
fn cmd_replay_approvals(path: &str) -> Result<(), String> {
    let entries = load_verified(Path::new(path)).map_err(|e| e.to_string())?;
    for a in approvals::replay(&entries) {
        println!("{}", format_approval(&a));
    }
    Ok(())
}

/// Dispatch a subcommand; unknown input returns the usage line.
fn run(args: &[String]) -> Result<(), String> {
    match args {
        [cmd, path] if cmd == "verify" => cmd_verify(path),
        [cmd, path] if cmd == "replay-approvals" => cmd_replay_approvals(path),
        [cmd, path, rest @ ..] if cmd == "show" => cmd_show(path, &parse_show_args(rest)?),
        _ => Err(USAGE.into()),
    }
}

/// Entry point: run and exit 1 on any error (fail closed).
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancientos_core::approvals::{ApprovalRequest, DecisionInfo, Status};

    #[test]
    fn rfc3339_vectors() {
        for (s, want) in [
            ("1970-01-01T00:00:00Z", 0),
            ("2023-11-14T22:13:20Z", 1_700_000_000),
            ("2023-11-14T22:13:20z", 1_700_000_000),
            ("2023-11-14t22:13:20Z", 1_700_000_000),
            ("2023-11-14T22:13:20.999Z", 1_700_000_000),
            ("2023-11-14T22:13:20+00:00", 1_700_000_000),
            ("2023-11-15T00:13:20+02:00", 1_700_000_000),
            ("2023-11-14T17:13:20-05:00", 1_700_000_000),
            ("2024-02-29T00:00:00Z", 1_709_164_800),
            ("2000-02-29T12:00:00Z", 951_825_600),
            ("2030-01-01T00:00:00Z", 1_893_456_000),
            ("2026-10-26T00:00:00Z", 1_792_972_800),
        ] {
            assert_eq!(parse_rfc3339_to_epoch(s).unwrap(), want, "{s}");
        }
    }

    #[test]
    fn rfc3339_rejects_anything_else() {
        for s in [
            "",
            "yesterday",
            "2023-11-14 22:13:20",
            "2023-11-14T22:13:20",      // naive: no offset
            "2023-11-14T22:13:20.",     // empty fraction
            "2023-11-14T22:13:20+0200", // offset needs the colon
            "2023-11-14T22:13:20+02:60",
            "2023-11-14T22:13:20+24:00",
            "2023-13-14T22:13:20Z",
            "2023-11-32T22:13:20Z",
            "2023-02-29T22:13:20Z", // 2023 is not a leap year
            "2023-11-14T24:13:20Z",
            "2023-11-14T22:63:20Z",
            "2023-11-14T22:13:60Z",
            "1969-12-31T23:59:59Z", // pre-epoch
            "2023-11-14T22:13:20Z ",
            " 2023-11-14T22:13:20Z",
        ] {
            assert!(parse_rfc3339_to_epoch(s).is_err(), "{s:?}");
        }
    }

    /// Test helper: a synthetic unsigned entry with the given payload.
    fn entry_with(payload: serde_json::Value) -> Entry {
        Entry {
            seq: 7,
            prev_hash: "prev".into(),
            kind: "route".into(),
            payload,
            hash: "hash".into(),
            sig: None,
        }
    }

    #[test]
    fn entry_time_prefers_record_fields() {
        assert_eq!(
            entry_time(&entry_with(serde_json::json!({"at": 5}))),
            Some(5)
        );
        assert_eq!(
            entry_time(&entry_with(serde_json::json!({"at": 5, "queued_at": 6}))),
            Some(6)
        );
        assert_eq!(
            entry_time(&entry_with(
                serde_json::json!({"requested_at": 1, "expires_at": 2})
            )),
            Some(1)
        );
        assert_eq!(
            entry_time(&entry_with(serde_json::json!({"to": "local"}))),
            None
        );
        assert_eq!(
            entry_time(&entry_with(serde_json::json!({"at": "soon"}))),
            None
        );
    }

    #[test]
    fn show_line_redacts_secrets_on_display() {
        // Scanner-safe synthetic values, built at runtime (never real keys).
        let key_value = format!("synthetic-key-material-{}", "K".repeat(12));
        let bearer = format!("SYNTHETIC-{}", "T".repeat(16));
        let e = entry_with(serde_json::json!({
            "api_key": key_value,
            "note": format!("call with Bearer {bearer} please"),
            "total_tokens": 150,
        }));
        let line = format_entry(&e);
        assert!(line.starts_with("#7 route"), "{line}");
        assert!(!line.contains(&key_value), "{line}");
        assert!(!line.contains(&bearer), "{line}");
        assert!(line.contains("[redacted:json_key]"), "{line}");
        assert!(line.contains("[redacted:bearer_token]"), "{line}");
        assert!(line.contains("\"total_tokens\":150"), "{line}");
    }

    #[test]
    fn approval_line_redacts_summary_and_stays_single_line() {
        let req = ApprovalRequest::new(
            "bookmarks-agent",
            "x.post",
            "first line\nsecond line",
            serde_json::json!({"draft": "synthetic"}),
            100,
            200,
        )
        .unwrap();
        let approved = Approval {
            request: req.clone(),
            status: Status::Approved,
            decision: Some(DecisionInfo {
                outcome: Status::Approved,
                decided_by: "human".into(),
                decided_at: 150,
                reason: None,
            }),
        };
        let line = format_approval(&approved);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("status=approved"), "{line}");
        assert!(line.contains("decided_by=human decided_at=150"), "{line}");
        assert!(
            line.contains(&format!("payload_hash={}", req.payload_hash)),
            "{line}"
        );
        assert!(line.contains("summary=first line second line"), "{line}");
        let denied = Approval {
            request: req,
            status: Status::Denied,
            decision: Some(DecisionInfo {
                outcome: Status::Denied,
                decided_by: "human".into(),
                decided_at: 160,
                reason: Some("not\ntoday".into()),
            }),
        };
        let line = format_approval(&denied);
        assert!(line.contains("reason=not today"), "{line}");
    }

    /// Test helper: turn string slices into owned args.
    fn show_args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn show_flags_accept_space_and_equals_forms() {
        let o = parse_show_args(&show_args(&["--kind", "feedback", "--limit", "2"])).unwrap();
        assert_eq!(o.kind.as_deref(), Some("feedback"));
        assert_eq!(o.limit, Some(2));
        assert!(o.since.is_none());
        let o = parse_show_args(&show_args(&[
            "--kind=route",
            "--since=2023-11-14T22:13:20Z",
            "--limit=1",
        ]))
        .unwrap();
        assert_eq!(o.kind.as_deref(), Some("route"));
        assert_eq!(o.since, Some(1_700_000_000));
        assert_eq!(o.limit, Some(1));
        let o = parse_show_args(&show_args(&[])).unwrap();
        assert!(o.kind.is_none() && o.since.is_none() && o.limit.is_none());
    }

    #[test]
    fn show_flags_reject_unknown_duplicate_and_bad_values() {
        for bad in [
            vec!["--bogus", "x"],
            vec!["--kind"],
            vec!["--kind="],
            vec!["--kind", "a", "--kind", "b"],
            vec!["--since", "yesterday"],
            vec![
                "--since",
                "2023-11-14T22:13:20Z",
                "--since",
                "2023-11-14T22:13:20Z",
            ],
            vec!["--limit", "0"],
            vec!["--limit", "-1"],
            vec!["--limit", "many"],
            vec!["--limit", "1", "--limit=2"],
            vec!["extra-positional"],
        ] {
            assert!(parse_show_args(&show_args(&bad)).is_err(), "{bad:?}");
        }
    }
}
