//! Agent session transcripts as Passporter threads (design §4).
//!
//! Claude Code appends every session to
//! `~/.claude/projects/<cwd-slug>/<session>.jsonl`; Codex CLI to
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<start>-<session>.jsonl`. The
//! transcript source is chosen by agent kind ([`Format`]). This crate:
//!
//! 1. finds that file from an agent's cwd and `agent_session` id
//!    ([`Locator`], [`SessionId`]; [`CodexLocator`] for Codex, which can
//!    also pick the latest session in a cwd), or, for an agent herdr
//!    reports without a session id, matches it by folder ([`FolderLookup`]);
//! 2. follows it as it grows, one complete line at a time, surviving
//!    truncation and replacement with bounded memory ([`Tailer`]);
//! 3. normalizes records into protocol [`ThreadItem`](protocol::ThreadItem)s:
//!    user and assistant messages, tool calls with their results, and diffs
//!    from file edits (Codex: `apply_patch`), segmented into turns
//!    ([`Thread`]);
//! 4. serves them by cursor for `GET /v1/agents/:id/thread?since=`
//!    ([`Thread::since`], [`Thread::resume_cursor`]).
//!
//! [`TranscriptReader`] ties 2–4 together for one file.
//!
//! # Robustness
//!
//! Transcripts are untrusted (design §8) and their format is undocumented
//! and changes between Claude Code releases. Reading never fails on content:
//! unknown record types, unknown fields, odd shapes and invalid JSON are
//! skipped and counted in [`Diagnostics`]. Only I/O and caller mistakes are
//! [`Error`]s. Rendered strings are size-capped ([`Limits`]) and remain
//! untrusted text for the UI to escape.

mod codex_locate;
mod diagnostics;
mod diff;
mod error;
mod folder;
mod locate;
mod reader;
mod record;
mod render;
mod session;
mod tail;
mod thread;
mod time;

pub use codex_locate::{CodexLocator, CodexLookup, CodexSession};
pub use diagnostics::{Diagnostics, INVALID_JSON, NO_TYPE, OTHER};
pub use error::{Error, Result};
pub use folder::{DEFAULT_MAX_AGE, FolderLookup, Matched};
pub use locate::{DEFAULT_SCAN_INTERVAL, Locator, Lookup, project_slug};
pub use reader::{Refresh, TranscriptReader};
pub use render::Limits;
pub use session::{InvalidSessionId, SessionId};
pub use tail::{DEFAULT_MAX_LINE, LineSink, Poll, Tailer};
pub use thread::{Format, PARSER_VERSION, Scope, Thread, Window};
