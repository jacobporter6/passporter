# Codex fixtures (codex-cli 0.166.0, captured 2026-11-08)

Real rollouts from `~/.codex/sessions`, captured by the circus master in a
scratch directory with throwaway prompts (no personal content). Long strings
(system instructions, tool schemas) are trimmed to 240 characters, the scratch
path is `/work/codex-probe` and the home directory `/home/user`.

- `exec_tool_call.jsonl`: `codex exec`, one shell tool call and a final answer.
- `interactive_declined_approval.jsonl`: the TUI with `-a on-request -s read-only`;
  the model asks to run `printf 'hi\n' > hello.txt`, the approval is declined
  (Esc), the turn is aborted.

Matching screens are in `crates/bridge/src/pending/fixtures/codex_0_166_*.txt`:
the command approval dialog, the folder-trust dialog and the screen after
declining.
