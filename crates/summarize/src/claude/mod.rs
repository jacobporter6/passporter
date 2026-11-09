//! [`ClaudeCli`]: the [`Summarizer`] backed by headless Claude Code.

mod invocation;
mod output;
mod process;
mod prompt;

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use protocol::Tldr;
use serde_json::Value;
use tokio::sync::Semaphore;

use self::invocation::Invocation;
use crate::digest::TurnDigest;
use crate::error::{Error, Result};
use crate::headline::{Headline, headline_schema, validate_headline};
use crate::status::{StatusText, status_schema, validate_status};
use crate::suggest::{Suggestion, SuggestionContext, parse_suggestions, suggestions_schema};
use crate::summarizer::Summarizer;
use crate::tldr::{TldrLimits, tldr_schema, validate_tldr};

/// The model (design §6.4). A full model id rather than the `haiku` alias,
/// so an alias change cannot silently switch models; the CLI documents
/// `claude --model claude-haiku-5-5` (Claude Code 2.1.293 or later).
pub const DEFAULT_MODEL: &str = "claude-haiku-5-5";

/// Default limit on one run, from spawn to exit.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);

/// Most concurrent runs allowed (design: concurrency 1–2).
const MAX_CONCURRENCY: usize = 2;

/// Settings for [`ClaudeCli`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeConfig {
    /// The `claude` executable: a path, or a name looked up on `PATH`.
    pub program: PathBuf,
    /// The model id passed to `--model`.
    pub model: String,
    /// Limit on one run; the process is killed when it expires.
    pub timeout: Duration,
    /// Concurrent runs, clamped to 1–2. Further requests wait their turn.
    pub concurrency: usize,
    /// Largest accepted stdout, in bytes; larger output is an error.
    pub max_output_bytes: usize,
    /// Caps applied to TL;DRs.
    pub tldr_limits: TldrLimits,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from("claude"),
            model: DEFAULT_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
            concurrency: 1,
            max_output_bytes: 256 * 1024,
            tldr_limits: TldrLimits::default(),
        }
    }
}

/// A [`Summarizer`] that runs `claude -p` once per request, hardened as
/// described in the [crate docs](crate): no tools, no settings or MCP, a
/// fresh empty cwd, a scrubbed environment, input on stdin as delimited
/// data, schema-constrained JSON output that is validated again here.
///
/// Its `Debug` output lists environment variable names only, never values.
pub struct ClaudeCli {
    config: ClaudeConfig,
    permits: Semaphore,
    /// The allowlisted part of the parent environment, passed to the child.
    env: Vec<(String, String)>,
}

impl fmt::Debug for ClaudeCli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env_names: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("ClaudeCli")
            .field("config", &self.config)
            .field("permits", &self.permits.available_permits())
            .field("env_names", &env_names)
            .finish()
    }
}

impl ClaudeCli {
    /// A summarizer with `config`, inheriting allowlisted variables from
    /// this process's environment.
    #[must_use]
    pub fn new(config: ClaudeConfig) -> Self {
        Self::with_parent_env(config, std::env::vars_os())
    }

    /// Like [`new`](Self::new), drawing the child's allowlisted variables
    /// from `parent_env` instead of this process's environment. Variables
    /// outside the allowlist are dropped immediately, not stored.
    #[must_use]
    pub fn with_parent_env(
        config: ClaudeConfig,
        parent_env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Self {
        let permits = Semaphore::new(config.concurrency.clamp(1, MAX_CONCURRENCY));
        Self {
            config,
            permits,
            env: invocation::allowlisted_env(parent_env),
        }
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> &ClaudeConfig {
        &self.config
    }

    fn invocation(&self, schema: &Value) -> Invocation {
        Invocation::new(
            self.config.program.clone(),
            &self.config.model,
            schema,
            prompt::SYSTEM_PROMPT,
            &self.env,
        )
    }

    /// One schema-constrained run: the structured output, unvalidated.
    async fn structured(&self, schema: &Value, task: &str, data: &str) -> Result<Value> {
        let invocation = self.invocation(schema);
        let input = prompt::wrap(task, data);
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| Error::io(std::io::Error::other("summarizer is shut down")))?;
        let finished = process::run(
            &invocation,
            input.as_bytes(),
            self.config.timeout,
            self.config.max_output_bytes,
        )
        .await?;
        output::interpret(&finished)
    }
}

impl Summarizer for ClaudeCli {
    async fn tldr(&self, turn: &TurnDigest) -> Result<Tldr> {
        if turn.is_empty() {
            return Err(Error::EmptyInput);
        }
        let limits = self.config.tldr_limits;
        let value = self
            .structured(&tldr_schema(&limits), prompt::TLDR_TASK, turn.as_str())
            .await?;
        validate_tldr(value, &limits)
    }

    async fn headline(&self, turn: &TurnDigest) -> Result<Headline> {
        if turn.is_empty() {
            return Err(Error::EmptyInput);
        }
        let value = self
            .structured(&headline_schema(), prompt::HEADLINE_TASK, turn.as_str())
            .await?;
        validate_headline(value)
    }

    async fn status(&self, turn: &TurnDigest) -> Result<StatusText> {
        if turn.is_empty() {
            return Err(Error::EmptyInput);
        }
        let value = self
            .structured(&status_schema(), prompt::STATUS_TASK, turn.as_str())
            .await?;
        validate_status(value)
    }

    async fn suggestions(&self, context: &SuggestionContext) -> Result<Vec<Suggestion>> {
        if context.is_empty() {
            return Err(Error::EmptyInput);
        }
        let value = self
            .structured(
                &suggestions_schema(),
                prompt::SUGGESTIONS_TASK,
                &context.render(),
            )
            .await?;
        parse_suggestions(value)
    }
}
