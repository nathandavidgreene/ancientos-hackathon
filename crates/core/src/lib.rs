//! AncientOS core: scheduler, fail-closed approval gate, human approvals
//! queue, append-only hash-chained decision log, and secret redaction.
//!
//! Core knows no app: it has no workspace dependencies, and apps register
//! their own actions on a [`gate::Gate`] (checked by
//! `scripts/check_core_boundary.sh`).
pub mod approvals;
pub mod gate;
pub mod log;
pub mod loop_guard;
pub mod redact;
pub mod scheduler;
