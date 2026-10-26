//! Fail-closed approval gate: anything not explicitly allowed is denied.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    Low,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Allow,
    NeedsHuman,
    Deny { reason: String },
}

/// A proposed action from an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub agent: String,
    pub action: String,
}

#[derive(Debug, Default)]
pub struct Gate {
    policies: HashMap<String, Risk>,
}

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, action: impl Into<String>, risk: Risk) {
        self.policies.insert(action.into(), risk);
    }

    pub fn evaluate(&self, p: &Proposal) -> Verdict {
        match self.policies.get(&p.action) {
            Some(Risk::Low) => Verdict::Allow,
            Some(Risk::High) => Verdict::NeedsHuman,
            None => Verdict::Deny {
                reason: format!("unknown action '{}': fail closed", p.action),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(action: &str) -> Proposal {
        Proposal {
            agent: "t".into(),
            action: action.into(),
        }
    }

    #[test]
    fn denies_unknown_action() {
        let g = Gate::new();
        assert!(matches!(
            g.evaluate(&p("delete_everything")),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn routes_by_registered_risk() {
        let mut g = Gate::new();
        g.register("read", Risk::Low);
        g.register("post", Risk::High);
        assert_eq!(g.evaluate(&p("read")), Verdict::Allow);
        assert_eq!(g.evaluate(&p("post")), Verdict::NeedsHuman);
    }
}
