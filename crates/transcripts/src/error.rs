//! Errors from locating and reading transcripts.

use std::io;
use std::path::PathBuf;

use crate::session::InvalidSessionId;

/// Everything that can go wrong while locating or tailing a transcript.
///
/// Malformed or unexpected *content* is never an error: it is skipped and
/// counted in [`Diagnostics`](crate::Diagnostics). Errors are reserved for
/// the environment (I/O, configuration, bad caller input).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The session id could escape the projects directory or is not an id.
    #[error(transparent)]
    InvalidSessionId(#[from] InvalidSessionId),

    /// Transcripts are keyed by absolute working directory.
    #[error("agent cwd must be an absolute path, got {}", path.display())]
    RelativeCwd {
        /// The offending path.
        path: PathBuf,
    },

    /// A working directory with a `..` component: not one herdr reports,
    /// so never joined onto a transcript directory.
    #[error("agent cwd must not contain `..`, got {}", path.display())]
    UnsafeCwd {
        /// The offending path.
        path: PathBuf,
    },

    /// Neither `CLAUDE_CONFIG_DIR` nor `HOME` is set.
    #[error("cannot find Claude's config dir: neither CLAUDE_CONFIG_DIR nor HOME is set")]
    NoConfigDir,

    /// The transcript path names something other than a regular file (a
    /// directory, FIFO or device). Symlinks fail earlier, at open.
    #[error("{} is not a regular file", path.display())]
    NotRegular {
        /// The offending path.
        path: PathBuf,
    },

    /// Reading a transcript or listing a directory failed.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What was being done, e.g. `reading`.
        action: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

impl Error {
    pub(crate) fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

/// Shorthand for results in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
