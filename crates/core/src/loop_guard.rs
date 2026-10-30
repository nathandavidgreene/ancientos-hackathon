//! Loop guard: hard budgets for any repeating agent loop (steps, wall time,
//! repeated identical calls, paid calls). It fails closed: once a budget is
//! spent the guard stays tripped and every later step is refused until a
//! human approves a resume through the approvals queue.
//!
//! Flow, all on the shared hash-chained decision log:
//! 1. The app calls [`LoopGuard::step`] before each iteration (with a key
//!    naming the call and whether it is paid).
//! 2. On a trip, [`propose_resume`] appends `loop_guard.tripped` and a
//!    [`RESUME_ACTION`] approval request through the app's own gate.
//! 3. After a human approves, [`LoopGuard::resume_if_approved`] resets the
//!    counters once; a denied, pending or expired request keeps it tripped.
//!
//! Core knows no app: the caller passes its gate and agent name.
use crate::approvals::{self, ApprovalError, ApprovalRequest, ReleaseToken, Status};
use crate::gate::Gate;
use crate::log::DecisionLog;
use std::collections::HashMap;

/// Action an app registers (as high risk) to let a human resume a loop.
pub const RESUME_ACTION: &str = "loop_guard.resume";
/// Log kind appended when a guard trips.
pub const TRIPPED_KIND: &str = "loop_guard.tripped";

/// Per-run limits. A zero limit means the first such step trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// Maximum steps of any kind.
    pub max_steps: u32,
    /// Maximum seconds since the guard started (or last resumed).
    pub max_wall_secs: u64,
    /// Maximum times the same call key may repeat.
    pub max_repeats: u32,
    /// Maximum paid steps.
    pub max_paid: u32,
}

impl Default for Budgets {
    /// Conservative defaults: 200 steps, 30 minutes, 3 repeats, 20 paid calls.
    fn default() -> Self {
        Self {
            max_steps: 200,
            max_wall_secs: 1800,
            max_repeats: 3,
            max_paid: 20,
        }
    }
}

/// Which budget ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripKind {
    Steps,
    WallTime,
    Repeat,
    Paid,
}

impl TripKind {
    /// Stable lowercase name for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steps => "steps",
            Self::WallTime => "wall_time",
            Self::Repeat => "repeat",
            Self::Paid => "paid",
        }
    }
}

/// Why a step was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trip {
    pub kind: TripKind,
    pub reason: String,
}

impl std::fmt::Display for Trip {
    /// One-line form: `loop guard tripped (kind): reason`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "loop guard tripped ({}): {}",
            self.kind.as_str(),
            self.reason
        )
    }
}

/// Budget tracker for one named loop.
#[derive(Debug, Clone)]
pub struct LoopGuard {
    name: String,
    budgets: Budgets,
    started_at: u64,
    steps: u32,
    paid: u32,
    repeats: HashMap<String, u32>,
    tripped: Option<Trip>,
}

impl LoopGuard {
    /// New guard for loop `name`, started at `now` (unix seconds).
    pub fn new(name: &str, budgets: Budgets, now: u64) -> Self {
        Self {
            name: name.to_string(),
            budgets,
            started_at: now,
            steps: 0,
            paid: 0,
            repeats: HashMap::new(),
            tripped: None,
        }
    }

    /// The loop's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current trip, if any.
    pub fn tripped(&self) -> Option<&Trip> {
        self.tripped.as_ref()
    }

    /// Steps and paid steps counted so far.
    pub fn counts(&self) -> (u32, u32) {
        (self.steps, self.paid)
    }

    /// Account for one step; `Err` refuses it. Once tripped, every later
    /// step is refused with the same trip (fail closed).
    pub fn step(&mut self, key: &str, paid: bool, now: u64) -> Result<(), Trip> {
        if let Some(t) = &self.tripped {
            return Err(t.clone());
        }
        let b = self.budgets;
        let trip = if now.saturating_sub(self.started_at) > b.max_wall_secs {
            Some((TripKind::WallTime, format!("over {}s", b.max_wall_secs)))
        } else if self.steps >= b.max_steps {
            Some((TripKind::Steps, format!("over {} steps", b.max_steps)))
        } else if paid && self.paid >= b.max_paid {
            Some((TripKind::Paid, format!("over {} paid calls", b.max_paid)))
        } else if self.repeats.get(key).copied().unwrap_or(0) >= b.max_repeats {
            Some((
                TripKind::Repeat,
                format!("call repeated over {} times", b.max_repeats),
            ))
        } else {
            None
        };
        if let Some((kind, reason)) = trip {
            let t = Trip { kind, reason };
            self.tripped = Some(t.clone());
            return Err(t);
        }
        self.steps += 1;
        if paid {
            self.paid += 1;
        }
        *self.repeats.entry(key.to_string()).or_insert(0) += 1;
        Ok(())
    }

    /// Payload bound to a resume approval: loop name, trip and counts.
    pub fn resume_payload(&self) -> serde_json::Value {
        let (kind, reason) = match &self.tripped {
            Some(t) => (t.kind.as_str(), t.reason.as_str()),
            None => ("none", ""),
        };
        serde_json::json!({
            "loop": self.name, "kind": kind, "reason": reason,
            "steps": self.steps, "paid": self.paid,
        })
    }

    /// Reset counters, consuming the resume approval's [`ReleaseToken`] by
    /// value: without a token the loop cannot resume. The token's approval
    /// must be a [`RESUME_ACTION`] request for this loop and currently
    /// Approved; otherwise returns `Ok(false)` and stays tripped (the token
    /// is dropped, the approval is untouched and stays claimable). A token
    /// whose hash no longer matches its request, or a request lost to a
    /// tampered log, is an error. The approval is released, so one token
    /// resumes once.
    pub fn resume_if_approved(
        &mut self,
        log: &mut DecisionLog,
        token: ReleaseToken,
        now: u64,
    ) -> Result<bool, ApprovalError> {
        let a = approvals::get(log, token.approval_id())?;
        if token.payload_hash() != a.request.payload_hash {
            return Err(ApprovalError::HashMismatch(a.request.id));
        }
        let ours = a.request.action == RESUME_ACTION
            && a.request.payload.get("loop").and_then(|v| v.as_str()) == Some(self.name.as_str());
        if !ours || a.status_at(now) != Status::Approved {
            return Ok(false);
        }
        approvals::release(log, token, &a.request.payload)?;
        self.started_at = now;
        self.steps = 0;
        self.paid = 0;
        self.repeats.clear();
        self.tripped = None;
        Ok(true)
    }
}

/// Record a trip and propose a resume through the caller's gate (which must
/// register [`RESUME_ACTION`] as high risk, or the request is denied).
/// A guard that has not tripped proposes nothing and returns `Ok(None)`.
pub fn propose_resume(
    log: &mut DecisionLog,
    gate: &Gate,
    agent: &str,
    guard: &LoopGuard,
    now: u64,
) -> Result<Option<ApprovalRequest>, ApprovalError> {
    let Some(t) = guard.tripped() else {
        return Ok(None);
    };
    let payload = guard.resume_payload();
    log.append(TRIPPED_KIND, payload.clone())?;
    let summary = format!("Resume loop '{}' ({})", guard.name(), t);
    approvals::request(
        log,
        gate,
        agent,
        RESUME_ACTION,
        &summary,
        payload,
        now,
        None,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::Risk;

    /// Test helper: small budgets.
    fn small() -> Budgets {
        Budgets {
            max_steps: 5,
            max_wall_secs: 100,
            max_repeats: 2,
            max_paid: 2,
        }
    }

    /// Test helper: gate with the resume action registered.
    fn gate() -> Gate {
        let mut g = Gate::new();
        g.register(RESUME_ACTION, Risk::High);
        g
    }

    /// Test helper: claim the release token for an approved resume request.
    fn claim(log: &DecisionLog, req: &ApprovalRequest) -> ReleaseToken {
        approvals::claim_release_token(log, &req.id, &req.payload_hash).unwrap()
    }

    #[test]
    fn step_budget_trips_and_stays_tripped() {
        let mut g = LoopGuard::new("t", small(), 0);
        for i in 0..5 {
            g.step(&format!("k{i}"), false, 1).unwrap();
        }
        let t = g.step("k9", false, 1).unwrap_err();
        assert_eq!(t.kind, TripKind::Steps);
        assert_eq!(g.step("fresh", false, 1).unwrap_err(), t);
    }

    #[test]
    fn repeat_paid_and_wall_time_trip() {
        let mut g = LoopGuard::new("t", small(), 0);
        g.step("same", false, 0).unwrap();
        g.step("same", false, 0).unwrap();
        assert_eq!(g.step("same", false, 0).unwrap_err().kind, TripKind::Repeat);
        let mut g = LoopGuard::new("t", small(), 0);
        g.step("a", true, 0).unwrap();
        g.step("b", true, 0).unwrap();
        g.step("c", false, 0).unwrap();
        assert_eq!(g.step("d", true, 0).unwrap_err().kind, TripKind::Paid);
        let mut g = LoopGuard::new("t", small(), 0);
        assert_eq!(
            g.step("a", false, 101).unwrap_err().kind,
            TripKind::WallTime
        );
    }

    #[test]
    fn zero_budget_refuses_first_step() {
        let b = Budgets {
            max_steps: 0,
            ..small()
        };
        assert!(LoopGuard::new("t", b, 0).step("a", false, 0).is_err());
    }

    /// Resume consumes a claimed token and works exactly once.
    #[test]
    fn resume_needs_human_approval_and_works_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = DecisionLog::open(dir.path().join("l.jsonl")).unwrap();
        let mut g = LoopGuard::new("cascade", small(), 0);
        assert!(propose_resume(&mut log, &gate(), "a", &g, 0)
            .unwrap()
            .is_none());
        for _ in 0..2 {
            g.step("x", true, 0).unwrap();
        }
        g.step("y", true, 0).unwrap_err();
        let req = propose_resume(&mut log, &gate(), "a", &g, 10)
            .unwrap()
            .unwrap();
        assert_eq!(req.action, RESUME_ACTION);
        // Pending: no token exists, so there is nothing to resume with.
        assert!(approvals::claim_release_token(&log, &req.id, &req.payload_hash).is_err());
        assert!(g.step("z", false, 11).is_err());
        approvals::approve(&mut log, &req.id, &req.payload_hash, "human", 12).unwrap();
        let token = claim(&log, &req);
        assert!(g.resume_if_approved(&mut log, token, 13).unwrap());
        g.step("z", true, 13).unwrap();
        // Released: no second token, so a still-tripped guard cannot resume.
        assert!(approvals::claim_release_token(&log, &req.id, &req.payload_hash).is_err());
        let mut g2 = LoopGuard::new("cascade", small(), 0);
        assert!(g2.step("q", false, 500).is_err());
        let kinds: Vec<String> = log.verify().unwrap().into_iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&TRIPPED_KIND.to_string()));
        assert_eq!(
            kinds
                .iter()
                .filter(|k| k.as_str() == "approval.released")
                .count(),
            1
        );
    }

    /// Denied resumes yield no token; a foreign loop's token is refused.
    #[test]
    fn denied_or_foreign_resume_keeps_tripped() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = DecisionLog::open(dir.path().join("l.jsonl")).unwrap();
        let mut g = LoopGuard::new("daemon", small(), 0);
        g.step("a", false, 200).unwrap_err();
        let req = propose_resume(&mut log, &gate(), "a", &g, 200)
            .unwrap()
            .unwrap();
        approvals::deny(&mut log, &req.id, &req.payload_hash, "human", "no", 201).unwrap();
        // Denied: no token exists and the guard stays tripped.
        assert!(approvals::claim_release_token(&log, &req.id, &req.payload_hash).is_err());
        assert!(g.step("b", false, 202).is_err());
        // A token approved for another loop is refused, and the approval is
        // kept: the right loop can still claim and resume afterwards.
        let req2 = propose_resume(&mut log, &gate(), "a", &g, 203)
            .unwrap()
            .unwrap();
        approvals::approve(&mut log, &req2.id, &req2.payload_hash, "human", 204).unwrap();
        let mut other = LoopGuard::new("other", small(), 0);
        other.step("a", false, 200).unwrap_err();
        let foreign = claim(&log, &req2);
        assert!(!other.resume_if_approved(&mut log, foreign, 205).unwrap());
        assert!(other.step("b", false, 205).is_err());
        let token = claim(&log, &req2);
        assert!(g.resume_if_approved(&mut log, token, 206).unwrap());
        g.step("c", false, 206).unwrap();
    }

    /// `resume_if_approved` takes the release token by value (compile-time).
    #[test]
    fn resume_takes_release_token_by_value() {
        // Compiles only if `resume_if_approved` takes `ReleaseToken` by value.
        let _sig: fn(
            &mut LoopGuard,
            &mut DecisionLog,
            ReleaseToken,
            u64,
        ) -> Result<bool, ApprovalError> = LoopGuard::resume_if_approved;
    }

    #[test]
    fn gate_without_resume_action_denies_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = DecisionLog::open(dir.path().join("l.jsonl")).unwrap();
        let mut g = LoopGuard::new("t", small(), 0);
        g.step("a", false, 200).unwrap_err();
        assert!(propose_resume(&mut log, &Gate::new(), "a", &g, 200).is_err());
    }
}
