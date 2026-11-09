//! Errors from producing a TL;DR or suggestions.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a summary or suggestion request failed. Nothing is cached on error,
/// so a later request tries again.
///
/// `Clone` (I/O errors are shared behind [`Arc`]) so that one failed
/// generation can be reported to every caller waiting on it.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The summarizer executable could not be started.
    #[error("failed to start summarizer {program}: {source}")]
    Spawn {
        /// The executable that was run.
        program: PathBuf,
        /// The underlying error.
        #[source]
        source: Arc<io::Error>,
    },
    /// The fresh temporary working directory could not be created.
    #[error("failed to create the summarizer's temporary working directory: {0}")]
    TempDir(#[source] Arc<io::Error>),
    /// Writing the input to, or reading the output from, the process failed.
    #[error("i/o with the summarizer process failed: {0}")]
    Io(#[source] Arc<io::Error>),
    /// The run took longer than the configured timeout. The process was
    /// killed.
    #[error("summarizer timed out after {0:?}; the process was killed")]
    Timeout(Duration),
    /// The process wrote more output than allowed; it is not parsed.
    #[error("summarizer output exceeded {limit} bytes")]
    OutputTooLarge {
        /// The output limit, in bytes.
        limit: usize,
    },
    /// The account's usage or rate limit was hit. Back off before trying
    /// again: retrying now fails the same way.
    #[error("summarizer usage limit reached: {message}")]
    UsageLimit {
        /// What the CLI said, truncated.
        message: String,
    },
    /// The CLI is not logged in, or its login expired.
    #[error("summarizer is not authenticated: {message}")]
    Auth {
        /// What the CLI said, truncated.
        message: String,
    },
    /// The process exited unsuccessfully without a parseable result.
    #[error("summarizer exited with {status}: {stderr}")]
    Exit {
        /// The exit status.
        status: ExitStatus,
        /// The start of its stderr.
        stderr: String,
    },
    /// The CLI ran but reported an error result (API error, structured
    /// output retries exhausted, ...).
    #[error("summarizer run failed: {message}")]
    Model {
        /// What the CLI said, truncated.
        message: String,
    },
    /// The run succeeded but produced no structured output, e.g. because
    /// the model declined.
    #[error("summarizer returned no structured output")]
    NoStructuredOutput,
    /// The output did not match the expected schema or failed validation.
    #[error("summarizer output is invalid: {0}")]
    InvalidOutput(String),
    /// There was nothing to summarize.
    #[error("nothing to summarize: the input is empty")]
    EmptyInput,
}

impl Error {
    /// An [`Error::Io`] from `error`.
    pub(crate) fn io(error: io::Error) -> Self {
        Self::Io(Arc::new(error))
    }

    /// Whether the account's usage limit was hit, so the caller should back
    /// off instead of retrying.
    #[must_use]
    pub fn is_usage_limit(&self) -> bool {
        matches!(self, Self::UsageLimit { .. })
    }
}
