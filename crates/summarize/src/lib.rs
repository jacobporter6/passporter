//! TL;DR and reply suggestions for Passporter (design §6.2, §6.4).
//!
//! The bridge asks a small model four things:
//!
//! - a four-part [`Tldr`](protocol::Tldr) of a long turn, built from a
//!   compact [`TurnDigest`] of the turn's thread items (never the repo);
//! - a one-sentence [`Headline`] of every finished turn, long or short,
//!   from a smaller digest ([`DigestLimits::headline`]) so it stays cheap;
//! - a one-line [`StatusText`] of a turn still in progress (what a working
//!   agent is doing now, and what it waits on), from a digest of the
//!   turn's tail ([`DigestLimits::status`]), refreshed every few minutes;
//! - 2–3 short [`Suggestion`]s for a question or free-text request, offered
//!   as chips and never sent on their own.
//!
//! All go through the [`Summarizer`] trait. [`ClaudeCli`] implements it with
//! headless Claude Code (`claude -p`, model `claude-haiku-5-5`), so the
//! existing login is used and no API key is stored. `FakeSummarizer`
//! (feature `fake`) stands in for tests.
//!
//! # Untrusted input (design §8)
//!
//! Turn text is agent output and may carry prompt injection. Headless Claude
//! is a full agent by default, so [`ClaudeCli`] starts it with every tool
//! disabled, no settings, hooks, `CLAUDE.md`, skills, slash commands or MCP
//! servers, no session persistence, a scrubbed environment and a fresh empty
//! temporary working directory. The turn goes in on stdin inside a delimited
//! data block, the output is constrained by a JSON schema, and every string
//! that comes back is re-validated, size-capped and (for suggestions) checked
//! for command prefixes. A malicious turn can at worst produce a bad summary.
//!
//! # Caching
//!
//! [`TldrService`] caches TL;DRs per (session, turn) behind [`TldrCache`]
//! ([`MemoryCache`] for now), generates them eagerly when a long turn
//! completes ([`TldrService::on_turn_complete`]) and never runs two
//! generations for the same turn at once.

mod cache;
mod claude;
mod digest;
mod error;
mod headline;
mod status;
mod suggest;
mod summarizer;
mod text;
mod tldr;

#[cfg(any(test, feature = "fake"))]
mod fake;
#[cfg(any(test, feature = "fake"))]
pub use fake::FakeSummarizer;

pub use cache::{DEFAULT_CACHE_CAPACITY, MemoryCache, TldrCache, TldrService, TurnKey};
pub use claude::{ClaudeCli, ClaudeConfig, DEFAULT_MODEL, DEFAULT_TIMEOUT};
pub use digest::{DigestLimits, LONG_TURN_FILES, LONG_TURN_WORDS, TurnDigest, TurnStats};
pub use error::{Error, Result};
pub use headline::{Headline, MAX_HEADLINE_CHARS, MAX_HEADLINE_SCHEMA_CHARS, validate_headline};
pub use status::{
    MAX_STATUS_LINE_CHARS, MAX_STATUS_LINE_SCHEMA_CHARS, StatusText, validate_status,
};
pub use suggest::{
    MAX_SUGGESTION_CHARS, MAX_SUGGESTIONS, RejectedSuggestion, Suggestion, SuggestionContext,
};
pub use summarizer::Summarizer;
pub use tldr::{TldrLimits, validate_tldr};
