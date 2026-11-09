//! Incremental, line-oriented reading of an append-only file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Bytes read per `read` call.
const CHUNK: usize = 64 * 1024;

/// Default cap on a single line. Claude Code writes some very large records
/// (`api-request-blob`, tool results that embed whole files); anything larger
/// than this is skipped rather than buffered.
pub const DEFAULT_MAX_LINE: usize = 16 * 1024 * 1024;

/// Follows a JSONL file as it grows, yielding each complete line once.
///
/// - Reads from a byte cursor; each [`poll`](Self::poll) picks up where the
///   last one stopped, and reads at most up to the file length seen when
///   the poll started (a writer racing ahead is picked up next time).
/// - A final line without its `\n` is held back until it is complete, so a
///   record being written is never parsed half-done.
/// - If the file shrinks (truncation) or the path now names a different file
///   (rotation, delete + recreate), the tailer starts again from byte 0 and
///   reports [`Poll::reset`]. A file truncated and regrown past the old
///   cursor between two polls is indistinguishable from an append; Claude Code
///   only appends, so this is accepted.
/// - The path must name a regular file: it is opened with `O_NOFOLLOW` (a
///   symlink planted in its place is an error, never followed) and
///   `O_NONBLOCK` (a FIFO cannot hang the reader), and checked with `fstat`.
/// - Memory is bounded by `max_line` plus one read chunk, whatever the file
///   size: lines longer than `max_line` are dropped and counted.
/// - A poll that finds the size unchanged does no reads.
/// - A missing file reads as empty: a new session creates its transcript
///   lazily. A file that disappears after being read is a reset: its lines
///   are no longer valid.
#[derive(Debug)]
pub struct Tailer {
    path: PathBuf,
    open: Option<OpenFile>,
    /// Bytes read from the current file: consumed lines plus `partial`.
    pos: u64,
    /// The start of an unterminated line.
    partial: Vec<u8>,
    /// Inside an oversized line: drop bytes until the next `\n`.
    discarding: bool,
    max_line: usize,
    /// Read buffer, allocated on first use and reused.
    buf: Vec<u8>,
    /// A reset detected but not yet reported to a sink. Survives errors, so
    /// a poll that fails right after reopening still reports the reset on
    /// the next call.
    reset_pending: bool,
}

#[derive(Debug)]
struct OpenFile {
    file: File,
    /// `(device, inode)` from `fstat`: identifies the file we hold.
    identity: (u64, u64),
}

impl OpenFile {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| Error::io("opening", path, e))?;
        let meta = file
            .metadata()
            .map_err(|e| Error::io("inspecting", path, e))?;
        if !meta.is_file() {
            return Err(Error::NotRegular { path: path.into() });
        }
        Ok(Self {
            file,
            identity: (meta.dev(), meta.ino()),
        })
    }
}

/// Receives what a [`Tailer`] reads.
pub trait LineSink {
    /// One complete line, without its line ending.
    fn line(&mut self, line: &[u8]);

    /// The file was truncated or replaced; lines delivered so far are stale.
    /// Called before the first line of the new content.
    fn reset(&mut self) {}
}

impl<F: FnMut(&[u8])> LineSink for F {
    fn line(&mut self, line: &[u8]) {
        self(line);
    }
}

/// What one [`Tailer::poll`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Poll {
    /// Complete lines delivered.
    pub lines: usize,
    /// Lines dropped for exceeding the size cap.
    pub oversized: usize,
    /// The file was truncated or replaced: reading restarted at byte 0 and
    /// anything derived from earlier lines is stale.
    pub reset: bool,
}

impl Tailer {
    /// A tailer for `path`, starting at byte 0, with [`DEFAULT_MAX_LINE`].
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            open: None,
            pos: 0,
            partial: Vec::new(),
            discarding: false,
            max_line: DEFAULT_MAX_LINE,
            buf: Vec::new(),
            reset_pending: false,
        }
    }

    /// Caps the length of a single line (in bytes, excluding the `\n`).
    #[must_use]
    pub fn with_max_line(mut self, max_line: usize) -> Self {
        self.max_line = max_line;
        self
    }

    /// The file being followed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Byte offset just past the last complete line delivered.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.pos - self.partial.len() as u64
    }

    /// Reads what was appended since the last poll and passes each new
    /// complete line (without its line ending; empty lines skipped) to `sink`.
    ///
    /// On error, lines already passed to `sink` stay delivered and reading
    /// resumes after them next time.
    pub fn poll(&mut self, sink: &mut impl LineSink) -> Result<Poll> {
        let mut poll = Poll::default();
        // `lstat`: the identity of whatever the path names now, symlink or not.
        let link = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if self.open.take().is_some() {
                    self.restart();
                    self.reset_pending = true;
                }
                self.report_reset(sink, &mut poll);
                return Ok(poll);
            }
            Err(err) => return Err(Error::io("inspecting", &self.path, err)),
        };
        if self
            .open
            .as_ref()
            .is_none_or(|o| o.identity != (link.dev(), link.ino()))
        {
            if self.open.take().is_some() {
                self.restart();
                self.reset_pending = true;
            }
            self.report_reset(sink, &mut poll);
            self.open = Some(OpenFile::open(&self.path)?);
        }
        let Some(open) = self.open.as_ref() else {
            return Ok(poll);
        };
        let len = open
            .file
            .metadata()
            .map_err(|e| Error::io("inspecting", &self.path, e))?
            .len();
        if len < self.pos {
            self.restart();
            self.reset_pending = true;
        }
        self.report_reset(sink, &mut poll);
        if len == self.pos {
            return Ok(poll);
        }

        let Some(open) = self.open.as_mut() else {
            return Ok(poll);
        };
        open.file
            .seek(SeekFrom::Start(self.pos))
            .map_err(|e| Error::io("seeking", &self.path, e))?;
        if self.buf.is_empty() {
            self.buf.resize(CHUNK, 0);
        }
        let mut remaining = len - self.pos;
        while remaining > 0 {
            let want = usize::try_from(remaining).map_or(CHUNK, |r| r.min(CHUNK));
            let Some(buf) = self.buf.get_mut(..want) else {
                break;
            };
            let n = match open.file.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(Error::io("reading", &self.path, err)),
            };
            self.pos += n as u64;
            remaining = remaining.saturating_sub(n as u64);
            split_lines(
                self.buf.get(..n).unwrap_or_default(),
                &mut self.partial,
                &mut self.discarding,
                self.max_line,
                &mut poll,
                sink,
            );
        }
        Ok(poll)
    }

    /// Tells the sink about a pending reset, once.
    fn report_reset(&mut self, sink: &mut impl LineSink, poll: &mut Poll) {
        if std::mem::take(&mut self.reset_pending) {
            sink.reset();
            poll.reset = true;
        }
    }

    fn restart(&mut self) {
        self.pos = 0;
        self.partial.clear();
        self.discarding = false;
    }
}

/// Feeds one chunk through the line splitter. A free function so the borrow
/// of the read buffer and of the tailer's state stay disjoint.
fn split_lines(
    mut chunk: &[u8],
    partial: &mut Vec<u8>,
    discarding: &mut bool,
    max_line: usize,
    poll: &mut Poll,
    sink: &mut impl LineSink,
) {
    while !chunk.is_empty() {
        let newline = chunk.iter().position(|&b| b == b'\n');
        let (segment, rest) = match newline {
            Some(i) => (chunk.get(..i).unwrap_or_default(), chunk.get(i + 1..)),
            None => (chunk, None),
        };
        chunk = rest.unwrap_or_default();

        if *discarding {
            *discarding = newline.is_none();
            continue;
        }
        if newline.is_none() {
            partial.extend_from_slice(segment);
            if partial.len() > max_line {
                partial.clear();
                *discarding = true;
                poll.oversized += 1;
            }
            continue;
        }
        let line = if partial.is_empty() {
            segment
        } else {
            partial.extend_from_slice(segment);
            partial.as_slice()
        };
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() > max_line {
            poll.oversized += 1;
        } else if !line.is_empty() {
            sink.line(line);
            poll.lines += 1;
        }
        partial.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use super::*;

    fn collect(tailer: &mut Tailer) -> (Vec<String>, Poll) {
        let mut lines = Vec::new();
        let poll = tailer
            .poll(&mut |l: &[u8]| lines.push(String::from_utf8_lossy(l).into_owned()))
            .expect("poll");
        (lines, poll)
    }

    fn append(path: &Path, bytes: &str) {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(bytes.as_bytes()))
            .expect("append");
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut tailer = Tailer::new(dir.path().join("nope.jsonl"));
        assert_eq!(collect(&mut tailer), (vec![], Poll::default()));
    }

    #[test]
    fn holds_back_a_partial_final_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        append(&path, "one\r\ntwo\n\nthr");
        let mut tailer = Tailer::new(&path);
        let (lines, poll) = collect(&mut tailer);
        assert_eq!(lines, ["one", "two"]);
        assert_eq!(poll.lines, 2);
        assert_eq!(tailer.offset(), 10);

        assert_eq!(collect(&mut tailer).0, Vec::<String>::new());
        append(&path, "ee\nfour");
        assert_eq!(collect(&mut tailer).0, ["three"]);
        append(&path, "\n");
        assert_eq!(collect(&mut tailer).0, ["four"]);
        assert_eq!(tailer.offset(), 21);
    }

    #[test]
    fn restarts_after_truncation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        append(&path, "aaaa\nbbbb\n");
        let mut tailer = Tailer::new(&path);
        assert_eq!(collect(&mut tailer).0.len(), 2);

        fs::write(&path, "c\n").expect("truncate");
        let (lines, poll) = collect(&mut tailer);
        assert_eq!(lines, ["c"]);
        assert!(poll.reset);
        assert!(!collect(&mut tailer).1.reset);
    }

    #[test]
    fn restarts_after_rotation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        append(&path, "old\n");
        let mut tailer = Tailer::new(&path);
        assert_eq!(collect(&mut tailer).0, ["old"]);

        let staged = dir.path().join("staged.jsonl");
        append(&staged, "new1\nnew2\nnew3\n");
        fs::rename(&staged, &path).expect("rotate");
        let (lines, poll) = collect(&mut tailer);
        assert_eq!(lines, ["new1", "new2", "new3"]);
        assert!(poll.reset);
    }

    #[test]
    fn drops_oversized_lines_with_bounded_buffering() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        let huge = "x".repeat(3 * CHUNK);
        append(&path, &format!("small\n{huge}\nafter\n{huge}"));
        let mut tailer = Tailer::new(&path).with_max_line(1000);
        let (lines, poll) = collect(&mut tailer);
        assert_eq!(lines, ["small", "after"]);
        assert_eq!(poll.oversized, 2);
        assert!(tailer.partial.capacity() <= 1000 + CHUNK);

        append(&path, "tail\nok\n");
        assert_eq!(collect(&mut tailer).0, ["ok"]);
    }

    #[test]
    fn refuses_symlinks_and_non_regular_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("real.jsonl");
        append(&target, "secret\n");
        let link = dir.path().join("t.jsonl");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let mut tailer = Tailer::new(&link);
        let mut lines = 0;
        assert!(tailer.poll(&mut |_: &[u8]| lines += 1).is_err());
        assert_eq!(lines, 0);

        let mut dir_tailer = Tailer::new(dir.path().join(""));
        assert!(matches!(
            dir_tailer.poll(&mut |_: &[u8]| {}),
            Err(Error::NotRegular { .. })
        ));
    }

    #[test]
    fn a_deleted_file_is_a_reset() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        append(&path, "a\n");
        let mut tailer = Tailer::new(&path);
        assert_eq!(collect(&mut tailer).0, ["a"]);
        fs::remove_file(&path).expect("rm");
        let (lines, poll) = collect(&mut tailer);
        assert_eq!(lines, Vec::<String>::new());
        assert!(poll.reset, "the old lines are stale");
        assert!(tailer.open.is_none(), "the old fd is dropped");
        assert_eq!(
            collect(&mut tailer),
            (vec![], Poll::default()),
            "reported once"
        );
        append(&path, "b\n");
        assert_eq!(collect(&mut tailer).0, ["b"]);
    }

    #[test]
    fn a_reset_survives_a_failed_reopen() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        append(&path, "old\n");
        let mut tailer = Tailer::new(&path);
        assert_eq!(collect(&mut tailer).0, ["old"]);

        // Replaced by something that cannot be opened as a transcript.
        fs::remove_file(&path).expect("rm");
        fs::create_dir(&path).expect("dir in its place");
        let mut resets = 0;
        let mut sink = Counting(&mut resets);
        assert!(tailer.poll(&mut sink).is_err());
        assert_eq!(resets, 1, "reported before the failing open");

        fs::remove_dir(&path).expect("rmdir");
        append(&path, "new\n");
        assert_eq!(collect(&mut tailer).0, ["new"]);
    }

    struct Counting<'a>(&'a mut usize);

    impl LineSink for Counting<'_> {
        fn line(&mut self, _: &[u8]) {}

        fn reset(&mut self) {
            *self.0 += 1;
        }
    }

    #[test]
    fn unchanged_size_reads_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        append(&path, "a\n");
        let mut tailer = Tailer::new(&path);
        assert_eq!(collect(&mut tailer).0, ["a"]);
        assert_eq!(collect(&mut tailer), (vec![], Poll::default()));
        assert_eq!(tailer.buf.len(), CHUNK, "the buffer is kept");
    }

    #[test]
    fn a_line_split_across_chunks_is_whole() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("t.jsonl");
        let long = "y".repeat(CHUNK + 17);
        append(&path, &format!("{long}\nz\n"));
        let mut tailer = Tailer::new(&path);
        assert_eq!(collect(&mut tailer).0, [long, "z".to_owned()]);
    }
}
