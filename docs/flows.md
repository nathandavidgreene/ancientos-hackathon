# Flows

Diagrams of the main runtime paths. Each is drawn from the module docs in the
source (see the [module reference](modules.md)).

## Try-it cascade

`bookmarks-agent::try_it_cascade`: cheap rules first, then Jev, then Muse Spark. Every
fallback lands on the rules verdict, and `Cascade::decide` never fails.

```mermaid
flowchart TD
  P[bookmark] --> LC[link_context::cascade_input<br/>sanitized post text + sanitized enrichment<br/>enrichment withheld for non-public posts]
  LC --> S0{Stage 0: topic_filter<br/>muted term?}
  S0 -- yes --> NO0[try_it = no<br/>no API call]
  S0 -- no --> S1{Stage 1: rules verdict}
  S1 -- ConfidentYes --> YES1[try_it = yes<br/>no API call]
  S1 -- otherwise --> G{gates: both clients present,<br/>ANCIENTOS_ALLOW_PAID=1,<br/>DataClass::Public?}
  G -- no --> RULES[fall back to rules verdict<br/>no call, no error]
  G -- yes --> S2{Stage 2: Jev noul >= threshold<br/>default 0.3}
  S2 -- yes --> YES2[try_it = yes]
  S2 -- no --> S3[Stage 3: Muse Spark<br/>strict JSON, cap 2048 tokens]
  S3 -- reply parsed --> V[try_it = Muse verdict]
  S3 -- empty + finish_reason=length --> R[retry once at 4096<br/>muse_retries += 1]
  R -- parsed --> V
  R -- truncated again --> T[rules verdict<br/>flagged truncated]
  S2 -- error --> E[rules verdict<br/>error recorded]
  S3 -- error / HTTP 429 --> E
```

## Data class and egress

`router::data_class`: every piece of content has a class; every route has a
ceiling. A class above the ceiling is refused before any network I/O.

```mermaid
flowchart LR
  C[content] --> CL{classify}
  CL -- unknown / unclassified --> RS[Restricted]
  CL --> PU[Public]
  CL --> IN[Internal]
  CL --> CO[Confidential]
  PU & IN & CO & RS --> CE{check_egress<br/>class <= route ceiling?}
  CE -- local route --> LOC[local_llama / local_jev<br/>ceiling: all classes]
  CE -- hosted route, Public --> HOST[muse_spark / muse_wonder / jev / modal<br/>ceiling: Public only]
  CE -- hosted route, above Public --> DENY[refused, no I/O<br/>router.egress_denied logged]
  HOST --> OK[router.egress_allowed logged]
```

The older sensitivity router still sits in front: Sensitive or Unknown data goes
to the local Llama, and a local outage is never retried on Muse.

```mermaid
flowchart TD
  A[request + Sensitivity] --> B{Sensitivity}
  B -- Sensitive / Unknown --> L[local Llama]
  B -- Sanitized --> P{ANCIENTOS_ALLOW_PAID=1<br/>and key set?}
  P -- yes --> M[Muse Spark]
  P -- no --> E[fail closed / local fallback]
  L -- down --> E2[error, never retried on Muse]
```

## Approvals and the daemon

`core::approvals` plus `bookmarks-agent::daemon`. State lives only in the
decision log; the daemon proposes paid work and only runs it after a human
approves the exact payload.

```mermaid
sequenceDiagram
  participant D as daemon (bookmarks-agent run)
  participant G as gate (default_gate)
  participant Q as approvals
  participant L as decision log
  participant H as human (CLI / web-ui)
  D->>G: evaluate brief.paid / deeper.paid / wiki.edit
  G-->>D: NeedsHuman (Risk::High)
  D->>Q: request(payload) - redacted, hashed
  Q->>L: approval.requested {id, payload_hash, expires_at}
  H->>Q: approve(id, payload_hash) or deny(id, reason)
  Q->>L: approval.approved / approval.denied
  D->>Q: claim_release_token + release(token, payload)
  Q->>Q: re-redact, re-check payload hash
  Q->>L: approval.released (exactly once)
  D->>D: run the paid action (still needs ANCIENTOS_ALLOW_PAID=1)
  Note over Q,L: undecided past expires_at reads as Expired;<br/>approvals_sweep records approval.expired.<br/>Denied or expired proposals never execute.
```

The daemon's six agents, each ticking on its own interval. One agent's error
never stops the others; every tick appends a `daemon.tick` entry.

```mermaid
flowchart LR
  S[Scheduler] --> A1[brief]
  S --> A2[digest]
  S --> A3[try_followup]
  S --> A4[deeper_queue]
  S --> A5[approvals_sweep]
  S --> A6[wiki_lint]
  A1 & A4 & A6 -- paid or risky work --> AP[approvals request]
  A1 & A2 & A3 & A4 & A5 & A6 --> LOG[(decision log<br/>daemon.tick)]
```

## Decision log and redaction

`core::log` and `core::redact`. Every payload is redacted before it is hashed
and written; the chain makes any edit, drop or reorder detectable.

```mermaid
flowchart LR
  IN[payload] --> RJ[redact_json<br/>secret-named keys -> REDACTED<br/>strings scanned by redact_text]
  RJ --> H[sha256 of seq, prev, kind, payload]
  H --> W[append JSONL line]
  W --> V{verify}
  V -- recomputed chain matches --> OK[ok]
  V -- edited / dropped / reordered --> BAD[tampered: web-ui shows the bad row]
```

```mermaid
flowchart LR
  e0["seq 0<br/>prev = genesis"] --> e1["seq 1<br/>prev = hash(e0)"] --> e2["seq 2<br/>prev = hash(e1)"] --> en["..."]
```

`redact_text` matches vendor keys (AWS, GitHub, Slack, `sk-` style, Google,
Stripe), JWTs, Bearer tokens, URL passwords, PEM private keys and credential
assignments, and replaces each with `[redacted:<kind>]`. Obvious placeholders
pass through. The same redaction runs inside `sanitize`, so every hosted-call
input is redacted.

## Wiki

`bookmarks-agent::wiki`: idea cards and research bookmarks distilled into cited
concept pages.

```mermaid
flowchart TD
  SRC[idea card / research bookmark / delegate notes] --> RAW[raw/post_id.md<br/>immutable, sanitized<br/>changed source -> raw/post_id-n.md]
  RAW --> CMP{compile}
  CMP -- paid on + Public sources --> MUSE[Muse<br/>only this ingest's sanitized sources]
  CMP -- non-public + local LLM --> LLM[local LLM]
  CMP -- otherwise / bad reply --> DET[deterministic local fallback]
  MUSE & LLM & DET --> CITE{every claim cites a raw id?}
  CITE -- no --> REJ[edit rejected]
  CITE -- yes --> PG[pages/slug.md<br/>class = max of sources]
  PG --> IDX[index.md]
  PG --> WLOG[log.md<br/>rendered from wiki.* log entries]
  Q[wiki query] --> IDX --> ANS[answer with page + raw citations]
  LINT[wiki lint] --> FIND[findings] --> APR[wiki.edit approvals<br/>never auto-applied]
```
