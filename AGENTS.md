# AGENTS.md

Guidance for coding agents (Muse Code, Codex, others) working in this repo.

## Project
AncientOS hackathon build: a Rust core (agent scheduler, approval gate, append-only
decision log, privacy router) plus one showcase agent, an X bookmarks curator that
writes a daily brief of 3-5 picks. Built new, Oct 26 - Nov 4 2026.

## Engineering principles
From the AncientOS identity and engineering principles (numbering kept from that doc).
2. **Fail closed.** If identity, intent, scope, evidence, authorization or operational
   truth is uncertain, perform no mutation and claim no success. Unknown, missing or
   ambiguous input means deny or stop. Never guess.
3. **Exact intent binding.** Bind authority to the precise operation, target, scope
   and relevant preconditions. Preserve replay protection and keep authority from
   leaking into adjacent actions. In this repo the approval queue already does this:
   each approval is bound to a payload hash, expires, and releases exactly once.
6. **Transport neutrality (light).** Discord, web, terminal and future interfaces are
   interchangeable entry points into the same runtime. Core must not import anything
   Discord- or web-UI-specific; transports are adapters and views, not business logic.
7. **Core vs app boundaries.** Core is distinct from applications. Apply the Core test:
   "If this component were removed, would AncientOS still be AncientOS?" If yes, it
   likely belongs in an application, not core.
8. **Build only until the next real thing becomes possible, then build the next real
   thing.** Make a real capability work, refine it through use, and handle proven edge
   cases without letting speculative perfection block progress. Use this to choose
   every next job.
9. **Subtractive design.** Challenge complexity and assumptions. Prefer
   delete -> simplify -> optimize -> automate, in that order. Compose or reuse existing
   capabilities before building new machinery.

## Repo policies
Rules for this repo (not principles):
- **Governance is the product.** Agents propose; a human approves risky actions.
  Every decision (routing, proposal, approval, threshold change) is appended to the
  decision log. Never rewrite or truncate the log.
- **Privacy router decides where data goes.** Sensitive data stays on local Llama.
  Only sanitized data may go to hosted Muse models. Log every routing decision.
- **Never delete bookmarks.** Bookmark access is read-only. Never infer a removal
  from a partial sync.
- **Paid calls are incremental and off by default.** Any X API or model call that
  costs money sits behind a switch that defaults to off (`ANCIENTOS_ALLOW_PAID`),
  and syncs only new items.
- **Demo only what is built.** No invented features or claimed results without a test.

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
