//! X bookmarks agent, the first app on the AncientOS core: read-only X
//! sign-in and incremental sync (never deletes bookmarks), a daily brief of
//! 3-5 picks, votes and feedback, a personal ranker, the try-it cascade,
//! go-deeper walkthroughs, idea cards, the wiki, and the scheduler daemon.
//! Risky actions go through [`app_gate`] and the core approvals queue.
pub mod analyze_media;
pub mod analyze_media_baseten;
pub mod analyze_media_cli;
pub mod analyze_media_live;
pub mod analyze_media_muse;
pub mod author_filter;
pub mod brief;
pub mod callback;
pub mod classify;
pub mod daemon;
pub mod deeper;
pub mod delegate;
pub mod digest;
pub mod feedback;
pub mod fixture;
pub mod idea_card;
pub mod ingest;
pub mod labels;
pub mod language;
pub mod link_context;
pub mod liveness;
pub mod media;
pub mod merge;
pub mod oauth;
pub mod ranker;
pub mod sanitize;
pub mod semantic;
pub mod stats;
#[cfg(test)]
pub(crate) mod test_support;
pub mod thumbs;
pub mod token_store;
pub mod topic_mute;
pub mod translate;
pub mod try_it;
pub mod try_it_cascade;
pub mod unroll;
pub mod vote;
pub mod wiki;
pub mod x_api;

/// Paid go-deeper walkthrough: proposed by the daemon, executed only after
/// a human approval is claimed and released.
pub const DEEPER_PAID_ACTION: &str = "deeper.paid";

/// Canonical policy used by the approval queue: read-only agent work is
/// low risk; only actions with exactly one releasing executor are high
/// risk (`deeper.paid`, released by the daemon's paid executor, and the
/// loop-guard resume); anything unlisted is denied (fail closed). X
/// writes, `brief.paid` and `wiki.edit` are deliberately absent: nothing
/// executes them, so approving one could only mislead. Labels Delete does
/// not go through this queue (it uses Social's remover directly).
pub fn app_gate() -> ancientos_core::gate::Gate {
    use ancientos_core::gate::Risk;
    let mut g = ancientos_core::gate::Gate::new();
    g.register("brief.include", Risk::Low);
    for action in [
        DEEPER_PAID_ACTION,
        ancientos_core::loop_guard::RESUME_ACTION,
    ] {
        g.register(action, Risk::High);
    }
    g
}

/// Categories whose content stays on the device (same rule the brief demo
/// has always used; see also [`class_of_category`]).
pub const SENSITIVE_CATEGORIES: &[&str] = &["Health & Personal"];

/// Class of a bookmark category: sensitive categories are
/// [`DataClass::Confidential`](ancientos_router::data_class::DataClass), empty
/// or `"unknown"` categories are
/// [`DataClass::Restricted`](ancientos_router::data_class::DataClass)
/// (unclassified: fail closed), anything else is
/// [`DataClass::Public`](ancientos_router::data_class::DataClass).
pub fn class_of_category(category: &str) -> ancientos_router::data_class::DataClass {
    use ancientos_router::data_class::DataClass;
    let trimmed = category.trim();
    if trimmed.is_empty() || trimmed == "unknown" {
        return DataClass::Restricted;
    }
    if SENSITIVE_CATEGORIES.contains(&trimmed) {
        DataClass::Confidential
    } else {
        DataClass::Public
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ancientos_core::approvals::{get, request, Status};
    use ancientos_core::gate::{Proposal, Verdict};
    use ancientos_core::log::DecisionLog;
    use ancientos_router::data_class::DataClass;
    use serde_json::json;

    const AGENT: &str = "bookmarks-agent";
    const NOW: u64 = 1_700_000_000;

    /// Test helper: minimal proposal for the given action.
    fn p(action: &str) -> Proposal {
        Proposal {
            agent: "t".into(),
            action: action.into(),
        }
    }

    /// Test helper: a fresh decision log in a temp dir.
    fn open_log() -> (tempfile::TempDir, DecisionLog) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decision_log.jsonl");
        let log = DecisionLog::open(&path).unwrap();
        (dir, log)
    }

    #[test]
    fn default_policy_has_no_decorative_x_write_approvals() {
        let g = app_gate();
        assert_eq!(g.evaluate(&p("brief.include")), Verdict::Allow);
        // No executor performs X writes, so they are not approvable: they
        // fail closed like any unknown action.
        for action in [
            "x.post",
            "x.reply",
            "x.repost",
            "x.like",
            "x.follow",
            "x.dm.send",
        ] {
            assert!(
                matches!(g.evaluate(&p(action)), Verdict::Deny { .. }),
                "{action}"
            );
        }
        assert!(matches!(g.evaluate(&p("wipe.drive")), Verdict::Deny { .. }));
    }

    #[test]
    fn default_policy_needs_human_only_for_executed_actions() {
        let g = app_gate();
        assert_eq!(g.evaluate(&p(DEEPER_PAID_ACTION)), Verdict::NeedsHuman);
        assert_eq!(
            g.evaluate(&p(ancientos_core::loop_guard::RESUME_ACTION)),
            Verdict::NeedsHuman
        );
        // Removed proposals (no executor) and neighbours fail closed.
        for action in [
            "brief.paid",
            "wiki.edit",
            "deeper.paid2",
            "brief",
            "deeper",
            "wiki",
        ] {
            assert!(
                matches!(g.evaluate(&p(action)), Verdict::Deny { .. }),
                "{action}"
            );
        }
    }

    #[test]
    fn daemon_paid_actions_round_trip_through_request() {
        let (_dir, mut log) = open_log();
        {
            let action = DEEPER_PAID_ACTION;
            let r = request(
                &mut log,
                &app_gate(),
                AGENT,
                action,
                "synthetic daemon proposal",
                json!({"n": 15}),
                NOW,
                None,
            )
            .unwrap();
            assert_eq!(r.action, action);
            assert_eq!(get(&log, &r.id).unwrap().status, Status::Pending);
        }
    }

    #[test]
    fn category_mapping_matches_the_existing_rule() {
        // Sensitive categories stay local; unknown/blank fail closed harder.
        assert_eq!(
            class_of_category("Health & Personal"),
            DataClass::Confidential
        );
        assert_eq!(class_of_category("unknown"), DataClass::Restricted);
        assert_eq!(class_of_category(""), DataClass::Restricted);
        assert_eq!(class_of_category("   "), DataClass::Restricted);
        // Anything else is public.
        for cat in ["Rust & Systems", "Hot Takes", "AI Agents", "Unknown Topics"] {
            assert_eq!(class_of_category(cat), DataClass::Public, "cat {cat:?}");
        }
    }
}
