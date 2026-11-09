# Recorded herdr replies

Captured from herdr 0.9.1 with the read-only CLI and scrubbed before
committing; `tests/contract.rs` replays them through a real socket.

| File | Command |
|---|---|
| `agent-list.json` | `herdr agent list` |
| `pane-list.json` | `herdr pane list` |
| `workspace-list.json` | `herdr workspace list` |

Scrubbing replaces home paths with `/home/dev`, project names, workspace
labels and terminal titles with neutral ones, and session ids with
`0c0f0000-…` ids. It keeps the structure exactly: which fields are present
or missing (no `agent_session` on some agents, no `title` at all), ids,
statuses, sequence numbers and the fields the client ignores. Re-record
when herdr is upgraded and keep the swarm workspace (`agent-swarm`)
and an `impl-` agent in it.
