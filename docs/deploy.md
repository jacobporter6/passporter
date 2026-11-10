# Deploying Passporter

Passporter's bridge runs on the machine where herdr and your agents run,
**as the same user** (it talks to herdr's socket and reads
`~/.claude/projects`). It listens on loopback; phones reach it through an
HTTPS reverse proxy you choose. Each service has a script under `deploy/`
that builds, installs and (re)starts it as a systemd **user** unit;
re-running a script is safe.

| Service | Script | Listens on |
|---|---|---|
| `passporter-bridge` | `deploy/bridge/setup.sh` | `listen` in `~/.config/passporter/config.toml` (default `127.0.0.1:8443`) |
| `whisper-server` (dictation, optional) | `deploy/whisper/build.sh` | `127.0.0.1:8178` only |

To keep them running while logged out: `loginctl enable-linger "$USER"`.
Agents only work while the machine is awake: a laptop that sleeps freezes
herdr, the agents and the bridge alike.

After any change, run `passporter doctor` (see [Checking a deployment](#checking-a-deployment-passporter-doctor)).


## Requirements

- Linux with systemd (other init systems work if you start
  `passporter-bridge` yourself; macOS is untested);
- [herdr](https://herdr.dev) (tested with 0.9.x), running as your user;
- Claude Code (tested with 2.1.x) and/or Codex CLI (0.166+) inside herdr;
- the Rust toolchain via [rustup](https://rustup.rs) (the version in
  `rust-toolchain.toml` installs itself), and `trunk` for the PWA
  (`mise install` gets `trunk`, `just`, `cargo-nextest` and `cargo-deny`
  from `mise.toml`; or `cargo install trunk --locked`);
- an HTTPS reverse proxy: [Tailscale](https://tailscale.com) (the quick
  start below) or [Caddy](https://caddyserver.com) with a domain;
- optional: an NVIDIA GPU with the CUDA toolkit for fast dictation
  (whisper.cpp also runs on the CPU with a smaller model);
- optional: the `claude` CLI, logged in, for TL;DRs and reply suggestions.

## Quick start (Tailscale Serve, about 15 minutes)

Tailscale Serve gives the bridge an HTTPS name on your tailnet
(`https://<machine>.<tailnet>.ts.net/`) with a publicly trusted
certificate. No domain, no CA, no open ports, and only your own devices
can connect. You need Tailscale on the dev machine and on the phone, with
[HTTPS certificates enabled](https://tailscale.com/kb/1153/enabling-https)
for your tailnet.

```sh
git clone https://github.com/jacobporter6/passporter && cd passporter
deploy/bridge/setup.sh                 # build, install, pairing-only config, systemd user unit
tailscale serve --bg --https=443 http://127.0.0.1:8443
passporter pair                        # QR + code, single use, 5 minutes
```

On the phone, open `https://<machine>.<tailnet>.ts.net/`, enter the code,
and add the page to the home screen. Then:

```sh
passporter doctor                      # every check should be ok, warn or off
```

`setup.sh` writes this config (no static token, no plain HTTP off
loopback):

```toml
listen = "127.0.0.1:8443"
trusted_proxy = "127.0.0.1"            # tailscale serve connects from loopback
static_dir = "/path/to/passporter/crates/web/dist"
state_dir = "/home/you/.local/state/passporter"
```

Next, optionally: [push notifications](#push-wake-pings-and-native-notices),
[dictation](#transcription-whisper-server), [new agents from the
phone](#new-agents-from-the-phone-spawn) and
[runbooks](#runbooks-from-the-phone-runbooks).

## HTTPS

The PWA needs a **secure context** for pairing (WebCrypto keys), its
service worker (offline shell and outbox), Web Push and, in most browsers,
the microphone. So the bridge is always reached over HTTPS, terminated by
one of:

1. **Tailscale Serve** on the same machine (the quick start above);
2. **Caddy with a public domain** on the same machine
   ([below](#caddy-with-a-public-domain));
3. **a reverse proxy on another host** that connects to the bridge over
   HTTPS with the bridge's own leaf certificate
   ([advanced example](#advanced-example-a-split-horizon-edge-on-another-host)).

The bridge refuses plain HTTP off loopback unless you opt into the
[insecure MVP mode](#insecure-mvp-mode-not-recommended).

### Caddy with a public domain

On a VM or a server with a public IP and a DNS name pointing at it, Caddy
gets a Let's Encrypt certificate by itself:

```caddyfile
passporter.example.com {
    reverse_proxy 127.0.0.1:8443
}
```

with the bridge on loopback, as `setup.sh` writes it:

```toml
listen = "127.0.0.1:8443"
trusted_proxy = "127.0.0.1"
```

Pairing and device keys are then all that stands between the internet and
your shells. If you can, keep the name reachable only from a VPN (a
private DNS name, or Caddy's `remote_ip` matcher with your VPN's range),
and firewall everything but 443.

### Advanced example: a split-horizon edge on another host

This is how you might run it with an existing home network edge: a
reverse proxy on another host (a small VM or container), reached by the
phone over WireGuard, that already serves other internal services. The
names below are placeholders:

| Placeholder | Meaning |
|---|---|
| `passporter.example.com` | the name the phone opens: a split-horizon name your LAN's DNS resolver answers with the edge's address, never published publicly |
| `192.0.2.46` | the edge proxy |
| `192.0.2.10` | the dev machine (bridge) |
| `198.51.100.0/24` | the phone tunnel's subnet |
| `passporter.internal` | the name in the bridge's own leaf, from your private CA |

The edge terminates a publicly trusted certificate (for example a
Let's Encrypt wildcard by DNS-01, so the name stays out of Certificate
Transparency logs) and proxies over HTTPS to the bridge, trusting only
your private CA. The bridge presents a leaf from that CA. The Caddy site on
the edge must verify the name the leaf carries:

```caddyfile
passporter.example.com {
    reverse_proxy https://192.0.2.10:8443 {
        transport http {
            tls_trusted_ca_certs /path/to/private-root.pem
            tls_server_name passporter.internal   # = tls.name in the bridge config
        }
    }
}
```

1. **Install the leaf.** Issue a leaf from your private CA with the
   DNS SAN `passporter.internal` (an ECDSA P-256 key is fine; RSA and
   Ed25519 work too), then:

   ```sh
   install -d -m 0700 ~/.config/passporter/tls
   install -m 0644 leaf-chain.pem ~/.config/passporter/tls/leaf.pem   # leaf first, then any intermediates
   install -m 0600 leaf.key       ~/.config/passporter/tls/leaf.key   # PKCS#8, SEC1 or PKCS#1 PEM
   ```

2. **Edit `~/.config/passporter/config.toml`**:

   ```toml
   listen = "192.0.2.10:8443"        # an address the edge can reach
   trusted_proxy = "192.0.2.46"      # believe X-Forwarded-For from the edge, nobody else

   [tls]
   cert = "tls/leaf.pem"             # relative to the config dir
   key = "tls/leaf.key"
   name = "passporter.internal"      # default; what doctor checks the SAN against
   ```

   The bridge refuses `insecure_http_over_wireguard`/`insecure_http_allow`
   alongside `[tls]`, and refuses `allow_static_token = true` alongside
   `[tls]` or `trusted_proxy`.

3. **Check** before restarting. `listen` and `tls` must be `ok`:

   ```sh
   passporter doctor
   ```

4. **Firewall**: admit the bridge's port from the edge only, for example:

   ```sh
   sudo ufw allow from 192.0.2.46 to any port 8443 proto tcp
   ```

5. **Restart** and look for `scheme=https` and the leaf's expiry in the
   log:

   ```sh
   systemctl --user restart passporter-bridge
   journalctl --user -u passporter-bridge -e
   ```

6. **Test the hop** from the dev machine (expect `401`: TLS works, and the
   request carries no token):

   ```sh
   curl -sS -o /dev/null -w '%{http_code}\n' --cacert private-root.pem \
     --resolve passporter.internal:8443:192.0.2.10 \
     https://passporter.internal:8443/v1/agents
   ```

   Then on the phone open `https://passporter.example.com/` and pair
   (`passporter pair`).

**Renewing the leaf**: install the new chain and key over the old files
(step 1's `install` commands). The bridge checks the files' content every
minute and, once they have stayed unchanged for one more check (so a
renewal caught between the chain and the key is not mistaken for a broken
one), serves the new leaf from the next connection: within two minutes.
`systemctl --user reload passporter-bridge` (SIGHUP) makes it re-read them
at once. The log says `TLS leaf reloaded` with the new expiry. The bridge
only ever serves a leaf the edge would accept: if the new files are broken
(a key that does not match or that others can read, a leaf without the
`tls.name` SAN, expired or not valid yet) it keeps serving the old leaf and
logs why, and it refuses to start with such a leaf. Problems outside the
files' content are retried at every check, so running the `chmod 600` the
log asks for, or waiting out a CA clock that is slightly ahead, is enough;
a bad pair waits until the files change again. The same error is logged
once, not every minute. From 14 days before the served leaf expires, the
bridge logs a daily warning (and an error once it has expired);
`passporter doctor` names the problem too.

Without `[tls]`, SIGHUP (and so `systemctl --user reload`) is logged and
ignored.

### Insecure MVP mode (not recommended)

For a phone that can only reach the dev machine over a WireGuard tunnel
and cannot use any of the HTTPS options above, the bridge can serve
**plain HTTP** off loopback and accept a **static bearer token**. Both are
explicit opt-ins, and the bridge refuses to start without them:

```sh
deploy/bridge/setup.sh --insecure-mvp 192.0.2.10   # the dev machine's tunnel-reachable IP
```

writes:

```toml
listen = "192.0.2.10:8443"
insecure_http_over_wireguard = true
insecure_http_allow = ["192.0.2.10"]   # exactly listen's IP; never 0.0.0.0 or ::
api_token = "<64 hex characters, printed once>"
allow_static_token = true
```

What this costs you:

- The token is a **permanent, unrevocable credential**: anyone who has it
  can type into your agents until you remove it from the config.
- Over plain HTTP the PWA has no secure context: it **cannot pair**, has no
  offline shell, no push, and dictation may be blocked by the browser.
- If WireGuard terminates on your router, it encrypts phone → router only;
  the router → dev machine hop is plain LAN traffic, so someone on that
  LAN can sniff the token. Restrict the port to the tunnel's subnet:

  ```sh
  sudo ufw allow from 198.51.100.0/24 to any port 8443 proto tcp   # your tunnel subnet
  ```

The bridge logs a loud warning at every start, and `passporter doctor`
warns about each compromise. To leave MVP mode: set up HTTPS, remove the
`insecure_http_*` lines, pair every phone, then set
`allow_static_token = false` and delete `api_token`. The bridge refuses
the static token once `[tls]` or `trusted_proxy` is configured.
`crates/bridge/README.md` ("MVP mode") lists the details.

## Bridge

### First install

```sh
deploy/bridge/setup.sh [--listen ADDR:PORT] [--trusted-proxy IP] [--no-service] [--config-only]
```

`setup.sh`:

1. builds the bridge (release) and installs `passporter-bridge` and the
   `passporter` CLI to `~/.local/bin`;
2. builds the PWA into `crates/web/dist` if `trunk` is installed (otherwise
   run `just web-build` later; the bridge serves only the API until then);
3. creates `~/.config/passporter/` (`0700`) and a pairing-only
   `config.toml` (`0600`): `listen` (default `127.0.0.1:8443`),
   `trusted_proxy` (default `127.0.0.1` on loopback), `static_dir` and
   `state_dir`. `~/.local/state/passporter/` (`0700`) is created too;
4. installs, enables and restarts `passporter-bridge.service` as a systemd
   user unit (`deploy/bridge/passporter-bridge.service`), unless
   `--no-service`.

`setup.sh` never touches an existing `config.toml`. `--config-only` does
step 3 alone, and `--help` lists every option.
`deploy/bridge/config.example.toml` shows a typical file; every key is
documented in `crates/bridge/src/config.rs`.

The unit runs `passporter-bridge --config-dir ~/.config/passporter` with a
read-only file system except the state dir (`ProtectSystem=strict`,
`ReadWritePaths=~/.local/state/passporter`, `UMask=0077`). Logs:

```sh
journalctl --user -u passporter-bridge -f
tail -f ~/.local/state/passporter/audit.jsonl     # every action a phone sent
```

### Files and modes

| Path | Mode | Holds |
|---|---|---|
| `~/.config/passporter/` | `0700` | config and keys; the bridge refuses to start if group or others have any access |
| `~/.config/passporter/config.toml` | `0600` | settings (and the static token, in MVP mode only) |
| `~/.config/passporter/vapid.pem` | `0600` | the Web Push signing key (if push is on) |
| `~/.config/passporter/tls/leaf.pem` | `0644` | the TLS chain, leaf first (only with `[tls]`) |
| `~/.config/passporter/tls/leaf.key` | `0600` | its private key; the bridge refuses a key group or others can read |
| `~/.local/state/passporter/` | `0700` | tightened to `0700` at every start |
| `…/state/passporter/audit.jsonl` | `0600` | append-only action log |
| `…/state/passporter/devices.json` | `0600` | paired devices (public keys only) |
| `…/state/passporter/admin.sock` | `0600` | how `passporter pair` / `devices` reach the running bridge |
| `…/state/passporter/push-subscriptions.json` | `0600` | push subscriptions per device |
| `…/state/passporter/audio/` | `0700` | dictation clips (see Transcription) |
| `…/state/passporter/attachments/` | `0700` | photos sent with prompts, re-encoded without metadata, `0600` each (see Photo attachments) |


### Pairing and devices

**Device pairing** is the way in (design §8):

```sh
passporter pair [--name Pixel]   # terminal QR + code, single use, 5 minutes
passporter devices list
passporter devices revoke <id>   # ends its sessions, closes its streams, drops its push subscriptions
```

The phone makes a non-extractable P-256 key with WebCrypto (which needs
HTTPS), registers its public key with the code, and from then on signs a
server challenge for each 15-minute session. Sessions live in memory:
restarting the bridge signs paired phones out until they sign their next
challenge (automatic).

### Upgrading

```sh
git pull
deploy/bridge/setup.sh
passporter doctor
```

Re-running `setup.sh` rebuilds and reinstalls both binaries and the PWA,
reinstalls the unit and restarts the bridge, keeping `config.toml`.

To confirm what is running: `curl -s <bridge>/v1/version` gives the
bridge's commit and build time (`-dirty` for a modified tree), the
bridge logs the same at startup, and the phone shows its own build and the
bridge's in Settings › About. When they differ the phone offers "New
version available: Reload".

If the bridge does not come back, `journalctl --user -u passporter-bridge -e`
names the config line it refuses; `passporter doctor` says the same with a
fix for each problem.

### Push (wake pings and native notices)

Push is off until the config has a `[push]` table. When an agent becomes
blocked or finishes a turn, every subscribed device gets a content-free
ping (an opaque random id, encrypted and VAPID-signed) and fetches the
actual state over the authenticated API. The native app's subscription
can opt into short **notices** instead (agent name, state, one line),
because Doze often blocks that fetch; they are encrypted end to end to the
phone exactly like the ping, so the push service only ever sees
ciphertext. PWA subscriptions keep the bare ping.

```sh
passporter-bridge push-keygen      # writes ~/.config/passporter/vapid.pem (0600), prints the public key
```

```toml
[push]
subject = "mailto:you@example.com"   # VAPID contact the push services see (mailto: or https:)
# vapid_key = "vapid.pem"            # default; relative to the config dir
# extra_hosts = ["push.example.org"] # self-hosted push services, beyond the browsers' own
# ntfy_host = "ntfy.example.org"     # a self-hosted ntfy server (exact name; native app)
# car_announce_done = "decide"       # default; Android Auto announces a finished agent only once
#                                    # its TL;DR has a Decide line ("always", "never")
# quiet_done_workspaces = ["agent-swarm"]       # finished turns here never ping (default: none)
# quiet_done_agent_prefixes = ["impl-", "rev-"]  # nor those of agents named so (default: none)
```

Then `systemctl --user restart passporter-bridge`. `push-keygen` never
overwrites a key; making a new one means every device must subscribe again.

Agents in the herdr workspaces named in `quiet_done_workspaces` (matched
by workspace label) never ping for finishing a turn, only for becoming
blocked. This is for automation: if an orchestrator runs agents in its own
workspace, its spun-out agents (reviewers, say) stay quiet while your own
sessions still ping when they are done. The list is empty by default, so
every finished turn pings. The bridge reads the labels from
herdr; when it has no label for an agent yet (a tab that just appeared), it
asks herdr before deciding, and only an agent whose workspace herdr cannot
tell pings as usual.

Independently, agents whose herdr name starts with one of
`quiet_done_agent_prefixes` are quiet when done, in any workspace (empty
by default). An entry that would match the names of agents started from the
phone (`pp-…`) is refused at startup. Each decision is logged at info
(`finished turn is quiet` / `finished turn pings`, with the agent id and
the reason), so `journalctl --user -u passporter-bridge` shows why a ping
did or did not go out. The same classification flags those agents `quietDone` in
the agent list, so the notification leaves their finished turns out (it
still names them when they are blocked) and triage shows their done cards
muted.


Pings go out over HTTPS to the browsers' push services (FCM, Mozilla, Apple)
and to nothing on `127.0.0.1`. The PWA subscribes through its service
worker, which also needs a secure context ([HTTPS](#https)).
UnifiedPush for the planned Android app is not built yet.

The planned Android app's UnifiedPush endpoint is a self-hosted ntfy server
(the bridge side is built). Allow it with
`ntfy_host`: that exact name only, unlike `extra_hosts`, which also admits
every subdomain. Its endpoint must still be `https` on port 443.

Blocked agents are always announced in the car. Finished turns reach the
phone's notification shade, but under the default `car_announce_done =
"decide"` Android Auto reads one aloud only when that turn's TL;DR has a
*Decide* line, which follows as a second notice (at most 15 minutes after
the turn, logged as `finished turn has a decision; notice follows`).
`"always"` announces every finished turn that pings; `"never"` keeps them
all silent in the car.

Android Auto signs in with a session of scope `car` (the same paired device
asking for `"scope": "car"`). Such a session reaches only the agent list,
pending inputs, TL;DRs, `POST /v1/agents/:id/car-reply` and car-origin
actions; everything else answers 403.

### New agents from the phone (`[spawn]`)

The phone can start a new Claude or Codex agent (the `+` beside the triage
segments), but only in folders you list. It is off until the config has a
`[spawn]` table with at least one project:

```toml
[spawn]
projects = [                         # the only folders an agent may start in
  { name = "passporter", path = "/home/you/git/passporter" },
  { name = "web", path = "/home/you/git/web" },
]
# workspace = "passporter"           # default; the herdr workspace (by label) they start in
# kinds = ["claude", "codex"]        # default; the agent kinds the phone may pick
```

Then `systemctl --user restart passporter-bridge`. Project names are short
lowercase slugs (`a-z`, `0-9`, `-`, at most 20 characters, starting with a
letter) and must be unique; paths must be absolute. The phone sees only the
names, never the paths.

Each new agent gets its own unfocused tab in the `workspace` (created on
first use), with its shell in the project folder. Once that shell is at its
prompt, the bridge runs the equivalent of
`herdr agent start pp-<id> --kind <kind> --pane <pane>` with **no other
arguments**: nothing the phone sends reaches a command line, and Claude
starts in its normal permission mode (never bypass-permissions). A first
prompt, typed or dictated, is sent once the agent is ready; if the agent
asks something first (Claude's folder-trust dialog, say), the prompt is not
sent and the phone says so. Trust the project folders in Claude once, on the
dev machine, to skip that dialog.

The workspace must not be one of `push.quiet_done_workspaces` (the bridge
refuses to start if it is): agents you start ping when they finish, like
your own sessions. Each device may start 6 agents per 10 minutes (refused
requests don't count). A start that takes longer than about two minutes
(a slow shell or agent) answers "still starting"; the phone keeps the
request, even if the sheet is closed, and tapping Start again repeats it
with the same request id, so it never starts a second agent. Every spawn is
in the audit log (`"kind": "spawn"`) before herdr is asked for anything.
To stop offering it, remove the `[spawn]` table.

### Runbooks from the phone (`[[runbooks]]`)

Runbooks are named operations you set up here and run from the phone, by
tap (the terminal button beside the triage segments) or by voice. They are
**never free-form shell**: the phone names a runbook and nothing else.
There are none until the config lists some:

```toml
[[runbooks]]
name = "Deploy bridge"               # what the phone shows, sends and hears
aliases = ["deploy the bridge"]      # more phrases that offer it by voice
cwd = "/home/you/git/passporter"     # absolute
argv = ["deploy/bridge/setup.sh"]    # executed directly: no shell
confirm = true                       # default; a tap asks first (speech always asks)
timeout_secs = 600                   # default; killed after this long (1 to 86400)
```

Then `systemctl --user restart passporter-bridge`. Names are up to 48
characters, with no `/` and not only dots. Names and aliases must sound
different: the bridge refuses two that are equal once case, punctuation
and filler words are ignored (`Deploy bridge` and `deploy-bridge, please`
clash), and one that has no words left. At most 32 runbooks, 8 aliases
each.

- **What runs**: `argv` exactly, with `argv[0]` looked up on `PATH`, or
  taken relative to `cwd` when it holds a `/` (`deploy/bridge/setup.sh`).
  No shell parses it, so there is no globbing, `$VAR`, `~`, pipes or `&&`:
  for those, point `argv` at a script in the repo. Nothing from the phone
  or from speech ever reaches it; there are no parameters.
- **Where**: each run gets its own unfocused tab, labelled with the
  runbook's name, in the spawn workspace (`[spawn] workspace`, default
  `passporter`; it works without a `[spawn]` table), its shell in `cwd`.
  The bridge writes the run's spec to `~/.local/state/passporter/runbooks/`
  (`0600`) and types one line into that shell, ` passporter-bridge runbook
  <spec>`, which runs `argv` with its output in the pane like any other.
  The leading space keeps it out of shell history where the shell allows
  (`HISTCONTROL=ignorespace`, zsh `hist_ignore_space`). The bridge waits
  (up to 5 s) for the fresh shell's prompt to settle, then types the line
  once; it never types it again, since a second copy left in a slow
  shell's input would be read by the runbook's program. The runner runs
  a spec at most once: it claims the run's status file exclusively
  before anything starts. If no runner has claimed the run after 30 s
  (the shell dropped the line, say), the bridge claims it as failed and
  a late runner does nothing; run the runbook again.
- **Restarts**: the run lives in your shell, outside the bridge's service
  sandbox, so a runbook may rebuild and restart the bridge itself, as
  `deploy/bridge/setup.sh` does. The run's files in
  `~/.local/state/passporter/runbooks/` outlive the bridge: the restarted
  bridge, before it serves anything, picks up runs still going (the
  runbook stays busy, and the phone sees how the run ends), writes the
  audit `finish` line of a run that ended while no bridge was watching
  (exactly once), gives up on a run whose runner never started (it never
  types into that pane again: herdr may have reused it), shows each
  runbook's latest run, and answers a retry of a request it has
  forgotten with that run instead of running it again. Files of finished
  runs are removed after 7 days.
- **Interrupted runs**: Ctrl+C in the tab, or closing it, stops the run
  as usual. The bridge notices at once that the runner is gone and
  reports the run as failed.
- **The phone** lists each runbook with its exact command. A tap asks
  first (unless `confirm = false`), showing the exact `argv` and `cwd`.
  On an agent screen, a dictated utterance that is exactly a runbook's
  name or alias, ignoring case, punctuation and filler words ("um",
  "uh", "please", "okay", "so"), offers that runbook behind the same
  confirm instead of being sent; anything else is dictated text as usual.
  The sheet follows the run: running, then the exit code, with the
  pane's last lines.
- **Limits**: one run per runbook at a time (a second request is refused
  while one runs); 10 runs per device per 10 minutes (refused requests,
  and requests that ran nothing because herdr or the audit log was down,
  don't count); never from Android Auto. After `timeout_secs` the program
  is killed (`SIGKILL`; processes it started are not followed). Every run
  is in the audit log (`"kind": "runbook"`, with the exact `argv` and
  `cwd`) before herdr is asked for anything, and again when it ends
  (`"phase": "finish"`, with the exit code).

To stop offering a runbook, remove its table and restart the bridge.

### Commands from the agent's message (terminal mode)

When an agent asks you to run something ("run `! gcloud auth login` so the
output lands here", "in another terminal, run `npm run dev`"), the agent
screen shows a **Run** strip above the composer: a chip per command in the
agent's latest message, in order, with `!` on the ones it wants in its
session and `new` on the ones it wants in a terminal of their own. Every
candidate is listed, so when the agent offers "this, or this" you see both
and pick. Nothing to configure: it works for Claude and Codex agents
whenever the bridge can read their transcript.

- **What counts as a command** (fixed rules in the bridge, no model): a
  fenced code block labelled with a shell (`bash`, `sh`, `zsh`, `console`,
  …; never `rust`, `json`, `text`, …), or unlabelled when every line reads
  like a shell command; an inline code span the sentence asks you to run
  ("run", "type", "paste", … but not "I ran"), or that is `!`-prefixed; a
  line of prose starting with `! `. Paths, identifiers and code in other
  languages are left out. ANSI escapes and hidden characters are removed,
  so what the chip shows is what runs. A candidate is never cut: one over
  2000 characters or 40 lines is left out (and counted).
- **Running one** takes a second tap: the confirm shows the exact command
  and offers **Run here (!)** and **Run in new terminal**, the one the
  agent asked for first.
  - *Here* types `!<command>` into the agent's prompt; Claude Code and Codex
    both run a `!` line as a shell command in their session, so the agent
    sees the output. Only single-line commands, and only while the agent
    is at its prompt.
  - *New terminal* opens a tab labelled `run: <agent>` in the spawn
    workspace (`[spawn] workspace`, default `passporter`; no `[spawn]`
    table needed), its shell in the agent's folder, and types the command
    there. **Watch** then shows the tab's last lines, with Ctrl+C, Enter,
    Esc and the arrow keys; the tab stays in herdr when you're done.
- **Checks**: the phone names the command by id and hash, never by its
  text. The bridge reads the agent's latest message again and runs nothing
  if it changed (409; pick again from the fresh strip). Never from Android
  Auto. 20 runs per device per 10 minutes, 60 key sends per minute. Every
  run is in the audit log (`"kind": "command"`, with the exact command)
  before anything is typed. These are commands an agent suggested, run as
  you: read the confirm before you tap (docs/security.md).

### Checking a deployment: `passporter doctor`

```sh
passporter doctor [--config-dir <dir>]
```

A read-only self-check: it reads files and their modes, pings herdr, opens
and drops a TCP connection to whisper-server, and asks `systemctl --user`.
Every probe is bounded by a 3-second timeout, and they run concurrently. It changes nothing and never prints a secret (the token is only checked for
presence; key files are loaded, not shown). Each line is `ok`, `off` (an
optional feature not configured), `warn` (works, but an MVP compromise or a
trap) or `FAIL`, and everything but `ok` comes with a `fix:` line. It exits
non-zero if any check fails. The report opens with the build of the
`passporter` binary itself (commit and build time).

| Check | Passes when |
|---|---|
| config dir / config file | `~/.config/passporter` is `0700`, `config.toml` `0600` |
| config | `config.toml` parses and validates (errors never quote the file) |
| state dir | the state dir is `0700` and listable, and nothing in it (nor the audit log, wherever it is configured) is open to group or others. Under the unit, both must also be inside its `ReadWritePaths`, and the dir must already exist: with `ProtectSystem=strict` the bridge can neither write elsewhere nor create it |
| listen | plain HTTP on loopback; HTTPS with `trusted_proxy` set (`warn` without it: behind a proxy every client would look like the proxy; `warn` on a loopback address, which a proxy on another host cannot reach); or the WireGuard opt-in with `listen`'s IP allowlisted (`warn`: MVP mode) |
| tls | without `[tls]`: `ok` on loopback with `trusted_proxy` set (HTTPS ends at the local proxy), else `warn` (phones cannot pair). With `[tls]`: its key is `0600` and matches the leaf, the leaf's SAN covers `tls.name` (default `passporter.internal`, the name the proxy verifies), it is already valid (`notBefore`) and more than 14 days from expiry (`warn` inside 14 days, `FAIL` once expired) |
| auth | pairing only (`warn` when the static token is accepted, also behind HTTPS in a build whose `STATIC_TOKEN_BEHIND_HTTPS` only warns, or when pairing is the only way in but the PWA is on plain HTTP and cannot pair) |
| herdr | herdr answers a ping on `herdr_socket`, else herdr's default socket under `HOME`. A `HERDR_SOCKET_PATH` in doctor's own shell (set inside herdr panes) is ignored and named, because the unit does not see it |
| whisper | something accepts connections on `audio.whisper` (default `127.0.0.1:8178`) |
| push | `off` without `[push]`; otherwise the VAPID key loads and is `0600` |
| systemd unit | `passporter-bridge.service` is enabled and active, and runs the config dir being checked. `warn` while it is starting, stopping or reloading, when it is not installed (fine for a bridge run another way), or when it runs a different `--config-dir` |

Run it as the bridge's user, in a session with a systemd user manager. A
healthy quick-start deployment reports no failures; whisper and push show
`off` until you set them up. An MVP-mode deployment warns about the listen
address, the static token and the missing HTTPS.

## Photo attachments

The composer's camera button sends a photo with a prompt (a screenshot of
an error, a board on the bench). Nothing to configure: the defaults are
fixed in `AttachmentSettings` (`crates/bridge/src/attachments/mod.rs`).

### How a photo flows

1. The phone shrinks the photo to a 2048 px long edge and re-encodes it as
   JPEG (quality 0.85), drawing the thumbnail as it goes. The upload starts
   as soon as the photo is picked, while the user types.
2. `POST /v1/attachments`, the raw image as the body, `Content-Type`
   `image/jpeg`, `image/png` or `image/webp`, and `Idempotency-Key: <uuid>`
   (the phone's id for the photo). The bridge sniffs the bytes (anything
   that is not really a JPEG, PNG or WebP is `415`, whatever it claimed),
   decodes them under size limits (`422` if they don't decode), applies the
   EXIF orientation, shrinks anything over 4096 px and encodes the pixels
   afresh: JPEG for photos, PNG for PNGs and anything with transparency.
   EXIF, GPS, XMP, ICC profiles and comments are never written. Answers
   `201` with `{attachmentId, mediaType, bytes, width, height}`; the same
   upload again is `200` with the same answer; another device's id, or
   other bytes under the same id, `409`.
3. Send queues the prompt in the outbox as usual, with
   `"attachments": ["<uuid>"]` in its payload. The bridge checks every id
   belongs to the sending device and still exists (`422`
   `unknown_attachment` otherwise), audits the prompt (the attachment's id,
   type and size; never its path or pixels) and types the text followed by
   one line per photo:

   ```text
   What is this error?

   Attached image: @/home/you/.local/state/passporter/attachments/<uuid>.jpg
   ```

### How agents see the photo

The path is absolute and the files are `0600`, owned by the bridge's user,
so an agent must run as that user (as herdr's agents do) to read them.

- **Claude Code** gets an `@` mention (`@/abs/path.jpg`). Claude Code
  attaches a mentioned image to the message itself: no tool call, and no
  permission prompt even though the file is outside the project
  (checked with Claude Code 2.1.317: `claude -p` described an image
  mentioned this way with the Read tool disallowed). A path containing
  whitespace (an unusual `state_dir`) cannot be mentioned; it is sent plain,
  and Claude then asks to read it with its Read tool, which shows on the
  phone as a permission dialog.
- **Codex** (and any other agent) gets the plain path. Codex 0.166 opens it
  with its `view_image` tool (stable, on by default); its sandbox reads the
  whole disk in every mode, so the state dir needs no extra permission
  (checked with `codex exec`: the rollout shows the `view_image` call).

Nothing is placed in the agent's repository, so attachments never show up
in `git status` and a hostile repo cannot redirect the bridge's writes
with a symlink.

### Limits and retention

- 10 MiB per upload (`413` beyond it), two minutes to send it;
- 30 uploads per device per 10 minutes (`429` with `Retry-After`; replays
  are free);
- at most 512 stored at once (`507` beyond it);
- up to 4 attachments per prompt; prompts with attachments are refused from
  the car (`403` `car_policy`);
- each attachment is deleted 7 days after upload by a sweep that runs every
  10 minutes; a prompt naming it after that is refused (`unknown_attachment`),
  and the phone says to attach it again.

There is no way to read an attachment back over the API.

## Transcription (whisper-server)

Dictation (design §6.3) is transcribed on the dev machine by whisper.cpp's
`whisper-server`. The bridge is its only client and talks to it over
loopback; audio never leaves the machine. It is optional: without it, the
phone can still type.

For reference: on a mid-range NVIDIA GPU (12 GB, CUDA, whisper.cpp v1.7.6,
`large-v3-turbo`), the 11 s `samples/jfk.wav` clip, re-encoded to 32 kbit/s
WebM/Opus as the phone sends it, transcribes word-perfect in 0.44 s end to
end through `/inference` with `--convert` (about 25× real time; 1.6 GB of
VRAM). On a CPU, pick a smaller model (`base.en`, `small.en`).

### Prerequisites (one-time, by hand)

`build.sh` checks for these and stops, with install hints for Arch,
Debian/Ubuntu and Fedora, if any is missing:

- `cmake`, a C++ compiler (`g++`), `git`, `curl` (or `wget`);
- `ffmpeg` (whisper-server's `--convert` uses it to decode the phone's
  WebM/Opus or AAC);
- for a GPU build: the CUDA toolkit (`nvcc`) on top of a working NVIDIA
  driver. Without `nvcc` the script builds for the CPU.

### Build and install

```sh
deploy/whisper/build.sh                       # CUDA if nvcc is found, else CPU
WHISPER_BACKEND=cpu deploy/whisper/build.sh   # force a CPU build
```

It clones whisper.cpp (tag `WHISPER_REF`, default `v1.7.6`, which must
still name its pinned commit) into `~/.local/src/whisper.cpp`, builds `whisper-server` statically (for the GPU
in this machine with CUDA, `CUDA_ARCH=native`), installs it to
`~/.local/bin`, downloads the model to `~/.local/share/passporter/whisper/`
(linked as `model.bin`; `large-v3-turbo` with CUDA, `base.en` on the CPU)
and checks its SHA-256 against the one Hugging Face publishes, and enables `whisper-server.service`. Overrides: `WHISPER_MODEL=small.en`,
`CUDA_ARCH=86` (an RTX 30xx; `89` for an RTX 40xx; needed when building on a
machine without the target GPU), `WHISPER_REF=<tag>` (with
`WHISPER_COMMIT=<sha>` to pin it; otherwise it is built unpinned, with a
warning), `WHISPER_MODEL_SHA256=<sha256>` (known for `large-v3-turbo`,
`base.en` and `small.en`; another model without it is installed unverified,
with a warning).

At boot, the unit waits for the NVIDIA device nodes before starting (the
user manager can otherwise start it before the driver is ready, and
whisper.cpp would quietly transcribe on the CPU until the next restart). It
waits up to 60 s, then starts anyway, on the CPU if the GPU never appeared;
on a machine without an NVIDIA GPU it doesn't wait. If `journalctl --user -u
whisper-server` shows "no GPU found" on a GPU machine, `systemctl --user
restart whisper-server` puts it back on the GPU.

The unit passes `--language en`, which skips language detection; edit
`deploy/whisper/whisper-server.service` to `--language auto` (or your
language) to dictate in other languages.

Check it:

```sh
journalctl --user -u whisper-server -f      # look for the device and "listening"
curl -s 127.0.0.1:8178/inference -F file=@clip.wav -F response_format=json
```

### Bridge settings

The `[audio]` table in the bridge config; every key is optional:

```toml
[audio]
whisper = "127.0.0.1:8178"   # must be loopback; the bridge refuses anything else
retention_hours = 168        # keep transcribed audio this long; 0 deletes it once transcribed
max_upload_mib = 25          # per clip
max_duration_secs = 300      # per clip
```

Clips live in `<state_dir>/audio/` (`~/.local/state/passporter/audio/`,
dir `0700`, files `0600`), already writable under the bridge unit's
`ReadWritePaths`.

### How a clip flows

1. `POST /v1/audio` `{"audioId"?, "totalBytes", "durationMs"}` declares the
   clip; over a cap is `413`/`422`. The phone should send its own `audioId`
   (a UUID made at record time): declaring it again returns the current
   offset instead of starting over.
2. `PATCH /v1/audio/:id` with `Upload-Offset: <n>` and up to 1 MiB of raw
   bytes appends a chunk. A chunk at the wrong offset is a `409` whose body
   carries the offset to resume from; `GET /v1/audio/:id` reports it too.
3. The last byte queues the clip; one worker sends clips to whisper-server
   in order.
4. `GET /v1/audio/:id/transcript`: `202` while pending, `200` with the
   `Transcript`, or `422` if transcription failed.

Uploads and the queue survive a bridge restart. While whisper-server is
not listening or answers 503 (restarting, loading its model after a boot)
the worker retries for about four minutes before failing the clip. A
connection that breaks mid-request is not retried: the clip may be what
crashed the server. Chunks are written at their offset, so a chunk the
bridge could not store (or a last chunk whose completion it could not
record) is simply sent again by the phone and overwrites whatever part of
it landed. Each chunk may take up to two minutes (a full 1 MiB chunk at
64 kbit/s); every other request gets 30 seconds. Records the bridge cannot
read at startup are renamed `<id>.json.unreadable` (with their audio;
numbered if that name is taken), never deleted, and their `audioId` is
refused with `409` while those files exist. Declaring a failed clip again
(same `audioId`, size and length) queues it again (its audio is kept for
that), or, if its audio is lost, restarts its upload at offset 0. Each
transcription may take up to a minute plus twice the duration cap.
At most 64 clips may be in flight (uploading, queued, transcribing) at
once; at the cap, a new clip evicts the upload that has gone longest
without a chunk (at least 10 minutes), or gets a `503`.

### Retention

A sweep runs every 10 minutes:

- a transcribed clip's audio is deleted `retention_hours` after it finished
  (kept for debugging; with `0`, as soon as it is transcribed);
- its record and transcript go after the same time, but never sooner than
  24 hours, so a phone that was offline can still fetch the transcript;
- a failed clip keeps its audio, for a retry, until its record goes;
- a clip whose files cannot be deleted stays known (its id is not reused)
  and the next sweep tries again;
- an upload untouched for that long is abandoned and deleted;
- queued clips are never swept.

A transcript the user sends becomes a `prompt` action, audited like any
other.

