//! Offline classifier eval: score any backend on labeled examples.
//!
//! Jev is only used where it beats the alternatives on accuracy; this crate is
//! the hook that proves it. [`evaluate`] scores a closure backend and
//! [`evaluate_backend`] scores a [`ClassifierBackend`] trait object (such as
//! the [`JevClassifier`] adapter over [`jev_client`](ancientos_router::jev_client))
//! on `(text, label)` examples, reporting accuracy plus per-label
//! precision/recall/F1 and macro F1. Everything runs offline: the tests use a
//! deterministic fake backend and a `127.0.0.1` mock server, never live
//! endpoints.

use ancientos_router::{jev_client::JevClient, RouterError, Sensitivity};

/// One labeled example: input `text` with expected `label`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledExample {
    pub text: String,
    pub label: String,
}

impl LabeledExample {
    /// New labeled example.
    pub fn new(text: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            label: label.into(),
        }
    }
}

impl From<(&str, &str)> for LabeledExample {
    /// Labeled example from a (text, label) pair.
    fn from((text, label): (&str, &str)) -> Self {
        Self::new(text, label)
    }
}

/// Per-label scores: precision, recall, and F1 with the raw counts behind them.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelMetrics {
    pub label: String,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    /// Examples actually carrying this label.
    pub support: usize,
    /// Predictions of this label.
    pub predicted: usize,
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
}

/// One scored example. Backend errors keep `predicted: None` and count as
/// incorrect; they never abort the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredExample {
    pub text: String,
    pub actual: String,
    pub predicted: Option<String>,
    pub correct: bool,
}

/// Whole-run report: accuracy, per-label precision/recall/F1, and macro F1
/// (the mean of per-label F1).
#[derive(Debug, Clone, PartialEq)]
pub struct EvalReport {
    pub accuracy: f64,
    pub macro_f1: f64,
    pub total: usize,
    pub correct: usize,
    pub errors: usize,
    /// One entry per label seen in actuals or predictions, sorted by label.
    pub per_label: Vec<LabelMetrics>,
    pub predictions: Vec<ScoredExample>,
}

impl EvalReport {
    /// Metrics for one label.
    pub fn metric_for(&self, label: &str) -> Option<&LabelMetrics> {
        self.per_label.iter().find(|m| m.label == label)
    }
}

/// Eval failure: a backend that cannot classify at all surfaces here. Note
/// [`evaluate`] counts per-example backend errors as incorrect instead of
/// returning this; the variant exists for adapters used outside an eval run.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("backend error: {0}")]
    Backend(String),
}

impl From<RouterError> for EvalError {
    /// Wrap a router error.
    fn from(e: RouterError) -> Self {
        EvalError::Backend(e.to_string())
    }
}

/// Classifier backend: map input text to exactly one label.
pub trait ClassifierBackend {
    /// Classify text into one label.
    fn classify(&self, text: &str) -> Result<String, EvalError>;
}

/// Any `Fn(&str) -> Result<String, E>` closure is a backend, so deterministic
/// fakes and one-off adapters need no new type.
impl<F, E> ClassifierBackend for F
where
    F: Fn(&str) -> Result<String, E>,
    E: std::fmt::Display,
{
    /// Test helper: label text with the wrapped closure, mapping errors to backend errors.
    fn classify(&self, text: &str) -> Result<String, EvalError> {
        (self)(text).map_err(|e| EvalError::Backend(e.to_string()))
    }
}

/// Score a closure backend on `examples`. Backend errors count as incorrect
/// (recorded with `predicted: None`); an empty example list scores 0.0
/// accuracy and 0.0 macro F1 with no per-label entries.
pub fn evaluate<F, E>(classify: F, examples: &[LabeledExample]) -> EvalReport
where
    F: Fn(&str) -> Result<String, E>,
{
    let scored = examples
        .iter()
        .map(|ex| {
            let predicted = classify(&ex.text).ok();
            let correct = predicted.as_deref() == Some(ex.label.as_str());
            ScoredExample {
                text: ex.text.clone(),
                actual: ex.label.clone(),
                predicted,
                correct,
            }
        })
        .collect();
    summarize(scored)
}

/// Score a [`ClassifierBackend`] trait object (same semantics as [`evaluate`]).
pub fn evaluate_backend(
    backend: &dyn ClassifierBackend,
    examples: &[LabeledExample],
) -> EvalReport {
    evaluate(|text| backend.classify(text), examples)
}

/// Build the eval report from scored examples.
fn summarize(scored: Vec<ScoredExample>) -> EvalReport {
    let total = scored.len();
    let correct = scored.iter().filter(|s| s.correct).count();
    let errors = scored.iter().filter(|s| s.predicted.is_none()).count();
    // Label universe: actuals plus predictions, sorted for determinism.
    // Predicted-only labels (hallucinations) score F1 0 and drag macro F1 down.
    let mut labels: Vec<String> = scored
        .iter()
        .flat_map(|s| {
            let mut v = vec![s.actual.clone()];
            v.extend(s.predicted.clone());
            v
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    labels.sort();
    let per_label = labels
        .iter()
        .map(|label| {
            let mut tp = 0usize;
            let mut fp = 0usize;
            let mut fn_ = 0usize;
            let mut support = 0usize;
            let mut predicted = 0usize;
            for s in &scored {
                let is_actual = s.actual == *label;
                let is_pred = s.predicted.as_deref() == Some(label.as_str());
                if is_actual {
                    support += 1;
                }
                if is_pred {
                    predicted += 1;
                }
                match (is_pred, is_actual) {
                    (true, true) => tp += 1,
                    (true, false) => fp += 1,
                    (false, true) => fn_ += 1,
                    (false, false) => {}
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
            LabelMetrics {
                label: label.clone(),
                precision,
                recall,
                f1,
                support,
                predicted,
                true_positives: tp,
                false_positives: fp,
                false_negatives: fn_,
            }
        })
        .collect::<Vec<_>>();
    EvalReport {
        accuracy: if total == 0 {
            0.0
        } else {
            correct as f64 / total as f64
        },
        macro_f1: if per_label.is_empty() {
            0.0
        } else {
            per_label.iter().map(|m| m.f1).sum::<f64>() / per_label.len() as f64
        },
        total,
        correct,
        errors,
        per_label,
        predictions: scored,
    }
}

/// [`ClassifierBackend`] adapter over [`JevClient`]: prompts Jev to reply with
/// exactly one label and returns the trimmed reply. Backend refusals (for
/// example a TypeSafe client asked to score non-sanitized content) surface as
/// backend errors — score sensitive fixtures with a localjev client instead.
#[derive(Debug, Clone)]
pub struct JevClassifier {
    client: JevClient,
    sensitivity: Sensitivity,
    labels: Vec<String>,
}

impl JevClassifier {
    /// Wrap `client`; `sensitivity` declares the fixture content (public
    /// fixtures use [`Sensitivity::Sanitized`]).
    pub fn new(client: JevClient, sensitivity: Sensitivity) -> Self {
        Self {
            client,
            sensitivity,
            labels: Vec::new(),
        }
    }

    /// Constrain the prompt to these labels (also documents the eval's label set).
    pub fn with_labels(mut self, labels: &[&str]) -> Self {
        self.labels = labels.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Underlying Jev client.
    pub fn client(&self) -> &JevClient {
        &self.client
    }

    /// Sensitivity used for calls.
    pub fn sensitivity(&self) -> Sensitivity {
        self.sensitivity
    }

    /// Allowed labels.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Prompt sent for `text`: asks for exactly one label, listing the
    /// constrained labels when [`Self::with_labels`] set any.
    pub fn prompt_for(&self, text: &str) -> String {
        if self.labels.is_empty() {
            format!(
                "Classify the following text. Reply with exactly one label and nothing else.\n\nText: {text}"
            )
        } else {
            format!(
                "Classify the following text. Reply with exactly one of these labels and nothing else: {}.\n\nText: {text}",
                self.labels.join(", ")
            )
        }
    }
}

impl ClassifierBackend for JevClassifier {
    /// Asks one System One `choice` question over the constrained labels
    /// (falls back to the free-text label prompt as a choice-less question,
    /// which Jev rejects, so labels are effectively required).
    fn classify(&self, text: &str) -> Result<String, EvalError> {
        let criteria: serde_json::Map<String, serde_json::Value> = self
            .labels
            .iter()
            .map(|l| (l.clone(), serde_json::Value::String(String::new())))
            .collect();
        let questions = serde_json::json!({
            "label": {"type": "choice", "instructions": "Which label best fits this text?", "criteria": criteria}
        });
        let reply = self.client.ask(self.sensitivity, text, &questions)?;
        reply
            .choice("label")
            .map(|c| c.trim().to_string())
            .ok_or_else(|| EvalError::from(RouterError::BadResponse("missing choice".into())))
    }
}

/// Bookmark triage labels (same fields as the Muse labeling prompt).
#[derive(Debug, Clone, PartialEq)]
pub struct BookmarkLabels {
    pub try_it: f64,
    pub worth_resurfacing: f64,
    pub category: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Category criteria mirroring the private labeling script.
pub const BOOKMARK_CATEGORIES: &[&str] = &[
    "ai_agents_homelab",
    "health_fitness_food",
    "mental_health_adhd",
    "mind_consciousness",
    "markets_investing",
    "space_tech_news",
    "cool_fun",
    "other_unsure",
];

/// System One questions for bookmark labeling (one request per post).
pub fn bookmark_questions() -> serde_json::Value {
    let cats: serde_json::Map<String, serde_json::Value> = BOOKMARK_CATEGORIES
        .iter()
        .map(|c| (c.to_string(), serde_json::Value::String(String::new())))
        .collect();
    serde_json::json!({
        "try_it": {"type": "noul", "instructions": "This X bookmark is try-it the reader would act on: hands-on practice to try now with actionable steps (exercise, stretch, breathing, meditation, recipe, concrete routine, tool walkthrough); research for the reader's personal AI OS project (AncientOS) to learn from to build their agent system better (how LLMs, agents and harnesses work, architecture write-ups, popular or growing GitHub repos worth reviewing for ideas); something to hand off to an agent or bot to review or explore (a repo, a list of tools or APIs, a course, a long thread); or learning content whose actionable thing is in the linked page or video (a tutorial, a tool to install, shortcuts to learn). Not news, hype, opinion, drama, general advice lists, or things the reader already uses."},
        "worth_resurfacing": {"type": "noul", "instructions": "This X bookmark has durable substantive value (insight, how-to, reference). False for jokes, hype, news blips, low-content."},
        "category": {"type": "choice", "instructions": "Topic category of this X bookmark for a personal-knowledge app.", "criteria": cats}
    })
}

/// Label one public bookmark with Jev. Sensitivity is the caller's call.
pub fn jev_label_bookmark(
    client: &JevClient,
    s: Sensitivity,
    text: &str,
) -> Result<BookmarkLabels, RouterError> {
    let r = client.ask(s, text, &bookmark_questions())?;
    let miss = |f: &str| RouterError::BadResponse(format!("missing {f}"));
    Ok(BookmarkLabels {
        try_it: r.noul("try_it").ok_or_else(|| miss("try_it"))?,
        worth_resurfacing: r
            .noul("worth_resurfacing")
            .ok_or_else(|| miss("worth_resurfacing"))?,
        category: r
            .choice("category")
            .ok_or_else(|| miss("category"))?
            .to_string(),
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancientos_router::jev_client::JevBackend;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Test helper: report whether two floats agree within 1e-12.
    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    /// Test helper: build labeled examples from (text, label) pairs.
    fn examples(pairs: &[(&str, &str)]) -> Vec<LabeledExample> {
        pairs.iter().copied().map(LabeledExample::from).collect()
    }

    #[test]
    fn perfect_fake_scores_one() {
        let ex = examples(&[("a", "x"), ("b", "y"), ("c", "x")]);
        let report = evaluate(
            |t| {
                Ok::<_, EvalError>(match t {
                    "a" | "c" => "x".to_string(),
                    _ => "y".to_string(),
                })
            },
            &ex,
        );
        assert_eq!(report.total, 3);
        assert_eq!(report.correct, 3);
        assert_eq!(report.errors, 0);
        assert_eq!(report.accuracy, 1.0);
        assert_eq!(report.macro_f1, 1.0);
        for m in &report.per_label {
            assert_eq!(m.precision, 1.0, "label {}", m.label);
            assert_eq!(m.recall, 1.0, "label {}", m.label);
            assert_eq!(m.f1, 1.0, "label {}", m.label);
        }
        assert_eq!(report.metric_for("x").unwrap().support, 2);
        assert_eq!(report.metric_for("y").unwrap().support, 1);
    }

    #[test]
    fn partial_credit_matches_hand_computation() {
        // Predictions: sports, tech, tech, sports on sports, sports, tech, tech.
        let ex = examples(&[
            ("t1", "sports"),
            ("t2", "sports"),
            ("t3", "tech"),
            ("t4", "tech"),
        ]);
        let report = evaluate(
            |t| {
                Ok::<_, EvalError>(
                    match t {
                        "t1" | "t4" => "sports",
                        _ => "tech",
                    }
                    .to_string(),
                )
            },
            &ex,
        );
        assert_eq!(report.accuracy, 0.5);
        assert_eq!(report.macro_f1, 0.5);
        for label in ["sports", "tech"] {
            let m = report.metric_for(label).unwrap();
            assert_eq!(m.precision, 0.5, "label {label}");
            assert_eq!(m.recall, 0.5, "label {label}");
            assert_eq!(m.f1, 0.5, "label {label}");
            assert_eq!(m.support, 2, "label {label}");
            assert_eq!(m.true_positives, 1, "label {label}");
            assert_eq!(m.false_positives, 1, "label {label}");
            assert_eq!(m.false_negatives, 1, "label {label}");
        }
    }

    #[test]
    fn skewed_predictions_and_zero_division_are_defined() {
        // Always predicts sports on 2x sports + 1x tech.
        let ex = examples(&[("a", "sports"), ("b", "sports"), ("c", "tech")]);
        let report = evaluate(|_| Ok::<_, EvalError>("sports".to_string()), &ex);
        assert!(approx_eq(report.accuracy, 2.0 / 3.0));
        let sports = report.metric_for("sports").unwrap();
        assert!(approx_eq(sports.precision, 2.0 / 3.0));
        assert_eq!(sports.recall, 1.0);
        assert!(approx_eq(sports.f1, 0.8));
        // Tech is never predicted: 0/0 precision is defined as 0, not NaN.
        let tech = report.metric_for("tech").unwrap();
        assert_eq!(tech.precision, 0.0);
        assert_eq!(tech.recall, 0.0);
        assert_eq!(tech.f1, 0.0);
        assert_eq!(tech.support, 1);
        assert!(approx_eq(report.macro_f1, 0.4));
        for m in &report.per_label {
            assert!(!m.precision.is_nan() && !m.recall.is_nan() && !m.f1.is_nan());
        }
    }

    #[test]
    fn backend_errors_count_as_wrong_without_aborting() {
        let ex = examples(&[("ok", "x"), ("boom", "x"), ("ok2", "y")]);
        let report = evaluate(
            |t| {
                if t == "boom" {
                    Err("fake failure")
                } else {
                    Ok(t.to_string())
                }
            },
            &ex,
        );
        // "ok"->"ok" and "ok2"->"ok2" miss their labels; "boom" errors. All
        // three are incorrect, exactly one is an error.
        assert_eq!(report.total, 3);
        assert_eq!(report.correct, 0);
        assert_eq!(report.errors, 1);
        assert_eq!(report.accuracy, 0.0);
        let boom = report
            .predictions
            .iter()
            .find(|s| s.text == "boom")
            .unwrap();
        assert_eq!(boom.predicted, None);
        assert!(!boom.correct);
    }

    #[test]
    fn hallucinated_labels_score_zero_and_drag_macro_f1() {
        let ex = examples(&[("a", "x")]);
        let report = evaluate(|_| Ok::<_, EvalError>("zzz".to_string()), &ex);
        let hall = report.metric_for("zzz").unwrap();
        assert_eq!(hall.support, 0);
        assert_eq!(hall.predicted, 1);
        assert_eq!(hall.f1, 0.0);
        assert_eq!(report.macro_f1, 0.0);
    }

    #[test]
    fn empty_examples_score_zero() {
        let report = evaluate(|_| Ok::<_, EvalError>("x".to_string()), &[]);
        assert_eq!(report.total, 0);
        assert_eq!(report.correct, 0);
        assert_eq!(report.errors, 0);
        assert_eq!(report.accuracy, 0.0);
        assert_eq!(report.macro_f1, 0.0);
        assert!(report.per_label.is_empty());
        assert!(report.predictions.is_empty());
    }

    #[test]
    fn trait_object_path_matches_closure_path() {
        struct Always(String);
        impl ClassifierBackend for Always {
            /// Test helper: return the fixed label regardless of input.
            fn classify(&self, text: &str) -> Result<String, EvalError> {
                let _ = text;
                Ok(self.0.clone())
            }
        }
        let ex = examples(&[("a", "x"), ("b", "y")]);
        let via_trait = evaluate_backend(&Always("x".to_string()), &ex);
        let via_closure = evaluate(|_| Ok::<_, EvalError>("x".to_string()), &ex);
        assert_eq!(via_trait, via_closure);
        // And a closure also satisfies the trait directly.
        let closure = |_: &str| Ok::<_, EvalError>("y".to_string());
        assert_eq!(closure.classify("anything").unwrap(), "y");
    }

    /// Captured request bodies, one per hit.
    type SeenBodies = Arc<Mutex<Vec<String>>>;

    /// Read one request: headers until the blank line, then Content-Length bytes.
    fn read_request_body(stream: &mut std::net::TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut content_length = 0usize;
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        let mut raw_len = 0usize;
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    raw_len += 1;
                    line.push(buf[0]);
                    if buf[0] == b'\n' {
                        let text = String::from_utf8_lossy(&line);
                        if text.to_lowercase().starts_with("content-length:") {
                            content_length = text
                                .split(':')
                                .nth(1)
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                        }
                        if line == b"\r\n" || line == b"\n" {
                            break;
                        }
                        line.clear();
                    }
                    if raw_len > 65536 {
                        break;
                    }
                }
            }
        }
        let mut rest = vec![0u8; content_length.min(1 << 20)];
        let mut filled = 0;
        while filled < rest.len() {
            match stream.read(&mut rest[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        rest.truncate(filled);
        String::from_utf8_lossy(&rest).into_owned()
    }

    /// Mock chat-completions server on 127.0.0.1: replies `replies` in order
    /// (one per request), captures request bodies, then exits. No external
    /// network is ever used.
    fn mock_server(replies: &[&str]) -> (String, SeenBodies, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let seen: SeenBodies = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        let bodies: Vec<String> = replies
            .iter()
            .map(|content| {
                format!(
                    "{{\"model\":\"m\",\"answers\":{{\"label\":{{\"type\":\"choice\",\"choice\":{}}}}},\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}",
                    serde_json::Value::String(content.to_string())
                )
            })
            .collect();
        let handle = std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let req_body = read_request_body(&mut stream);
                seen_thread.lock().expect("seen lock").push(req_body);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (url, seen, handle)
    }

    /// A base URL with nothing listening (connection refused, fast). Tests
    /// that must not touch the network use this to prove it.
    fn closed_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let url = format!("http://{}", listener.local_addr().expect("probe addr"));
        drop(listener);
        url
    }

    /// Test helper: build a tech/sports classifier against the given mock server.
    fn localjev_classifier(base_url: String, sensitivity: Sensitivity) -> JevClassifier {
        let client = JevClient::new(JevBackend::LocalJev, base_url, "test-localjev", false);
        JevClassifier::new(client, sensitivity).with_labels(&["tech", "sports"])
    }

    #[test]
    fn jev_adapter_scores_against_mock_server() {
        // Mock always replies tech (padded with whitespace to prove trimming).
        let (url, seen, handle) = mock_server(&["  tech\n", "tech", "tech", "tech"]);
        let backend = localjev_classifier(url, Sensitivity::Sanitized);
        let ex = examples(&[
            ("gpu cluster", "tech"),
            ("new phone", "tech"),
            ("chip launch", "tech"),
            ("cup final", "sports"),
        ]);
        let report = evaluate_backend(&backend, &ex);
        handle.join().expect("server thread");
        assert_eq!(report.total, 4);
        assert_eq!(report.correct, 3);
        assert_eq!(report.errors, 0);
        assert_eq!(report.accuracy, 0.75);
        let tech = report.metric_for("tech").unwrap();
        assert_eq!(tech.precision, 0.75);
        assert_eq!(tech.recall, 1.0);
        assert!(approx_eq(tech.f1, 6.0 / 7.0));
        assert!(approx_eq(report.macro_f1, 3.0 / 7.0));
        // Every prompt listed the constrained labels and carried the input text.
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 4);
        for (body, ex) in seen.iter().zip(ex.iter()) {
            let sent: serde_json::Value = serde_json::from_str(body).expect("request is json");
            assert_eq!(sent["state"], ex.text.as_str());
            let crit = sent["questions"]["label"]["criteria"].as_object().unwrap();
            assert!(crit.contains_key("tech") && crit.contains_key("sports"));
        }
    }

    #[test]
    fn jev_adapter_prompt_lists_labels_only_when_set() {
        let client = JevClient::new(JevBackend::LocalJev, closed_url(), "m", false);
        let bare = JevClassifier::new(client.clone(), Sensitivity::Sanitized);
        assert!(!bare.prompt_for("hi").contains("these labels"));
        let constrained =
            JevClassifier::new(client, Sensitivity::Sanitized).with_labels(&["a", "b"]);
        let prompt = constrained.prompt_for("hi");
        assert!(prompt.contains("a, b"));
        assert!(prompt.contains("hi"));
    }

    #[test]
    fn bookmark_try_it_wording_matches_cascade() {
        // The router cannot depend on this crate, so the cascade's Jev
        // question duplicates this wording; the two must stay identical.
        let q = bookmark_questions();
        assert_eq!(q["try_it"]["type"], "noul");
        let instructions = q["try_it"]["instructions"].as_str().unwrap();
        assert_eq!(
            instructions,
            bookmarks_agent::try_it_cascade::TRY_IT_INSTRUCTIONS
        );
        assert!(instructions.contains("personal AI OS project (AncientOS)"));
        assert!(instructions.contains("hand off"));
    }

    #[test]
    fn jev_typesafe_refusal_counts_as_error_without_network() {
        // TypeSafe + sensitive fixtures: refused before any network use, and
        // the eval counts each refusal as an error (incorrect), not a crash.
        let client = JevClient::new(JevBackend::TypeSafe, closed_url(), "m", true);
        let backend = JevClassifier::new(client, Sensitivity::Sensitive);
        let ex = examples(&[("note one", "x"), ("note two", "y")]);
        let report = evaluate_backend(&backend, &ex);
        assert_eq!(report.total, 2);
        assert_eq!(report.correct, 0);
        assert_eq!(report.errors, 2);
        assert_eq!(report.accuracy, 0.0);
        assert!(report.predictions.iter().all(|s| s.predicted.is_none()));
    }
}

pub mod harness;
/// Offline try_it / brief-ranking harness (formerly the docs-eval `eval` crate).
pub mod query_relevance;
pub use harness::{render_markdown, run};
