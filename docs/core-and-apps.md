# Core and apps

Principle 7: core is distinct from applications. The test is "if this component
were removed, would AncientOS still be AncientOS?" If yes, it belongs in an app.

## Crate graph

Arrows point from a crate to what it depends on. Core depends on nothing in the
workspace; the router depends only on core; everything app-specific sits above.

```mermaid
flowchart BT
  subgraph CORE[core: AncientOS itself]
    core[ancientos-core<br/>scheduler, gate, approvals,<br/>decision log, redact]
    router[ancientos-router<br/>privacy router, data classes,<br/>local Llama / Muse / Jev / Modal clients]
  end
  subgraph APPS[apps]
    agent[bookmarks-agent<br/>first app: X bookmarks curator + CLI]
    web[web-ui<br/>loopback view]
    muse[muse-runs<br/>second app: gated Muse Code runs + CLI]
    wonder[wonder<br/>third app: gated Muse Wonder questions + CLI]
  end
  subgraph TOOLS[tools]
    demo[demo-runner<br/>offline demo]
    eval[ancientos-eval<br/>offline scoring]
  end
  router --> core
  agent --> core
  agent --> router
  web --> core
  web --> agent
  web --> eval
  muse --> core
  wonder --> core
  wonder --> router
  demo --> core
  demo --> router
  demo --> agent
  demo --> web
  demo --> muse
  eval --> router
  eval --> agent
```

## Apps on one core

`bookmarks-agent` and `muse-runs` share the same gate, approvals and decision
log without either depending on the other. `muse-runs` depends on
`ancientos-core` only and registers one action, `muse.run`, in its own gate
(anything else is denied). Its pending runs show on web-ui's `/approvals`
page with no web-ui code specific to it.

`wonder` is the third app: it depends on `ancientos-core` and
`ancientos-router` only, registers one action, `wonder.ask`, and asks Muse
Wonder (the router's creative route, sanitized text only) one approved
question exactly once, behind `ANCIENTOS_ALLOW_PAID=1` and `WONDER_MODEL`.
Its questions show on `/approvals` and the phone page `/m` with no web-ui
code specific to it.

```mermaid
sequenceDiagram
  participant Person
  participant App as muse-runs
  participant Appr as core::approvals
  participant Log as core::log
  participant Script as scripts/muse_run.sh
  App->>Appr: propose muse.run (app_gate, prompt sha256 in payload)
  Appr->>Log: approval.requested
  Person->>Appr: approve or deny (CLI or web-ui /approvals)
  Appr->>Log: approval.approved / approval.denied
  App->>App: run: approved? ANCIENTOS_ALLOW_PAID=1? prompt unchanged?
  App->>Log: approval.released (once)
  App->>Script: log-no, summary, prompt file
  App->>Log: muse_runs.finished (exit code)
```

## What stays in core, what moved out

| In core | Why it is core |
|---|---|
| `gate` | Generic, fail-closed: empty gate denies all, unknown actions deny. Apps register their own actions (`bookmarks_agent::app_gate`). |
| `approvals` | Human approval queue; the caller passes its gate. Bound to a payload hash, expires, releases once. |
| `log` | Append-only, hash-chained decision log every component writes to. |
| `redact` | Secret redaction before anything is logged or sent to a hosted model. |
| `scheduler` | Runs agents on a schedule. |
| `router::*` | Decides where data may go and talks to models; knows data classes, not bookmarks. |

Moved into `bookmarks-agent` (app specifics): `feedback`, `try_it_cascade`,
`topic_filter`, the bookmark category table, and the paid action constants.

## How the boundary is enforced

`scripts/check_core_boundary.sh` runs first in `scripts/ci_local.sh` and in demo
shot 14. It fails when:

- `ancientos-core` has any workspace dependency;
- `ancientos-router` depends on anything but `ancientos-core`;
- core or router source names app, transport or UI specifics (Discord, web UI,
  bookmarks-agent identifiers, bookmark categories, app action names).

## Runtime data flow across the boundary

```mermaid
sequenceDiagram
  participant App as bookmarks-agent
  participant Gate as core::gate
  participant Appr as core::approvals
  participant Router as router
  participant Log as core::log
  App->>Gate: propose action (app_gate)
  Gate-->>App: Allow / NeedsHuman / Deny
  App->>Appr: request (NeedsHuman)
  Appr->>Log: approval.requested
  Note over Appr: a person approves or denies
  Appr->>Log: approval.approved, approval.released (once)
  App->>Router: route(text, data class)
  Router->>Log: router.egress_allowed or router.egress_denied
  Router-->>App: local Llama reply, or Muse reply (sanitized + paid switch only)
```
