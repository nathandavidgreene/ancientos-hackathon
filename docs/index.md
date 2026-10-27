# AncientOS Hackathon Docs

AncientOS turns your X bookmarks into a short daily brief of 3-5 picks. Agents
**propose**, a human **approves** anything risky, and every decision lands in an
append-only, hash-chained **decision log**. A privacy router decides where data
may go: private text stays on a local Llama; only sanitized, Public-class text may
reach hosted Meta (Muse) models, and only when the paid switch
`ANCIENTOS_ALLOW_PAID=1` is on.

Built for the Meta hackathon (Indie, Oct 26 - Nov 4 2026). Everything on this
site except the hand-written overview pages is generated from the doc comments
in the source, so it cannot drift from the code.

## Core and apps

A small **core** (scheduler, approval gate and queue, decision log, redaction)
and a **router** (privacy routing and model clients) carry no app knowledge. The
**bookmarks agent** is the first app on top; the web UI, demo runner and eval
harness are views and tools around it. See [Core and apps](core-and-apps.md).

| Crate | Layer | What it does | Reference |
|---|---|---|---|
| `ancientos-core` | core | Scheduler, fail-closed gate, approvals queue, append-only decision log, secret redaction | [page](reference/ancientos-core.md) |
| `ancientos-router` | core | Privacy router, data classes and egress ceilings, model clients (local Llama, Muse, Jev, Modal) | [page](reference/ancientos-router.md) |
| `bookmarks-agent` | app | X sign-in and read-only sync, brief, votes and feedback, ranker, try-it cascade, deeper, wiki, daemon, CLI | [page](reference/bookmarks-agent.md) |
| `web-ui` | app (view) | Loopback-only, server-rendered UI over the agent's data | [page](reference/web-ui.md) |
| `demo-runner` | tool | Deterministic offline demo on synthetic fixtures | [page](reference/demo-runner.md) |
| `ancientos-eval` | tool | Offline scoring of classifiers, the try-it cascade and brief ranking | [page](reference/ancientos-eval.md) |

## Engineering principles (from AGENTS.md)

- **P2 Fail closed.** Uncertain input means no mutation and no claimed success.
- **P3 Exact intent binding.** Each approval is bound to a payload hash, expires, and releases once.
- **P6 Transport neutrality.** Core imports nothing Discord- or web-specific.
- **P7 Core vs app boundaries.** Enforced by `scripts/check_core_boundary.sh` in CI.
- **P8 Build only until the next real thing.**
- **P9 Subtractive design.** Delete, simplify, optimize, automate, in that order.

## Quick start

```sh
cp .env.example .env      # fill in locally; never commit
scripts/ci_local.sh       # boundary, doc coverage, fmt, clippy, tests
scripts/demo.sh           # offline 14-shot demo
cargo run -p web-ui -- --demo   # then open http://127.0.0.1:8787/
tools/build_docs.sh       # this site -> site/index.html
```

## Where to read next

- [Core and apps](core-and-apps.md): crate graph and the boundary rules.
- [Architecture](architecture.md): data flow, module table, guarantees and the tests that prove them.
- [Flows](flows.md): try-it cascade, data-class egress, approvals and daemon, decision log, wiki.
- [Crates](modules.md): one generated page per crate, every module and public item.
- [CLI reference](reference/cli.md) and the [API reference](api.md) (rustdoc and Python tools).
- [Doc comments](docstrings.md): the doc rule, the CI check and how this site is built.
