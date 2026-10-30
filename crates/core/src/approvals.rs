//! Human approval queue: agents propose risky actions, a person approves or denies.
//!
//! State lives only in the existing hash-chained [`DecisionLog`]:
//! entries of kind `approval.requested`, `approval.approved`, `approval.denied`,
//! `approval.expired` and `approval.released` are folded into current state.
//! The log is never rewritten or truncated. Every function here fails closed:
//! an unknown id, a bad payload hash, an expired request, an empty decider or
//! a tampered log is an error that appends nothing.
//!
//! Flow:
//! 1. [`request`] records a proposal, but only for actions the caller-provided
//!    [`Gate`] evaluates as `NeedsHuman`.
//!    Gate-allowed actions are refused ("no approval is needed") and
//!    gate-denied actions are refused with the deny reason.
//! 2. A human calls [`approve`] or [`deny`] exactly once, passing the
//!    `payload_hash` they read from the pending request so a stale view can
//!    never decide a changed request.
//! 3. The caller passes the [`ReleaseToken`] from `approve` to [`release`]
//!    with the payload it is about to execute; only after `release` returns
//!    `Ok` may the action run.
//!
//! Requests carry a mandatory expiry (default 24h). A request that is still
//! undecided past `expires_at` reads as [`Status::Expired`]; [`sweep_expired`]
//! records that lazily as `approval.expired` entries. A decision, once made,
//! stands: decided requests never become expired.
//!
//! ## Secrets
//!
//! Payloads are [`redact`]ed before hashing and before writing: key detection
//! delegates to [`crate::redact::is_secret_key`] (any object key whose
//! lowercased alphanumeric form contains `token`, `secret`, `password`,
//! `apikey`, `credential` or `authorization`, at any depth) and values behind
//! those keys become `"[REDACTED]"`; every other string is additionally
//! scrubbed of embedded secrets via [`crate::redact::redact_text`], so the
//! hashed payload is already a fixed point under the log layer's own
//! redaction. The stored payload and the `payload_hash` therefore never
//! contain secret values, and callers may pass the original payload to
//! [`release`] — it is redacted again before the hash is re-checked. Never put
//! secrets in `summary` either: summaries reach the log verbatim unless they
//! contain secret-shaped content, which the log layer redacts on write.
//!
//! ## Entry schemas
//!
//! - `approval.requested`: `{id, agent, action, summary, payload, payload_hash,
//!   requested_at, expires_at}`
//! - `approval.approved`: `{id, payload_hash, decided_by, decided_at}`
//! - `approval.denied`: `{id, payload_hash, decided_by, reason, decided_at}`
//! - `approval.expired`: `{id, payload_hash, expired_at}`
//! - `approval.released`: `{id, payload_hash}` (no clock read: `release` takes no
//!   `now`, and the approve decision already carries `decided_at`)
//!
//! Folding is defensive: malformed approval entries, entries for unknown ids,
//! decision entries whose hash does not match the request, duplicate requests
//! and second decisions are ignored (first valid entry wins). Request entries
//! are additionally checked by recomputing the id and the payload hash.

use crate::gate::{Gate, Proposal, Verdict};
use crate::log::{DecisionLog, Entry, LogError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Time-to-live used when [`request`] is called with `ttl: None`.
pub const DEFAULT_TTL_SECS: u64 = 24 * 60 * 60;

pub const REQUESTED_KIND: &str = "approval.requested";
pub const APPROVED_KIND: &str = "approval.approved";
pub const DENIED_KIND: &str = "approval.denied";
pub const EXPIRED_KIND: &str = "approval.expired";
pub const RELEASED_KIND: &str = "approval.released";

/// Replacement for redacted payload values.
pub const REDACTED: &str = "[REDACTED]";

/// Decider recorded for system-applied expiry (no human involved).
pub const SYSTEM_DECIDER: &str = "system";

#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error("no approval is needed for action '{0}' (gate allows it)")]
    NotNeeded(String),
    #[error("action refused by gate: {0}")]
    GateDenied(String),
    #[error("unknown approval id '{0}'")]
    Unknown(String),
    #[error("approval '{0}' is already decided ({1})")]
    AlreadyDecided(String, String),
    #[error("approval '{0}' has expired")]
    Expired(String),
    #[error("payload hash mismatch for approval '{0}'")]
    HashMismatch(String),
    #[error("decided_by must be non-empty")]
    EmptyDecider,
    #[error("expires_at ({0}) must be after requested_at ({1})")]
    BadExpiry(u64, u64),
    #[error("approval '{0}' is not approved (status: {1})")]
    NotApproved(String, String),
    #[error("approval '{0}' was already released")]
    AlreadyReleased(String),
    #[error("log: {0}")]
    Log(#[from] LogError),
}

/// A proposed risky action awaiting (or holding) a human decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Content-derived id: first 16 hex chars of the sha256 of
    /// `agent|action|payload_hash|requested_at`.
    pub id: String,
    pub agent: String,
    pub action: String,
    pub summary: String,
    /// Redacted payload ([`redact`] applied at construction).
    pub payload: serde_json::Value,
    /// sha256 hex of the canonical JSON of the redacted payload.
    pub payload_hash: String,
    pub requested_at: u64,
    pub expires_at: u64,
}

/// Lifecycle state of one approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Approved,
    Denied,
    Expired,
    Released,
}

impl Status {
    /// Stable string form of the approval status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Released => "released",
        }
    }
}

/// A recorded decision (approve, deny, or system expiry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionInfo {
    pub outcome: Status,
    pub decided_by: String,
    pub decided_at: u64,
    pub reason: Option<String>,
}

/// A request plus its folded state. `status` is folded from log entries only;
/// use [`status_at`](Approval::status_at) to overlay time-based expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub request: ApprovalRequest,
    pub status: Status,
    pub decision: Option<DecisionInfo>,
}

impl Approval {
    /// Status at `now`: still-Pending past `expires_at` reads Expired.
    /// Decided requests keep their decision regardless of time.
    pub fn status_at(&self, now: u64) -> Status {
        if self.status == Status::Pending && now > self.request.expires_at {
            Status::Expired
        } else {
            self.status
        }
    }

    /// Effective decision at `now`, synthesizing system expiry for requests
    /// that expired without a swept `approval.expired` entry yet.
    pub fn decision_at(&self, now: u64) -> Option<DecisionInfo> {
        if let Some(d) = &self.decision {
            return Some(d.clone());
        }
        if self.status_at(now) == Status::Expired {
            return Some(DecisionInfo {
                outcome: Status::Expired,
                decided_by: SYSTEM_DECIDER.to_string(),
                decided_at: self.request.expires_at,
                reason: None,
            });
        }
        None
    }

    /// True if still pending (not decided, not expired) at `now`.
    pub fn is_pending_at(&self, now: u64) -> bool {
        self.status_at(now) == Status::Pending
    }
}

/// Single-use token proving a human approved one request. Deliberately not
/// `Clone` and not `Serialize`: it cannot be duplicated or persisted, and
/// [`release`] consumes it by value. The log is still the source of truth —
/// a second release for the same id fails even with another token.
#[derive(Debug)]
pub struct ReleaseToken {
    approval_id: String,
    payload_hash: String,
}

impl ReleaseToken {
    /// Id of the approval this token releases.
    pub fn approval_id(&self) -> &str {
        &self.approval_id
    }

    /// Hash of the approved payload this token releases.
    pub fn payload_hash(&self) -> &str {
        &self.payload_hash
    }
}

/// Deep copy of `value` with secret-bearing keys replaced by `"[REDACTED]"`
/// (key rule delegated to [`crate::redact::is_secret_key`], matched at any
/// depth) and embedded secrets in every other string scrubbed via
/// [`crate::redact::redact_text`]. Arrays are walked, other scalars are kept.
/// The result is a fixed point under [`DecisionLog::append`]'s redaction, so
/// the stored payload always hashes to the precomputed `payload_hash`.
pub fn redact(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if crate::redact::is_secret_key(k) {
                        (k.clone(), serde_json::Value::String(REDACTED.to_string()))
                    } else {
                        (k.clone(), redact(v))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact).collect())
        }
        serde_json::Value::String(s) => {
            serde_json::Value::String(crate::redact::redact_text(s).text)
        }
        scalar => scalar.clone(),
    }
}

/// Canonical JSON text: object keys sorted recursively, no whitespace.
/// (Independent of serde_json's map ordering or formatting choices.)
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                out.push_str(&canonical(&map[*k]));
            }
            out.push('}');
            out
        }
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        scalar => scalar.to_string(),
    }
}

/// sha256 hex of the canonical JSON of `value`. Hash the redacted payload:
/// `canonical_hash(&redact(&payload))`.
pub fn canonical_hash(value: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(canonical(value));
    hex::encode(h.finalize())
}

/// First 16 hex chars of sha256(`agent|action|payload_hash|requested_at`).
fn derive_id(agent: &str, action: &str, payload_hash: &str, requested_at: u64) -> String {
    let mut h = Sha256::new();
    h.update(format!("{agent}|{action}|{payload_hash}|{requested_at}"));
    hex::encode(h.finalize())[..16].to_string()
}

impl ApprovalRequest {
    /// Build a request, redacting the payload before hashing. Refuses
    /// `expires_at <= requested_at`. (The gate check lives in [`request`],
    /// the only entry point that writes to the log.)
    pub fn new(
        agent: &str,
        action: &str,
        summary: &str,
        payload: serde_json::Value,
        requested_at: u64,
        expires_at: u64,
    ) -> Result<Self, ApprovalError> {
        if expires_at <= requested_at {
            return Err(ApprovalError::BadExpiry(expires_at, requested_at));
        }
        let payload = redact(&payload);
        let payload_hash = canonical_hash(&payload);
        let id = derive_id(agent, action, &payload_hash, requested_at);
        Ok(Self {
            id,
            agent: agent.to_string(),
            action: action.to_string(),
            summary: summary.to_string(),
            payload,
            payload_hash,
            requested_at,
            expires_at,
        })
    }

    /// Decision log payload for this request.
    fn to_entry_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "agent": self.agent,
            "action": self.action,
            "summary": self.summary,
            "payload": self.payload,
            "payload_hash": self.payload_hash,
            "requested_at": self.requested_at,
            "expires_at": self.expires_at,
        })
    }
}

/// String field `key` of a JSON object.
fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(str::to_string)
}

/// u64 field `key` of a JSON object.
fn u64_field(v: &serde_json::Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

/// Parse and validate a stored request entry; `None` when malformed or when
/// the recomputed id / payload hash does not match (fail closed: the entry
/// is ignored while folding).
fn parse_request(v: &serde_json::Value) -> Option<ApprovalRequest> {
    let req = ApprovalRequest {
        id: str_field(v, "id")?,
        agent: str_field(v, "agent")?,
        action: str_field(v, "action")?,
        summary: str_field(v, "summary")?,
        payload: v.get("payload")?.clone(),
        payload_hash: str_field(v, "payload_hash")?,
        requested_at: u64_field(v, "requested_at")?,
        expires_at: u64_field(v, "expires_at")?,
    };
    if req.expires_at <= req.requested_at {
        return None;
    }
    if req.id != derive_id(&req.agent, &req.action, &req.payload_hash, req.requested_at) {
        return None;
    }
    if req.payload_hash != canonical_hash(&req.payload) {
        return None;
    }
    Some(req)
}

/// Apply an approve or deny entry to the folded approval state, checking the payload hash.
fn apply_decision(
    folded: &mut BTreeMap<String, Approval>,
    id: &str,
    payload_hash: &str,
    outcome: Status,
    decided_by: String,
    decided_at: u64,
    reason: Option<String>,
) {
    let Some(a) = folded.get_mut(id) else {
        return;
    };
    if a.status != Status::Pending || decided_by.is_empty() {
        return;
    }
    if a.request.payload_hash != payload_hash {
        return;
    }
    a.status = outcome;
    a.decision = Some(DecisionInfo {
        outcome,
        decided_by,
        decided_at,
        reason,
    });
}

/// Mark an approval released, if the payload hash matches.
fn apply_release(folded: &mut BTreeMap<String, Approval>, id: &str, payload_hash: &str) {
    let Some(a) = folded.get_mut(id) else {
        return;
    };
    if a.status != Status::Approved {
        return;
    }
    if a.request.payload_hash != payload_hash {
        return;
    }
    a.status = Status::Released;
}

/// Fold log entries into per-id state. First valid entry wins for requests
/// and decisions; malformed entries are ignored (see module docs).
fn fold(entries: &[Entry]) -> BTreeMap<String, Approval> {
    let mut out: BTreeMap<String, Approval> = BTreeMap::new();
    for e in entries {
        match e.kind.as_str() {
            REQUESTED_KIND => {
                if let Some(req) = parse_request(&e.payload) {
                    out.entry(req.id.clone()).or_insert(Approval {
                        request: req,
                        status: Status::Pending,
                        decision: None,
                    });
                }
            }
            APPROVED_KIND => {
                if let (Some(id), Some(hash), Some(by), Some(at)) = (
                    str_field(&e.payload, "id"),
                    str_field(&e.payload, "payload_hash"),
                    str_field(&e.payload, "decided_by"),
                    u64_field(&e.payload, "decided_at"),
                ) {
                    apply_decision(&mut out, &id, &hash, Status::Approved, by, at, None);
                }
            }
            DENIED_KIND => {
                if let (Some(id), Some(hash), Some(by), Some(reason), Some(at)) = (
                    str_field(&e.payload, "id"),
                    str_field(&e.payload, "payload_hash"),
                    str_field(&e.payload, "decided_by"),
                    str_field(&e.payload, "reason"),
                    u64_field(&e.payload, "decided_at"),
                ) {
                    apply_decision(&mut out, &id, &hash, Status::Denied, by, at, Some(reason));
                }
            }
            EXPIRED_KIND => {
                if let (Some(id), Some(hash), Some(at)) = (
                    str_field(&e.payload, "id"),
                    str_field(&e.payload, "payload_hash"),
                    u64_field(&e.payload, "expired_at"),
                ) {
                    apply_decision(
                        &mut out,
                        &id,
                        &hash,
                        Status::Expired,
                        SYSTEM_DECIDER.to_string(),
                        at,
                        None,
                    );
                }
            }
            RELEASED_KIND => {
                if let (Some(id), Some(hash)) = (
                    str_field(&e.payload, "id"),
                    str_field(&e.payload, "payload_hash"),
                ) {
                    apply_release(&mut out, &id, &hash);
                }
            }
            _ => {}
        }
    }
    out
}

/// Fold already-loaded entries into per-id lifecycle state, oldest request
/// first. Read-only view over the same defensive fold the writers use
/// (first valid entry wins, malformed entries ignored); callers must verify
/// the chain first (fail closed). Powers `ancientos-log replay-approvals`.
pub fn replay(entries: &[Entry]) -> Vec<Approval> {
    let mut out: Vec<Approval> = fold(entries).into_values().collect();
    out.sort_by(|a, b| {
        (a.request.requested_at, &a.request.id).cmp(&(b.request.requested_at, &b.request.id))
    });
    out
}

/// All approvals, oldest request first. Reads an empty/missing log as empty;
/// a tampered log is an error (fail closed).
fn all(log: &DecisionLog) -> Result<Vec<Approval>, ApprovalError> {
    Ok(replay(&log.verify_or_empty()?))
}

/// Record a proposal for a risky action. Only actions the caller-provided
/// `gate` evaluates as `NeedsHuman` are accepted: gate-allowed actions fail
/// with "no approval is needed" and gate-denied (including unknown) actions
/// fail with the deny reason. An empty gate denies every action, so a caller
/// with no policy registered gets deny-all, never allow-all. A duplicate
/// pending request for the same agent+action+payload hash returns the existing
/// request without appending. `ttl` is seconds from `now`; `None` means
/// [`DEFAULT_TTL_SECS`].
#[allow(clippy::too_many_arguments)]
pub fn request(
    log: &mut DecisionLog,
    gate: &Gate,
    agent: &str,
    action: &str,
    summary: &str,
    payload: serde_json::Value,
    now: u64,
    ttl: Option<u64>,
) -> Result<ApprovalRequest, ApprovalError> {
    match gate.evaluate(&Proposal {
        agent: agent.to_string(),
        action: action.to_string(),
    }) {
        Verdict::Allow => return Err(ApprovalError::NotNeeded(action.to_string())),
        Verdict::Deny { reason } => return Err(ApprovalError::GateDenied(reason)),
        Verdict::NeedsHuman => {}
    }
    let expires_at = now.saturating_add(ttl.unwrap_or(DEFAULT_TTL_SECS));
    let req = ApprovalRequest::new(agent, action, summary, payload, now, expires_at)?;
    for a in all(log)? {
        if a.is_pending_at(now)
            && a.request.agent == req.agent
            && a.request.action == req.action
            && a.request.payload_hash == req.payload_hash
        {
            return Ok(a.request);
        }
    }
    log.append(REQUESTED_KIND, req.to_entry_payload())?;
    Ok(req)
}

/// Look up one approval by id, folding log entries (no time overlay; use
/// [`status_at`](Approval::status_at) for time-based expiry).
pub fn get(log: &DecisionLog, id: &str) -> Result<Approval, ApprovalError> {
    all(log)?
        .into_iter()
        .find(|a| a.request.id == id)
        .ok_or_else(|| ApprovalError::Unknown(id.to_string()))
}

/// Requests still awaiting a decision at `now` (undecided and unexpired),
/// oldest first.
pub fn pending(log: &DecisionLog, now: u64) -> Result<Vec<Approval>, ApprovalError> {
    Ok(all(log)?
        .into_iter()
        .filter(|a| a.is_pending_at(now))
        .collect())
}

/// Decided requests at `now` (approved, released, denied, expired including
/// unswept time-expired ones), most recently decided first.
pub fn decided(log: &DecisionLog, now: u64) -> Result<Vec<Approval>, ApprovalError> {
    let mut out: Vec<Approval> = all(log)?
        .into_iter()
        .filter(|a| !a.is_pending_at(now))
        .collect();
    out.sort_by(|a, b| {
        let at = a.decision_at(now).map(|d| d.decided_at).unwrap_or(0);
        let bt = b.decision_at(now).map(|d| d.decided_at).unwrap_or(0);
        bt.cmp(&at).then_with(|| a.request.id.cmp(&b.request.id))
    });
    Ok(out)
}

/// Approve a pending request exactly once. `expected_payload_hash` must match
/// the pending request (a stale view fails); `decided_by` names the human and
/// must be non-empty. Anything not currently Pending — unknown id, already
/// decided, expired, hash mismatch — is an error that appends nothing.
pub fn approve(
    log: &mut DecisionLog,
    id: &str,
    expected_payload_hash: &str,
    decided_by: &str,
    now: u64,
) -> Result<ReleaseToken, ApprovalError> {
    if decided_by.is_empty() {
        return Err(ApprovalError::EmptyDecider);
    }
    let a = get(log, id)?;
    match a.status_at(now) {
        Status::Pending => {}
        Status::Expired => return Err(ApprovalError::Expired(id.to_string())),
        other => {
            return Err(ApprovalError::AlreadyDecided(
                id.to_string(),
                other.as_str().to_string(),
            ))
        }
    }
    if a.request.payload_hash != expected_payload_hash {
        return Err(ApprovalError::HashMismatch(id.to_string()));
    }
    log.append(
        APPROVED_KIND,
        serde_json::json!({
            "id": id,
            "payload_hash": a.request.payload_hash,
            "decided_by": decided_by,
            "decided_at": now,
        }),
    )?;
    Ok(ReleaseToken {
        approval_id: id.to_string(),
        payload_hash: a.request.payload_hash,
    })
}

/// Deny a pending request exactly once. Same exactly-once and hash rules as
/// [`approve`]; `reason` may be empty. Errors append nothing.
pub fn deny(
    log: &mut DecisionLog,
    id: &str,
    expected_payload_hash: &str,
    decided_by: &str,
    reason: &str,
    now: u64,
) -> Result<(), ApprovalError> {
    if decided_by.is_empty() {
        return Err(ApprovalError::EmptyDecider);
    }
    let a = get(log, id)?;
    match a.status_at(now) {
        Status::Pending => {}
        Status::Expired => return Err(ApprovalError::Expired(id.to_string())),
        other => {
            return Err(ApprovalError::AlreadyDecided(
                id.to_string(),
                other.as_str().to_string(),
            ))
        }
    }
    if a.request.payload_hash != expected_payload_hash {
        return Err(ApprovalError::HashMismatch(id.to_string()));
    }
    log.append(
        DENIED_KIND,
        serde_json::json!({
            "id": id,
            "payload_hash": a.request.payload_hash,
            "decided_by": decided_by,
            "reason": reason,
            "decided_at": now,
        }),
    )?;
    Ok(())
}

/// Append one `approval.expired` entry per still-Pending request past
/// `expires_at` that has none yet; returns how many were appended. Decided
/// requests are never expired.
pub fn sweep_expired(log: &mut DecisionLog, now: u64) -> Result<usize, ApprovalError> {
    let entries = log.verify_or_empty()?;
    let swept: BTreeSet<String> = entries
        .iter()
        .filter(|e| e.kind == EXPIRED_KIND)
        .filter_map(|e| str_field(&e.payload, "id"))
        .collect();
    let folded = fold(&entries);
    let mut n = 0;
    for (id, a) in folded.iter() {
        if a.status == Status::Pending && now > a.request.expires_at && !swept.contains(id) {
            log.append(
                EXPIRED_KIND,
                serde_json::json!({
                    "id": id,
                    "payload_hash": a.request.payload_hash,
                    "expired_at": now,
                }),
            )?;
            n += 1;
        }
    }
    Ok(n)
}

/// Recover a [`ReleaseToken`] for an already-approved request. Front ends (the CLI,
/// a UI) record the human decision but drop the token from [`approve`] (it
/// is deliberately not serializable), so a later daemon tick re-derives it
/// from the log: only a request whose folded status is exactly Approved
/// yields a token, and only when `expected_payload_hash` matches the stored
/// request. Anything else — unknown id, pending, denied, expired, released,
/// hash mismatch — is an error that appends nothing. The caller still passes
/// the token to [`release`], which enforces exactly-once execution on the
/// log check. This never bypasses the human: the log must already show an
/// approval decision.
pub fn claim_release_token(
    log: &DecisionLog,
    id: &str,
    expected_payload_hash: &str,
) -> Result<ReleaseToken, ApprovalError> {
    let a = get(log, id)?;
    match a.status {
        Status::Approved => {}
        Status::Released => return Err(ApprovalError::AlreadyReleased(a.request.id)),
        other => {
            return Err(ApprovalError::NotApproved(
                a.request.id,
                other.as_str().to_string(),
            ))
        }
    }
    if a.request.payload_hash != expected_payload_hash {
        return Err(ApprovalError::HashMismatch(a.request.id));
    }
    Ok(ReleaseToken {
        approval_id: a.request.id,
        payload_hash: a.request.payload_hash,
    })
}

/// Consume a [`ReleaseToken`] and authorize one execution of the approved
/// payload. Re-checks the payload hash against the request (redacting first,
/// so callers pass the original payload) and requires the request to be
/// currently Approved: a second release for the same id fails on the log
/// check. Appends `approval.released`. Callers execute only after `Ok`.
///
/// Cross-process safe: holds the log's exclusive lock across the
/// already-released check and the release write, so two processes racing to
/// release the same approval yield exactly one success. A lock failure fails
/// closed: nothing is written and the token is consumed without effect.
pub fn release(
    log: &mut DecisionLog,
    token: ReleaseToken,
    payload: &serde_json::Value,
) -> Result<(), ApprovalError> {
    let guard = log.acquire_lock()?;
    let a = get(log, &token.approval_id)?;
    match a.status {
        Status::Approved => {}
        Status::Released => return Err(ApprovalError::AlreadyReleased(a.request.id)),
        other => {
            return Err(ApprovalError::NotApproved(
                a.request.id,
                other.as_str().to_string(),
            ))
        }
    }
    if token.payload_hash != a.request.payload_hash {
        return Err(ApprovalError::HashMismatch(a.request.id));
    }
    if canonical_hash(&redact(payload)) != a.request.payload_hash {
        return Err(ApprovalError::HashMismatch(a.request.id));
    }
    log.append_locked(
        RELEASED_KIND,
        serde_json::json!({
            "id": a.request.id,
            "payload_hash": a.request.payload_hash,
        }),
        &guard,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const AGENT: &str = "bookmarks-agent";
    const RISKY: &str = "x.post";
    const NOW: u64 = 1_700_000_000;

    /// Test helper: a gate with one low-risk and one high-risk action.
    fn test_gate() -> Gate {
        use crate::gate::Risk;
        let mut g = Gate::new();
        g.register("brief.include", Risk::Low);
        g.register(RISKY, Risk::High);
        g
    }

    /// Test helper: fresh temporary decision log with its directory and path.
    fn open_log() -> (tempfile::TempDir, DecisionLog, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decision_log.jsonl");
        let log = DecisionLog::open(&path).unwrap();
        (dir, log, path)
    }

    /// Test helper: number of entries currently in the log.
    fn len(log: &DecisionLog) -> usize {
        log.verify_or_empty().unwrap().len()
    }

    /// Test helper: synthetic high-risk approval request appended to the log.
    fn propose(log: &mut DecisionLog, now: u64, payload: serde_json::Value) -> ApprovalRequest {
        request(
            log,
            &test_gate(),
            AGENT,
            RISKY,
            "synthetic test proposal",
            payload,
            now,
            None,
        )
        .unwrap()
    }

    #[test]
    fn request_needs_human_action_appends_one_entry() {
        let (_dir, mut log, _path) = open_log();
        let r = propose(&mut log, NOW, json!({"draft": "hello synthetic world"}));
        assert_eq!(r.id.len(), 16);
        assert!(r.id.chars().all(|c| c.is_ascii_hexdigit()), "{}", r.id);
        assert_eq!(r.agent, AGENT);
        assert_eq!(r.action, RISKY);
        assert_eq!(r.requested_at, NOW);
        assert_eq!(r.expires_at, NOW + DEFAULT_TTL_SECS);
        assert_eq!(r.payload_hash, canonical_hash(&r.payload));
        assert_eq!(len(&log), 1);
        let back = get(&log, &r.id).unwrap();
        assert_eq!(back.request, r);
        assert_eq!(back.status, Status::Pending);
    }

    #[test]
    fn request_refuses_gate_allowed_action_without_appending() {
        let (_dir, mut log, _path) = open_log();
        let err = request(
            &mut log,
            &test_gate(),
            AGENT,
            "brief.include",
            "synthetic",
            json!({}),
            NOW,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no approval is needed"), "{err}");
        assert!(matches!(err, ApprovalError::NotNeeded(_)));
        assert_eq!(len(&log), 0);
    }

    #[test]
    fn request_refuses_unknown_action_with_deny_reason() {
        let (_dir, mut log, _path) = open_log();
        let err = request(
            &mut log,
            &test_gate(),
            AGENT,
            "wipe.drive",
            "synthetic",
            json!({}),
            NOW,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown action"), "{err}");
        assert!(matches!(err, ApprovalError::GateDenied(_)));
        assert_eq!(len(&log), 0);
    }

    #[test]
    fn request_dedupes_pending_same_agent_action_payload() {
        let (_dir, mut log, _path) = open_log();
        let payload = json!({"draft": "same"});
        let first = propose(&mut log, NOW, payload.clone());
        // Same key at a later time returns the existing request, no new entry.
        let dup = request(
            &mut log,
            &test_gate(),
            AGENT,
            RISKY,
            "different summary, same key",
            payload.clone(),
            NOW + 5,
            None,
        )
        .unwrap();
        assert_eq!(dup, first);
        assert_eq!(len(&log), 1);
        // A different payload is a new request.
        let other = propose(&mut log, NOW + 6, json!({"draft": "other"}));
        assert_ne!(other.id, first.id);
        assert_eq!(len(&log), 2);
    }

    #[test]
    fn request_after_decision_creates_a_new_entry() {
        let (_dir, mut log, _path) = open_log();
        let payload = json!({"draft": "again"});
        let first = propose(&mut log, NOW, payload.clone());
        approve(&mut log, &first.id, &first.payload_hash, "human", NOW + 1).unwrap();
        let second = request(
            &mut log,
            &test_gate(),
            AGENT,
            RISKY,
            "re-propose",
            payload,
            NOW + 2,
            None,
        )
        .unwrap();
        assert_ne!(second.id, first.id);
        assert_eq!(len(&log), 3); // requested, approved, requested
    }

    #[test]
    fn constructor_refuses_non_positive_ttl() {
        assert!(matches!(
            ApprovalRequest::new("a", RISKY, "s", json!({}), NOW, NOW),
            Err(ApprovalError::BadExpiry(_, _))
        ));
        assert!(matches!(
            ApprovalRequest::new("a", RISKY, "s", json!({}), NOW, NOW - 1),
            Err(ApprovalError::BadExpiry(_, _))
        ));
        let (_dir, mut log, _path) = open_log();
        assert!(matches!(
            request(
                &mut log,
                &test_gate(),
                AGENT,
                RISKY,
                "s",
                json!({}),
                NOW,
                Some(0)
            ),
            Err(ApprovalError::BadExpiry(_, _))
        ));
        assert_eq!(len(&log), 0);
    }

    #[test]
    fn id_is_content_derived_and_stable() {
        let a = ApprovalRequest::new(AGENT, RISKY, "s", json!({"b": 1, "a": 2}), NOW, NOW + 10)
            .unwrap();
        let b = ApprovalRequest::new(
            AGENT,
            RISKY,
            "other summary, same key material",
            json!({"a": 2, "b": 1}),
            NOW,
            NOW + 99,
        )
        .unwrap();
        // Key order and summary do not affect the id; requested_at does.
        assert_eq!(a.id, b.id);
        assert_eq!(a.payload_hash, b.payload_hash);
        let c = ApprovalRequest::new(
            AGENT,
            RISKY,
            "s",
            json!({"a": 2, "b": 1}),
            NOW + 1,
            NOW + 11,
        )
        .unwrap();
        assert_ne!(a.id, c.id);
    }

    #[test]
    fn approve_then_double_approve_and_deny_fail_without_appending() {
        let (_dir, mut log, _path) = open_log();
        let r = propose(&mut log, NOW, json!({"n": 1}));
        let token = approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 1).unwrap();
        assert_eq!(token.approval_id(), r.id);
        assert_eq!(token.payload_hash(), r.payload_hash);
        assert_eq!(len(&log), 2);
        let again = approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 2).unwrap_err();
        assert!(
            matches!(again, ApprovalError::AlreadyDecided(_, _)),
            "{again}"
        );
        let denied = deny(&mut log, &r.id, &r.payload_hash, "human", "late", NOW + 2).unwrap_err();
        assert!(
            matches!(denied, ApprovalError::AlreadyDecided(_, _)),
            "{denied}"
        );
        assert_eq!(len(&log), 2);
        assert_eq!(get(&log, &r.id).unwrap().status, Status::Approved);
    }

    #[test]
    fn deny_then_approve_fails() {
        let (_dir, mut log, _path) = open_log();
        let r = propose(&mut log, NOW, json!({"n": 2}));
        deny(
            &mut log,
            &r.id,
            &r.payload_hash,
            "human",
            "not today",
            NOW + 1,
        )
        .unwrap();
        let back = get(&log, &r.id).unwrap();
        assert_eq!(back.status, Status::Denied);
        assert_eq!(back.decision.unwrap().reason.as_deref(), Some("not today"));
        let err = approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 2).unwrap_err();
        assert!(matches!(err, ApprovalError::AlreadyDecided(_, _)), "{err}");
        assert_eq!(len(&log), 2);
    }

    #[test]
    fn hash_mismatch_and_unknown_id_append_nothing() {
        let (_dir, mut log, _path) = open_log();
        let r = propose(&mut log, NOW, json!({"n": 3}));
        let bad = "0".repeat(64);
        let err = approve(&mut log, &r.id, &bad, "human", NOW + 1).unwrap_err();
        assert!(matches!(err, ApprovalError::HashMismatch(_)), "{err}");
        let err = deny(&mut log, &r.id, &bad, "human", "x", NOW + 1).unwrap_err();
        assert!(matches!(err, ApprovalError::HashMismatch(_)), "{err}");
        let err = approve(&mut log, "deadbeefdeadbeef", &bad, "human", NOW + 1).unwrap_err();
        assert!(matches!(err, ApprovalError::Unknown(_)), "{err}");
        let err = get(&log, "deadbeefdeadbeef").unwrap_err();
        assert!(matches!(err, ApprovalError::Unknown(_)), "{err}");
        assert_eq!(len(&log), 1);
        assert_eq!(get(&log, &r.id).unwrap().status, Status::Pending);
    }

    #[test]
    fn empty_decider_is_refused() {
        let (_dir, mut log, _path) = open_log();
        let r = propose(&mut log, NOW, json!({"n": 4}));
        assert!(matches!(
            approve(&mut log, &r.id, &r.payload_hash, "", NOW + 1).unwrap_err(),
            ApprovalError::EmptyDecider
        ));
        assert!(matches!(
            deny(&mut log, &r.id, &r.payload_hash, "", "x", NOW + 1).unwrap_err(),
            ApprovalError::EmptyDecider
        ));
        assert_eq!(len(&log), 1);
    }

    #[test]
    fn expiry_blocks_approval_and_sweep_appends_once() {
        let (_dir, mut log, _path) = open_log();
        let r = request(
            &mut log,
            &test_gate(),
            AGENT,
            RISKY,
            "short-lived",
            json!({"n": 5}),
            NOW,
            Some(10),
        )
        .unwrap();
        assert_eq!(r.expires_at, NOW + 10);
        // At exactly expires_at the request is still pending.
        assert!(get(&log, &r.id).unwrap().is_pending_at(NOW + 10));
        // Past expires_at it reads Expired and cannot be approved or denied.
        assert!(!get(&log, &r.id).unwrap().is_pending_at(NOW + 11));
        let err = approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 11).unwrap_err();
        assert!(matches!(err, ApprovalError::Expired(_)), "{err}");
        let err = deny(&mut log, &r.id, &r.payload_hash, "human", "x", NOW + 11).unwrap_err();
        assert!(matches!(err, ApprovalError::Expired(_)), "{err}");
        assert_eq!(len(&log), 1);
        assert!(pending(&log, NOW + 11).unwrap().is_empty());
        // Sweep records the expiry exactly once.
        assert_eq!(sweep_expired(&mut log, NOW + 11).unwrap(), 1);
        assert_eq!(sweep_expired(&mut log, NOW + 12).unwrap(), 0);
        assert_eq!(len(&log), 2);
        let back = get(&log, &r.id).unwrap();
        assert_eq!(back.status, Status::Expired);
        let d = back.decision.unwrap();
        assert_eq!(d.outcome, Status::Expired);
        assert_eq!(d.decided_by, SYSTEM_DECIDER);
    }

    #[test]
    fn decided_requests_never_expire() {
        let (_dir, mut log, _path) = open_log();
        let r = request(
            &mut log,
            &test_gate(),
            AGENT,
            RISKY,
            "decide fast",
            json!({"n": 6}),
            NOW,
            Some(10),
        )
        .unwrap();
        approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 1).unwrap();
        assert_eq!(sweep_expired(&mut log, NOW + 10_000).unwrap(), 0);
        assert_eq!(
            get(&log, &r.id).unwrap().status_at(NOW + 10_000),
            Status::Approved
        );
        assert_eq!(len(&log), 2);
    }

    #[test]
    fn release_once_ok_twice_fails_on_the_log_check() {
        let (_dir, mut log, _path) = open_log();
        let payload = json!({"draft": "ship it"});
        let r = propose(&mut log, NOW, payload.clone());
        let token = approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 1).unwrap();
        release(&mut log, token, &payload).unwrap();
        assert_eq!(get(&log, &r.id).unwrap().status, Status::Released);
        assert_eq!(len(&log), 3);
        // A second token for the same id (only constructible inside this
        // module: fields are private, the type is neither Clone nor Serialize)
        // still fails, on the log state rather than the type.
        let forged = ReleaseToken {
            approval_id: r.id.clone(),
            payload_hash: r.payload_hash.clone(),
        };
        let err = release(&mut log, forged, &payload).unwrap_err();
        assert!(matches!(err, ApprovalError::AlreadyReleased(_)), "{err}");
        assert_eq!(len(&log), 3);
    }

    #[test]
    fn release_rejects_wrong_payload_and_unapproved_state() {
        let (_dir, mut log, _path) = open_log();
        let r = propose(&mut log, NOW, json!({"draft": "original"}));
        let token = approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 1).unwrap();
        let err = release(&mut log, token, &json!({"draft": "swapped"})).unwrap_err();
        assert!(matches!(err, ApprovalError::HashMismatch(_)), "{err}");
        assert_eq!(len(&log), 2);
        // Forged token for a merely pending request: not approved.
        let p = propose(&mut log, NOW + 2, json!({"draft": "pending"}));
        let forged = ReleaseToken {
            approval_id: p.id.clone(),
            payload_hash: p.payload_hash.clone(),
        };
        let err = release(&mut log, forged, &p.payload).unwrap_err();
        assert!(matches!(err, ApprovalError::NotApproved(_, _)), "{err}");
        assert_eq!(len(&log), 3);
    }

    #[test]
    fn release_accepts_original_payload_with_secrets() {
        let (_dir, mut log, _path) = open_log();
        let original = json!({"draft": "hi", "access_token": "synthetic-secret-value"});
        let r = propose(&mut log, NOW, original.clone());
        let token = approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 1).unwrap();
        // The caller passes the original payload; release redacts before
        // re-checking the hash, so this succeeds without storing the secret.
        release(&mut log, token, &original).unwrap();
        assert_eq!(get(&log, &r.id).unwrap().status, Status::Released);
    }

    #[test]
    fn payloads_are_redacted_before_hashing_and_writing() {
        let (_dir, mut log, path) = open_log();
        let r = propose(
            &mut log,
            NOW,
            json!({
                "draft": "keep me",
                "access_token": "synthetic-secret-1",
                "api_key": "synthetic-secret-2",
                "nested": {"password": "synthetic-secret-3", "credentials": {"x": 1}},
                "items": [{"refreshToken": "synthetic-secret-4"}]
            }),
        );
        assert_eq!(r.payload["draft"], "keep me");
        assert_eq!(r.payload["access_token"], REDACTED);
        assert_eq!(r.payload["api_key"], REDACTED);
        assert_eq!(r.payload["nested"]["password"], REDACTED);
        assert_eq!(r.payload["nested"]["credentials"], REDACTED);
        assert_eq!(r.payload["items"][0]["refreshToken"], REDACTED);
        assert_eq!(r.payload_hash, canonical_hash(&r.payload));
        let raw = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "synthetic-secret-1",
            "synthetic-secret-2",
            "synthetic-secret-3",
            "synthetic-secret-4",
        ] {
            assert!(!raw.contains(secret), "secret leaked: {secret}");
        }
        assert!(raw.contains("keep me"));
    }

    #[test]
    fn redact_delegates_key_and_text_rules_to_redact_module() {
        // `authorization` (shared key rule) and embedded vendor patterns
        // (shared text rule) are both caught; clean strings pass through.
        let fake = format!("ghp_{}", "C".repeat(36));
        let v = redact(&json!({
            "authorization": format!("Bearer {}", "L".repeat(32)),
            "cmd": format!("use {fake} now"),
            "draft": "keep me",
        }));
        assert_eq!(v["authorization"], REDACTED);
        assert!(!v["cmd"].as_str().unwrap().contains(&fake));
        assert!(v["cmd"]
            .as_str()
            .unwrap()
            .contains("[redacted:github_token]"));
        assert_eq!(v["draft"], "keep me");
    }

    #[test]
    fn release_round_trips_payload_with_embedded_secret() {
        let (_dir, mut log, path) = open_log();
        let fake = format!("ghp_{}", "C".repeat(36));
        let original = json!({"draft": "hi", "cmd": format!("use {fake} now")});
        // The hashed payload is a fixed point under log-layer redaction, so
        // the original (with the embedded secret) still releases.
        let r = propose(&mut log, NOW, original.clone());
        let approval = approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 1).unwrap();
        release(&mut log, approval, &original).unwrap();
        assert_eq!(get(&log, &r.id).unwrap().status, Status::Released);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(&fake), "secret leaked");
    }

    #[test]
    fn tampered_log_fails_closed_without_appending() {
        let (_dir, mut log, path) = open_log();
        propose(&mut log, NOW, json!({"n": 7}));
        drop(log);
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            raw.replacen("approval.requested", "approval.REQUESTED", 1),
        )
        .unwrap();
        // Reopening itself fails on the broken chain (existing behavior).
        let err = DecisionLog::open(&path).unwrap_err();
        assert!(matches!(err, LogError::Chain(0)), "{err}");
    }

    #[test]
    fn tampered_log_errors_every_approval_read_and_write() {
        let (_dir, mut log, path) = open_log();
        let r = propose(&mut log, NOW, json!({"n": 8}));
        // Tamper behind the open handle: every fold re-verifies the file.
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            raw.replacen("\"action\":\"x.post\"", "\"action\":\"x.POST\"", 1),
        )
        .unwrap();
        assert!(matches!(
            pending(&log, NOW + 1).unwrap_err(),
            ApprovalError::Log(_)
        ));
        assert!(matches!(
            get(&log, &r.id).unwrap_err(),
            ApprovalError::Log(_)
        ));
        assert!(matches!(
            approve(&mut log, &r.id, &r.payload_hash, "human", NOW + 1).unwrap_err(),
            ApprovalError::Log(_)
        ));
        assert!(matches!(
            deny(&mut log, &r.id, &r.payload_hash, "human", "x", NOW + 1).unwrap_err(),
            ApprovalError::Log(_)
        ));
        assert!(matches!(
            sweep_expired(&mut log, NOW + 1).unwrap_err(),
            ApprovalError::Log(_)
        ));
        // Nothing was appended: the file still holds exactly one line.
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
    }

    #[test]
    fn pending_and_decided_lists_split_and_order() {
        let (_dir, mut log, _path) = open_log();
        let a = propose(&mut log, NOW, json!({"which": "a"}));
        let b = propose(&mut log, NOW + 1, json!({"which": "b"}));
        let c = request(
            &mut log,
            &test_gate(),
            AGENT,
            RISKY,
            "short",
            json!({"which": "c"}),
            NOW + 2,
            Some(10),
        )
        .unwrap();
        approve(&mut log, &a.id, &a.payload_hash, "human", NOW + 3).unwrap();
        deny(&mut log, &b.id, &b.payload_hash, "human", "no", NOW + 4).unwrap();
        let pend = pending(&log, NOW + 5).unwrap();
        assert_eq!(
            pend.iter().map(|x| &x.request.id).collect::<Vec<_>>(),
            vec![&c.id]
        );
        // Past c's expiry it joins the decided list (unswept time-expiry).
        let done = decided(&log, NOW + 100).unwrap();
        let ids: Vec<&str> = done.iter().map(|x| x.request.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        // Most recently decided first: b (denied at +4), a (approved at +3),
        // c (expired at +12).
        assert_eq!(ids, vec![c.id.as_str(), b.id.as_str(), a.id.as_str()]);
        assert_eq!(done[0].status_at(NOW + 100), Status::Expired);
    }

    #[test]
    fn claim_recovers_token_for_approved_only() {
        let (_dir, mut log, _path) = open_log();
        let approved = propose(&mut log, NOW, json!({"n": 11}));
        let pending_req = propose(&mut log, NOW + 1, json!({"n": 12}));
        let denied = propose(&mut log, NOW + 2, json!({"n": 13}));
        deny(
            &mut log,
            &denied.id,
            &denied.payload_hash,
            "human",
            "no",
            NOW + 3,
        )
        .unwrap();
        // Approve then drop the token, like the CLI does.
        approve(
            &mut log,
            &approved.id,
            &approved.payload_hash,
            "human",
            NOW + 4,
        )
        .unwrap();
        // Pending, denied, unknown and wrong-hash claims all fail closed.
        assert!(matches!(
            claim_release_token(&log, &pending_req.id, &pending_req.payload_hash).unwrap_err(),
            ApprovalError::NotApproved(_, _)
        ));
        assert!(matches!(
            claim_release_token(&log, &denied.id, &denied.payload_hash).unwrap_err(),
            ApprovalError::NotApproved(_, _)
        ));
        assert!(matches!(
            claim_release_token(&log, "deadbeefdeadbeef", &approved.payload_hash).unwrap_err(),
            ApprovalError::Unknown(_)
        ));
        assert!(matches!(
            claim_release_token(&log, &approved.id, &"0".repeat(64)).unwrap_err(),
            ApprovalError::HashMismatch(_)
        ));
        let before = len(&log);
        // The approved claim succeeds and releases exactly once.
        let token = claim_release_token(&log, &approved.id, &approved.payload_hash).unwrap();
        assert_eq!(token.approval_id(), approved.id);
        release(&mut log, token, &json!({"n": 11})).unwrap();
        assert_eq!(get(&log, &approved.id).unwrap().status, Status::Released);
        // After release the claim is refused (already released).
        assert!(matches!(
            claim_release_token(&log, &approved.id, &approved.payload_hash).unwrap_err(),
            ApprovalError::AlreadyReleased(_)
        ));
        assert_eq!(len(&log), before + 1, "claim appends nothing");
    }

    #[test]
    fn claim_refuses_expired_requests() {
        let (_dir, mut log, _path) = open_log();
        let r = request(
            &mut log,
            &test_gate(),
            AGENT,
            RISKY,
            "short-lived",
            json!({"n": 14}),
            NOW,
            Some(10),
        )
        .unwrap();
        sweep_expired(&mut log, NOW + 11).unwrap();
        assert!(matches!(
            claim_release_token(&log, &r.id, &r.payload_hash).unwrap_err(),
            ApprovalError::NotApproved(_, _)
        ));
    }

    #[test]
    fn replay_folds_loaded_entries_oldest_first() {
        let (_dir, mut log, _path) = open_log();
        let first = propose(&mut log, NOW, json!({"n": 20}));
        let second = propose(&mut log, NOW + 5, json!({"n": 21}));
        approve(&mut log, &first.id, &first.payload_hash, "human", NOW + 9).unwrap();
        let entries = log.verify().unwrap();
        let folded = replay(&entries);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].request.id, first.id);
        assert_eq!(folded[0].status, Status::Approved);
        assert_eq!(folded[1].request.id, second.id);
        assert_eq!(folded[1].status, Status::Pending);
        assert!(replay(&[]).is_empty());
    }

    #[test]
    fn request_with_empty_gate_denies_every_action() {
        for gate in [Gate::new(), Gate::default()] {
            let (_dir, mut log, _path) = open_log();
            for action in ["x.post", "brief.include", "paid.action", "anything.at.all"] {
                let err = request(
                    &mut log,
                    &gate,
                    AGENT,
                    action,
                    "synthetic",
                    json!({}),
                    NOW,
                    None,
                )
                .unwrap_err();
                assert!(
                    matches!(err, ApprovalError::GateDenied(_)),
                    "action {action:?} must deny, got {err:?}"
                );
            }
            assert_eq!(len(&log), 0, "denied requests append nothing");
        }
    }

    #[test]
    fn request_refuses_unregistered_action_with_populated_gate() {
        use crate::gate::Risk;
        let mut gate = Gate::new();
        gate.register("demo.write", Risk::High);
        let (_dir, mut log, _path) = open_log();
        for action in ["demo.write2", "demo", "DEMO.WRITE", ""] {
            let err = request(
                &mut log,
                &gate,
                AGENT,
                action,
                "synthetic",
                json!({}),
                NOW,
                None,
            )
            .unwrap_err();
            assert!(
                matches!(err, ApprovalError::GateDenied(_)),
                "action {action:?} must deny, got {err:?}"
            );
        }
        assert_eq!(len(&log), 0, "denied requests append nothing");
        // The registered action itself still round-trips.
        let r = request(
            &mut log,
            &gate,
            AGENT,
            "demo.write",
            "synthetic",
            json!({"n": 1}),
            NOW,
            None,
        )
        .unwrap();
        assert_eq!(r.action, "demo.write");
        assert_eq!(get(&log, &r.id).unwrap().status, Status::Pending);
    }
}
