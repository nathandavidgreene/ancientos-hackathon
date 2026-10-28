//! `analyze_media`: read a bookmark's images and video poster frames and
//! turn them into findings with provenance, behind the shared gate.
//!
//! Input matches the planned `x_bookmark_media` table (one row per
//! `post_id` + `media_key`). A request names a post and its
//! `attachments.media_keys`; each key is resolved against the rows. A key
//! with no row, or a row with no safe X media URL, is skipped and logged
//! (fail closed). Photos use `url`; videos and GIFs use the
//! `preview_image_url` poster as frame 0 (no SAM, no video decoding).
//!
//! Built-in check: a `text` or `claim` finding must carry a non-empty OCR
//! quote or a valid normalized region box, else it is rejected and logged.
//! Accepted findings are never written to LifeVault here: [`propose_store`]
//! records a `media.findings.store` approval on the hash-chained log, and
//! [`release_store`] hands them back only after a human approved it.
//!
//! The model call is behind `ANCIENTOS_ALLOW_PAID=1` plus a per-run item cap
//! (`ANCIENTOS_MEDIA_MAX_ITEMS`); without both, [`PaidConfig::from_env`]
//! refuses. No live backend ships yet; tests use [`MockBackend`].
use ancientos_core::approvals::{self, ApprovalRequest};
use ancientos_core::gate::{Gate, Risk};
use ancientos_core::log::DecisionLog;
use serde::{Deserialize, Serialize};

/// Agent name recorded on log entries and approvals.
pub const AGENT: &str = "bookmarks-agent";
/// Gated action that stores accepted findings.
pub const STORE_ACTION: &str = "media.findings.store";
/// Paid-call switch; analysis runs only when it equals `"1"`.
pub const PAID_ENV_VAR: &str = "ANCIENTOS_ALLOW_PAID";
/// Per-run cap on media items sent to the model; required, 1..=[`HARD_MAX_ITEMS`].
pub const MAX_ITEMS_ENV_VAR: &str = "ANCIENTOS_MEDIA_MAX_ITEMS";
/// Upper bound on the per-run cap, whatever the env says.
pub const HARD_MAX_ITEMS: usize = 200;
/// Log kind for a skipped media key.
pub const SKIPPED_KIND: &str = "media.skipped";
/// Log kind for a finding rejected by the evidence check.
pub const REJECTED_KIND: &str = "media.finding_rejected";

/// One `variants[]` entry of an `x_bookmark_media` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub url: String,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub bit_rate: Option<u64>,
}

/// One row of the planned `x_bookmark_media` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRow {
    pub post_id: String,
    pub media_key: String,
    /// `photo`, `video`, or `animated_gif`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub preview_image_url: Option<String>,
    /// `null` in the table for photos; read as empty.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub variants: Vec<Variant>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub alt_text: Option<String>,
}

/// Deserialize a JSON `null` (or absent field) as an empty list.
fn null_as_empty<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// What to analyze: a post and its `attachments.media_keys`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRequest {
    pub post_id: String,
    pub media_keys: Vec<String>,
}

/// Where a finding came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub post_id: String,
    pub media_key: String,
    pub media_url: String,
    /// Frame index for video/GIF posters; `None` for photos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_idx: Option<u32>,
}

/// A media item ready for the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedMedia {
    pub provenance: Provenance,
    pub kind: String,
    pub alt_text: Option<String>,
}

/// Region box in normalized image coordinates (0..=1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Region {
    /// True when the box is finite, non-empty, and inside the image.
    pub fn is_valid(&self) -> bool {
        let v = [self.x, self.y, self.w, self.h];
        v.iter().all(|n| n.is_finite())
            && self.x >= 0.0
            && self.y >= 0.0
            && self.w > 0.0
            && self.h > 0.0
            && self.x + self.w <= 1.0
            && self.y + self.h <= 1.0
    }
}

/// Kind of finding the model reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Text read off the image (OCR).
    Text,
    /// A factual claim the image makes.
    Claim,
    /// A plain visual description.
    Description,
}

/// What a backend returns for one media item, before checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawFinding {
    pub kind: FindingKind,
    pub text: String,
    #[serde(default)]
    pub quote: Option<String>,
    #[serde(default)]
    pub region: Option<Region>,
}

/// A checked finding with provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
    pub provenance: Provenance,
}

/// The model behind `analyze_media`.
pub trait MediaBackend {
    /// Analyze one media item; returns unchecked findings.
    fn analyze(&self, item: &ResolvedMedia) -> Result<Vec<RawFinding>, String>;
}

/// Test/offline backend returning fixed findings for every item.
pub struct MockBackend {
    pub findings: Vec<RawFinding>,
}

impl MediaBackend for MockBackend {
    /// Return a copy of the canned findings.
    fn analyze(&self, _item: &ResolvedMedia) -> Result<Vec<RawFinding>, String> {
        Ok(self.findings.clone())
    }
}

/// Permission to make paid calls, with the per-run cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaidConfig {
    pub max_items: usize,
}

impl PaidConfig {
    /// Read the paid switch and cap from the environment; refuses unless
    /// `ANCIENTOS_ALLOW_PAID=1` and a cap in 1..=[`HARD_MAX_ITEMS`] is set.
    pub fn from_env() -> Result<Self, String> {
        Self::from_values(
            std::env::var(PAID_ENV_VAR).ok().as_deref(),
            std::env::var(MAX_ITEMS_ENV_VAR).ok().as_deref(),
        )
    }

    /// Pure form of [`from_env`](Self::from_env) for tests.
    pub fn from_values(paid: Option<&str>, cap: Option<&str>) -> Result<Self, String> {
        if paid != Some("1") {
            return Err(format!(
                "analyze_media refused: paid model calls are off (set {PAID_ENV_VAR}=1)"
            ));
        }
        let cap = cap.ok_or_else(|| {
            format!("analyze_media refused: {MAX_ITEMS_ENV_VAR} (per-run cap) is not set")
        })?;
        match cap.trim().parse::<usize>() {
            Ok(n) if (1..=HARD_MAX_ITEMS).contains(&n) => Ok(Self { max_items: n }),
            _ => Err(format!(
                "analyze_media refused: {MAX_ITEMS_ENV_VAR} must be 1..={HARD_MAX_ITEMS}"
            )),
        }
    }
}

/// A media key that could not be analyzed, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skipped {
    pub post_id: String,
    pub media_key: String,
    pub reason: String,
}

/// A finding that failed the evidence check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejected {
    pub finding: RawFinding,
    pub provenance: Provenance,
    pub reason: String,
}

/// Outcome of one `analyze_media` run.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub accepted: Vec<Finding>,
    pub rejected: Vec<Rejected>,
    pub skipped: Vec<Skipped>,
}

/// Parse `x_bookmark_media` rows from JSONL; any bad line is an error.
pub fn parse_rows(jsonl: &str) -> Result<Vec<MediaRow>, String> {
    jsonl
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).map_err(|e| format!("line {}: {e}", i + 1)))
        .collect()
}

/// Resolve each requested key against the rows. Photos use `url`; videos
/// and GIFs use `preview_image_url` as frame 0. Missing rows, other post
/// ids, unknown types, and unsafe or absent URLs are skipped.
pub fn resolve(req: &MediaRequest, rows: &[MediaRow]) -> (Vec<ResolvedMedia>, Vec<Skipped>) {
    let mut ok = Vec::new();
    let mut skipped = Vec::new();
    for key in &req.media_keys {
        let skip = |reason: &str| Skipped {
            post_id: req.post_id.clone(),
            media_key: key.clone(),
            reason: reason.to_string(),
        };
        let Some(row) = rows
            .iter()
            .find(|r| &r.media_key == key && r.post_id == req.post_id)
        else {
            skipped.push(skip("no x_bookmark_media row for this post and key"));
            continue;
        };
        let (raw, frame_idx) = match row.kind.as_str() {
            "photo" => (row.url.as_deref(), None),
            "video" | "animated_gif" => (row.preview_image_url.as_deref(), Some(0)),
            _ => {
                skipped.push(skip("unknown media type"));
                continue;
            }
        };
        let Some(raw) = raw else {
            skipped.push(skip("row has no image URL"));
            continue;
        };
        let Some(url) = crate::media::safe_media_url(raw) else {
            skipped.push(skip("URL is not a safe X media URL"));
            continue;
        };
        ok.push(ResolvedMedia {
            provenance: Provenance {
                post_id: req.post_id.clone(),
                media_key: key.clone(),
                media_url: url,
                frame_idx,
            },
            kind: row.kind.clone(),
            alt_text: row.alt_text.clone(),
        });
    }
    (ok, skipped)
}

/// Evidence check: text and claim findings need a non-empty quote or a
/// valid region; every finding needs non-empty text. `Err` is the reason.
pub fn check(f: &RawFinding) -> Result<(), String> {
    if f.text.trim().is_empty() {
        return Err("empty finding text".into());
    }
    if f.region.is_some_and(|r| !r.is_valid()) {
        return Err("region box is out of bounds".into());
    }
    if matches!(f.kind, FindingKind::Text | FindingKind::Claim) {
        let has_quote = f.quote.as_deref().is_some_and(|q| !q.trim().is_empty());
        if !has_quote && f.region.is_none() {
            return Err("text/claim finding has no OCR quote or region box".into());
        }
    }
    Ok(())
}

/// Run `analyze_media`: resolve keys, enforce the per-run cap, call the
/// backend, check every finding, and log skips and rejections. Paid
/// permission is the caller's [`PaidConfig`]; nothing is stored here.
pub fn analyze(
    log: &mut DecisionLog,
    req: &MediaRequest,
    rows: &[MediaRow],
    backend: &dyn MediaBackend,
    paid: PaidConfig,
) -> Result<Report, String> {
    let (items, mut skipped) = resolve(req, rows);
    if items.len() > paid.max_items {
        return Err(format!(
            "analyze_media refused: {} items exceeds per-run cap {}",
            items.len(),
            paid.max_items
        ));
    }
    let mut report = Report::default();
    for item in &items {
        let raws = match backend.analyze(item) {
            Ok(r) => r,
            Err(e) => {
                skipped.push(Skipped {
                    post_id: item.provenance.post_id.clone(),
                    media_key: item.provenance.media_key.clone(),
                    reason: format!("backend error: {e}"),
                });
                continue;
            }
        };
        for raw in raws {
            match check(&raw) {
                Ok(()) => report.accepted.push(Finding {
                    kind: raw.kind,
                    text: raw.text,
                    quote: raw.quote,
                    region: raw.region,
                    provenance: item.provenance.clone(),
                }),
                Err(reason) => {
                    let r = Rejected {
                        finding: raw,
                        provenance: item.provenance.clone(),
                        reason,
                    };
                    log.append(
                        REJECTED_KIND,
                        serde_json::to_value(&r).map_err(|e| e.to_string())?,
                    )
                    .map_err(|e| e.to_string())?;
                    report.rejected.push(r);
                }
            }
        }
    }
    for s in &skipped {
        log.append(
            SKIPPED_KIND,
            serde_json::to_value(s).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    report.skipped = skipped;
    Ok(report)
}

/// Gate with `media.findings.store` registered as high risk (needs a human).
pub fn store_gate() -> Gate {
    let mut g = Gate::new();
    g.register(STORE_ACTION, Risk::High);
    g
}

/// Payload bound by the store approval.
pub fn store_payload(findings: &[Finding]) -> serde_json::Value {
    serde_json::json!({ "findings": findings })
}

/// Record a `media.findings.store` approval request for accepted findings.
/// Refuses an empty list. Nothing is stored until [`release_store`].
pub fn propose_store(
    log: &mut DecisionLog,
    gate: &Gate,
    findings: &[Finding],
    now: u64,
) -> Result<ApprovalRequest, String> {
    if findings.is_empty() {
        return Err("no accepted findings to store".into());
    }
    let summary = format!("Store {} media finding(s)", findings.len());
    approvals::request(
        log,
        gate,
        AGENT,
        STORE_ACTION,
        &summary,
        store_payload(findings),
        now,
        None,
    )
    .map_err(|e| e.to_string())
}

/// Release an approved store request and return the findings to write.
/// Fails unless the log shows a human approval for exactly these findings.
pub fn release_store(
    log: &mut DecisionLog,
    approval_id: &str,
    findings: &[Finding],
) -> Result<Vec<Finding>, String> {
    let payload = store_payload(findings);
    let hash = approvals::canonical_hash(&approvals::redact(&payload));
    let token =
        approvals::claim_release_token(log, approval_id, &hash).map_err(|e| e.to_string())?;
    approvals::release(log, token, &payload).map_err(|e| e.to_string())?;
    Ok(findings.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../fixtures/media/x_bookmark_media_synthetic.jsonl");
    const NOW: u64 = 1_800_000_000;

    /// Test helper: temp decision log.
    fn open_log() -> (tempfile::TempDir, DecisionLog) {
        let d = tempfile::tempdir().unwrap();
        let l = DecisionLog::open(d.path().join("log.jsonl")).unwrap();
        (d, l)
    }

    /// Test helper: paid config with a cap.
    fn paid(n: usize) -> PaidConfig {
        PaidConfig { max_items: n }
    }

    /// Test helper: request for a post with keys.
    fn req(post: &str, keys: &[&str]) -> MediaRequest {
        MediaRequest {
            post_id: post.into(),
            media_keys: keys.iter().map(|k| k.to_string()).collect(),
        }
    }

    /// Test helper: raw finding.
    fn raw(kind: FindingKind, quote: Option<&str>, region: Option<Region>) -> RawFinding {
        RawFinding {
            kind,
            text: "[synthetic] 10k steps".into(),
            quote: quote.map(String::from),
            region,
        }
    }

    #[test]
    fn fixture_parses_in_table_shape() {
        let rows = parse_rows(FIXTURE).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[1].variants[0].bit_rate, Some(832000));
        assert_eq!(rows[1].duration_ms, Some(15000));
    }

    #[test]
    fn null_optional_fields_parse() {
        let line = r#"{"post_id":"demo-1","media_key":"3_k","type":"photo","url":"https://pbs.twimg.com/media/demo.jpg","preview_image_url":null,"variants":null,"width":null,"height":null,"duration_ms":null,"alt_text":null}"#;
        let rows = parse_rows(line).unwrap();
        assert!(rows[0].variants.is_empty());
        assert_eq!(rows[0].duration_ms, None);
        let (ok, sk) = resolve(&req("demo-1", &["3_k"]), &rows);
        assert_eq!((ok.len(), sk.len()), (1, 0));
    }

    #[test]
    fn resolves_photo_url_and_video_poster_frame0() {
        let rows = parse_rows(FIXTURE).unwrap();
        let (ok, sk) = resolve(&req("demo-9001", &["3_demo_photo", "7_demo_video"]), &rows);
        assert!(sk.is_empty());
        assert_eq!(ok[0].provenance.frame_idx, None);
        assert!(ok[0]
            .provenance
            .media_url
            .ends_with("demo_chart_placeholder.jpg"));
        assert_eq!(ok[1].provenance.frame_idx, Some(0));
        assert!(ok[1].provenance.media_url.contains("ext_tw_video_thumb"));
    }

    #[test]
    fn skips_missing_row_no_url_bad_host_and_wrong_post() {
        let rows = parse_rows(FIXTURE).unwrap();
        let (ok, sk) = resolve(
            &req(
                "demo-9002",
                &["16_demo_gif", "3_demo_nourl", "nope", "3_demo_photo"],
            ),
            &rows,
        );
        assert_eq!(ok.len(), 1);
        assert_eq!(sk.len(), 3);
        let (ok, sk) = resolve(&req("demo-9003", &["3_demo_badhost"]), &rows);
        assert!(ok.is_empty());
        assert!(sk[0].reason.contains("safe"));
    }

    #[test]
    fn paid_off_by_default_and_cap_required() {
        assert!(PaidConfig::from_values(None, Some("20"))
            .unwrap_err()
            .contains(PAID_ENV_VAR));
        assert!(PaidConfig::from_values(Some("true"), Some("20")).is_err());
        assert!(PaidConfig::from_values(Some("1"), None)
            .unwrap_err()
            .contains(MAX_ITEMS_ENV_VAR));
        assert!(PaidConfig::from_values(Some("1"), Some("0")).is_err());
        assert!(PaidConfig::from_values(Some("1"), Some("999")).is_err());
        assert_eq!(PaidConfig::from_values(Some("1"), Some("20")), Ok(paid(20)));
    }

    #[test]
    fn check_rejects_unbacked_text_and_claims() {
        assert!(check(&raw(FindingKind::Text, None, None)).is_err());
        assert!(check(&raw(FindingKind::Claim, Some("  "), None)).is_err());
        assert!(check(&raw(FindingKind::Claim, Some("10k"), None)).is_ok());
        let r = Region {
            x: 0.1,
            y: 0.1,
            w: 0.2,
            h: 0.2,
        };
        assert!(check(&raw(FindingKind::Text, None, Some(r))).is_ok());
        let bad = Region {
            x: 0.9,
            y: 0.1,
            w: 0.2,
            h: 0.2,
        };
        assert!(check(&raw(FindingKind::Text, Some("q"), Some(bad))).is_err());
        assert!(check(&raw(FindingKind::Description, None, None)).is_ok());
    }

    #[test]
    fn analyze_logs_rejections_and_skips_with_provenance() {
        let (_d, mut log) = open_log();
        let rows = parse_rows(FIXTURE).unwrap();
        let b = MockBackend {
            findings: vec![
                raw(FindingKind::Text, Some("10k"), None),
                raw(FindingKind::Claim, None, None),
            ],
        };
        let rep = analyze(
            &mut log,
            &req("demo-9002", &["16_demo_gif", "3_demo_nourl"]),
            &rows,
            &b,
            paid(5),
        )
        .unwrap();
        assert_eq!(rep.accepted.len(), 1);
        assert_eq!(rep.accepted[0].provenance.media_key, "16_demo_gif");
        assert_eq!(rep.rejected.len(), 1);
        assert_eq!(rep.skipped.len(), 1);
        let kinds: Vec<_> = log.verify().unwrap().into_iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![REJECTED_KIND, SKIPPED_KIND]);
    }

    #[test]
    fn analyze_refuses_over_cap_before_any_call() {
        struct Panics;
        impl MediaBackend for Panics {
            /// Fails the test if called.
            fn analyze(&self, _: &ResolvedMedia) -> Result<Vec<RawFinding>, String> {
                panic!("backend must not be called")
            }
        }
        let (_d, mut log) = open_log();
        let rows = parse_rows(FIXTURE).unwrap();
        let e = analyze(
            &mut log,
            &req("demo-9001", &["3_demo_photo", "7_demo_video"]),
            &rows,
            &Panics,
            paid(1),
        )
        .unwrap_err();
        assert!(e.contains("cap"));
    }

    #[test]
    fn store_needs_human_approval_then_releases_once() {
        let (_d, mut log) = open_log();
        let rows = parse_rows(FIXTURE).unwrap();
        let b = MockBackend {
            findings: vec![raw(FindingKind::Text, Some("10k"), None)],
        };
        let rep = analyze(
            &mut log,
            &req("demo-9001", &["3_demo_photo"]),
            &rows,
            &b,
            paid(5),
        )
        .unwrap();
        let gate = store_gate();
        let r = propose_store(&mut log, &gate, &rep.accepted, NOW).unwrap();
        assert!(
            release_store(&mut log, &r.id, &rep.accepted).is_err(),
            "pending must not release"
        );
        approvals::approve(&mut log, &r.id, &r.payload_hash, "nathan", NOW + 1).unwrap();
        assert!(
            release_store(&mut log, &r.id, &rep.accepted[..0]).is_err(),
            "other payload"
        );
        assert_eq!(
            release_store(&mut log, &r.id, &rep.accepted).unwrap().len(),
            1
        );
        assert!(
            release_store(&mut log, &r.id, &rep.accepted).is_err(),
            "exactly once"
        );
        assert!(propose_store(&mut log, &gate, &[], NOW).is_err());
        assert!(
            propose_store(&mut log, &Gate::new(), &rep.accepted, NOW).is_err(),
            "empty gate denies"
        );
    }
}
