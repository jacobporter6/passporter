# bridge: passporter-bridge

The service on the dev machine that wraps herdr in a small, typed,
authenticated API and serves the PWA (design §3, §5). Wire types live in
`crates/protocol`.

## API

All `/v1` routes are authenticated except the pairing routes and
`GET /v1/version`. Bodies are protocol types.

| Route | Returns | Notes |
|---|---|---|
| `GET /v1/agents` | `Agent[]` | |
| `GET /v1/agents/:id/thread?since=&limit=` | `ThreadPage` | Claude agents with a session; others get an empty page. `limit` is capped at 500. |
| `GET /v1/agents/:id/screen` | `AgentScreen` | Viewport while working, last 60 lines of recent output otherwise. |
| `GET /v1/agents/:id/pending` | `PendingInput \| null` | `null` unless herdr reports the agent `blocked`; then its screen, read and parsed on every request (see "Pending input" below). |
| `POST /v1/actions` | `ActionResult` | `prompt`, `keys`, `interrupt`, `choose`. The origin is the session's scope's (`car` for a car session, whatever the body says). |
| `POST /v1/agents/:id/car-reply` | `CarReply` | Android Auto's raw utterance (`crate::actions::car`): `yes`/`allow`/`approve` → *Allow once*, `no`/`deny` [+ feedback] → *Deny*, on a hook-described dialog named by `pendingId`; anything else a prompt while no dialog is open. Runs the actions pipeline as `origin: "car"`; answers with a sentence to read aloud. |
| `GET /v1/version` | `BridgeVersion` | Public: `{"bridge": "<short sha>[-dirty]", "built": "<RFC 3339 UTC>"}`, so a stale or unpaired PWA can still tell it is behind. Also logged at startup and shown by `passporter doctor` (the CLI's own build). |
| `GET /v1/projects` | `SpawnOptions` | The `[spawn]` project names and kinds the phone may start; both empty without `[spawn]`. |
| `POST /v1/agents` | `SpawnResult` | Starts an agent in an allowlisted project (`crate::spawn`): idempotent, audited first, 6 per 10 min per device, no client-supplied paths or flags. |
| `GET /v1/runbooks` | `RunbookList` | The `[[runbooks]]` with their exact `argv` and `cwd` and latest run; empty without any. |
| `POST /v1/runbooks/:name/run` | `RunbookResult` | Runs a configured runbook in its own herdr tab (`crate::runbooks`): idempotent, audited first, one run per runbook, 10 per 10 min per device, never from the car, no client-supplied arguments. Progress goes out as `runbook_updated`. |
| `GET /v1/agents/:id/commands` | `CommandList` | Terminal mode (`crate::commands`): every command candidate in the agent's latest message, in order, with its intent and a content hash; empty without a transcript. |
| `POST /v1/agents/:id/commands/run` | `CommandResult` | Runs one listed candidate (by id and hash, never by text) here (`!<cmd>` into the agent) or in a new terminal tab in the agent's folder: re-extracted and re-checked after the audit write, idempotent, 20 per 10 min per device, never from the car. |
| `GET /v1/terminals/:pane/screen` | `TerminalScreen` | The last lines of a terminal a command run opened; 404 for any other pane. |
| `POST /v1/terminals/:pane/keys` | `CommandResult` | Allowlisted keys into such a terminal: audited, idempotent, rate limited, never from the car. |
| `WS /v1/stream` | `AgentEvent`s | First client message must be `StreamAuth`. Then `snapshot`, one `pending_changed` per known pending input, then changes. |

A session of scope `car` (`"scope": "car"` at `POST /v1/auth/session`)
reaches only `GET /v1/agents`, `GET /v1/agents/:id/pending`,
`GET /v1/agents/:id/tldr`, `POST /v1/agents/:id/car-reply` and
`POST /v1/actions`; every other route is 403 and the stream closes.

`:id` is the agent id (`<pane>` or `<pane>~<session>`). An id whose pane now
runs another session is 404 on the GET routes and 409 `stale` on actions.

`POST /v1/actions` HTTP status by result: `sent` 200, `failed` 200 (final:
delivery is uncertain), `stale` 409, `refused` 400 (`invalid_action`), 403
(`step_up_required`, `car_policy`), 404 (`unknown_agent`), 429
(`rate_limited`: more than 60 new actions a minute from one device, or
from one address with the static token, with `Retry-After`; replays and
`interrupt` are never limited; or the same key still in flight), 422 (everything else),
and 503 when herdr or the audit log is unavailable *before* anything was
sent (not recorded: retry the same key). Clients should read the body
whatever the status.

## MVP mode and known simplifications

The bridge ships with deliberate simplifications. Each is opt-in or
documented here. The two insecure ones (the static token and plain HTTP
off loopback) are **off by default** and exist only for a phone that
reaches the machine solely over WireGuard (docs/deploy.md, "Insecure MVP
mode").

| Simplification | Now | Replaced by |
|---|---|---|
| **Auth** | Device pairing is the real way in (design §8): `passporter pair` prints a terminal QR and a code (single use, 5 min); the phone registers a P-256 public key (`POST /v1/auth/pair`), then signs its device id and a single-use challenge (`POST /v1/auth/challenge`, then `/v1/auth/session`) for a 15-minute bearer token, and refreshes by signing again. Open streams close (1008) when their session expires; the phone reconnects with its renewed token. A 401 means a bad or stale code, challenge or signature; an unknown or revoked device gets 410, the only answer after which the phone deletes its key. `passporter devices list` / `devices revoke <id>` (revocation ends the device's sessions, closes its open streams with 1008 and drops its push subscriptions). The three endpoints are rate limited per client IP (pair 5/min, challenge + session 30/min; 429 with `Retry-After`), and ten wrong codes burn every pending code. `stepUp` sessions are refused (400) until real user verification (WebAuthn) exists. Devices persist in `state_dir/devices.json` (public keys only); sessions are in memory, so a restart signs every phone out until it signs again. The CLI reaches the running bridge over `state_dir/admin.sock` (0600; the state dir is tightened to 0700 at startup). The device flow needs a secure context (WebCrypto), so over plain HTTP the static `api_token` (32–256 visible ASCII chars, constant-time compare) is accepted **only** with `allow_static_token = true`, and never alongside `[tls]` or `trusted_proxy` (a startup error); `false` refuses it, and an `api_token` with neither is a startup error naming the line to add. `deploy/bridge/setup.sh` writes a token only with `--insecure-mvp`. HTTP: `Authorization: Bearer <token>`. WS: the first message must be `{"token": "<token>"}` within 5 s, or the socket is closed with 1008 (policy violation); nothing is sent before it. | Pairing over HTTPS (docs/deploy.md, "HTTPS"); then set `allow_static_token = false` |
| **Transport** | Plain HTTP off loopback **only** with `insecure_http_over_wireguard = true` **and** `listen`'s IP in `insecure_http_allow` (never `0.0.0.0`/`::`). WireGuard terminates on the router, so it encrypts phone → router only; the router → workstation hop is plain LAN traffic. Restrict the port to your WireGuard subnet in the firewall (for example `sudo ufw allow from <tunnel subnet> to any port 8443 proto tcp`) so other LAN devices can't connect; the remaining exposure is someone already on the trusted LAN sniffing that hop to capture the token. A loud warning is logged at startup. Without the opt-in the bridge still refuses plain HTTP off loopback. | Loopback behind an HTTPS proxy (the default), or a `[tls]` table, which makes the bridge serve HTTPS only (and refuses the opt-in alongside it); docs/deploy.md, "HTTPS" |
| **Pending input** | Screen parsing only (no hooks yet). A `blocked` agent always has a pending input with id `screen-<seq>-<12 hex>` (the hex hashes kind, prompt, detail and each option's id, label and key, so two dialogs at the same herdr `seq` get different ids, and `pending_changed` goes out whenever it changes), `source: "screen"`, a `confidence` and the `screenTail`. Claude Code's permission prompts, plan approval and `❯` menus parse into options (ids such as `yes`, `yes_always`, `no`, `no_feedback`, `yes_auto_accept`, `yes_auto_mode`, `yes_manual`, `no_keep_planning`, else `opt_N`); below 60 % confidence the input has **no options** and kind `unknown`, and the phone falls back to the screen and keypad. Blocked agents are re-parsed every second and when they turn blocked; changes go out as `pending_changed` (and `null` when the agent leaves `blocked`). | Hook events (docs/hooks.md) |
| **`choose`** | Re-parses the screen at action time; `pendingId` must be the current content-hashed id **and** the option must be in that parse, else 409 `stale` (with the current `pending`). After the audit entry the bridge reads and parses once more and requires the same `seq` and id, else 409 and nothing is sent; only the few milliseconds between that read and herdr delivering the key remain. The same id check (and re-check) applies to `keys` carrying a `pendingId`: the web keypad will send it so its keys cannot land in a different dialog; `keys` without one behave as before. Sends the option's digit, or Esc for an `(esc)` option. `feedbackText` only on `no_feedback` (older dialogs with "No, and tell Claude what to do differently (esc)"; Claude Code 2.1.317 shows a plain `no` with "Tab to amend", which is not supported yet, so there feedback is not accepted): after Esc the bridge waits up to 3 s for herdr to report `idle`/`done`, then sends the text with `agent.prompt`; otherwise `failed` (option chosen, feedback not sent). | — |
| **Step-up** | There is no step-up mechanism yet, so anything protocol `policy` says needs step-up is refused with `step_up_required` (transient), **except raw `keys` and `interrupt` from origin `app`**: the keypad is the MVP's only way to answer a dialog, and `interrupt` is exactly the keypad's `ctrl+c`. Elevated prompts (`!`, `/`, `#`), prompts while a dialog is open, and `choose` of an elevated option (*don't ask again*, *auto-accept*, *bypass*, anything unrecognized) or with elevated feedback are refused. Low-risk options can be chosen from `app` and `notification`; from the `car` only on a dialog a hook described (screen-parsed alone is refused). The origin comes from the session's scope, so a car session never gets the exemption. Origin `car` always follows `check_car_policy`; origin `notification` gets no exemption. | WebAuthn step-up (planned) |
| **Idempotency** | An in-memory map, `idempotencyKey` → (body, result), bounded to 1024 results. Same key + same body replays the stored result (`sent` comes back with `replayed: true`); same key + other body is `idempotency_conflict`. **It does not survive a restart**: a retry after a restart can execute again, though the stale guard catches most such retries (a delivered prompt or key usually moves `state_change_seq`). | SQLite ledger (planned) |
| **Audit** | Append-only JSON lines at `audit_log` (default `~/.local/state/passporter/audit.jsonl`, file 0600, dir 0700). An `execute` entry is written and synced **before** herdr is called (if it cannot be written, nothing is sent); a `result` entry follows. Prompts are cut to 200 characters; no tokens are logged. | `passporter audit` and in-app view (later) |
| **Threads** | One transcript reader per agent, created on the first thread request; at most 16, dropped after 15 minutes without a request or when the agent leaves. Held readers are polled every second and changes are pushed as `thread_items`. A transcript that appears later is announced with a `thread_items` event carrying its new epoch and no items (refetch from 0). | Event-driven readers |

## Running it

```sh
deploy/bridge/setup.sh    # once: build, install, write a pairing-only config, enable the user unit
journalctl --user -u passporter-bridge -f
```

See `deploy/bridge/` for the unit and an example config. Config reference:
`src/config.rs`.
