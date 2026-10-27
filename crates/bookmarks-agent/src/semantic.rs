//! Rank bookmarks by meaning with a small LOCAL embedding model.
//!
//! Opt-in and local only: active when both `ANCIENTOS_LOCAL_LLM_URL` (the
//! local Ollama server the router already uses) and [`MODEL_ENV`] (for
//! example `all-minilm`) are set. No paid API is ever called. Vectors are
//! cached on disk keyed by `sha256(model, sanitized text)`, so each post is
//! embedded once and the cache holds no post text.
//!
//! Model: the centroid of approved posts minus the centroid of skipped posts
//! (cosine). Its scores are blended with the vote-learned
//! [`crate::ranker::Ranker`] after putting both on the same scale.
//!
//! Fail closed: when the model is unconfigured or unreachable, or any post
//! lacks a vector, no semantic score is produced and callers keep the
//! vote-learned order unchanged.

use crate::fixture::Bookmark;
use crate::ranker::{LabeledEvent, Ranker};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Env var naming the local embedding model; unset means semantic ranking is off.
pub const MODEL_ENV: &str = "ANCIENTOS_EMBED_MODEL";
/// Env var overriding the semantic share of the blend (0.0 to 1.0).
pub const WEIGHT_ENV: &str = "ANCIENTOS_SEMANTIC_WEIGHT";
/// Semantic share of the blend when [`WEIGHT_ENV`] is unset or invalid.
pub const DEFAULT_WEIGHT: f64 = 0.5;
/// Vector cache file name, kept next to the decision log.
pub const CACHE_FILE_NAME: &str = "embeddings.json";
/// Texts sent per embedding request.
const BATCH: usize = 32;
/// Embedding request timeout.
const TIMEOUT_SECS: u64 = 60;

/// Where to embed: the local server base URL and the model name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub base_url: String,
    pub model: String,
}

impl Config {
    /// Read the config from the environment; `None` (semantic off) unless
    /// both the local server URL and the embedding model are set.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Some(Self {
            base_url: get(ancientos_router::local_llm::URL_ENV)?,
            model: get(MODEL_ENV)?,
        })
    }
}

/// Embedding vectors for one model, keyed by [`cache_key`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VectorCache {
    pub model: String,
    pub vectors: BTreeMap<String, Vec<f32>>,
}

/// Text a post is embedded from: the same sanitized text the ranker reads.
pub fn embed_text(b: &Bookmark) -> String {
    crate::sanitize::sanitize_uncapped(&b.text)
}

/// Cache key for one text under one model: hex sha256 of `model`, a NUL, then `text`.
pub fn cache_key(model: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update([0u8]);
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl VectorCache {
    /// Empty cache for `model`.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            vectors: BTreeMap::new(),
        }
    }

    /// Parse a cache from JSON.
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("vector cache: {e}"))
    }

    /// Load the cache at `path` for `model`. A missing file, unreadable
    /// JSON, or a cache for another model gives an empty cache: vectors are
    /// rebuilt, never mixed across models.
    pub fn load(path: &Path, model: &str) -> Self {
        match std::fs::read_to_string(path)
            .ok()
            .and_then(|s| Self::parse(&s).ok())
        {
            Some(c) if c.model == model => c,
            _ => Self::new(model),
        }
    }

    /// Write the cache to `path` (temp file then rename, so a crash never leaves half a file).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
    }

    /// Cached vector for a post, if any.
    pub fn vector_for(&self, b: &Bookmark) -> Option<&[f32]> {
        self.vectors
            .get(&cache_key(&self.model, &embed_text(b)))
            .map(Vec::as_slice)
    }

    /// Embed every post missing from the cache with `fetch` (batched,
    /// duplicates sent once). Returns how many vectors were added. Errors
    /// when `fetch` fails or returns the wrong count, empty vectors, or a
    /// dimension that differs from the cache; nothing is added then.
    pub fn fill<F>(&mut self, bookmarks: &[Bookmark], mut fetch: F) -> Result<usize, String>
    where
        F: FnMut(&[String]) -> Result<Vec<Vec<f32>>, String>,
    {
        let mut missing: BTreeMap<String, String> = BTreeMap::new();
        for b in bookmarks {
            let text = embed_text(b);
            let key = cache_key(&self.model, &text);
            if !self.vectors.contains_key(&key) {
                missing.insert(key, text);
            }
        }
        let mut dim = self.vectors.values().next().map(Vec::len);
        let pending: Vec<(String, String)> = missing.into_iter().collect();
        let mut added: Vec<(String, Vec<f32>)> = Vec::new();
        for chunk in pending.chunks(BATCH) {
            let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
            let vectors = fetch(&texts)?;
            if vectors.len() != texts.len() {
                return Err(format!(
                    "embedding count mismatch: sent {}, got {}",
                    texts.len(),
                    vectors.len()
                ));
            }
            for ((key, _), v) in chunk.iter().zip(vectors) {
                if v.is_empty() || dim.is_some_and(|d| d != v.len()) {
                    return Err("embedding dimension mismatch".to_string());
                }
                dim = Some(v.len());
                added.push((key.clone(), v));
            }
        }
        let n = added.len();
        self.vectors.extend(added);
        Ok(n)
    }
}

/// Embed `texts` on the local Ollama server (`POST {base}/api/embed`).
/// A trailing `/v1` on the base URL is dropped. Local only; no paid call.
pub fn fetch_local(base_url: &str, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let raw: serde_json::Value = ureq::post(&format!("{base}/api/embed"))
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .send_json(serde_json::json!({ "model": model, "input": texts }))
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("local embedding server: http status {code}"),
            ureq::Error::Transport(_) => "local embedding server unreachable".to_string(),
        })?
        .into_json()
        .map_err(|e| format!("local embedding reply: {e}"))?;
    parse_embed_reply(&raw)
}

/// Parse an Ollama `/api/embed` reply: `{"embeddings": [[f32, ...], ...]}`.
pub fn parse_embed_reply(raw: &serde_json::Value) -> Result<Vec<Vec<f32>>, String> {
    let rows = raw
        .get("embeddings")
        .and_then(|v| v.as_array())
        .ok_or("local embedding reply has no embeddings")?;
    rows.iter()
        .map(|row| {
            row.as_array()
                .ok_or("embedding is not an array")?
                .iter()
                .map(|x| {
                    x.as_f64()
                        .map(|f| f as f32)
                        .ok_or("embedding value is not a number")
                })
                .collect::<Result<Vec<f32>, _>>()
                .map_err(str::to_string)
        })
        .collect()
}

/// Cosine similarity; 0.0 when either vector is all zeros or lengths differ.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Learned taste in embedding space: centroids of approved and skipped posts.
#[derive(Debug, Clone, PartialEq)]
pub struct Taste {
    pos: Vec<f32>,
    neg: Vec<f32>,
}

impl Taste {
    /// Learn from labeled events. `None` (fail closed) when any event lacks
    /// a cached vector, or there is not at least one positive and one
    /// negative event.
    pub fn train(events: &[LabeledEvent], cache: &VectorCache) -> Option<Self> {
        let mut pos: Option<(Vec<f64>, usize)> = None;
        let mut neg: Option<(Vec<f64>, usize)> = None;
        for e in events {
            let v = cache.vector_for(&e.bookmark)?;
            let slot = if e.signal.positive() {
                &mut pos
            } else {
                &mut neg
            };
            let (sum, n) = slot.get_or_insert_with(|| (vec![0.0; v.len()], 0));
            if sum.len() != v.len() {
                return None;
            }
            for (s, x) in sum.iter_mut().zip(v) {
                *s += f64::from(*x);
            }
            *n += 1;
        }
        /// Divide a summed vector by its count.
        fn mean((sum, n): (Vec<f64>, usize)) -> Vec<f32> {
            sum.into_iter().map(|s| (s / n as f64) as f32).collect()
        }
        Some(Self {
            pos: mean(pos?),
            neg: mean(neg?),
        })
    }

    /// Score one vector: cosine to the approved centroid minus cosine to the skipped one.
    pub fn score(&self, v: &[f32]) -> f64 {
        cosine(v, &self.pos) - cosine(v, &self.neg)
    }

    /// Score every post into a `post_id -> score` map; `None` (fail closed)
    /// when any post lacks a cached vector.
    pub fn score_all(
        &self,
        rows: &[Bookmark],
        cache: &VectorCache,
    ) -> Option<HashMap<String, f64>> {
        rows.iter()
            .map(|b| Some((b.post_id.clone(), self.score(cache.vector_for(b)?))))
            .collect()
    }
}

/// Standardize scores to mean 0, standard deviation 1 (all zeros when they are all equal).
fn standardize(scores: &HashMap<String, f64>) -> HashMap<String, f64> {
    let n = scores.len().max(1) as f64;
    let mean = scores.values().sum::<f64>() / n;
    let sd = (scores.values().map(|s| (s - mean).powi(2)).sum::<f64>() / n).sqrt();
    scores
        .iter()
        .map(|(k, s)| (k.clone(), if sd > 0.0 { (s - mean) / sd } else { 0.0 }))
        .collect()
}

/// Blend vote-learned and semantic scores on one scale:
/// `(1 - weight) * z(ranker) + weight * z(semantic)`. Posts missing from one
/// map count as that map's mean. `weight` is clamped to 0.0..=1.0.
pub fn blend(
    ranker: &HashMap<String, f64>,
    semantic: &HashMap<String, f64>,
    weight: f64,
) -> HashMap<String, f64> {
    let w = weight.clamp(0.0, 1.0);
    let (zr, zs) = (standardize(ranker), standardize(semantic));
    ranker
        .keys()
        .chain(semantic.keys())
        .map(|k| {
            let r = zr.get(k).copied().unwrap_or(0.0);
            let s = zs.get(k).copied().unwrap_or(0.0);
            (k.clone(), (1.0 - w) * r + w * s)
        })
        .collect()
}

/// Semantic share of the blend from [`WEIGHT_ENV`]: [`DEFAULT_WEIGHT`]
/// when unset or not a number in 0.0..=1.0.
pub fn weight_from_env() -> f64 {
    std::env::var(WEIGHT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|w| (0.0..=1.0).contains(w))
        .unwrap_or(DEFAULT_WEIGHT)
}

/// Semantic scores for `rows`, embedding misses with `fetch` and saving the
/// cache at `cache_path`. `Err` means fail closed: the caller keeps the
/// vote-learned order.
pub fn scores_with<F>(
    model: &str,
    events: &[LabeledEvent],
    rows: &[Bookmark],
    cache_path: &Path,
    fetch: F,
) -> Result<HashMap<String, f64>, String>
where
    F: FnMut(&[String]) -> Result<Vec<Vec<f32>>, String>,
{
    let mut cache = VectorCache::load(cache_path, model);
    let all: Vec<Bookmark> = rows
        .iter()
        .cloned()
        .chain(events.iter().map(|e| e.bookmark.clone()))
        .collect();
    if cache.fill(&all, fetch)? > 0 {
        cache.save(cache_path)?;
    }
    let taste = Taste::train(events, &cache).ok_or("not enough embedded votes")?;
    taste
        .score_all(rows, &cache)
        .ok_or_else(|| "a post has no vector".to_string())
}

/// Personal scores for `rows`: the vote-learned ranker's, blended with
/// semantic scores when the local embedding model is configured and works.
/// Returns the scores and whether semantic ranking was applied. Any
/// semantic failure is reported on stderr (no post text) and falls back to
/// the ranker alone. Callers only use this when `ranker` is active.
pub fn personal_scores(
    ranker: &Ranker,
    events: &[LabeledEvent],
    rows: &[Bookmark],
    cache_path: &Path,
) -> (HashMap<String, f64>, bool) {
    let base = ranker.score_all(rows.iter());
    let Some(cfg) = Config::from_env() else {
        return (base, false);
    };
    let fetch = |t: &[String]| fetch_local(&cfg.base_url, &cfg.model, t);
    match scores_with(&cfg.model, events, rows, cache_path, fetch) {
        Ok(sem) => (blend(&base, &sem, weight_from_env()), true),
        Err(e) => {
            eprintln!("semantic ranking off (fail closed): {e}");
            (base, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ranker::Signal;

    /// Test helper: a bookmark with `id` and `text`.
    fn bm(id: &str, text: &str) -> Bookmark {
        crate::fixture::parse_jsonl(
            &serde_json::json!({"post_id": id, "url": format!("https://x.com/a/status/{id}"), "text": text})
                .to_string(),
        )
        .unwrap()
        .remove(0)
    }

    /// Test helper: deterministic fake embedder; "stretch"/"hip" texts point
    /// one way, "recipe"/"oats" the other.
    fn fake(texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                let a = (t.contains("stretch") || t.contains("hip")) as u8 as f32;
                let b = (t.contains("recipe") || t.contains("oats")) as u8 as f32;
                vec![a, b, 0.1]
            })
            .collect())
    }

    /// Test helper: labeled event.
    fn ev(b: &Bookmark, signal: Signal) -> LabeledEvent {
        LabeledEvent {
            bookmark: b.clone(),
            signal,
        }
    }

    #[test]
    fn cache_key_depends_on_model_and_text() {
        assert_eq!(cache_key("m", "t"), cache_key("m", "t"));
        assert_ne!(cache_key("m", "t"), cache_key("n", "t"));
        assert_ne!(cache_key("m", "t"), cache_key("m", "u"));
        assert_eq!(cache_key("m", "t").len(), 64);
    }

    #[test]
    fn fill_embeds_each_missing_text_once_and_keeps_no_text() {
        let rows = vec![
            bm("1", "hip stretch"),
            bm("2", "hip stretch"),
            bm("3", "oats"),
        ];
        let mut cache = VectorCache::new("m");
        let mut calls = 0;
        let n = cache
            .fill(&rows, |t| {
                calls += 1;
                fake(t)
            })
            .unwrap();
        assert_eq!((n, calls), (2, 1));
        assert_eq!(
            cache.fill(&rows, |_| Err("must not call".into())).unwrap(),
            0
        );
        let json = serde_json::to_string(&cache).unwrap();
        assert!(!json.contains("stretch") && !json.contains("oats"));
    }

    #[test]
    fn fill_rejects_bad_replies_and_adds_nothing() {
        let rows = vec![bm("1", "a"), bm("2", "b")];
        let mut cache = VectorCache::new("m");
        assert!(cache.fill(&rows, |_| Ok(vec![vec![1.0]])).is_err());
        assert!(cache
            .fill(&rows, |_| Ok(vec![vec![1.0], vec![1.0, 2.0]]))
            .is_err());
        assert!(cache.fill(&rows, |_| Ok(vec![vec![], vec![]])).is_err());
        assert!(cache.fill(&rows, |_| Err("down".into())).is_err());
        assert!(cache.vectors.is_empty());
    }

    #[test]
    fn cache_round_trips_and_ignores_other_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE_NAME);
        let mut cache = VectorCache::new("m");
        cache.fill(&[bm("1", "hip")], fake).unwrap();
        cache.save(&path).unwrap();
        assert_eq!(VectorCache::load(&path, "m"), cache);
        assert!(VectorCache::load(&path, "other").vectors.is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(VectorCache::load(&path, "m").vectors.is_empty());
    }

    #[test]
    fn taste_prefers_posts_like_approved_ones() {
        let liked = bm("1", "hip stretch");
        let skipped = bm("2", "oats recipe");
        let mut cache = VectorCache::new("m");
        let rows = vec![bm("3", "morning hip opener"), bm("4", "baked oats")];
        cache.fill(&[liked.clone(), skipped.clone()], fake).unwrap();
        cache.fill(&rows, fake).unwrap();
        let taste = Taste::train(
            &[ev(&liked, Signal::Approve), ev(&skipped, Signal::Skip)],
            &cache,
        )
        .unwrap();
        let s = taste.score_all(&rows, &cache).unwrap();
        assert!(s["3"] > 0.0 && s["4"] < 0.0, "{s:?}");
    }

    #[test]
    fn taste_fails_closed_without_vectors_or_both_classes() {
        let a = bm("1", "hip");
        let b = bm("2", "oats");
        let mut cache = VectorCache::new("m");
        assert!(Taste::train(&[ev(&a, Signal::Approve), ev(&b, Signal::Skip)], &cache).is_none());
        cache.fill(&[a.clone(), b.clone()], fake).unwrap();
        assert!(Taste::train(&[ev(&a, Signal::Approve)], &cache).is_none());
        let t = Taste::train(&[ev(&a, Signal::Approve), ev(&b, Signal::Skip)], &cache).unwrap();
        assert!(t.score_all(&[bm("3", "unseen")], &cache).is_none());
    }

    #[test]
    fn blend_puts_both_scores_on_one_scale() {
        let r: HashMap<String, f64> = [("a".into(), 10.0), ("b".into(), -10.0)].into();
        let s: HashMap<String, f64> = [("a".into(), -0.1), ("b".into(), 0.1)].into();
        let even = blend(&r, &s, 0.5);
        assert!(even["a"].abs() < 1e-9 && even["b"].abs() < 1e-9);
        assert!(blend(&r, &s, 0.0)["a"] > 0.0);
        assert!(blend(&r, &s, 1.0)["b"] > 0.0);
        assert!(blend(&r, &s, 7.0)["b"] > 0.0, "weight is clamped");
        let flat: HashMap<String, f64> = [("a".into(), 1.0), ("b".into(), 1.0)].into();
        assert_eq!(blend(&flat, &flat, 0.5)["a"], 0.0);
    }

    #[test]
    fn cosine_handles_edge_cases() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn parse_embed_reply_reads_ollama_shape() {
        let ok = serde_json::json!({"embeddings": [[0.5, -1], [2, 3]]});
        assert_eq!(
            parse_embed_reply(&ok).unwrap(),
            vec![vec![0.5, -1.0], vec![2.0, 3.0]]
        );
        assert!(parse_embed_reply(&serde_json::json!({"error": "no model"})).is_err());
        assert!(parse_embed_reply(&serde_json::json!({"embeddings": [["x"]]})).is_err());
    }

    #[test]
    fn scores_with_fails_closed_when_the_model_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE_NAME);
        let (a, b) = (bm("1", "hip"), bm("2", "oats"));
        let events = [ev(&a, Signal::Approve), ev(&b, Signal::Skip)];
        let rows = [bm("3", "hip stretch")];
        assert!(scores_with("m", &events, &rows, &path, |_| Err("down".into())).is_err());
        assert!(!path.exists());
        let s = scores_with("m", &events, &rows, &path, fake).unwrap();
        assert!(s["3"] > 0.0);
        assert!(path.exists());
        let again = scores_with("m", &events, &rows, &path, |_| Err("cached".into())).unwrap();
        assert_eq!(s, again);
    }

    #[test]
    fn fetch_local_reads_a_loopback_server_and_fails_closed_when_down() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Read the whole request (headers and body) before replying, so a
            // loaded machine never sees the reply race the body upload.
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if got.len() >= end + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&got).to_string();
            let body = r#"{"embeddings":[[1.0,2.0]]}"#;
            write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            req
        });
        let v = fetch_local(&format!("http://{addr}/v1"), "m", &["hi".into()]).unwrap();
        assert_eq!(v, vec![vec![1.0, 2.0]]);
        assert!(server.join().unwrap().starts_with("POST /api/embed"));
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        assert!(fetch_local(&format!("http://{dead_addr}"), "m", &["hi".into()]).is_err());
    }
}
