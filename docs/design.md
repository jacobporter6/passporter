# Passporter — design

> Work from anywhere. A mobile-first, voice-first control surface for the coding
> agents running in herdr on your dev machine.

Status: **v0.1**. This document mixes what is built (the bridge and the PWA)
with what is planned (the Android app, WebAuthn step-up); each section says
which.

---

## 1. Problem

Without Passporter, steering agents from a phone means an SSH client →
a VPN or tunnel → the herdr TUI. It works, but:

- A full terminal UI on a 6" screen is slow to navigate; finding *which* agent
  needs me is the bulk of the effort.
- Answering "plan ready — proceed?" takes a dozen taps.
- Long agent turns (big diffs, long explanations) are unreadable on a phone.
- Typing is the worst input method on a phone; voice is the best one, and
  there is no voice path at all.
- Signal drops (trains, tunnels) kill the SSH session and anything half-typed.
- None of it works in the car.

## 2. Goals

1. **Triage in one glance**: blocked / working / done agents, blocked first.
2. **Quick reply**: approve, deny, or answer an agent in one tap, including from
   the notification shade and from Android Auto.
3. **Voice first**: dictation is a primary input, not an add-on.
4. **TL;DR**: a short, structured summary of any long agent turn.
5. **Offline tolerant**: nothing I record or type is ever lost to a dead zone;
   it sends when signal returns, and never sends something stale silently.
6. **Android Auto**: hear what an agent needs, and reply by voice.
7. **Secure by construction**: this thing can type into shells on your dev
   machine, so treat it as remote code execution with a nice UI.

### Non-goals

- Replacing herdr or running agents somewhere else. herdr stays the source of
  truth; Passporter is a client.
- Multi-user / team features. One user, a few devices.
- A general SSH/terminal client. There is a raw-keys fallback (§6.3) for when
  parsing fails, not a terminal.
- A native iOS app (the PWA works in mobile Safari as far as the browser
  allows; it is not a tested target).

## 3. System overview

```
 Phone                              HTTPS reverse proxy                 Dev machine (runs herdr)
┌──────────────────────────────┐    ┌────────────────────────────┐    ┌──────────────────────────────────────────┐
│ Passporter PWA (Leptos/WASM) │HTTPS│ one of:                    │HTTP │ passporter-bridge 127.0.0.1:8443          │
│  • outbox (IndexedDB)        │+WSS │  • tailscale serve         │(loop│  ├─ herdr adapter ──► herdr socket API     │
│  • recorder                  │════►│  • Caddy + public domain   │back)│  ├─ transcript reader ──► ~/.claude/…jsonl │
│  • Web Push (wake pings) ◄───┼──┐  │  • your own edge (§3, ex.) │════►│  ├─ hook receiver ◄── Claude Code hooks    │
└──────────────────────────────┘  │  └────────────────────────────┘    │  ├─ tl;dr + suggestions (claude -p)        │
                                  │                                    │  ├─ transcription ──► whisper.cpp (local)  │
                                  └── browser push service ◄───────────┤  ├─ Web Push sender (content-free pings)   │
                                                                       │  └─ devices, audit log, state              │
                                                                       └──────────────────────────────────────────┘
```

### Network path

The bridge listens on loopback by default and never needs to be exposed to
the internet directly. Phones reach it through an HTTPS reverse proxy that
you choose (docs/deploy.md has recipes):

- **Tailscale Serve** (the quick start): `tailscale serve` terminates HTTPS
  with a `*.ts.net` certificate on the same machine and proxies to
  `127.0.0.1:8443`. Only devices on your tailnet can connect. No domain, no
  CA, no open ports.
- **Caddy with a public domain**: Caddy gets a Let's Encrypt certificate and
  proxies to the bridge on loopback. Pairing and device keys are then the
  only thing between the internet and your shells, so prefer a VPN-only
  name or an allowlist where you can.
- **Your own edge** (advanced example): a reverse proxy on another host
  (say, a small VM or container on your LAN) serves a split-horizon name
  with a public wildcard certificate and proxies over HTTPS to the bridge,
  which then serves its own leaf from a private CA (`[tls]`). The phone
  reaches the edge over a VPN such as WireGuard.

In every case:

- the bridge believes `X-Forwarded-For` only from `trusted_proxy`;
- device authentication (§8) runs end to end at the application layer, so
  a compromised proxy can relay traffic but cannot mint sessions;
- plain HTTP off loopback is refused unless you opt into the insecure MVP
  mode (docs/deploy.md), which exists for a phone that can only reach the
  machine over a WireGuard tunnel.

Three deliverables:

| Component | What | Where |
|---|---|---|
| `crates/bridge` | Rust service (axum) on the dev machine. Wraps herdr + Claude transcripts into a small, typed, authenticated API. | systemd user unit (or any supervisor); reachable only through your HTTPS proxy |
| `crates/web` | Mobile-first PWA, Rust compiled to WASM (Leptos). Works on its own in Chrome; also the UI inside the Android app. | served by the bridge |
| `apps/android` (built: shell; plugins in progress) | Tauri 2 mobile shell around `crates/web` plus Kotlin plugins: notifications with inline reply, Android Auto messaging, UnifiedPush, recording that survives app kills. | sideloaded APK |

Shared crates: `crates/protocol` (serde types compiled into both the bridge and
the WASM frontend, so an API mismatch is a build error), `crates/herdr`
(typed herdr client), `crates/transcripts` (Claude Code and Codex transcript
readers) and `crates/summarize` (the TL;DR and suggestion model calls).

## 4. Data sources on the dev machine

Passporter reads structured data wherever it exists and falls back to the
screen only as a last resort.

| Need | Primary source | Fallback |
|---|---|---|
| List of agents and their state | herdr socket API: `agent list` + event subscription. Gives `agent_status` ∈ {idle, working, blocked, done, unknown}, `state_change_seq`, `revision`, title, cwd, pane id, `agent_session` | none needed |
| What the agent said and did | Claude session transcript `~/.claude/projects/<cwd-slug>/<agent_session.value>.jsonl` (when herdr reports no session, e.g. hooks off: matched by folder, the session in `<cwd-slug>/` most recently written, within a day and since the agent appeared, kept until it goes quiet and a newer session replaces it; the thread page says `inferred`, the agent id stays the pane alone and summaries key on the matched session): messages, tool calls, edits. Codex: rollout `~/.codex/sessions/YYYY/MM/DD/rollout-<start>-<session>.jsonl` (by `agent_session` when herdr reports one, else by cwd: the earliest session started there since herdr detected the agent, or the latest when the agent predates the bridge; `codex_sessions_dir` in the config overrides the directory): `response_item` messages, function/custom tool calls and outputs, `apply_patch` diffs | `herdr agent read --source recent-unwrapped` |
| What a blocked agent is asking | Claude Code hook events (Notification / PermissionRequest), delivered to the bridge by our own hook script over a local unix socket. Gives tool name, input and message | parse `herdr agent read --source detection` (Claude and Codex approval dialogs) |
| Sending a prompt | `herdr agent prompt <target> <text>` (rejects agents that are blocked) | — |
| Answering a dialog | `herdr agent send-keys` (digits, arrows, enter, esc) | raw-keys pad in the UI |
| Code changes in a turn | transcript edit/write tool calls + `git diff --stat` in the agent's cwd | — |

Non-Claude agents (codex, opencode, pi and others herdr knows) still get list,
state, prompt, keys and screen-read support through herdr. Transcripts are
read per agent kind (`transcripts::Format`): Claude and Codex (codex-cli
0.166) have threads, and TL;DRs, headlines, status lines and reply
suggestions only when herdr reports their session id (never from a rollout
guessed by cwd). Codex approval
dialogs are screen-parsed into chips with the same risk rules as Claude's
(approve once is low; *don't ask again* / *for this session* is elevated).
Hooks are Claude-only. Under `~/.codex` the bridge reads `sessions/` and
nothing else (it holds credentials).

Hook event names and payloads were checked against Claude Code 2.1.x
(docs/hooks.md); a newer Claude Code may change them.

## 5. Bridge API (sketch)

All endpoints are authenticated (§8), except pairing and `GET /v1/version`
(the bridge's build, so a stale app can tell). The authoritative types live
in `crates/protocol`.

```
GET  /v1/agents                       → Agent[]            (state, title, repo, worktree, ages, unread)
GET  /v1/agents/:id/thread?since=     → ThreadPage         ({ epoch, items: ThreadItem[], resumeCursor }: user/assistant/tool/diff, upsert by cursor)
GET  /v1/agents/:id/pending           → PendingInput|null  (structured question/permission + options + suggestions)
GET  /v1/agents/:id/tldr?turn=        → Tldr               (cached per turn)
POST /v1/actions                      → ActionResult       (the only mutating endpoint for the app)
POST /v1/agents/:id/car-reply         → CarReply           (Android Auto's utterance, mapped to an action by the bridge; §6.6)
POST /v1/audio                        → { audioId }        (chunked/resumable upload)
GET  /v1/audio/:id/transcript         → Transcript
POST /v1/attachments                  → AttachmentReceipt  (one image; re-encoded, metadata stripped)
WS   /v1/stream                       → AgentEvent stream (state changes, new thread items, pending inputs)
```

`POST /v1/actions` body:

```ts
{
  idempotencyKey: string,        // UUID generated on device when the action is created (offline too)
  agentId: string,               // pane id + agent_session id; both must still match
  expectedSeq: number,           // agent's state_change_seq when the user made the decision
  createdAt: string,
  kind: "prompt" | "choose" | "keys" | "interrupt",
  payload: { text, attachments? } | { optionId, feedbackText? } | { keys: AllowedKey[] } | {}
  origin: "app" | "notification" | "car", // a car-scoped session is always "car" (§8)
}
```

Rules:

- **Idempotent**: replaying the same `idempotencyKey` returns the original
  result and never sends twice. This matters because herdr says a timeout does
  not prove non-delivery, and the phone *will* retry after tunnels.
- **Stale guard**: if `expectedSeq` ≠ current `state_change_seq`, or the agent
  session changed, return `409 stale` with the current state. The UI then asks
  "this agent moved on since you recorded this: send anyway / edit / discard".
  The bridge never auto-sends a stale action.
- **Fixed verbs only**: no arbitrary shell, no arbitrary pane. `keys` accepts
  an allowlist (digits, arrows, enter, esc, tab, ctrl+c).
- Every action is written to the audit log *before* it executes.

## 6. Features

### 6.1 Triage

- Three segments, **Blocked · Working · Done** (herdr `idle` and `done` both
  count as Done/ready, with an unread dot for completions not yet seen). The
  Blocked badge count also shows on the app icon.
- Each card shows the agent name or terminal title, repo and worktree, time in
  state, and one line of the latest question or result.
- Swipe left/right in the detail view to move to the next agent *in the same
  segment*. A "Next blocked" button gives an inbox-zero loop.
- Live updates over the WebSocket. A cached snapshot shows instantly when
  offline, marked as stale with "as of 14:02".

### 6.2 Quick reply

The pending-input model normalizes everything a blocked agent can ask:

```ts
PendingInput = {
  kind: "permission" | "question" | "plan_approval" | "free_text",
  prompt: string,            // e.g. "Run `pnpm test`?"  (short; used for notifications and the car)
  detail?: string,           // command / file / plan body
  options: { id, label, keys: AllowedKey[], risk: "low"|"elevated" }[],
  suggestions: string[],     // 2–3 model-generated short replies for free_text/question
  source: "hook" | "screen",
}
```

- **Permission prompts** become chips: *Allow once*, *Always allow* (marked
  elevated), *Deny…*. Deny opens dictation so the reason goes back to Claude
  through its "No, and tell Claude what to do differently" path.
- **Questions / "shall I proceed?"** at the end of a turn (agent is
  idle/done, last assistant message ends in a question) get suggestion chips
  plus the mic. A tap on a chip sends it as the reply at once, like
  dictation (a draft in the composer stays; at a dialog it goes into the
  composer instead). Suggestions are computed on the bridge **when the event
  arrives, before the push goes out**, so they are ready when the notification
  is opened.
- **Natural-language → option mapping** (used for car and notification
  replies): a strict, conservative classifier. Clear affirmatives ("yes",
  "go ahead", "do it") map to *Allow once* / proceed. Anything else maps to
  *Deny + feedback text*, or is sent as a prompt. "Always allow" is never chosen
  from free text.
- **Fallback**: if the parser can't produce options (`source: "screen"` with
  low confidence), show the last ~30 screen lines plus a key pad
  (1–9, ↑, ↓, ⏎, Esc). You are never stuck.

### 6.3 Dictation (first class)

- A big mic button on every agent screen. Tap to toggle or hold to talk; the
  mode is configurable. Waveform, haptic start/stop, auto-stop on silence
  (configurable).
- **Record first, transcribe later**: audio goes straight into durable local
  storage (IndexedDB in the PWA; app-private files via a native plugin in the
  APK) *before* anything touches the network.
- Transcription runs on the dev machine with whisper.cpp
  (`whisper-server` on 127.0.0.1; `deploy/whisper/build.sh` builds it for
  CUDA, or for the CPU). On a mid-range NVIDIA GPU, `large-v3-turbo` handles
  a 30 s clip in about a second; on a CPU, use a smaller model such as
  `base.en` or `small.en`. Audio never leaves your machine.
- In the native app (planned), Android's on-device `SpeechRecognizer` (where the phone
  supports offline recognition) gives a live preview transcript, even offline.
  The server transcript is used for anything that gets sent.
- **Send on stop**: tap the mic, speak, tap again (or let go of a hold) and
  the transcript is sent to the agent as a prompt through the outbox (§6.5),
  after anything typed in the composer. A take that sends never stops on
  silence (a pause mid-thought would send half a sentence); only the length
  cap ends it early, and then it sends with a note. Cancel while it
  transcribes discards the clip; nothing heard sends nothing; while the
  agent waits at a dialog the words go into the composer instead of a
  prompt the bridge would refuse.
  "Review dictation before sending" (Settings, per device, off by default)
  brings back the editable card with Use / Re-record / Discard. Optional
  auto-answer for transcripts that map with high confidence to a low-risk
  option ("yes"). The car path is special (§6.6).
- **The screen stays on for a take**: from the tap until the transcript is
  sent (or the take is cancelled, fails or waits on a card) the PWA holds a
  Screen Wake Lock, and asks again when the page comes back into view
  while the take is still on its way. If the page is hidden anyway (the
  power button, another app, a denied lock), the take stops there: what was
  said is saved, uploaded and transcribed, and waits on its card (never sent
  unseen) with "Recording stopped when the screen turned off or you switched
  apps." The PWA does not try to record on in the background: Chrome on
  Android keeps a page's microphone running while it is hidden (with its
  "using your microphone" notification), but throttles its timers (the
  length cap would slip) and may freeze or discard the tab, and a
  `MediaRecorder` holds the take in memory until it stops. iOS Safari stops
  the capture outright. Recording in the background belongs to the native
  app, with a foreground service.
- whisper.cpp's server ends every segment with a newline, and a segment can
  end mid-word or before punctuation (`" dict"`, `"ation,"`): segments join
  as they are, each one's leading space deciding where a word starts.
- Voice commands inside a dictation: "next agent", "read me the summary",
  "cancel". Parsed only when they are the whole utterance.
- Audio is deleted from the bridge after transcription, with 7-day retention
  for debugging (configurable). Transcripts are kept in the audit log.

### 6.4 TL;DR

For any turn above a size threshold (say >1500 words of output or >5 files
changed), the bridge produces:

```
Outcome:   one sentence — did it work?
Changed:   3–6 bullets (files/areas, +/- lines)
Decide:    what it needs from you, if anything
Risks:     anything it flagged or skipped (failing tests, TODOs, assumptions)
```

- Input: the transcript slice for the turn plus `git diff --stat`. Never the
  whole repo.
- Model: Claude Haiku (`claude-haiku-5-5`), called through headless
  `claude -p` on the dev machine so it uses your existing Claude Code login
  and **no new API key needs to be stored**. This sends the transcript slice
  to Anthropic under your own account, exactly as Claude Code itself does.
  Turn it off with `[tldr] enabled = false`; without a `claude` executable,
  there are no TL;DRs or suggestions. The summarizer sits behind an interface so it can
  switch to the API later.
- The summarizer has no tools. Agent output is untrusted input and is treated
  as data. Headless `claude -p` is a full agent by default (shell, file edits,
  and it loads `CLAUDE.md`, hooks and settings from its cwd), so the bridge
  starts it with every tool disabled, no setting sources, no MCP servers, and
  cwd set to a fresh empty temp dir. Transcript text goes in on stdin as data.
  A malicious transcript can at worst produce a bad summary.
- Cached per (agent session, turn). Generated eagerly when a long turn
  finishes, so it's there by the time you look.
- Rendered at the top of the agent view. The full thread sits below, with
  tool calls and diffs collapsed.

### 6.5 Offline and sync

- **App shell** is cached by a service worker, so the app opens instantly with
  no signal.
- **Read cache**: the last snapshot of agents, threads, pending inputs and
  TL;DRs is stored in IndexedDB.
- **Outbox**: every action (text, choice, audio-plus-intent) is a durable
  record with `idempotencyKey` and `expectedSeq` (§5). It flushes on
  reconnect, through Background Sync in the PWA and WorkManager in the APK, and
  retries with backoff.
- Outbox UI: pending items show a clock icon. Sent items show a tick. Stale
  items (409) become a decision card.
- Audio uploads are chunked and resumable, so a tunnel mid-upload resumes
  instead of restarting.

### 6.6 Notifications and Android Auto

- **Push transport**: the bridge sends a **content-free wake ping** with
  Web Push (VAPID), through the browser vendor's push service. The PWA wakes
  on it and fetches the actual content over the authenticated API. The
  Android app uses UnifiedPush instead (for example a self-hosted ntfy,
  allowed with `[push] ntfy_host`). Doze often blocks that
  fetch for a native app, so its subscription opts into **encrypted
  notices** (`"payload": "notice"`): the same message also carries, per
  agent with news, its name, state, one line (what it asks, or the turn's
  headline), the TL;DR *Decide* line once there is one, and the pending id
  to answer with. Every message is end-to-end encrypted to the phone (RFC
  8291), so the push service only sees ciphertext. The app registers
  with the phone's distributor while paired and shows a notice as a
  notification with no network call, also with its process cold (T31a,
  T32a); a tap opens that agent (T30c); replies and Android Auto are not
  built yet. Web Push (PWA) subscriptions keep the content-free ping, and
  the PWA inside the app makes none.
- **Only news disrupts.** A message is sent only if its reason still holds
  when its debounce ends (the agent still blocked, or its finished turn
  still unread), and only for a change since the last one; it carries
  notices only for the reasons that still hold. A *Decide* notice also needs
  its finished turn still unread. If a push still finds nothing to report (a
  late race), the PWA never alerts: a showing notification is re-shown
  silently in place, else a silent one is shown and closes itself.
- **Notifications** (native, planned) use `MessagingStyle`, one conversation per agent.
  A reply action uses `RemoteInput` (inline text/voice reply in the shade),
  plus option actions such as *Allow once* / *Deny* for permissions and a
  mark-as-read action.
- **Android Auto** (planned) uses the messaging category: no custom car UI, just
  correctly built notifications plus the `automotive_app_desc` declaration
  (`<uses name="notification"/>`). Android Auto reads "widget: run
  pnpm test?" aloud and takes a spoken reply. Notes:
  - Sideloaded app, so you must enable Android Auto developer settings →
    "Unknown sources". This is accepted (decision D3).
  - Car text is the notice's one line (what a blocked agent asks), or a
    finished turn's headline and *Decide* line. Never code beyond the
    one-line command a permission asks about.
  - **Announcements**: blocked agents always; a finished agent only once
    its TL;DR has a *Decide* line (`[push] car_announce_done`, default
    `"decide"`; `"always"` or `"never"`).
  - **Car replies** go to `POST /v1/agents/:id/car-reply` with the
    transcribed utterance as heard, the pending id from the notice, and an
    idempotency key; the app adds nothing. The bridge maps it with a tiny
    exact vocabulary (normalised like runbook phrases: case, punctuation,
    filler words): `allow` / `yes` / `approve` alone chooses the low-risk
    *Allow once* of the current dialog, re-read and pending-id checked;
    `deny` / `no` first chooses *Deny*, the rest of the utterance as
    feedback; "yes, but…" and "no problem, go ahead" are refused as
    ambiguous; a yes or no while nothing is open (after a *Decide* line) is
    sent to the agent as said; anything else is a plain
    prompt, only while no dialog is open. It answers with one sentence to
    read aloud ("Allowed: cargo test", "Sent.", "That needs the phone.").
  - **Car policy** (safe defaults; a setting may only tighten them):
    *Allow once* and *Deny + feedback* only on a dialog a Claude Code hook
    matched exactly (the screen parse alone, or a hook matching only the
    start of a cut command, is too unreliable to answer unseen),
    and plain prompts. *Always allow* and other elevated options, keys,
    interrupts, runbooks, terminal commands (`!`, `/`, `#`) and photos are
    unreachable from the car.
  - **Origin from the token**: the car signs in with a session of scope
    `car`. The bridge derives every action's origin from the session's
    scope, never the body, and a car session reaches only the car-safe
    endpoints (agent list, pending, TL;DR, car reply, car-origin actions).
  - Tested with the Android Auto Desktop Head Unit (DHU), then in the car.
    These tasks need a human to verify them.

## 7. UI design principles

- One thumb: primary actions in the bottom third, the mic as a bottom-center FAB.
- Dark by default, high contrast, large tap targets (≥48dp).
- Each agent state has its own hue, so a glance says where everything is:
  blocked (warm orange), working (blue), done (green). Blocked is the only
  loud one, the only state ever shown as a filled surface; working and done
  stay quiet (dots, rails, tinted text and soft washes). Nothing else uses
  these hues at full strength (diff marks use desaturated relatives).
- Monospace only inside code and diff blocks. Diffs are collapsed and
  horizontally scrollable *inside* the block; the page never scrolls sideways.
- Haptics on send, on record start/stop and on stale warnings.
- Everything works in the browser at phone width. The APK only adds native
  powers.

## 8. Security

Threat model: the bridge is a remote-control channel into shells on the dev
machine, running as the user who runs herdr. It is single-user. The attacker classes considered are: someone on the same network as
the phone; someone who has stolen or found the phone; a compromised
third-party push or CDN service; and **malicious content in agent output**
(prompt injection via repo content or web pages the agent read).

| Control | Detail |
|---|---|
| Network | The bridge listens on loopback by default; the only way in is your HTTPS proxy (§3). Keep it reachable only over a VPN (Tailscale, WireGuard) where you can. Plain HTTP off loopback is refused unless explicitly opted into (insecure MVP mode). |
| TLS | Required: pairing (WebCrypto), service workers, push and the mic need a secure context. Use a publicly trusted certificate (Tailscale's `*.ts.net`, Let's Encrypt) so nothing needs installing on the phone. A proxy on another host connects to the bridge over HTTPS with the bridge's own leaf (`[tls]`). |
| Device pairing | `passporter pair` on the dev machine shows a one-time QR (5 min, single use). The phone generates a **non-extractable** P-256 keypair and registers the public key. In a browser it is a WebCrypto key in the PWA's IndexedDB. In the APK it is an Android Keystore key (StrongBox when the phone has one) owned by native code, which also pairs, signs in and caches tokens, so native features (notification replies, push registration and Android Auto, as they land) can authenticate with the app closed; the PWA in the APK asks the native layer for app-scope tokens over IPC and never gets a car token. Native IPC is granted only on a WebView that labels each frame's origin (`WEB_MESSAGE_LISTENER`); on an older one the app grants nothing and the PWA keeps a WebCrypto key, as in Chrome. The Keystore key needs no user authentication, so background replies work; biometric step-up is the planned Step-up below. Every session token comes from signing a server challenge. Tokens are short-lived (15 min) and refresh by signature. |
| Revocation | `passporter devices list / revoke <id>`. Revocation takes effect immediately and kills open WebSockets. |
| Step-up (planned) | Elevated actions (*Always allow*, interrupt, any option tagged elevated) are to require device biometric unlock (WebAuthn user verification / BiometricPrompt). Not built yet: today they are refused, except raw keys and interrupt from the app, which go through without step-up (the keypad is the only way to answer a dialog, and an interrupt is one of its keys; see docs/security.md, known gaps). Never available from the car. |
| Session scope | Every session has a scope chosen when it is made: `app` or `car`. The action origin is derived from it (a car session is always `car`; an app session's claim can only narrow), and car sessions reach only the car-safe endpoints (§6.6). |
| App lock (planned) | Optional biometric lock on app open, with a configurable timeout. Not built: rely on the phone's own lock. |
| Least privilege | Fixed verbs only (§5). No arbitrary command endpoint. Input size limits. Per-device rate limits. |
| Audit | Append-only log of every action: device, origin (app/notification/car, as derived from the session's scope), agent, payload, result (`audit.jsonl` in the state dir; an in-app viewer is planned). |
| Untrusted rendering | Agent output is rendered as sanitized Markdown with **no raw HTML**. Strict CSP (`default-src 'self'`, no inline scripts). Links show the full URL and need confirmation. |
| Untrusted summarizing | TL;DR and suggestion models get no tools and a fixed output schema. Their output is shown, never executed. Suggestions are only ever *offered* (a tap sends one), never auto-sent (except the explicit high-confidence "yes" auto-send setting, which only maps to low-risk options). |
| Secrets | No secrets in the repo. The bridge reads config from `~/.config/passporter/` (mode 600). Local storage on the phone is app-private. |
| Supply chain | `Cargo.lock` committed, `cargo deny` (advisories, licenses, sources) in CI, minimal dependencies, no runtime CDN loads. |

## 9. Stack

| Concern | Choice | Why |
|---|---|---|
| Language | **Rust** everywhere it can run: bridge, herdr client, protocol, web UI (WASM). Kotlin only for Android-only APIs (notifications, Android Auto, recorder); a thin JS service-worker shim because browsers require one | One type system end to end; fast, small, smooth |
| Workspace | Cargo workspace (`crates/*`); toolchain pinned in `rust-toolchain.toml`, tools (just, nextest, cargo-deny, trunk) in `mise.toml` | Reproducible for every contributor |
| Bridge | axum + tokio + tower-http, rusqlite, rustls | Small, typed, one static binary |
| Validation | serde types in `crates/protocol`, strict deserialization (`deny_unknown_fields` on inbound) | Shared types on both ends; the compiler checks the contract |
| Web | Leptos (CSR) → WASM, built with trunk; `web-sys`/`gloo` for browser APIs (IndexedDB, MediaRecorder, WebAuthn, Push) | Fine-grained reactivity without a virtual DOM; small bundle. Browser APIs are clunkier from Rust, which costs effort, not quality |
| Android (shell built; plugins in progress) | Tauri 2 mobile + Kotlin plugins | Rust-native shell around the same Leptos UI; proper Kotlin plugin system |
| Speech-to-text | whisper.cpp server (CUDA or CPU), local | Private. A GPU makes large models fast |
| Push | Web Push (VAPID) in the PWA; UnifiedPush (e.g. ntfy) in the APK, with notices end-to-end encrypted | No content through third parties |
| Summaries | `claude -p --model haiku` | Uses the existing login, no stored key |
| Tests | cargo-nextest; a fake herdr (`FakeHerdr`) for integration tests; headless Chromium over CDP for E2E smoke tests (`just e2e`) | Everything but device and car behavior is verifiable in CI |
| CI | `just gate` (fmt, clippy pedantic `-D warnings` on host + wasm, nextest, release build, PWA size budget, cargo deny) in GitHub Actions | One gate, locally and in CI |

## 10. Status and roadmap

| Phase | Outcome | Status |
|---|---|---|
| **0. Foundations** | Cargo workspace, gate, `crates/herdr` | done |
| **1. Bridge** | Agents, threads, pending inputs, actions, pairing, TL;DR, transcription, Web Push, spawn, runbooks | done |
| **2. PWA** | Triage, detail, quick reply, dictation, outbox, TL;DR | done; usable in Chrome on Android |
| **3. Android** | APK with native notifications + inline reply, UnifiedPush, native recorder, Android Auto | planned |
| **4. Hardening** | WebAuthn step-up, app lock, SQLite ledger, audit viewer | planned (see docs/security.md, known gaps) |
| **5. Sessions** | One app over several herdr sessions (below) | planned |

### Planned: several herdr sessions

Today the bridge watches exactly one herdr session: the socket in
`herdr_socket`, else herdr's default (`~/.config/herdr/herdr.sock`). Each
named session (`herdr --session <name>`) has its own socket, so agents in
any other session are invisible to the phone.

The plan is sessions as the app's top level, kept apart rather than merged:

- **One session at a time.** The triage page shows a single session, say
  `prod` or `dev`. A switcher in the header's top corner cycles through them
  and can add one. Blocked, working and done never mix across sessions, so
  each list keeps meaning one place.
- **Still loud across sessions.** A session that is not on screen but has a
  blocked agent shows it on the switcher (a count), and pushes still fire, so
  switching to `dev` never hides that `prod` is waiting on you.
- **Bridge.** One herdr connection per session, found from herdr's session
  list (new sessions show up without a restart) or listed in config.
  Actions (reply, approve, spawn) go to the socket of the agent's session.
- **Protocol.** Agent ids gain the session name, because pane ids are only
  unique within a session. This changes ids, so existing deep links and
  outbox entries need a migration or a clean break on upgrade.


## 11. Decisions

| # | Decision | Status |
|---|---|---|
| D1 | Bring your own HTTPS: the bridge stays on loopback (or serves a private-CA leaf to a proxy), and a reverse proxy you run terminates public TLS | **accepted** |
| D2 | Summaries via `claude -p` (subscription) rather than an API key | proposed |
| D3 | Sideloaded APK; Android Auto developer "Unknown sources" accepted | **accepted** |
| D4 | No third-party relays for content (no Telegram/Matrix bots). Push carries no plaintext content: the PWA gets a content-free ping; the native app's opt-in notices (name, state, one line) are end-to-end encrypted to the phone (RFC 8291), so a push service only relays ciphertext | **accepted** (amended by T33A) |
| D5 | Tauri 2 mobile (Rust) rather than Capacitor or a fully native Compose app | **accepted** |
| D7 | Rust everywhere it can run, including a Leptos/WASM frontend | **accepted** |
| D6 | Built largely by coding agents under human review; every change passes the same gate | **accepted** |
| D8 | The Android app loads the PWA from the paired bridge's HTTPS origin rather than bundling it. The shell bundles only a Connect page that asks for that origin once; Tauri IPC is granted at runtime to exactly that origin (a remote capability), and the webview refuses to navigate anywhere else, so no other page reaches native plugins. Bundling the PWA was rejected: it would need a configurable API base, CORS and a widened CSP on the bridge, in-app pairing, and service workers and WebCrypto storage on Tauri's local scheme, and every UI change would need a new APK. The cost: the first launch needs the bridge reachable, and Web Push is unavailable in the webview (the app's UnifiedPush replaces it, §6.6). Since T32c the device key in the APK is native (Android Keystore, §8), so native plugins authenticate on their own | **accepted** (T30b) |
