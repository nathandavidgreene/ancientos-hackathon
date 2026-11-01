//! Minimal tick scheduler: runs agents whose interval has elapsed.
//!
//! An agent's `last_run` is stamped only when its tick succeeds; a failed
//! tick is retried after a short backoff ([`FAILURE_BACKOFF`], capped at
//! the agent's own interval) instead of waiting a full interval. Intervals
//! are clamped to at least [`MIN_INTERVAL`] so a zero interval cannot spin.
use std::time::{Duration, Instant};

/// Smallest interval the scheduler honours (also the smallest backoff).
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Wait before retrying an agent whose last tick failed (capped at its
/// interval, floored at [`MIN_INTERVAL`]).
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(60);

/// Clamp an interval to at least [`MIN_INTERVAL`].
pub fn clamp_interval(d: Duration) -> Duration {
    d.max(MIN_INTERVAL)
}

/// Backoff after a failed tick for an agent with `interval`.
pub fn failure_backoff(interval: Duration) -> Duration {
    FAILURE_BACKOFF.min(clamp_interval(interval))
}

/// Per-agent run state.
#[derive(Debug, Clone, Copy, Default)]
struct RunState {
    /// When the agent last ticked successfully.
    last_ok: Option<Instant>,
    /// Earliest retry after a failed tick (cleared on success).
    retry_at: Option<Instant>,
}

impl RunState {
    /// Whether an agent with `interval` is due at `now`.
    fn due(&self, now: Instant, interval: Duration) -> bool {
        if let Some(r) = self.retry_at {
            return now >= r;
        }
        self.last_ok
            .is_none_or(|l| now.saturating_duration_since(l) >= clamp_interval(interval))
    }
}

pub trait Agent {
    /// Agent name, used in logs.
    fn name(&self) -> &str;
    /// How often the scheduler runs this agent.
    fn interval(&self) -> Duration;
    /// Do one unit of work.
    fn tick(&mut self) -> Result<(), String>;
}

pub struct Scheduler {
    agents: Vec<(Box<dyn Agent>, RunState)>,
}

impl Default for Scheduler {
    /// Empty scheduler.
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    /// Empty scheduler.
    pub fn new() -> Self {
        Self { agents: Vec::new() }
    }

    /// Add an agent.
    pub fn add(&mut self, a: Box<dyn Agent>) {
        self.agents.push((a, RunState::default()));
    }

    /// Run every due agent once; returns (name, result) for each run.
    /// Success stamps `last_run`; failure leaves it and schedules a retry
    /// after [`failure_backoff`].
    pub fn run_due(&mut self, now: Instant) -> Vec<(String, Result<(), String>)> {
        let mut out = Vec::new();
        for (a, state) in &mut self.agents {
            let interval = a.interval();
            if !state.due(now, interval) {
                continue;
            }
            let result = a.tick();
            match &result {
                Ok(()) => {
                    state.last_ok = Some(now);
                    state.retry_at = None;
                }
                Err(_) => state.retry_at = Some(now + failure_backoff(interval)),
            }
            out.push((a.name().to_string(), result));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(u32);
    impl Agent for Counter {
        /// Test helper: fixed name identifying the counting test agent.
        fn name(&self) -> &str {
            "counter"
        }
        /// Test helper: one-minute tick cadence for the test agent.
        fn interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        /// Test helper: increments the test counter and reports success.
        fn tick(&mut self) -> Result<(), String> {
            self.0 += 1;
            Ok(())
        }
    }

    #[test]
    fn runs_only_when_due() {
        let mut s = Scheduler::new();
        s.add(Box::new(Counter(0)));
        let t = Instant::now();
        assert_eq!(s.run_due(t).len(), 1);
        assert_eq!(s.run_due(t + Duration::from_secs(10)).len(), 0);
        assert_eq!(s.run_due(t + Duration::from_secs(61)).len(), 1);
    }

    /// Test helper: agent that fails while `fail` is set.
    struct Flaky {
        interval: Duration,
        fail: std::rc::Rc<std::cell::Cell<bool>>,
    }
    impl Agent for Flaky {
        /// Test helper: fixed name.
        fn name(&self) -> &str {
            "flaky"
        }
        /// Test helper: configured interval.
        fn interval(&self) -> Duration {
            self.interval
        }
        /// Test helper: fails while the flag is set.
        fn tick(&mut self) -> Result<(), String> {
            if self.fail.get() {
                Err("synthetic failure".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn failure_does_not_stamp_last_run_and_retries_after_backoff() {
        let fail = std::rc::Rc::new(std::cell::Cell::new(true));
        let mut s = Scheduler::new();
        s.add(Box::new(Flaky {
            interval: Duration::from_secs(3600),
            fail: fail.clone(),
        }));
        let t = Instant::now();
        assert!(s.run_due(t)[0].1.is_err());
        // Inside the backoff: not retried.
        assert!(s
            .run_due(t + FAILURE_BACKOFF - Duration::from_secs(1))
            .is_empty());
        // After the short backoff (not a full hour): retried.
        fail.set(false);
        assert!(s.run_due(t + FAILURE_BACKOFF)[0].1.is_ok());
        // Success stamps last_run: next run waits the full interval.
        let ok_at = t + FAILURE_BACKOFF;
        assert!(s.run_due(ok_at + Duration::from_secs(3599)).is_empty());
        assert_eq!(s.run_due(ok_at + Duration::from_secs(3600)).len(), 1);
    }

    #[test]
    fn backoff_is_capped_at_short_intervals() {
        assert_eq!(
            failure_backoff(Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(failure_backoff(Duration::ZERO), MIN_INTERVAL);
        assert_eq!(failure_backoff(Duration::from_secs(86400)), FAILURE_BACKOFF);
    }

    #[test]
    fn zero_interval_is_clamped_to_one_second() {
        let fail = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut s = Scheduler::new();
        s.add(Box::new(Flaky {
            interval: Duration::ZERO,
            fail,
        }));
        let t = Instant::now();
        assert_eq!(s.run_due(t).len(), 1);
        assert!(s.run_due(t).is_empty(), "zero interval must not spin");
        assert!(s.run_due(t + Duration::from_millis(999)).is_empty());
        assert_eq!(s.run_due(t + MIN_INTERVAL).len(), 1);
    }
}
