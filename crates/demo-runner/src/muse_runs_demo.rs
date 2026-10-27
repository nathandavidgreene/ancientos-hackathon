//! Shot 13 add-on: the second app on the core. Two synthetic Muse Code runs
//! are proposed through muse-runs' own gate, one is approved and one denied,
//! and both `run` attempts are refused: the denied one by the log, the
//! approved one by the paid switch (unset in the demo). The runner points at
//! a stub `scripts/muse_run.sh` in the demo data dir, never the real script,
//! and the stub's call count is printed to prove it never ran. Pending runs
//! are shown on web-ui's `/approvals` page, rendered offline.
use crate::{approvals_demo_now, require_paid_off, Workspace};
use ancientos_core::log::DecisionLog;
use std::path::Path;

/// Subdirectory of the demo data dir holding the stub repo and prompts.
pub const STUB_DIR: &str = "muse-runs-demo";
/// Stub script body: records its args, never calls Muse or the network.
const STUB_SCRIPT: &str = "#!/usr/bin/env bash\n# Demo stub for scripts/muse_run.sh: records args only, never calls Muse.\nprintf '%s\\n' \"$@\" >> \"$(dirname \"$0\")/../calls.txt\"\n";
/// The run the demo approves: (log number, summary, prompt text).
pub const APPROVED_RUN: (&str, &str, &str) = (
    "D1",
    "[synthetic] add a why-badge test",
    "[synthetic] Add one test for the why badge.\n",
);
/// The run the demo denies: (log number, summary, prompt text).
pub const DENIED_RUN: (&str, &str, &str) = (
    "D2",
    "[synthetic] rewrite the approval gate",
    "[synthetic] Rewrite the approval gate from scratch.\n",
);
/// Reason recorded on the denial.
pub const DENY_REASON: &str = "out of scope (synthetic)";

/// Everything the shot prints; ids are left out because they hash the
/// temp prompt path and would break byte-identical output.
#[derive(Debug, Clone, PartialEq)]
pub struct MuseRunsDemo {
    /// Refusal for an action muse-runs never registered.
    pub unknown_action_refusal: String,
    /// `Muse Code run #N: summary` for each proposal, in order.
    pub proposed: Vec<String>,
    /// Pending count web-ui's `/approvals` page shows after proposing.
    pub page_pending_before: usize,
    /// Whether both proposals appear as `muse-runs · muse.run` cards.
    pub page_lists_both: bool,
    /// Refusal for running the denied proposal (id replaced by `#N`).
    pub denied_run_refusal: String,
    /// Refusal for running the approved proposal with the paid switch off.
    pub approved_run_refusal: String,
    /// `(#N, status)` for each run after the decisions, oldest first.
    pub statuses: Vec<(String, String)>,
    /// Pending count on `/approvals` after the decisions.
    pub page_pending_after: usize,
    /// Times the stub script ran (the demo expects 0).
    pub stub_calls: usize,
    /// Entries in the verified shared hash chain afterwards.
    pub chain_len: usize,
}

/// Write the stub repo (`scripts/muse_run.sh`, mode 0755) and both prompt
/// files under `dir`; returns the two prompt paths.
fn write_stub_repo(dir: &Path) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    use std::os::unix::fs::PermissionsExt;
    let scripts = dir.join("scripts");
    std::fs::create_dir_all(&scripts).map_err(|e| e.to_string())?;
    let stub = scripts.join("muse_run.sh");
    std::fs::write(&stub, STUB_SCRIPT).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    let p1 = dir.join("prompt-d1.md");
    let p2 = dir.join("prompt-d2.md");
    std::fs::write(&p1, APPROVED_RUN.2).map_err(|e| e.to_string())?;
    std::fs::write(&p2, DENIED_RUN.2).map_err(|e| e.to_string())?;
    Ok((p1, p2))
}

/// Pending count from a rendered `/approvals` page (`Pending (N)` heading).
pub fn pending_count(html: &str) -> Option<usize> {
    let after = html.split_once("Pending (")?.1;
    after.split_once(')')?.0.parse().ok()
}

impl Workspace {
    /// Render web-ui's `/approvals` page for this data dir.
    fn approvals_page(&self) -> Result<String, String> {
        let state = web_ui::State::open(&self.dir)?;
        let resp = web_ui::handle(&web_ui::Request::get("/approvals"), &state);
        if resp.status != 200 {
            return Err(format!("/approvals returned {}", resp.status));
        }
        Ok(resp.body)
    }

    /// Propose, approve and deny two synthetic Muse Code runs through the
    /// muse-runs app on this workspace's shared decision log, on the fixed
    /// 2030 demo clock. Paid-off only; `run` is refused for both and the
    /// stub never executes. Run once per data dir.
    pub fn muse_runs_demo(&self) -> Result<MuseRunsDemo, String> {
        require_paid_off()?;
        let dir = self.dir.join(STUB_DIR);
        let (p1, p2) = write_stub_repo(&dir)?;
        let now = approvals_demo_now() + 3600;
        let mut log = DecisionLog::open(self.log_path()).map_err(|e| e.to_string())?;
        let unknown_action_refusal = ancientos_core::approvals::request(
            &mut log,
            &muse_runs::app_gate(),
            muse_runs::AGENT,
            "muse.run.unattended",
            "[synthetic] run without asking",
            serde_json::json!({}),
            now,
            None,
        )
        .err()
        .map(|e| e.to_string())
        .ok_or("unregistered action was not refused")?;
        let s1 = muse_runs::RunSpec::from_prompt(APPROVED_RUN.0, APPROVED_RUN.1, &p1)?;
        let s2 = muse_runs::RunSpec::from_prompt(DENIED_RUN.0, DENIED_RUN.1, &p2)?;
        let r1 = muse_runs::propose(&mut log, &s1, now)?;
        let r2 = muse_runs::propose(&mut log, &s2, now + 1)?;
        let page = self.approvals_page()?;
        let page_pending_before = pending_count(&page).ok_or("no pending count")?;
        let page_lists_both = [&r1.summary, &r2.summary]
            .iter()
            .all(|s| page.contains(&web_ui::escape(s)) && page.contains("muse-runs · muse.run"));
        muse_runs::approve(&mut log, &r1.id, "nathan (demo)", now + 10)?;
        muse_runs::deny(&mut log, &r2.id, "nathan (demo)", DENY_REASON, now + 11)?;
        let runner = muse_runs::Runner {
            repo: dir.clone(),
            allow_paid: false,
        };
        let refusal = |log: &mut DecisionLog, id: &str, label: &str| -> Result<String, String> {
            match muse_runs::run(log, id, &runner) {
                Ok(code) => Err(format!(
                    "run {label} executed (exit {code}); demo must refuse"
                )),
                Err(e) => Ok(e.replace(id, &format!("#{label}"))),
            }
        };
        let denied_run_refusal = refusal(&mut log, &r2.id, DENIED_RUN.0)?;
        let approved_run_refusal = refusal(&mut log, &r1.id, APPROVED_RUN.0)?;
        let statuses = muse_runs::runs(&log, now + 12)?
            .into_iter()
            .map(|a| {
                let label = if a.request.id == r1.id {
                    APPROVED_RUN.0
                } else {
                    DENIED_RUN.0
                };
                (
                    format!("#{label}"),
                    a.status_at(now + 12).as_str().to_string(),
                )
            })
            .collect();
        let page_pending_after =
            pending_count(&self.approvals_page()?).ok_or("no pending count")?;
        let stub_calls = std::fs::read_to_string(dir.join("calls.txt"))
            .map(|s| s.lines().count())
            .unwrap_or(0);
        let chain_len = log.verify().map_err(|e| e.to_string())?.len();
        Ok(MuseRunsDemo {
            unknown_action_refusal,
            proposed: vec![r1.summary, r2.summary],
            page_pending_before,
            page_lists_both,
            denied_run_refusal,
            approved_run_refusal,
            statuses,
            page_pending_after,
            stub_calls,
            chain_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_count_reads_heading() {
        assert_eq!(pending_count("<h2>Pending (2)</h2>"), Some(2));
        assert_eq!(pending_count("<h2>Pending (x)</h2>"), None);
        assert_eq!(pending_count("nothing"), None);
    }

    #[test]
    fn muse_runs_demo_proposes_approves_denies_and_never_runs_stub() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let demo = ws.muse_runs_demo().unwrap();
        assert!(
            demo.unknown_action_refusal.contains("fail closed"),
            "{}",
            demo.unknown_action_refusal
        );
        assert_eq!(
            demo.proposed,
            vec![
                "Muse Code run #D1: [synthetic] add a why-badge test",
                "Muse Code run #D2: [synthetic] rewrite the approval gate",
            ]
        );
        assert_eq!(demo.page_pending_before, 2);
        assert!(demo.page_lists_both);
        assert!(
            demo.denied_run_refusal.contains("#D2") && demo.denied_run_refusal.contains("denied"),
            "{}",
            demo.denied_run_refusal
        );
        assert!(
            demo.approved_run_refusal
                .contains("set ANCIENTOS_ALLOW_PAID=1 to run (fail closed; approval kept)"),
            "{}",
            demo.approved_run_refusal
        );
        assert_eq!(
            demo.statuses,
            vec![
                ("#D1".to_string(), "approved".to_string()),
                ("#D2".to_string(), "denied".to_string()),
            ]
        );
        assert_eq!(demo.page_pending_after, 0);
        assert_eq!(demo.stub_calls, 0, "stub must never run in the demo");
        // requested x2, approved, denied; refusals append nothing.
        assert_eq!(demo.chain_len, 4);
        let kinds: Vec<String> = DecisionLog::open(ws.log_path())
            .unwrap()
            .verify()
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(!kinds.iter().any(|k| k == muse_runs::FINISHED_KIND));
        assert!(!kinds
            .iter()
            .any(|k| k == ancientos_core::approvals::RELEASED_KIND));
    }

    #[test]
    fn stub_script_records_args_only() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_repo(dir.path()).unwrap();
        let out = std::process::Command::new(dir.path().join("scripts/muse_run.sh"))
            .args(["D9", "x"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty(), "stub prints nothing");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("calls.txt")).unwrap(),
            "D9\nx\n"
        );
    }
}
