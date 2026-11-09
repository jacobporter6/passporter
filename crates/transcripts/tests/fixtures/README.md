# Transcript fixtures

| File | What |
|---|---|
| `session.jsonl`, `session.rest` | A synthetic Claude Code 2.1.x session, with a partial last line and its rest, for incremental reads. |
| `realistic.jsonl` | A synthetic session with **every record kind the scrubbed corpus shows** (counted by kind only, never copied): slash-command caveats and output, a long turn (over 1500 words, six files edited, a failed tool), `/compact`'s boundary and summary, a queued prompt, a subagent's report (`origin.kind = peer`), task notifications (direct and queued), a `!` command with its output, injected reminders and session noise. The e2e (`e2e/src/lib.rs`) serves it as the finished agent's transcript. |
| `codex/` | Real Codex 0.166 rollouts, scrubbed. |

When the corpus shows a new record kind (`tests/corpus.rs` reports unknown
`origin.kind`s and record types), add one to `realistic.jsonl` with the
shape it has in the corpus and synthetic text.
