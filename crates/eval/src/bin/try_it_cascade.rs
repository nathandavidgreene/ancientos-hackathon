//! Score the try-it cascade (rules -> Jev -> Muse Spark) on labeled bookmarks.
//!
//! Usage: `try_it_cascade --labels <jsonl> --bookmarks <jsonl> [--out <jsonl>]`.
//! Labels lines carry an id (`post_id`, or `n`) plus a boolean `try_it`;
//! bookmarks lines are [`Bookmark`] JSON joined by id. Labels without a
//! bookmark are skipped with a count, as are malformed lines. Non-English
//! bookmarks are always sampled (never hidden for language) and scored on
//! the cached English translation when available, else on the original.
//!
//! Items run through the cascade sequentially from env
//! ([`JevClient::from_env`] with `JEV_BACKEND`, [`MuseClient::from_env`]
//! pinned to muse-spark-1.3 / low reasoning / 200 completion tokens).
//! Bookmark text is public X content, so every item runs as
//! [`Sensitivity::Sanitized`]. The run stops early on HTTP 429 instead of
//! continuing. Without `ANCIENTOS_ALLOW_PAID=1` it still runs, rules only,
//! and says so.
use ancientos_router::jev_client::JevClient;
use ancientos_router::muse::MuseClient;
use ancientos_router::try_it_cascade::{Cascade, CascadeStats, RulesVerdict};
use ancientos_router::Sensitivity;
use bookmarks_agent::brief::topic_of;
use bookmarks_agent::fixture::Bookmark;
use bookmarks_agent::try_it::rules_confident;
use std::collections::HashMap;
use std::path::PathBuf;

const USAGE: &str = "usage: try_it_cascade --labels <jsonl> --bookmarks <jsonl> [--out <jsonl>]";

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    match run(&args) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}

struct Args {
    labels: PathBuf,
    bookmarks: PathBuf,
    out: Option<PathBuf>,
}

/// Parse argv. Unknown or repeated flags, missing values, and missing required
/// flags are all errors (fail closed); paths are never echoed back.
fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut labels = None;
    let mut bookmarks = None;
    let mut out = None;
    let mut i = 1;
    while i < argv.len() {
        let flag = argv[i].as_str();
        if !matches!(flag, "--labels" | "--bookmarks" | "--out") {
            return Err(format!("unknown argument: {flag}"));
        }
        let value = argv
            .get(i + 1)
            .ok_or_else(|| format!("missing value for {flag}"))?;
        let slot = match flag {
            "--labels" => &mut labels,
            "--bookmarks" => &mut bookmarks,
            _ => &mut out,
        };
        if slot.is_some() {
            return Err(format!("repeated argument: {flag}"));
        }
        *slot = Some(PathBuf::from(value));
        i += 2;
    }
    Ok(Args {
        labels: labels.ok_or_else(|| "missing --labels <jsonl>".to_string())?,
        bookmarks: bookmarks.ok_or_else(|| "missing --bookmarks <jsonl>".to_string())?,
        out,
    })
}

/// One gold label: bookmark id plus the expected try-it answer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Label {
    id: String,
    try_it: bool,
}

/// Parse one labels line. The id is `post_id`, or `n` (string or number);
/// `try_it` must be a boolean. Anything else is skipped by the caller.
fn parse_label(value: &serde_json::Value) -> Option<Label> {
    let id = value
        .get("post_id")
        .and_then(id_string)
        .or_else(|| value.get("n").and_then(id_string))?;
    let try_it = value.get("try_it")?.as_bool()?;
    Some(Label { id, try_it })
}

fn id_string(value: &serde_json::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        if s.is_empty() {
            return None;
        }
        return Some(s.to_string());
    }
    if let Some(n) = value.as_u64() {
        return Some(n.to_string());
    }
    value.as_i64().map(|n| n.to_string())
}

/// Load bookmarks keyed by post id (first row wins), counting bad lines.
/// Blank lines are ignored, never counted.
fn load_bookmarks(text: &str) -> (HashMap<String, Bookmark>, usize) {
    let mut map = HashMap::new();
    let mut bad = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Bookmark>(line) {
            Ok(b) => {
                map.entry(b.post_id.clone()).or_insert(b);
            }
            Err(_) => bad += 1,
        }
    }
    (map, bad)
}

/// Load labels in file order, counting malformed lines (blank lines ignored).
fn load_labels(text: &str) -> (Vec<Label>, usize) {
    let mut labels = Vec::new();
    let mut bad = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| parse_label(&v))
        {
            Some(label) => labels.push(label),
            None => bad += 1,
        }
    }
    (labels, bad)
}

/// Binary precision/recall/F1 with the raw counts. `pairs` is
/// `(actual, predicted)`; empty input scores all zeros (defined, not NaN).
#[derive(Debug, Clone, PartialEq)]
struct Prf {
    precision: f64,
    recall: f64,
    f1: f64,
    tp: usize,
    fp: usize,
    tn: usize,
    fn_: usize,
}

fn prf(pairs: &[(bool, bool)]) -> Prf {
    let mut tp = 0;
    let mut fp = 0;
    let mut tn = 0;
    let mut fn_ = 0;
    for (actual, predicted) in pairs {
        match (*actual, *predicted) {
            (true, true) => tp += 1,
            (false, true) => fp += 1,
            (false, false) => tn += 1,
            (true, false) => fn_ += 1,
        }
    }
    let precision = if tp + fp == 0 {
        0.0
    } else {
        tp as f64 / (tp + fp) as f64
    };
    let recall = if tp + fn_ == 0 {
        0.0
    } else {
        tp as f64 / (tp + fn_) as f64
    };
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    Prf {
        precision,
        recall,
        f1,
        tp,
        fp,
        tn,
        fn_,
    }
}

fn run(args: &Args) -> Result<(), String> {
    let labels_text = std::fs::read_to_string(&args.labels)
        .map_err(|e| format!("cannot read labels file: {e}"))?;
    let bookmarks_text = std::fs::read_to_string(&args.bookmarks)
        .map_err(|e| format!("cannot read bookmarks file: {e}"))?;
    let (bookmarks, bookmarks_bad) = load_bookmarks(&bookmarks_text);
    let (labels, labels_bad) = load_labels(&labels_text);

    let paid = std::env::var("ANCIENTOS_ALLOW_PAID")
        .map(|v| v == "1")
        .unwrap_or(false);
    let jev = match JevClient::from_env() {
        Ok(client) => Some(client),
        Err(e) => {
            println!("jev: unavailable ({e}): rules only");
            None
        }
    };
    let muse = MuseClient::from_env()
        .with_model("muse-spark-1.3")
        .with_reasoning_effort("low")
        .with_max_completion_tokens(200);
    let cascade = Cascade::new(jev.as_ref(), Some(&muse));

    // Translations come from $TRANSLATIONS_CACHE_PATH when set, else
    // translations.json next to the bookmarks file, else none. Read-only.
    let tcache_path = match std::env::var(bookmarks_agent::translate::CACHE_ENV_VAR)
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(p) => PathBuf::from(p),
        None => args
            .bookmarks
            .parent()
            .map(|d| d.join(bookmarks_agent::translate::CACHE_FILE_NAME))
            .unwrap_or_else(|| PathBuf::from(bookmarks_agent::translate::CACHE_FILE_NAME)),
    };
    let tcache = bookmarks_agent::translate::load_cache(&tcache_path);

    let mut stats = CascadeStats::default();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut cascade_pairs: Vec<(bool, bool)> = Vec::new();
    let mut rules_pairs: Vec<(bool, bool)> = Vec::new();
    let mut missing = 0usize;
    let mut translated = 0usize;
    let mut untranslated = 0usize;
    let mut stopped_early = false;
    for label in &labels {
        let Some(bookmark) = bookmarks.get(&label.id) else {
            missing += 1;
            continue;
        };
        // Non-English posts are always sampled: score the cached English
        // translation when available, else the original. Aggregate counts
        // only (translated/untranslated), never text or ids in the report.
        let scoring_text: std::borrow::Cow<'_, str> =
            if bookmarks_agent::language::is_english(bookmark) {
                std::borrow::Cow::Borrowed(bookmark.text.as_str())
            } else {
                let t = bookmarks_agent::translate::cached_only(bookmark, &tcache);
                if t.status == bookmarks_agent::translate::TranslationStatus::Translated {
                    translated += 1;
                    match t.english {
                        Some(english) => std::borrow::Cow::Owned(english),
                        None => std::borrow::Cow::Borrowed(bookmark.text.as_str()),
                    }
                } else {
                    untranslated += 1;
                    std::borrow::Cow::Borrowed(bookmark.text.as_str())
                }
            };
        let confident = rules_confident(&topic_of(bookmark), &scoring_text);
        let verdict = if confident {
            RulesVerdict::ConfidentYes
        } else {
            RulesVerdict::Unsure
        };
        let decision = cascade.decide(&scoring_text, Sensitivity::Sanitized, verdict, &mut stats);
        cascade_pairs.push((label.try_it, decision.try_it));
        rules_pairs.push((label.try_it, confident));
        rows.push(serde_json::json!({
            "post_id": label.id,
            "actual": label.try_it,
            "predicted": decision.try_it,
            "stage": decision.stage.as_str(),
            "rules_confident": confident,
            "error": decision.error,
        }));
        if decision.is_rate_limited() {
            stopped_early = true;
            break;
        }
    }

    if let Some(path) = &args.out {
        let mut out = String::new();
        for row in &rows {
            out.push_str(&row.to_string());
            out.push('\n');
        }
        std::fs::write(path, out).map_err(|e| format!("cannot write output file: {e}"))?;
    }

    let decided = rows.len();
    println!(
        "try_it_cascade: {decided} decided, {missing} missing, {} malformed, {translated} translated, {untranslated} untranslated",
        labels_bad + bookmarks_bad
    );
    if stopped_early {
        println!("stopped early: rate limited (HTTP 429) after {decided} items");
    }
    if paid {
        println!("paid: on (ANCIENTOS_ALLOW_PAID=1)");
    } else {
        println!("paid: off (ANCIENTOS_ALLOW_PAID!=1): rules only, no API calls");
    }
    let cascade_prf = prf(&cascade_pairs);
    let rules_prf = prf(&rules_pairs);
    println!(
        "cascade try_it: P={:.3} R={:.3} F1={:.3} (tp={} fp={} fn={} tn={})",
        cascade_prf.precision,
        cascade_prf.recall,
        cascade_prf.f1,
        cascade_prf.tp,
        cascade_prf.fp,
        cascade_prf.fn_,
        cascade_prf.tn,
    );
    println!(
        "rules-only try_it: P={:.3} R={:.3} F1={:.3} (tp={} fp={} fn={} tn={})",
        rules_prf.precision,
        rules_prf.recall,
        rules_prf.f1,
        rules_prf.tp,
        rules_prf.fp,
        rules_prf.fn_,
        rules_prf.tn,
    );
    println!(
        "calls: jev={} jev_yes={} muse={} muse_yes={} rules_yes={} fail_closed={} errors={}",
        stats.jev_calls,
        stats.jev_yes,
        stats.muse_calls,
        stats.muse_yes,
        stats.rules_yes,
        stats.fail_closed,
        stats.errors,
    );
    println!(
        "tokens: jev_in={} jev_out={}",
        stats.jev_input_tokens, stats.jev_output_tokens
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn args_matrix() {
        let a = parse_args(&argv(&["bin", "--labels", "l", "--bookmarks", "b"])).unwrap();
        assert_eq!(a.labels, PathBuf::from("l"));
        assert_eq!(a.bookmarks, PathBuf::from("b"));
        assert_eq!(a.out, None);
        let a = parse_args(&argv(&[
            "bin",
            "--labels",
            "l",
            "--bookmarks",
            "b",
            "--out",
            "o",
        ]))
        .unwrap();
        assert_eq!(a.out, Some(PathBuf::from("o")));
        // Order is free.
        assert!(parse_args(&argv(&[
            "bin",
            "--out",
            "o",
            "--bookmarks",
            "b",
            "--labels",
            "l"
        ]))
        .is_ok());
        for bad in [
            argv(&["bin"]),
            argv(&["bin", "--labels", "l"]),
            argv(&["bin", "--labels", "l", "--bookmarks", "b", "--out"]),
            argv(&["bin", "--labels", "l", "--bookmarks", "b", "--bogus", "x"]),
            argv(&["bin", "--labels", "l", "--labels", "m", "--bookmarks", "b"]),
            argv(&["bin", "l", "b"]),
        ] {
            assert!(parse_args(&bad).is_err(), "argv {bad:?} must fail");
        }
    }

    #[test]
    fn label_id_prefers_post_id_then_n() {
        assert_eq!(
            parse_label(&serde_json::json!({"post_id": "p1", "try_it": true})),
            Some(Label {
                id: "p1".to_string(),
                try_it: true,
            })
        );
        assert_eq!(
            parse_label(&serde_json::json!({"n": "p2", "try_it": false})),
            Some(Label {
                id: "p2".to_string(),
                try_it: false,
            })
        );
        assert_eq!(
            parse_label(&serde_json::json!({"n": 7, "try_it": true})),
            Some(Label {
                id: "7".to_string(),
                try_it: true,
            })
        );
        // post_id wins over n; numeric ids work for either key.
        assert_eq!(
            parse_label(&serde_json::json!({"post_id": "p", "n": "q", "try_it": true})),
            Some(Label {
                id: "p".to_string(),
                try_it: true,
            })
        );
        assert_eq!(
            parse_label(&serde_json::json!({"post_id": 42, "try_it": true})),
            Some(Label {
                id: "42".to_string(),
                try_it: true,
            })
        );
        for bad in [
            serde_json::json!({"try_it": true}),
            serde_json::json!({"post_id": "", "try_it": true}),
            serde_json::json!({"post_id": "p"}),
            serde_json::json!({"post_id": "p", "try_it": "yes"}),
            serde_json::json!({"post_id": "p", "try_it": 1}),
            serde_json::json!({"n": null, "try_it": true}),
        ] {
            assert_eq!(parse_label(&bad), None, "line {bad} must skip");
        }
    }

    #[test]
    fn loaders_skip_blank_and_bad_lines() {
        let (map, bad) = load_bookmarks(
            "{\"post_id\":\"a\",\"url\":\"u\",\"text\":\"t\"}\n\nnot json\n{\"post_id\":\"b\"}",
        );
        assert_eq!(map.len(), 1);
        assert_eq!(bad, 2);
        // First row wins on duplicate ids.
        let (map, _) = load_bookmarks(
            "{\"post_id\":\"a\",\"url\":\"u\",\"text\":\"first\"}\n{\"post_id\":\"a\",\"url\":\"u\",\"text\":\"second\"}",
        );
        assert_eq!(map["a"].text, "first");

        let (labels, bad) = load_labels("{\"post_id\":\"a\",\"try_it\":true}\n\noops\n{\"n\":3}");
        assert_eq!(labels.len(), 1);
        assert_eq!(bad, 2);
    }

    #[test]
    fn prf_matches_hand_computation() {
        let perfect = prf(&[(true, true), (false, false), (true, true)]);
        assert_eq!(
            (perfect.precision, perfect.recall, perfect.f1),
            (1.0, 1.0, 1.0)
        );
        assert_eq!(
            (perfect.tp, perfect.fp, perfect.tn, perfect.fn_),
            (2, 0, 1, 0)
        );

        // tp=1 fp=1 fn=1 tn=1: P=0.5 R=0.5 F1=0.5.
        let half = prf(&[(true, true), (true, false), (false, true), (false, false)]);
        assert_eq!((half.precision, half.recall, half.f1), (0.5, 0.5, 0.5));

        // Empty and all-negative predictions are defined, never NaN.
        let empty = prf(&[]);
        assert_eq!((empty.precision, empty.recall, empty.f1), (0.0, 0.0, 0.0));
        let none_predicted = prf(&[(true, false), (false, false)]);
        assert_eq!(none_predicted.precision, 0.0);
        assert_eq!(none_predicted.recall, 0.0);
        assert_eq!(none_predicted.f1, 0.0);
    }
}
