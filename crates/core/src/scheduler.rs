//! Minimal tick scheduler: runs agents whose interval has elapsed.
use std::time::{Duration, Instant};

pub trait Agent {
    fn name(&self) -> &str;
    fn interval(&self) -> Duration;
    fn tick(&mut self) -> Result<(), String>;
}

pub struct Scheduler {
    agents: Vec<(Box<dyn Agent>, Option<Instant>)>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    pub fn new() -> Self {
        Self { agents: Vec::new() }
    }

    pub fn add(&mut self, a: Box<dyn Agent>) {
        self.agents.push((a, None));
    }

    /// Run every due agent once; returns (name, result) for each run.
    pub fn run_due(&mut self, now: Instant) -> Vec<(String, Result<(), String>)> {
        let mut out = Vec::new();
        for (a, last) in &mut self.agents {
            let due = last.is_none_or(|l| now.duration_since(l) >= a.interval());
            if due {
                *last = Some(now);
                out.push((a.name().to_string(), a.tick()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(u32);
    impl Agent for Counter {
        fn name(&self) -> &str {
            "counter"
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(60)
        }
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
}
