# Transcript corpus

Passporter's transcript reader, pending-input detection and TL;DR prompts are
tested against real Claude Code history: optionally yours, on your own
machine. Real data is needed to get the parsing right. Synthetic transcripts
miss the messy shapes that real usage produces. Nothing in this page is
needed to build or test Passporter; the corpus tests skip without it.

## Where it lives

On your own machine only, outside the repo:

```
~/.local/share/passporter/corpus/        (mode 700)
  raw/        immutable .tar.zst dumps of ~/.claude/projects (one per machine), mode 600
  scrubbed/   best-effort secret-scrubbed copy, derived from raw/ (plain .jsonl)
    <dump>/projects/<slug>/<session>.jsonl          one directory per raw dump
    <dump>/projects/<slug>/<session>/subagents/...  subagent transcripts
    redaction-report.txt                            counts per rule, never values
```

- `raw/` is read only by the scrub step, run interactively. **Coding agents
  should never read `raw/`**: give them a deny rule for it.
- `scrubbed/` is what tests and agents use. It's still personal data: keep it
  out of the repo, out of PR bodies and out of logs.
- `raw/` is stored compressed (zstd). `scrubbed/` is plain `.jsonl`, written
  `0700`/`0600`, so tests can read it directly.
- The scrubber redacts provider tokens (GitHub, GitLab, Anthropic, OpenAI,
  AWS, Google, Slack, Stripe, Hugging Face, npm, Tailscale, Docker, SendGrid,
  Telegram, Discord webhooks, Azure keys, JWTs, private keys), auth headers
  and cookies, URL and CLI credentials (`--password`, `mysql -p…`, `sshpass`,
  `docker login -p`, `curl -u`, `.netrc`), password and `.env`/compose/YAML
  assignments, high-entropy values next to key-like names, and everything
  under a secret-looking JSON key or a secret-named `{name, value}` pair.
  JSON nested or escaped inside strings is unpacked and scanned too, and
  object keys go through the rules.
- **Redacted once, redacted everywhere:** each file gets a second pass that
  replaces every other occurrence of a value redacted in it, if that value
  looks like a secret (recognised by its shape, or 8+ mixed characters with
  real entropy; never plain words) and only where it stands as a whole
  token, so a secret recognisable in one copy (a parsed config) is
  also removed from copies no rule could recognise (the same file,
  line-numbered, in a `Read` result).
- Known gap: a secret split across separate JSON strings is invisible, and
  one that no rule recognises anywhere in its file survives. It is best
  effort: treat `scrubbed/` as personal data regardless.

## Tests

| Kind | Data | Where it runs |
|---|---|---|
| Unit tests (`#[cfg(test)]`) | Small excerpts from `scrubbed/` (hand-checked) or spoofed records, committed next to the code | Everywhere |
| Corpus acceptance tests (`tests/corpus*.rs`) | Every transcript under `$PASSPORTER_CORPUS` | Only where a corpus exists; each test returns early with a note when `PASSPORTER_CORPUS` is unset |

```bash
PASSPORTER_CORPUS=~/.local/share/passporter/corpus/scrubbed cargo nextest run --workspace
```

## Adding data

Dump `~/.claude/projects` into `raw/` (from another machine, stream it over
ssh instead):

```bash
tar -C ~/.claude -cf - projects | zstd -19 -T0 \
  > ~/.local/share/passporter/corpus/raw/$(hostname)-claude-projects-$(date +%F).tar.zst
```

Then re-run the scrub step to regenerate `scrubbed/` (release build; a debug
build is much slower):

```bash
just scrub-corpus   # defaults: --raw ~/.local/share/passporter/corpus/raw --out .../scrubbed
```
