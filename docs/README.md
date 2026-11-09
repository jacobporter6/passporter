---
description: Passporter is a mobile-first, voice-first control surface for coding agents running in herdr. Triage, quick replies, dictation and TL;DRs from your phone.
template: splash
hero:
  title: Your agents, in your pocket.
  tagline: Passporter puts the coding agents running in herdr on your phone. See which ones need you, answer in one tap or by voice, and read a TL;DR instead of a wall of output. Nothing you type or say is lost to a dead zone.
  actions:
    - text: Quick start
      link: /start/quick-start/
      icon: right-arrow
      variant: primary
    - text: How it works
      link: /architecture/overview/
      icon: open-book
      variant: secondary
    - text: Security model
      link: /security/
      icon: approve-check
      variant: minimal
sources:
  - README.md
  - docs/design.md
---

# Passporter

> [!NOTE]
> **Status.** The bridge and the phone web app (PWA) work today. The
> Android app, with native notifications and Android Auto, is planned and
> not built yet ([design §10](design.md#10-status-and-roadmap)).

## The system at a glance

```mermaid
flowchart TB
  subgraph phone["Phone"]
    pwa["Passporter PWA<br/>Leptos · WASM<br/>outbox in IndexedDB"]
  end
  subgraph ws["Dev machine"]
    bridge["passporter-bridge<br/>typed, authenticated API<br/>audit log"]
    herdr["herdr<br/>terminal multiplexer<br/>for agents"]
    agents["Claude Code · Codex<br/>agents in herdr panes"]
    transcripts[("Session transcripts<br/>~/.claude · ~/.codex")]
    whisper["whisper-server<br/>local speech-to-text"]
    haiku["claude -p (Haiku)<br/>TL;DRs · suggestions<br/>no tools"]
  end
  push["Browser push service<br/>content-free pings"]
  pwa <-- "HTTPS / WSS<br/>via your proxy" --> bridge
  bridge -- "socket API" --> herdr
  herdr --> agents
  agents -. writes .-> transcripts
  bridge -- reads --> transcripts
  bridge --> whisper
  bridge --> haiku
  bridge -. "wake ping" .-> push
  push -.-> pwa
```

herdr stays the source of truth: it runs the agents and knows their state.
The bridge wraps it in a small API with fixed verbs, reads each agent's
transcript for its thread, and serves the PWA. The bridge listens on
loopback; the phone reaches it through an HTTPS reverse proxy you choose,
such as Tailscale Serve.

## What it does

<div class="cards">
  <a href="app/triage.md"><strong>Triage in one glance</strong><p>Blocked, Working and Done, blocked first, live over a WebSocket. A cached snapshot shows when offline.</p></a>
  <a href="app/replies.md"><strong>Reply in one tap</strong><p>Permission dialogs become chips; questions get suggested replies; a keypad answers anything else.</p></a>
  <a href="app/dictation.md"><strong>Voice first</strong><p>Record first, transcribe later, on your own GPU. A take survives tunnels and sends when signal returns.</p></a>
  <a href="app/summaries.md"><strong>TL;DR of long turns</strong><p>Outcome, changes, decisions and risks in four lines, plus a headline for every turn.</p></a>
  <a href="app/terminal-mode.md"><strong>Run what the agent asks</strong><p>Commands in an agent's message become chips; nothing runs without a confirm showing the exact text.</p></a>
  <a href="app/runbooks.md"><strong>Runbooks and new agents</strong><p>Start an agent in an allowlisted project, or run a named operation, by tap or by voice.</p></a>
</div>

## Principles

- **herdr is the source of truth.** Passporter is a client: it never runs
  agents anywhere else.
- **Remote code execution with a nice UI.** The bridge can type into shells,
  so every action is a fixed verb, checked against the agent's current
  state, and written to the audit log before it runs ([Security](security.md)).
- **Agent output is untrusted.** It is rendered as text, never as HTML, and
  summarised by a model with no tools.
- **Offline tolerant.** Every reply is stored on the phone before it is
  sent, retried with the same idempotency key, and never sent stale without
  asking.
- **One thumb.** Primary actions sit in the bottom third; blocked is the only
  loud colour.

## Where to go next

| I want to… | Read |
| --- | --- |
| Install the bridge and connect a phone | [Quick start](start/quick-start.md), then [Deploy](deploy.md) |
| Learn what each screen does | [Using the app](app/triage.md) |
| Understand the moving parts | [System overview](architecture/overview.md) and the [design](design.md) |
| Check what protects the shell | [Security controls](security.md) |
| Diagnose a deployment | [`passporter doctor`](operations/doctor.md) |
| Change the code | [Contributing](development/contributing.md) and [Testing](testing.md) |
