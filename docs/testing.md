# Testing

| Command | What runs | Where |
|---|---|---|
| `just verify` | fmt, clippy (host + wasm), unit and integration tests | everywhere; must pass before pushing |
| `just gate` | `verify`, release build, PWA size budget, `cargo deny` | CI (GitHub Actions), and locally before a PR |
| `just e2e` | headless browser smoke tests | opt-in, on a machine with Chromium |

Unit tests sit next to the code, integration tests in each crate's `tests/`
(see `AGENTS.md`). The live-herdr test (`crates/herdr/tests/live_session.rs`)
is opt-in too: `PASSPORTER_HERDR_IT=1`.

## Browser smoke tests (`just e2e`)

The `e2e` crate drives the real app the way a phone uses it:

- the real `passporter-bridge` binary, on a free `127.0.0.1` port, with a
  throwaway config (`allow_static_token = true`, a random token), state dir,
  Claude projects dir and Codex sessions dir (and `[push]`, with a key made
  by `passporter-bridge push-keygen`, where a test needs it);
- the real PWA, a release `trunk` build in `crates/web/dist`;
- a **scripted fake herdr** (`e2e/src/herdr.rs`): a unix socket speaking
  herdr's JSON protocol. The bridge is pointed at it with `herdr_socket`, so
  the bridge has no test-only mode. It reports agents the way herdr 0.9.1
  really does (checked against the recorded replies in
  `crates/herdr/tests/fixtures`): no `agent_session` for some, no name for
  some (known by their terminal title), labelled workspaces including the
  swarm's quiet one, `state_change_seq` that moves with every status, and
  screens that tick between reads. It records keys and prompts, plays a
  shell for runbooks and new agents, and can fail a call;
- **realistic transcripts**: the finished agent's is
  `crates/transcripts/tests/fixtures/realistic.jsonl` (every record kind the
  corpus shows, injected ones included), the Codex agent's a real Codex
  0.166 rollout;
- a **fake `claude`** (`[tldr] claude`, a shell script written by
  `e2e/src/lib.rs`) that answers the summarizer's structured requests by
  schema, so headlines, TL;DRs, suggestions and status lines run end to end
  without a model (and the real CLI is never run);
- a **fake `whisper-server`** with a canned transcript, in the real server's
  shape (a line per segment, one split mid-word);
- system Chromium, headless at 390×844 with touch emulation and a fake
  microphone, driven over the `DevTools` protocol by a small client in
  `e2e/src/cdp.rs`.

The plain-HTTP journey reaches the bridge as `http://passporter.test:<port>/`,
mapped to loopback with `--host-resolver-rules`: the MVP phone reaches the
bridge over plain HTTP, so it gets the token form. Scenarios that need a
secure context (the mic, the service worker, push) use the loopback origin
with the token stored where the form stores it.

**Rules for scenarios** (`e2e/src/app.rs` has the shared steps):

- Only the app's own code paths: taps, typing, switching apps, the network
  failing. Never call a WASM module's exports or other internals directly
  (that is how a broken lazy loader shipped).
- Wait on what the page shows (`expect`) or what the fakes saw (`until`),
  never on time.
- Fixtures follow reality: when herdr, Claude Code or Codex is seen doing
  something new, add it to the fixtures first.
- Every user-reported bug gets a failing e2e reproduction before its fix.

| Test | Covers |
|---|---|
| `smoke::phone_smoke` | token connect; triage segments and blocked card's ask; a session-less agent's thread matched by folder; summary-first thread with asides (report, task notifications, `!` command), a queued prompt, no injected text; Summarise; chips through leaving and returning (failed and hung re-reads, ticking screen, chips never vanish); versions; new agent; runbook; no sideways scroll at 360 px |
| `smoke::phone_secure` | suggested reply (double tap sends once); dictation through the dock mic, send on tap; photo attachment; terminal mode with a decoy |
| `reality::agents_as_herdr_reports_them` | unnamed and session-less agents, the swarm's agent listed; status line on tap and on the card; live status changes move cards |
| `reality::a_codex_agent` | Codex thread from its rollout; approval chips with Codex risk rules; approve sends its key |
| `reality::chips_and_keypad_answer_a_dialog` | elevated chip locked and explained; Yes sends its key; keypad on the screen view |
| `reality::the_outbox_offline_and_stale` | a prompt waits offline and sends once; a stale one becomes a decision, never sent silently; send anyway |
| `reality::settings_and_dictation_review` | Settings switches persist; full transcripts; review mode's transcript card, Use, Send |
| `reality::a_long_dictation_keeps_the_screen_on` | a take holds a screen wake lock until it is sent; the screen going off mid-take keeps and uploads what was said, takes the lock again on return while the clip is on its way, and shows it on its card with why it stopped |
| `reality::a_newer_bridge_offers_a_reload` | the update banner after a bridge deploy |
| `reality::the_notification_names_only_the_users_agents` | a push delivered to the real service worker: the notification names blocked and done user agents, never the swarm's |
| `reality::a_push_whose_reason_is_gone_does_not_alert` | a push after the blocked agent was answered and the done one read: one silent notification that closes itself, never "all caught up"; a later empty push leaves a showing alert in place, silently |

Every test also fails on console errors, uncaught exceptions and browser
log errors, except Chromium's notice that it ignored the bridge's
`Cross-Origin-Opener-Policy` header on a plain-HTTP origin (which the MVP
transport causes by design) and errors a scenario provokes on purpose.

### Running it

```sh
just e2e
```

builds the PWA (`just web-build`) and the bridge (debug), then runs
`cargo nextest run -p e2e --run-ignored only`. The tests are `#[ignore]`d,
so `just verify` and `just gate` compile them but never run them.

- **No Chromium**: `just e2e` prints `e2e: SKIPPED: no Chromium found` and
  exits 0, and the test itself skips with the same message if run directly.
  It looks for `chromium`, `chromium-browser`, `google-chrome-stable`,
  `google-chrome` and `chrome` on `PATH`.
- `PASSPORTER_CHROMIUM=/path/to/chrome` picks a specific browser.
- `PASSPORTER_E2E_NO_SANDBOX=1` passes `--no-sandbox` to Chromium. Only for
  containers and sandboxed shells where Chromium cannot start its own sandbox
  (it aborts with "The SUID sandbox helper binary was found, but is not
  configured correctly"); the page it loads is only the local PWA.
- `PASSPORTER_BRIDGE_BIN` and `PASSPORTER_WEB_DIST` override the bridge binary
  and PWA build the test uses (`just e2e` sets both).

A failed step names what it waited for and prints the page's text and the
bridge's log. The tests take under a minute once built (each starts its own
bridge and browser).

## Demo video (`just demo`)

`e2e/examples/demo.rs` records the README's demo on the same rig
(`Rig::start_with`, with its own world of six agents in three neutral
repos): a scripted walk through the app, captured over CDP and set on a
1080p stage with captions by `ffmpeg`, into
`target/demo/passporter-demo.mp4`. Narration is optional:
`uv run e2e/examples/narrate.py` speaks `target/demo/narration/lines.tsv`
(written by each run) with Kokoro, and the next `just demo` lays the clips
under their scenes. It is an example, so `just verify` compiles and lints it
but nothing runs it.
