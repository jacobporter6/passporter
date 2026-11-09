//! A [`Tailer`] feeding a [`Thread`]: one live transcript.

use std::path::{Path, PathBuf};

use protocol::ThreadCursor;

use crate::error::Result;
use crate::render::Limits;
use crate::tail::{LineSink, Tailer};
use crate::thread::{Format, Scope, Thread, Window};

/// Follows one transcript file and keeps its normalized [`Thread`] current.
///
/// I/O is blocking and proportional to what was appended since the last
/// [`refresh`](Self::refresh); an async caller runs it on a blocking thread.
#[derive(Debug)]
pub struct TranscriptReader {
    tailer: Tailer,
    thread: Thread,
    /// A reset seen by a refresh that then failed; reported by the next
    /// successful one.
    reset_unreported: bool,
}

/// What changed since the previous successful [`TranscriptReader::refresh`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Refresh {
    /// Cursors of items added or updated (a tool call finishing), ascending.
    /// Each change is reported exactly once, even across failed refreshes:
    /// push `thread.get(c)` for each to subscribed clients.
    pub changed: Vec<ThreadCursor>,
    /// The file was truncated or replaced and the thread rebuilt, with a new
    /// [`Thread::generation`]: clients must drop their cached thread.
    pub reset: bool,
}

impl TranscriptReader {
    /// A reader for the transcript at `path` (which may not exist yet).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, scope: Scope) -> Self {
        Self {
            tailer: Tailer::new(path),
            thread: Thread::new(scope),
            reset_unreported: false,
        }
    }

    /// Overrides the size caps for rendered text.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.thread = self.thread.with_limits(limits);
        self
    }

    /// Reads the file as `format` (Claude Code by default).
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.thread = self.thread.with_format(format);
        self
    }

    /// Overrides the thread's memory bounds.
    #[must_use]
    pub fn with_window(mut self, window: Window) -> Self {
        self.thread = self.thread.with_window(window);
        self
    }

    /// Caps the size of a single transcript line; see [`Tailer::with_max_line`].
    #[must_use]
    pub fn with_max_line(mut self, max_line: usize) -> Self {
        self.tailer = self.tailer.with_max_line(max_line);
        self
    }

    /// The transcript file.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.tailer.path()
    }

    /// The thread so far.
    #[must_use]
    pub fn thread(&self) -> &Thread {
        &self.thread
    }

    /// Reads whatever was appended since the last refresh.
    ///
    /// If reading fails part way, the lines already read stay in the thread
    /// and their changes are reported by the next successful refresh.
    pub fn refresh(&mut self) -> Result<Refresh> {
        let mut sink = Sink {
            thread: &mut self.thread,
            reset: &mut self.reset_unreported,
        };
        let poll = self.tailer.poll(&mut sink)?;
        self.thread.note_oversized(poll.oversized);
        Ok(self.drain())
    }

    /// Settles every running tool call as unknown, for when the agent's
    /// session has ended, and reports the resulting changes.
    pub fn settle_pending(&mut self) -> Refresh {
        self.thread.settle_pending();
        self.drain()
    }

    fn drain(&mut self) -> Refresh {
        Refresh {
            changed: self.thread.take_changed(),
            reset: std::mem::take(&mut self.reset_unreported),
        }
    }
}

/// Feeds tailed lines into a thread.
struct Sink<'a> {
    thread: &'a mut Thread,
    reset: &'a mut bool,
}

impl LineSink for Sink<'_> {
    fn line(&mut self, line: &[u8]) {
        self.thread.ingest(line);
    }

    fn reset(&mut self) {
        self.thread.reset();
        *self.reset = true;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_failed_refresh_loses_no_changes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        )
        .expect("write");
        let mut reader = TranscriptReader::new(&path, Scope::Main);
        // Ingest without draining, as a poll that failed after some lines would.
        let mut sink = Sink {
            thread: &mut reader.thread,
            reset: &mut reader.reset_unreported,
        };
        sink.line(b"{\"type\":\"assistant\",\"message\":{\"content\":\"partial\"}}");
        sink.reset();
        sink.line(b"{\"type\":\"assistant\",\"message\":{\"content\":\"after\"}}");

        let refresh = reader.refresh().expect("refresh");
        assert!(refresh.reset, "the reset is still reported");
        assert_eq!(refresh.changed, [ThreadCursor(2), ThreadCursor(3)]);
        assert_eq!(reader.refresh().expect("idle"), Refresh::default());
    }
}
