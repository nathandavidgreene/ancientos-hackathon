//! Off-topic pre-filter for the try-it classifiers.
//!
//! The reader has moved past the OpenClaw and Hermes Agent ecosystems, so
//! posts about them are never "try it". This filter runs as stage 0 in front
//! of the rules/Jev/Muse cascade (see [`crate::try_it_cascade`]) and inside
//! the bookmarks-agent local classifier: a match answers try_it = no with no
//! model call. It only marks try_it = no; it never hides a post from briefs.
//!
//! Deterministic, local, and free (plain case-insensitive substring match,
//! no network, no paid gate needed).

/// One excluded topic: a stable id plus the case-insensitive substrings
/// that count as mentioning it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExcludedTopic {
    /// Stable id returned by [`excluded_topic`] on a match.
    pub id: &'static str,
    /// Lowercase substrings; a match on any one excludes the text.
    pub patterns: &'static [&'static str],
}

/// Topics the reader has moved past. Extend by appending a new entry; both
/// try-it classifiers pick it up with no other change.
pub const EXCLUDED_TOPICS: &[ExcludedTopic] = &[
    ExcludedTopic {
        id: "openclaw",
        patterns: &["openclaw", "open claw", "open-claw", "clawdbot", "moltbot"],
    },
    ExcludedTopic {
        // Bare "hermes" alone is NOT a pattern: it matches the fashion
        // brand and mythology, not the agent ecosystem.
        id: "hermes",
        patterns: &["hermes agent", "hermes-agent", "nousresearch/hermes"],
    },
];

/// The excluded topic mentioned by `text`, or `None`.
///
/// Case-insensitive substring match against [`EXCLUDED_TOPICS`]: "OpenClaw",
/// "open claw", "open-claw", "clawdbot"/"moltbot" return `"openclaw", and
/// "Hermes Agent"/"hermes-agent"/"nousresearch/hermes" return `"hermes"`.
/// The bare word "hermes" alone (fashion brand) and lookalikes such as
/// "clawback" never match.
pub fn excluded_topic(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    for topic in EXCLUDED_TOPICS {
        if topic.patterns.iter().any(|p| lower.contains(p)) {
            return Some(topic.id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_list_covers_both_ecosystems() {
        let ids: Vec<&str> = EXCLUDED_TOPICS.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["openclaw", "hermes"]);
        for topic in EXCLUDED_TOPICS {
            assert!(!topic.patterns.is_empty(), "topic {}", topic.id);
        }
    }

    #[test]
    fn openclaw_variants_match() {
        for text in [
            "OpenClaw 2.0 is out, try it now",
            "OPENCLAW release notes",
            "oPeNcLaW thread",
            "Have you tried Open Claw yet?",
            "open-claw setup guide",
            "Open-Claw walkthrough with steps",
            "clawdbot tips and tricks",
            "Clawdbot setup: 1) install 2) run",
            "moltbot vs the rest",
            "MOLTBOT quickstart",
            "using openclaw/clawdbot daily",
        ] {
            assert_eq!(excluded_topic(text), Some("openclaw"), "text {text:?}");
        }
    }

    #[test]
    fn hermes_agent_variants_match() {
        for text in [
            "Hermes Agent can now do it for you",
            "HERMES AGENT benchmark results",
            "hermes agent tutorial with steps",
            "hermes-agent quickstart",
            "Hermes-Agent Docker setup",
            "try nousresearch/hermes today",
            "NousResearch/Hermes 2 is out",
            "NOUSRESEARCH/HERMES weights",
        ] {
            assert_eq!(excluded_topic(text), Some("hermes"), "text {text:?}");
        }
    }

    #[test]
    fn bare_hermes_and_lookalikes_do_not_match() {
        for text in [
            "",
            "a quiet morning routine",
            "Hermes",
            "hermes",
            "HERMES",
            "my new Hermes bag",
            "Hermes bag unboxing and review",
            "Hermes, the Greek messenger god",
            "clawback provisions explained",
            "the clawback debate continues",
            "a crab claw recipe",
            "white claw seltzer review",
            "hermes delivery tracking",
        ] {
            assert_eq!(excluded_topic(text), None, "text {text:?}");
        }
    }
}
