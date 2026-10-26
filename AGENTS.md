# AGENTS.md

Guidance for coding agents (Muse Code, Codex, others) working in this repo.

## Project
AncientOS hackathon build: a Rust core (agent scheduler, approval gate, append-only
decision log, privacy router) plus one showcase agent, an X bookmarks curator that
writes a daily brief of 3-5 picks. Built new, Oct 26 - Nov 4 2026.

## Principles
1. **Fail closed.** Unknown, missing or ambiguous input means deny or stop. Never guess.
2. **Governance is the product.** Agents propose; a human approves risky actions.
   Every decision (routing, proposal, approval, threshold change) is appended to the
   decision log. Never rewrite or truncate the log.
3. **Privacy router decides where data goes.** Sensitive data stays on local Llama.
   Only sanitized data may go to hosted Muse models. Log every routing decision.
4. **Never delete bookmarks.** Bookmark access is read-only. Never infer a removal
   from a partial sync.
5. **Paid calls are incremental and off by default.** Any X API or model call that
   costs money sits behind a switch that defaults to off, and syncs only new items.
6. **Demo only what is built.** No invented features or claimed results without a test.

## Secrets and privacy
- Secrets live only in environment variables / a local `.env` (git-ignored).
  Commit `.env.example` with placeholder names only.
- Never print, log or commit keys, tokens, hostnames, IPs or personal paths.
- Fixture data must be public bookmark content only; strip internal labels
  (`source`, `model_id`) before committing.

## Job contract
Every task states, before work starts:
- **Goal** and **scope**
- **Do not touch** (paths/systems out of bounds)
- **Validation** (which tests/checks prove it works)
- **Git boundary** (what gets committed)
and ends by naming one `NEXT_REAL_ACTION`.

## Workflow
- Small slices: one verified change per commit, clear message.
- Run targeted tests, then the full local CI script (`cargo fmt --check`,
  `cargo clippy -- -D warnings`, `cargo test`; Python: strict Pyright, zero errors)
  before every commit.
- Commit and push are separate steps. Plain `git push` only: no force push,
  no stash, no reset, no history rewrites.
- Try an approach at most twice, then switch approach and say why.
- Report in short plain language; flag anything that costs money or is irreversible
  and wait for approval.
