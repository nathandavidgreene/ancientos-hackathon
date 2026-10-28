//! Runner for `analyze_media`: `analyze-media --rows FILE [--log PATH] [--live]`.
//!
//! Reads `x_bookmark_media`-shaped JSONL, builds one request per post (all
//! that post's media keys, in file order), and by default only prints the
//! dry-run plan: items that would be sent and keys that would be skipped. No
//! log write, no model call. `--live` checks [`PaidConfig::from_env`] and the
//! live backend env first (fail closed), enforces the per-run cap over the
//! whole run, then calls `analyze` and `propose_store` per post on the shared
//! decision log. Approvals are decided with the usual `approve`/`deny` tools.
use crate::analyze_media::{self, MediaBackend, MediaRequest, MediaRow, PaidConfig};
use ancientos_core::log::DecisionLog;

/// Group rows into one request per post, keeping first-seen order.
pub fn requests_from_rows(rows: &[MediaRow]) -> Vec<MediaRequest> {
    let mut out: Vec<MediaRequest> = Vec::new();
    for r in rows {
        match out.iter_mut().find(|q| q.post_id == r.post_id) {
            Some(q) => {
                if !q.media_keys.contains(&r.media_key) {
                    q.media_keys.push(r.media_key.clone());
                }
            }
            None => out.push(MediaRequest {
                post_id: r.post_id.clone(),
                media_keys: vec![r.media_key.clone()],
            }),
        }
    }
    out
}

/// Parsed CLI flags.
#[derive(Debug, PartialEq)]
struct Args {
    rows: String,
    log: Option<String>,
    live: bool,
}

/// Parse flags; unknown flags are errors.
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut rows = None;
    let mut log = None;
    let mut live = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rows" => rows = it.next().cloned(),
            "--log" => log = it.next().cloned(),
            "--live" => live = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        rows: rows.ok_or("usage: analyze-media --rows FILE [--log PATH] [--live]")?,
        log,
        live,
    })
}

/// Decision log path: `--log`, else `$DECISION_LOG_PATH`, else
/// `$ANCIENTOS_HOME/decision_log.jsonl`, else
/// `$HOME/.ancientos-hackathon/decision_log.jsonl`.
fn log_path(flag: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(p) = flag
        .map(str::to_string)
        .or_else(|| get("DECISION_LOG_PATH"))
    {
        return Ok(p);
    }
    let dir = match get("ANCIENTOS_HOME") {
        Some(d) => d,
        None => format!(
            "{}/.ancientos-hackathon",
            get("HOME").ok_or("HOME not set")?
        ),
    };
    Ok(format!("{dir}/decision_log.jsonl"))
}

/// Run the CLI. `make_backend` builds the live backend (tests inject a
/// mock); it is only called after the paid check passes. Returns the text
/// to print.
pub fn run(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    make_backend: &dyn Fn(PaidConfig) -> Result<Box<dyn MediaBackend>, String>,
    now: u64,
) -> Result<String, String> {
    let a = parse_args(args)?;
    let text = std::fs::read_to_string(&a.rows).map_err(|e| format!("{}: {e}", a.rows))?;
    let rows = analyze_media::parse_rows(&text)?;
    let reqs = requests_from_rows(&rows);
    let mut out = String::new();
    let mut total = 0usize;
    for q in &reqs {
        let (ok, skipped) = analyze_media::resolve(q, &rows);
        total += ok.len();
        out.push_str(&format!(
            "{}: {} item(s), {} skipped\n",
            q.post_id,
            ok.len(),
            skipped.len()
        ));
        for s in &skipped {
            out.push_str(&format!("  skip {}: {}\n", s.media_key, s.reason));
        }
    }
    out.push_str(&format!(
        "{} post(s), {total} item(s) to analyze\n",
        reqs.len()
    ));
    if !a.live {
        out.push_str("dry run: no model call, no log write (add --live to run)\n");
        return Ok(out);
    }
    let paid = PaidConfig::from_values(
        env(analyze_media::PAID_ENV_VAR).as_deref(),
        env(analyze_media::MAX_ITEMS_ENV_VAR).as_deref(),
    )?;
    if total > paid.max_items {
        return Err(format!(
            "refused: {total} items exceeds per-run cap {}",
            paid.max_items
        ));
    }
    let backend = make_backend(paid)?;
    let mut log = DecisionLog::open(log_path(a.log.as_deref(), env)?).map_err(|e| e.to_string())?;
    let gate = analyze_media::store_gate();
    for q in &reqs {
        let rep = analyze_media::analyze(&mut log, q, &rows, backend.as_ref(), paid)?;
        let line = match analyze_media::propose_store(&mut log, &gate, &rep.accepted, now) {
            Ok(r) => format!("approval {}", r.id),
            Err(e) => format!("no store request ({e})"),
        };
        out.push_str(&format!(
            "{}: {} accepted, {} rejected, {} skipped; {line}\n",
            q.post_id,
            rep.accepted.len(),
            rep.rejected.len(),
            rep.skipped.len()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze_media::{FindingKind, MockBackend, RawFinding};

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/media/x_bookmark_media_synthetic.jsonl"
    );

    /// Test helper: argument vector.
    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Test helper: backend factory that must never be called.
    fn never(_: PaidConfig) -> Result<Box<dyn MediaBackend>, String> {
        panic!("backend must not be built")
    }

    /// Test helper: mock backend factory with one evidenced finding.
    fn mock(_: PaidConfig) -> Result<Box<dyn MediaBackend>, String> {
        Ok(Box::new(MockBackend {
            findings: vec![RawFinding {
                kind: FindingKind::Text,
                text: "[synthetic] 10k".into(),
                quote: Some("10k".into()),
                region: None,
            }],
        }))
    }

    #[test]
    fn groups_rows_per_post() {
        let rows = analyze_media::parse_rows(&std::fs::read_to_string(FIXTURE).unwrap()).unwrap();
        let q = requests_from_rows(&rows);
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].media_keys, vec!["3_demo_photo", "7_demo_video"]);
    }

    #[test]
    fn dry_run_by_default_never_builds_backend() {
        let out = run(&argv(&["--rows", FIXTURE]), &|_| None, &never, 1).unwrap();
        assert!(out.contains("dry run"));
        assert!(out.contains("3 item(s) to analyze"));
    }

    #[test]
    fn live_refuses_without_paid_or_over_cap() {
        let e = run(&argv(&["--rows", FIXTURE, "--live"]), &|_| None, &never, 1).unwrap_err();
        assert!(e.contains("ALLOW_PAID"));
        let env = |k: &str| match k {
            "ANCIENTOS_ALLOW_PAID" => Some("1".into()),
            "ANCIENTOS_MEDIA_MAX_ITEMS" => Some("2".into()),
            _ => None,
        };
        let e = run(&argv(&["--rows", FIXTURE, "--live"]), &env, &never, 1).unwrap_err();
        assert!(e.contains("cap"));
    }

    #[test]
    fn live_with_mock_proposes_store_per_post() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("log.jsonl").to_string_lossy().to_string();
        let env = |k: &str| match k {
            "ANCIENTOS_ALLOW_PAID" => Some("1".into()),
            "ANCIENTOS_MEDIA_MAX_ITEMS" => Some("10".into()),
            _ => None,
        };
        let out = run(
            &argv(&["--rows", FIXTURE, "--live", "--log", &log]),
            &env,
            &mock,
            1_800_000_000,
        )
        .unwrap();
        assert_eq!(out.matches("approval ").count(), 2, "{out}");
        assert!(out.contains("demo-9003: 0 accepted"), "{out}");
    }

    #[test]
    fn rejects_unknown_args() {
        assert!(run(&argv(&["--rows", FIXTURE, "--yolo"]), &|_| None, &never, 1).is_err());
        assert!(run(&argv(&[]), &|_| None, &never, 1).is_err());
    }
}
