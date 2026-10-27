//! Score the try-it cascade (rules -> Jev -> Muse Spark) on labeled bookmarks.
//!
//! Usage: `try_it_cascade --labels <jsonl> --bookmarks <jsonl> [--out <jsonl>] [--jev-threshold <f>]`.
//! Labels lines carry an id (`post_id`, or `n`) plus a boolean `try_it`;
//! bookmarks lines are [`Bookmark`] JSON joined by id. Labels without a
//! bookmark are skipped with a count, as are malformed lines. Non-English
//! bookmarks are always sampled (never hidden for language) and scored on
//! the cached English translation when available, else on the original.
//!
//! Items run through the cascade sequentially from env
//! ([`JevClient::from_env`] with `JEV_BACKEND`, [`MuseClient::from_env`]
//! pinned to muse-spark-1.3 / low reasoning / `TRY_IT_MUSE_MAX_COMPLETION_TOKENS`
//! (2048) completion tokens, with one length-truncation retry at
//! `TRY_IT_MUSE_RETRY_MAX_COMPLETION_TOKENS` (4096)). The summary reports the
//! retry count as `muse_retries`.
//! Every item is classified ([`class_of`](bookmarks_agent::translate::class_of)):
//! Public items (URL/@handle-only differences included) run the paid stages
//! on sanitized text; anything else falls back to the rules verdict. The run
//! stops early on HTTP 429 instead of
//! continuing. Without `ANCIENTOS_ALLOW_PAID=1` it still runs, rules only,
//! and says so.
//!
//! The Jev yes threshold defaults to 0.3, reads
//! `ANCIENTOS_JEV_TRY_IT_THRESHOLD` when set (see
//! [`jev_threshold_from_env`]), and `--jev-threshold <f>` overrides both.
//! The summary prints the threshold actually used.
//!
//! `--enrich` (default off) enriches each bookmark with linked-page text
//! and media context (see `bookmarks_agent::link_context`) before the
//! cascade decides: cascade input becomes post text plus
//! `"\n\nLinked/Media context:\n"` plus the enrichment, so topic filtering
//! and the paid stages all see it. Fetches need `ANCIENTOS_FETCH_LINKS=1`
//! and reuse the on-disk link cache (saved after the run); without the
//! opt-in nothing is fetched and nothing is appended. The summary reports
//! an `enriched` count.
use ancientos_router::data_class::{max_of, DataClass};
use ancientos_router::jev_client::JevClient;
use ancientos_router::muse::MuseClient;
use bookmarks_agent::brief::topic_of;
use bookmarks_agent::fixture::Bookmark;
use bookmarks_agent::try_it::rules_confident;
use bookmarks_agent::try_it_cascade::{
    jev_threshold_from_env, Cascade, CascadeStats, RulesVerdict, TRY_IT_MUSE_MAX_COMPLETION_TOKENS,
};
use std::collections::HashMap;
use std::path::PathBuf;

const USAGE: &str = "usage: try_it_cascade --labels <jsonl> --bookmarks <jsonl> [--out <jsonl>] [--jev-threshold <f>] [--enrich]";

/// Try-it cascade eval entry point.
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
    jev_threshold: Option<f64>,
    enrich: bool,
}

/// Parse argv. Unknown or repeated flags, missing values, missing required
/// flags, and invalid `--jev-threshold` values are all errors (fail closed);
/// paths are never echoed back.
fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut labels = None;
    let mut bookmarks = None;
    let mut out = None;
    let mut jev_threshold = None;
    let mut enrich = false;
    let mut i = 1;
    while i < argv.len() {
        let flag = argv[i].as_str();
        if !matches!(
            flag,
            "--labels" | "--bookmarks" | "--out" | "--jev-threshold" | "--enrich"
        ) {
            return Err(format!("unknown argument: {flag}"));
        }
        if flag == "--enrich" {
            if enrich {
                return Err(format!("repeated argument: {flag}"));
            }
            enrich = true;
            i += 1;
            continue;
        }
        let value = argv
            .get(i + 1)
            .ok_or_else(|| format!("missing value for {flag}"))?;
        if flag == "--jev-threshold" {
            if jev_threshold.is_some() {
                return Err(format!("repeated argument: {flag}"));
            }
            jev_threshold = Some(parse_jev_threshold(value)?);
        } else {
            let slot = match flag {
                "--labels" => &mut labels,
                "--bookmarks" => &mut bookmarks,
                _ => &mut out,
            };
            if slot.is_some() {
                return Err(format!("repeated argument: {flag}"));
            }
            *slot = Some(PathBuf::from(value));
        }
        i += 2;
    }
    Ok(Args {
        labels: labels.ok_or_else(|| "missing --labels <jsonl>".to_string())?,
        bookmarks: bookmarks.ok_or_else(|| "missing --bookmarks <jsonl>".to_string())?,
        out,
        jev_threshold,
        enrich,
    })
}

/// Parse a `--jev-threshold` value: a number in \[0, 1\]. Anything else is an
/// error (fail closed): an explicit bad flag never silently falls back.
fn parse_jev_threshold(value: &str) -> Result<f64, String> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| !v.is_nan() && (0.0..=1.0).contains(v))
        .ok_or_else(|| {
            format!("invalid value for --jev-threshold: {value:?} (want a number in [0,1])")
        })
}

/// Resolve the threshold actually used: the CLI flag wins, else
/// [`jev_threshold_from_env`] (env var or the 0.3 default).
fn resolve_jev_threshold(flag: Option<f64>) -> f64 {
    flag.unwrap_or_else(jev_threshold_from_env)
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

/// Bookmark id as a string (accepts number or string).
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

/// Precision, recall and F1 from (predicted, actual) pairs.
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

/// Run the cascade over the labeled set and write the report.
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
    let jev_threshold = resolve_jev_threshold(args.jev_threshold);
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
        .with_max_completion_tokens(TRY_IT_MUSE_MAX_COMPLETION_TOKENS);
    let cascade = Cascade::new(jev.as_ref(), Some(&muse)).with_jev_threshold(jev_threshold);

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

    // Link-context cache for --enrich: $LINK_CONTEXT_CACHE_PATH when set,
    // else link_context_cache.json next to the bookmarks file. Loaded here,
    // saved after the run; fetches reuse it so reruns make no new calls.
    let lcache_path = if args.enrich {
        let path = match std::env::var(bookmarks_agent::link_context::CACHE_ENV_VAR)
            .ok()
            .filter(|v| !v.is_empty())
        {
            Some(p) => PathBuf::from(p),
            None => args
                .bookmarks
                .parent()
                .map(|d| d.join(bookmarks_agent::link_context::CACHE_FILE_NAME))
                .unwrap_or_else(|| PathBuf::from(bookmarks_agent::link_context::CACHE_FILE_NAME)),
        };
        Some(path)
    } else {
        None
    };
    let mut lcache = lcache_path
        .as_ref()
        .map(|p| bookmarks_agent::link_context::load_cache(p))
        .unwrap_or_default();

    let mut stats = CascadeStats::default();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut cascade_pairs: Vec<(bool, bool)> = Vec::new();
    let mut rules_pairs: Vec<(bool, bool)> = Vec::new();
    let mut missing = 0usize;
    let mut translated = 0usize;
    let mut untranslated = 0usize;
    let mut enriched = 0usize;
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
        // Matched-cue text for the rules-stage reason (confident posts
        // always carry a tag, since confidence implies tagging).
        let rules_reason = if confident {
            bookmarks_agent::try_it::classify_text(&topic_of(bookmark), &scoring_text)
                .map(|tag| tag.reason)
        } else {
            None
        };
        // With --enrich, the cascade (topic filter, Jev, Muse) decides on
        // the sanitized post text plus linked/media context. Non-public
        // bookmarks keep their enrichment local-only: `cascade_input`
        // withholds it, so it is never counted here and never reaches a
        // paid model. The class gate runs on the ORIGINALS: an email post
        // stays rules-only even though its sanitized text would be safe.
        let (cascade_text, class) = if args.enrich {
            let e = bookmarks_agent::link_context::enrich(bookmark, &mut lcache);
            if bookmarks_agent::link_context::will_append(&e) {
                enriched += 1;
            }
            let class = max_of([
                bookmarks_agent::translate::class_of(&scoring_text),
                if bookmarks_agent::link_context::will_append(&e) {
                    bookmarks_agent::translate::class_of(&e.text)
                } else {
                    DataClass::Public
                },
            ]);
            (
                bookmarks_agent::link_context::cascade_input(&scoring_text, &e),
                class,
            )
        } else {
            let class = bookmarks_agent::translate::class_of(&scoring_text);
            (scoring_text.into_owned(), class)
        };
        let decision = cascade.decide_with_rules_reason(
            &cascade_text,
            class,
            verdict,
            rules_reason.as_deref(),
            &mut stats,
            None,
        );
        cascade_pairs.push((label.try_it, decision.try_it));
        rules_pairs.push((label.try_it, confident));
        rows.push(serde_json::json!({
            "post_id": label.id,
            "actual": label.try_it,
            "predicted": decision.try_it,
            "stage": decision.stage.as_str(),
            "reason": decision.explanation.reason,
            "rules_confident": confident,
            "truncated": decision.truncated,
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

    if let Some(path) = &lcache_path {
        bookmarks_agent::link_context::save_cache(path, &lcache)
            .map_err(|e| format!("cannot write link context cache: {e}"))?;
    }

    let decided = rows.len();
    println!(
        "try_it_cascade: {decided} decided, {missing} missing, {} malformed, {translated} translated, {untranslated} untranslated, {enriched} enriched",
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
    println!("jev threshold: {jev_threshold:.3}");
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
        "calls: jev={} jev_yes={} muse={} muse_yes={} muse_truncated={} muse_retries={} rules_yes={} topic_filtered={} fail_closed={} errors={}",
        stats.jev_calls,
        stats.jev_yes,
        stats.muse_calls,
        stats.muse_yes,
        stats.muse_truncated,
        stats.muse_retries,
        stats.rules_yes,
        stats.topic_filtered,
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

    /// Test helper: collect argv words into owned strings.
    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn args_matrix() {
        let a = parse_args(&argv(&["bin", "--labels", "l", "--bookmarks", "b"])).unwrap();
        assert_eq!(a.labels, PathBuf::from("l"));
        assert_eq!(a.bookmarks, PathBuf::from("b"));
        assert_eq!(a.out, None);
        assert_eq!(a.jev_threshold, None);
        assert!(!a.enrich);
        // --enrich is a bare flag, default off, free order.
        let a = parse_args(&argv(&[
            "bin",
            "--enrich",
            "--labels",
            "l",
            "--bookmarks",
            "b",
        ]))
        .unwrap();
        assert!(a.enrich);
        let a = parse_args(&argv(&[
            "bin",
            "--labels",
            "l",
            "--bookmarks",
            "b",
            "--out",
            "o",
            "--enrich",
        ]))
        .unwrap();
        assert!(a.enrich);
        assert_eq!(a.out, Some(PathBuf::from("o")));
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
        // The threshold flag parses a number in [0,1], endpoints included.
        for (raw, expected) in [("0", 0.0), ("1", 1.0), ("0.7", 0.7)] {
            let a = parse_args(&argv(&[
                "bin",
                "--labels",
                "l",
                "--bookmarks",
                "b",
                "--jev-threshold",
                raw,
            ]))
            .unwrap();
            assert_eq!(a.jev_threshold, Some(expected), "argv {raw:?}");
        }
        for bad in [
            argv(&["bin"]),
            argv(&["bin", "--labels", "l"]),
            argv(&["bin", "--labels", "l", "--bookmarks", "b", "--out"]),
            argv(&["bin", "--labels", "l", "--bookmarks", "b", "--bogus", "x"]),
            argv(&["bin", "--labels", "l", "--labels", "m", "--bookmarks", "b"]),
            argv(&["bin", "l", "b"]),
            // Repeated --enrich fails like any repeated flag.
            argv(&[
                "bin",
                "--labels",
                "l",
                "--bookmarks",
                "b",
                "--enrich",
                "--enrich",
            ]),
            // Threshold flag: missing, repeated, or unparseable values fail.
            argv(&[
                "bin",
                "--labels",
                "l",
                "--bookmarks",
                "b",
                "--jev-threshold",
            ]),
            argv(&[
                "bin",
                "--labels",
                "l",
                "--bookmarks",
                "b",
                "--jev-threshold",
                "0.5",
                "--jev-threshold",
                "0.7",
            ]),
            argv(&[
                "bin",
                "--labels",
                "l",
                "--bookmarks",
                "b",
                "--jev-threshold",
                "bogus",
            ]),
            argv(&[
                "bin",
                "--labels",
                "l",
                "--bookmarks",
                "b",
                "--jev-threshold",
                "NaN",
            ]),
            argv(&[
                "bin",
                "--labels",
                "l",
                "--bookmarks",
                "b",
                "--jev-threshold",
                "-0.1",
            ]),
            argv(&[
                "bin",
                "--labels",
                "l",
                "--bookmarks",
                "b",
                "--jev-threshold",
                "1.5",
            ]),
        ] {
            assert!(parse_args(&bad).is_err(), "argv {bad:?} must fail");
        }
    }

    /// Set one process env var for a test, restoring the previous value on drop.
    /// Only the threshold test below touches env, so no lock is needed.
    struct EnvGuard {
        key: &'static str,
        saved: Option<String>,
    }

    impl EnvGuard {
        /// Test helper: set an env var, saving the prior value for restore.
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let saved = std::env::var(key).ok();
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
            Self { key, saved }
        }
    }

    impl Drop for EnvGuard {
        /// Test helper: restore the saved env var value on scope exit.
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn threshold_flag_overrides_env() {
        use bookmarks_agent::try_it_cascade::{
            ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV, TRY_IT_THRESHOLD,
        };
        // The flag wins over whatever the env says.
        let _env = EnvGuard::set(ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV, Some("0.9"));
        assert_eq!(resolve_jev_threshold(Some(0.1)), 0.1);
        // Without the flag, a valid env value wins over the default.
        assert_eq!(resolve_jev_threshold(None), 0.9);
        // Without the flag, a bad env value falls back to the default.
        let _env = EnvGuard::set(ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV, Some("bogus"));
        assert_eq!(resolve_jev_threshold(None), TRY_IT_THRESHOLD);
        let _env = EnvGuard::set(ANCIENTOS_JEV_TRY_IT_THRESHOLD_ENV, None);
        assert_eq!(resolve_jev_threshold(None), TRY_IT_THRESHOLD);
        assert_eq!(TRY_IT_THRESHOLD, 0.3);
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
