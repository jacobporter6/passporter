//! `cargo xtask scrub-corpus`: derive the scrubbed transcript corpus from
//! the raw `.tar.zst` dumps of `~/.claude/projects` (docs/corpus.md).
//!
//! Each `<raw>/<name>.tar.zst` is streamed (never unpacked to disk) and every
//! `*.jsonl` entry is written to `<out>/<name>/<entry path>`, so the
//! `projects/<slug>/<session>.jsonl` layout survives and dumps from different
//! machines never collide. Each line is parsed as JSON and every string
//! value (and key) goes through the [`rules`] (see [`value`] for JSON
//! nested in strings); lines with redactions are re-serialized (still valid
//! JSON). A line where parsing found nothing is also scanned as raw text,
//! since parsing hides some text (a duplicate key keeps only its last
//! value); only lines clean both ways are copied byte for byte. Lines that
//! are not JSON are scrubbed as plain text. Lines over [`MAX_LINE`] are
//! dropped.
//!
//! Each file then gets a second pass ([`propagate`]): every value redacted
//! anywhere in the file is replaced wherever else it appears in that file.
//!
//! Output is personal data even after scrubbing: every directory under
//! `--out` is `0700` and every file `0600`, enforced with explicit
//! `chmod`s (so a rerun into an existing tree tightens it too), and nothing
//! is written through a symlink. The report holds counts only, never
//! matched text.

mod propagate;
mod rules;
mod value;
mod zstd;

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use propagate::Secrets;
use rules::{Counts, Redactor};

/// Command-line options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Options {
    /// Directory holding the raw `*.tar.zst` dumps.
    pub(crate) raw: PathBuf,
    /// Directory to write the scrubbed tree and report into.
    pub(crate) out: PathBuf,
}

/// The name of the report written next to the scrubbed tree.
const REPORT_FILE: &str = "redaction-report.txt";

/// Longest line scrubbed; longer ones (huge API request logs) are dropped
/// rather than buffered. Above the transcript reader's own cap, so nothing
/// the reader would use is lost.
const MAX_LINE: usize = 32 * 1024 * 1024;

/// What a scrub did. Counts only.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    archives: u64,
    skipped_archives: u64,
    files: u64,
    lines: u64,
    redacted_lines: u64,
    unparsed_lines: u64,
    oversized_lines: u64,
    skipped_entries: u64,
    unsafe_entries: u64,
    /// Distinct values (per file) the propagation pass replaced.
    propagated_values: u64,
    counts: Counts,
}

/// Scrubs every dump in `opts.raw` into `opts.out`.
pub(crate) fn run(opts: &Options) -> io::Result<Report> {
    let raw = fs::canonicalize(&opts.raw).map_err(|e| context(&e, &opts.raw))?;
    let out = resolve(&opts.out)?;
    if out.starts_with(&raw) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--out must not be inside --raw (raw dumps are immutable)",
        ));
    }
    let redactor = Redactor::new().map_err(io::Error::other)?;
    let mut report = Report::default();
    let archives = list_archives(&raw, &mut report)?;
    if archives.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no *.tar.zst dumps in {}", raw.display()),
        ));
    }
    let mut out_dir = OutDir::new(out)?;

    for (name, path) in archives {
        let dest = out_dir.root.join(&name);
        // `--out` may be an ancestor of `--raw`: a dump named after the path
        // between them would land in the raw directory.
        if dest.starts_with(&raw) || raw.starts_with(&dest) {
            report.skipped_archives += 1;
            continue;
        }
        scrub_archive(&path, &dest, &mut out_dir, &redactor, &mut report)
            .map_err(|e| context(&e, &path))?;
        report.archives += 1;
    }
    let report_path = out_dir.root.join(REPORT_FILE);
    let mut file = private_file(&report_path)?;
    file.write_all(report.to_string().as_bytes())?;
    file.flush()?;
    Ok(report)
}

/// `path` made absolute with symlinks and `..` resolved, even if its tail
/// does not exist yet: the deepest existing ancestor is canonicalized and
/// the rest, which must be plain names, appended.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut existing = absolute.as_path();
    let mut tail = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(base) => {
                let mut resolved = base;
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(Component::Normal(name))) =
                    (existing.parent(), existing.components().next_back())
                else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("cannot resolve {}", path.display()),
                    ));
                };
                tail.push(name.to_owned());
                existing = parent;
            }
            Err(err) => return Err(context(&err, existing)),
        }
    }
}

/// `(stem, path)` of each `*.tar.zst` regular file with a safe stem, sorted.
fn list_archives(raw: &Path, report: &mut Report) -> io::Result<Vec<(String, PathBuf)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(raw).map_err(|e| context(&e, raw))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".tar.zst")) else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        if is_safe_stem(stem) {
            found.push((stem.to_owned(), entry.path()));
        } else {
            report.skipped_archives += 1;
        }
    }
    found.sort();
    Ok(found)
}

/// A dump name usable as one directory under `--out`: letters, digits, `-`,
/// `_` and `.`, not starting with a dot (so never `.` or `..`).
fn is_safe_stem(stem: &str) -> bool {
    !stem.is_empty()
        && !stem.starts_with('.')
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn scrub_archive(
    path: &Path,
    dest: &Path,
    out_dir: &mut OutDir,
    redactor: &Redactor,
    report: &mut Report,
) -> io::Result<()> {
    let file = File::open(path)?;
    let mut archive = tar::Archive::new(zstd::Frames::new(BufReader::new(file)));
    for entry in archive.entries()? {
        let entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            report.skipped_entries += 1;
            continue;
        }
        let Some(rel) = safe_relative(&entry.path()?) else {
            report.unsafe_entries += 1;
            continue;
        };
        if rel.extension().is_none_or(|ext| ext != "jsonl") {
            report.skipped_entries += 1;
            continue;
        }
        let target = dest.join(&rel);
        if let Some(parent) = target.parent() {
            out_dir.dir(parent)?;
        }
        scrub_file(entry, &target, redactor, report).map_err(|e| context(&e, &rel))?;
        report.files += 1;
    }
    Ok(())
}

/// Scrubs one transcript into `target` in two passes: rules into a staging
/// file while collecting the redacted values, then (if any) a rewrite that
/// redacts every other occurrence of them. Only the finished file is
/// renamed into place.
fn scrub_file(
    input: impl io::Read,
    target: &Path,
    redactor: &Redactor,
    report: &mut Report,
) -> io::Result<()> {
    let first = target.with_extension("jsonl.pass1");
    let mut secrets = Secrets::default();
    scrub_stream(
        BufReader::new(input),
        BufWriter::new(private_file(&first)?),
        redactor,
        report,
        &mut secrets,
    )?;
    if secrets.is_empty() {
        return fs::rename(&first, target);
    }
    let second = target.with_extension("jsonl.partial");
    report.propagated_values += secrets.rewrite(
        BufReader::new(File::open(&first)?),
        BufWriter::new(private_file(&second)?),
        MAX_LINE,
        &mut report.counts,
    )?;
    fs::remove_file(&first)?;
    fs::rename(&second, target)
}

/// The output tree: creates private directories and files under `root`,
/// refusing symlinks anywhere below it.
struct OutDir {
    root: PathBuf,
    /// Directories already created, checked and chmodded.
    prepared: HashSet<PathBuf>,
}

impl OutDir {
    fn new(root: PathBuf) -> io::Result<Self> {
        let mut out = Self {
            root,
            prepared: HashSet::new(),
        };
        let root = out.root.clone();
        out.dir(&root)?;
        Ok(out)
    }

    /// Creates `dir` (inside the root) one component at a time from the
    /// root down, so an existing symlink anywhere on the way is refused
    /// before anything is created through it. Every directory between the
    /// root and `dir` ends up a real `0700` directory.
    fn dir(&mut self, dir: &Path) -> io::Result<()> {
        if self.prepared.contains(dir) {
            return Ok(());
        }
        let rel = dir.strip_prefix(&self.root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is outside the output dir", dir.display()),
            )
        })?;
        let mut current = self.root.clone();
        self.prepare(&current, true)?;
        for part in rel.components() {
            current.push(part);
            self.prepare(&current, false)?;
        }
        Ok(())
    }

    /// Makes `dir` exist as a plain directory with mode `0700`. Only the
    /// root may have missing parents (outside the tree we police).
    fn prepare(&mut self, dir: &Path, is_root: bool) -> io::Result<()> {
        if self.prepared.contains(dir) {
            return Ok(());
        }
        match fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a plain directory", dir.display()),
                ));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                DirBuilder::new()
                    .recursive(is_root)
                    .mode(0o700)
                    .create(dir)
                    .map_err(|e| context(&e, dir))?;
            }
            Err(err) => return Err(context(&err, dir)),
        }
        fs::set_permissions(dir, Permissions::from_mode(0o700)).map_err(|e| context(&e, dir))?;
        self.prepared.insert(dir.to_owned());
        Ok(())
    }
}

/// Creates or truncates `path`, never through a symlink, as `0600`.
fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| context(&e, path))?;
    file.set_permissions(Permissions::from_mode(0o600))
        .map_err(|e| context(&e, path))?;
    Ok(file)
}

/// `path` if it is relative and made only of normal components (no `..`,
/// no root, no prefix), so joining it under the output dir cannot escape.
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!clean.as_os_str().is_empty()).then_some(clean)
}

/// One line read by [`read_line`].
enum Line {
    /// End of input.
    Eof,
    /// A line in the buffer, with or without its final `\n`.
    Read { newline: bool },
    /// A line longer than the cap, skipped without buffering it.
    Oversized,
}

/// Reads one line into `buf` (without the `\n`), holding at most `cap`
/// bytes of it.
fn read_line(input: &mut impl BufRead, buf: &mut Vec<u8>, cap: usize) -> io::Result<Line> {
    buf.clear();
    let mut oversized = false;
    let mut any = false;
    loop {
        let chunk = match input.fill_buf() {
            Ok(chunk) => chunk,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if chunk.is_empty() {
            return Ok(match (any, oversized) {
                (false, _) => Line::Eof,
                (true, true) => Line::Oversized,
                (true, false) => Line::Read { newline: false },
            });
        }
        any = true;
        let (data, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (chunk.get(..i).unwrap_or_default(), Some(i + 1)),
            None => (chunk, None),
        };
        if !oversized {
            if buf.len() + data.len() > cap {
                oversized = true;
                buf.clear();
            } else {
                buf.extend_from_slice(data);
            }
        }
        let used = done.unwrap_or(chunk.len());
        input.consume(used);
        if done.is_some() {
            return Ok(if oversized {
                Line::Oversized
            } else {
                Line::Read { newline: true }
            });
        }
    }
}

/// Copies `input` to `output` line by line, scrubbing each line.
fn scrub_stream(
    input: impl BufRead,
    output: impl Write,
    redactor: &Redactor,
    report: &mut Report,
    secrets: &mut Secrets,
) -> io::Result<()> {
    scrub_stream_capped(input, output, redactor, report, secrets, MAX_LINE)
}

fn scrub_stream_capped(
    mut input: impl BufRead,
    mut output: impl Write,
    redactor: &Redactor,
    report: &mut Report,
    secrets: &mut Secrets,
    cap: usize,
) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        match read_line(&mut input, &mut line, cap)? {
            Line::Eof => break,
            Line::Oversized => report.oversized_lines += 1,
            Line::Read { newline } => {
                output.write_all(&scrub_line(&line, redactor, report, secrets))?;
                if newline {
                    output.write_all(b"\n")?;
                }
            }
        }
    }
    output.flush()
}

fn scrub_line<'a>(
    line: &'a [u8],
    redactor: &Redactor,
    report: &mut Report,
    secrets: &mut Secrets,
) -> Cow<'a, [u8]> {
    report.lines += 1;
    let mut counts = Counts::new();
    let mut found = Vec::new();
    let scrubbed = if let Ok(mut parsed) = serde_json::from_slice::<Value>(line) {
        value::scrub_value(&mut parsed, None, redactor, &mut counts, &mut found);
        if counts.is_empty() {
            // Parsing can hide text (duplicate keys keep only the last
            // value), so a clean parse is double-checked on the raw line.
            let text = String::from_utf8_lossy(line);
            let raw = redactor.redact(&text, &mut counts, &mut found);
            (!counts.is_empty()).then(|| {
                // Valid JSON must stay valid. If redacting the raw text broke
                // it, re-serializing the parsed value is safe too: it drops
                // the duplicate keys that hid the secret.
                let raw = raw.into_owned().into_bytes();
                if serde_json::from_slice::<Value>(&raw).is_ok() {
                    raw
                } else {
                    parsed.to_string().into_bytes()
                }
            })
        } else {
            Some(parsed.to_string().into_bytes())
        }
    } else {
        report.unparsed_lines += 1;
        let mut text = Value::String(String::from_utf8_lossy(line).into_owned());
        value::scrub_value(&mut text, None, redactor, &mut counts, &mut found);
        match text {
            Value::String(text) if !counts.is_empty() => Some(text.into_bytes()),
            _ => None,
        }
    };
    secrets.add(&found);
    let Some(scrubbed) = scrubbed else {
        return Cow::Borrowed(line);
    };
    report.redacted_lines += 1;
    for (rule, n) in counts {
        *report.counts.entry(rule).or_default() += n;
    }
    Cow::Owned(scrubbed)
}

fn context(err: &io::Error, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {err}", path.display()))
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "scrub-corpus: {} archives, {} transcripts, {} lines",
            self.archives, self.files, self.lines
        )?;
        let rows = [
            ("lines with redactions", self.redacted_lines),
            ("lines not JSON (scrubbed as text)", self.unparsed_lines),
            ("lines dropped (over size cap)", self.oversized_lines),
            ("entries skipped (not *.jsonl)", self.skipped_entries),
            ("entries rejected (unsafe path)", self.unsafe_entries),
            ("dumps skipped (unsafe name)", self.skipped_archives),
            ("values propagated (distinct/file)", self.propagated_values),
        ];
        for (label, n) in rows {
            writeln!(f, "  {label:<34} {n}")?;
        }
        if self.counts.is_empty() {
            return writeln!(f, "redactions: none");
        }
        writeln!(f, "redactions by rule:")?;
        for (rule, n) in &self.counts {
            writeln!(f, "  {rule:<20} {n}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
