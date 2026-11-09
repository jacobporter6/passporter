---
description: The moving parts of Passporter and how a request flows through them, from a tap on the phone to keys in an agent's pane.
sidebar:
  order: 1
sources:
  - Cargo.toml
  - crates/bridge/src/lib.rs
  - crates/web/src/lib.rs
  - crates/protocol/src/lib.rs
---

# System overview

Passporter is a Rust workspace. One service runs on the dev machine beside
herdr; one web app runs on the phone; a shared crate of wire types keeps the
two honest. The [design](../design.md) has the reasoning; this page is the
map.

```mermaid
flowchart TB
  subgraph phone["Phone"]
    direction LR
    ui["crates/web<br/>Leptos PWA (WASM)"]
    idb[("IndexedDB<br/>snapshot · outbox · clips")]
    sw["Service worker<br/>app shell · push wake"]
  end
  subgraph bridge["passporter-bridge · crates/bridge"]
    direction LR
    server["server · auth<br/>routes · event stream"]
    reads["registry · threads<br/>pending · tldr"]
    muts["actions · commands<br/>runbooks · spawn<br/>audio · attachments"]
    pushm["push"]
  end
  subgraph deps["Beside it on the dev machine"]
    direction LR
    herdr["herdr"]
    tr[("transcripts")]
    sum["claude -p"]
    whisper["whisper-server"]
    hook["passporter-hook"]
  end
  ui --- idb
  ui <-->|"crates/protocol<br/>HTTPS + WS"| server
  server --> reads
  server --> muts
  reads --> herdr
  muts --> herdr
  reads --> tr
  reads --> sum
  muts --> whisper
  hook -->|"unix socket"| reads
  pushm -.->|"Web Push"| sw
```

| Crate | Role | Page |
| --- | --- | --- |
| `crates/bridge` | The service: wraps herdr and transcripts in a typed, authenticated API; serves the PWA | [Bridge](bridge.md) |
| `crates/web` | The phone app: Leptos compiled to WASM, offline-first | [Web app](web.md) |
| `crates/protocol` | Wire types compiled into both, so an API mismatch is a build error | [Protocol](protocol.md) |
| `crates/transcripts` | Claude Code and Codex session files as threads | [Transcripts](transcripts.md) |
| `crates/herdr` | Typed client for herdr's socket API, plus a fake for tests | [herdr integration](herdr.md) |
| `crates/summarize` | TL;DRs, headlines, status lines and suggestions through `claude -p` | [TL;DR and summaries](../app/summaries.md) |
| `crates/xtask` | Repository chores: pruning build output, scrubbing the corpus | [Transcript corpus](../corpus.md) |

## A request, end to end

Answering a permission dialog with a chip:

```mermaid
sequenceDiagram
  autonumber
  participant W as PWA
  participant O as Outbox
  participant B as Bridge
  participant L as Audit
  participant H as herdr
  participant A as Agent
  H-->>B: event: agent blocked (seq 41)
  B->>H: read screen, parse dialog
  B-->>W: stream: pending_changed
  W->>B: GET /v1/agents/:id/pending
  B-->>W: PendingInput (id screen-41-…, options)
  Note over W: you tap "Yes"
  W->>O: store action (idempotency key, expectedSeq 41, pendingId)
  O->>B: POST /v1/actions choose yes
  Note over B: replay? rate limit?<br/>fresh agent.get + screen parse
  Note over B: stale guard: seq and<br/>pendingId still match · policy
  B->>L: execute entry (synced) before anything is sent
  Note over B: parse once more:<br/>same seq, same dialog
  B->>H: send-keys "1"
  H->>A: keypress
  B->>L: result entry
  B-->>O: 200 sent
  H-->>B: event: agent working (seq 42)
  B-->>W: stream: agent_updated
```

The same shape serves every mutating request: prompts, keys, interrupts,
[terminal mode](../app/terminal-mode.md), [runbooks](../app/runbooks.md) and
[new agents](../app/new-agents.md). Each is idempotent, checked against the
agent's current state, refused from the car where it must be, and audited
before it runs.

## Where things run

- **herdr** runs the agents in panes and is the source of truth for their
  state. The bridge talks to it over its unix socket.
- **The bridge** runs as a systemd user unit, as the same user, with a
  read-only file system except its state directory.
- **The phone** reaches the bridge through an HTTPS reverse proxy: Tailscale
  Serve or Caddy on the same machine (the bridge stays on loopback), or an
  edge proxy on another host that verifies the bridge's own leaf
  ([Deploy › HTTPS](../deploy.md#https)). Device pairing runs end to end, so
  a proxy can relay traffic but cannot mint sessions.
- **whisper-server** listens on loopback only; **`claude -p`** runs as a
  child process with no tools.
