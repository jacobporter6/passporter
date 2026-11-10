# Security policy

## What Passporter is, in security terms

Passporter is a **single-user remote-control channel into your coding
agents**. A paired phone can type prompts and keys into agents running in
herdr, start new agents in allowlisted folders and run configured
runbooks. All of that executes **as the user who runs herdr and the
bridge**. Treat it as remote code execution with a nice UI.

- **Never expose the bridge without HTTPS and device pairing.** The bridge
  listens on `127.0.0.1` by default; put an HTTPS reverse proxy in front of
  it (Tailscale Serve, Caddy) and prefer a name reachable only over a VPN.
- The insecure MVP mode (plain HTTP plus a static bearer token, for a phone
  that reaches the machine only over WireGuard) is off by default and
  exists for that narrow case. Avoid it; the bridge refuses the static
  token once `[tls]` or `trusted_proxy` is configured. It listens only on
  the single address you allowlist (never a wildcard), but the bridge
  cannot tell whether that address is a WireGuard interface: making sure
  only the tunnel reaches it is up to you.
- There is one user. Every paired device has the same authority. There is
  no multi-tenant isolation, and none is planned.

## Data that leaves your machine

- **TL;DRs, headlines, status lines and reply suggestions** send slices of
  your agents' transcripts to Anthropic, by running the `claude` CLI
  (`claude -p`) under **your own Claude Code login**, with no tools. This
  is the same data flow as using Claude Code itself. Disable it with
  `[tldr] enabled = false`; without a `claude` executable, nothing is sent.
- **Web Push** goes through your browser vendor's push service. The PWA's
  subscriptions (the default) get pings: an opaque random id, encrypted,
  and no agent content. Subscriptions that opt in to notices (the native
  app) also carry short agent content, such as agent names, a dialog's
  question and a *Decide* line, encrypted end to end (RFC 8291), so the
  push service sees only ciphertext.
- **Dictation** is transcribed by a local whisper.cpp server on loopback.
  Audio never leaves the machine.

## Scope

In scope (please report):

- authentication or pairing bypasses: getting a session without a valid
  pairing code or device key, or keeping one after revocation;
- anything that lets a client run a command, path or argument that is not
  in the operator's config (`[spawn]` allowlist, `[[runbooks]]`), or send
  keys or prompts to an agent in a way the protocol policy should refuse;
- prompt injection from agent output that makes Passporter itself act
  (send, choose an option, run a runbook) without the user's tap, or that
  breaks out of the sanitized rendering (script execution in the PWA, CSP
  bypass, hidden characters that change what a user approves);
- secrets leaking into logs, the audit log, error messages, `passporter
  doctor` output or the PWA's storage beyond what docs/security.md
  documents;
- file permission or path handling bugs in the bridge's config, state or
  upload directories;
- the bridge's defaults being unsafe for a new install.

Out of scope:

- anything that requires an already paired device or access to the user
  account that runs the bridge (that is the intended authority), except
  bypassing the step-up or car policy rules the protocol defines;
- the known gaps listed in [docs/security.md](docs/security.md#deferred-and-known-gaps)
  (for example: no WebAuthn step-up yet, the static token's permanence in
  insecure MVP mode, no app lock), unless you find a way to make them worse
  than documented;
- deployments that ignore the docs: plain HTTP without the explicit opt-in
  is refused by the bridge, and exposing it publicly without HTTPS is not
  supported;
- vulnerabilities in herdr, Claude Code, Codex, whisper.cpp, Tailscale or
  your reverse proxy (report those upstream);
- the behaviour of the coding agents themselves.

## Supported versions

Only the latest commit on `main` is supported. There are no release
branches yet.

## Reporting a vulnerability

Please report privately; do not open a public issue.

- Preferred: GitHub's private vulnerability reporting ("Report a
  vulnerability" under the repository's Security tab).
- Or email: info@portemporal.dev.

Include what you found, how to reproduce it, and the impact you expect.
This is a personal project maintained on a best-effort basis: expect an
acknowledgement within a week. Please give a reasonable time to fix before
disclosing publicly; we will credit you unless you prefer otherwise.
