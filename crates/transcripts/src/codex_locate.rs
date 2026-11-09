//! Finding a Codex session's rollout under `~/.codex/sessions`.
//!
//! Codex CLI writes each session to
//! `<sessions>/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<session>.jsonl`,
//! dated by the session's start, whose first line is a `session_meta`
//! record naming the session's `cwd`. Nothing else under `~/.codex` is
//! read here (it holds credentials).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use protocol::Timestamp;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::locate::Throttle;
use crate::session::SessionId;
use crate::time::parse_rfc3339;

/// Most day directories searched, newest first. A session older than this
/// is not found (one resumed weeks later writes to its original file).
const MAX_DAYS: usize = 31;

/// Most rollout files whose first line is read in one search by cwd. The
/// [`CodexLocator`] remembers what it read, so the next search goes on
/// from there.
const MAX_PEEKS: usize = 256;

/// How far a rollout's file-name time (local, read as UTC) may be from its
/// real start: past any time zone offset.
const NAME_TIME_SLACK_MS: u64 = 24 * 60 * 60 * 1000;

/// Longest first line read when matching a cwd. `session_meta` carries the
/// base instructions, so it can be tens of kilobytes.
const MAX_META_LINE: u64 = 1024 * 1024;

/// Which session to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexSession {
    /// The session with this id (herdr's `agent_session`), wherever it was
    /// started.
    Id(SessionId),
    /// A session in a working directory, for an agent herdr reports
    /// without a session id: with `not_before`, the **earliest** started
    /// at or after it (the agent's own first session, not one another pane
    /// started later in the same directory); without, the most recent.
    LatestIn {
        /// The agent's working directory (absolute).
        cwd: PathBuf,
        /// When the agent started, if known: sessions started before it
        /// (by their `session_meta` timestamp) belong to someone else.
        not_before: Option<Timestamp>,
    },
}

/// Resolves rollout paths inside one Codex sessions directory, remembering
/// each rollout's `session_meta` (shared by every search, so N agents
/// looking by cwd read each file once between them).
#[derive(Debug)]
pub struct CodexLocator {
    sessions: PathBuf,
    peeked: Mutex<HashMap<PathBuf, Peeked>>,
}

/// What a rollout's first line was found to be.
#[derive(Debug, Clone)]
enum Peeked {
    /// A `session_meta`: never changes.
    Meta(Meta),
    /// Not one, and never will be.
    Other,
    /// Unreadable or not newline-terminated yet, at this file length:
    /// read again only once the file has grown.
    Unfinished(u64),
}

impl CodexLocator {
    /// A locator over an explicit sessions directory (e.g. a test fixture).
    #[must_use]
    pub fn new(sessions_dir: impl Into<PathBuf>) -> Self {
        Self {
            sessions: sessions_dir.into(),
            peeked: Mutex::new(HashMap::new()),
        }
    }

    /// The sessions directory Codex itself uses: `$CODEX_HOME/sessions`,
    /// else `$HOME/.codex/sessions`.
    pub fn from_env() -> Result<Self> {
        crate::locate::config_dir("CODEX_HOME", ".codex", "sessions").map(Self::new)
    }

    /// The sessions directory this locator searches.
    #[must_use]
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions
    }

    /// The rollout of `session`, if one exists in the last [`MAX_DAYS`] day
    /// directories. A search by id only lists directories; a search by cwd
    /// also reads the first line of up to [`MAX_PEEKS`] rollouts not read
    /// before, newest first, and finds nothing while more remain to be
    /// read (the next search carries on). Symlinks are never followed.
    pub fn find(&self, session: &CodexSession) -> Result<Option<PathBuf>> {
        let (cwd, not_before) = match session {
            CodexSession::Id(id) => return self.search_id(id),
            CodexSession::LatestIn { cwd, not_before } => (cwd, *not_before),
        };
        crate::folder::check_cwd(cwd)?;
        // List first (directories only, cheap), so the cache can forget
        // rollouts that aged out or were deleted on every search.
        let mut listed = Vec::new();
        for day in self.days()? {
            listed.extend(rollouts(&day)?);
        }
        {
            let keep: HashSet<&PathBuf> = listed.iter().collect();
            self.peeked().retain(|path, _| keep.contains(path));
        }
        let mut peeks = 0;
        let mut earliest = None;
        for path in listed {
            // Newest first: once file names are a day older than the bound,
            // no older rollout can have started after it.
            if let (Some(bound), Some(named)) = (not_before, name_time(&path))
                && named.unix_millis().saturating_add(NAME_TIME_SLACK_MS) < bound.unix_millis()
            {
                break;
            }
            let Ok(meta) = self.peek(&path, &mut peeks) else {
                // Out of reads with rollouts left unread: the answer could
                // still change, so wait for the next search.
                return Ok(None);
            };
            let matched = meta.is_some_and(|meta| {
                Path::new(&meta.cwd) == cwd
                    && not_before
                        .is_none_or(|bound| meta.started.is_some_and(|started| started >= bound))
            });
            if matched {
                if not_before.is_none() {
                    return Ok(Some(path));
                }
                earliest = Some(path);
            }
        }
        Ok(earliest)
    }

    /// The shared cache. Held only around map access, never across I/O.
    fn peeked(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Peeked>> {
        self.peeked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `session_meta` of the rollout at `path`, from the cache or read
    /// now (counting against `peeks`, at most [`MAX_PEEKS`]): `None` when
    /// it has none (yet).
    fn peek(
        &self,
        path: &Path,
        peeks: &mut usize,
    ) -> std::result::Result<Option<Meta>, OutOfReads> {
        // The length before reading: if Codex finishes the line in between,
        // the file has outgrown it and is read again next time.
        let len = fs::symlink_metadata(path).map_or(0, |m| m.len());
        match self.peeked().get(path) {
            Some(Peeked::Meta(meta)) => return Ok(Some(meta.clone())),
            Some(Peeked::Other) => return Ok(None),
            Some(Peeked::Unfinished(at)) if *at == len => return Ok(None),
            Some(Peeked::Unfinished(_)) | None => {}
        }
        if *peeks >= MAX_PEEKS {
            return Err(OutOfReads);
        }
        *peeks += 1;
        let (found, meta) = match read_meta(path) {
            Peek::Meta(meta) => (Peeked::Meta(meta.clone()), Some(meta)),
            Peek::Other => (Peeked::Other, None),
            Peek::Unfinished => (Peeked::Unfinished(len), None),
        };
        self.peeked().insert(path.to_path_buf(), found);
        Ok(meta)
    }

    /// The session a rollout's file name carries, if it is one of Codex's
    /// (`rollout-<start>-<uuid>.jsonl`).
    #[must_use]
    pub fn session_of(rollout: &Path) -> Option<SessionId> {
        rollout_session(rollout).and_then(|id| SessionId::new(id).ok())
    }

    /// The newest rollout named for session `id`.
    fn search_id(&self, id: &SessionId) -> Result<Option<PathBuf>> {
        for day in self.days()? {
            if let Some(path) = rollouts(&day)?
                .into_iter()
                .find(|path| rollout_session(path) == Some(id.as_str()))
            {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    /// The `YYYY/MM/DD` directories, newest first, at most [`MAX_DAYS`].
    fn days(&self) -> Result<Vec<PathBuf>> {
        let mut days = Vec::new();
        for year in numbered_dirs(&self.sessions, 4)? {
            for month in numbered_dirs(&year, 2)? {
                for day in numbered_dirs(&month, 2)? {
                    days.push(day);
                    if days.len() >= MAX_DAYS {
                        return Ok(days);
                    }
                }
            }
        }
        Ok(days)
    }
}

/// Subdirectories of `dir` named by exactly `width` digits, newest
/// (largest) first. A missing `dir` has none.
fn numbered_dirs(dir: &Path, width: usize) -> Result<Vec<PathBuf>> {
    let mut found: Vec<(String, PathBuf)> = entries(dir)?
        .filter(|(name, kind, _)| {
            kind.is_dir() && name.len() == width && name.bytes().all(|b| b.is_ascii_digit())
        })
        .map(|(name, _, path)| (name, path))
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(found.into_iter().map(|(_, path)| path).collect())
}

/// The `rollout-*.jsonl` files in `dir`, newest first (their names start
/// with the start time).
fn rollouts(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found: Vec<(String, PathBuf)> = entries(dir)?
        .filter(|(name, kind, _)| {
            // Codex names them exactly so: no case folding.
            kind.is_file()
                && name.starts_with("rollout-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext == "jsonl")
        })
        .map(|(name, _, path)| (name, path))
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(found.into_iter().map(|(_, path)| path).collect())
}

/// The entries of `dir` as (UTF-8 name, type without following symlinks,
/// path); a missing `dir` is empty, entries with other names are skipped.
fn entries(dir: &Path) -> Result<impl Iterator<Item = (String, fs::FileType, PathBuf)>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => Some(listing),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(Error::io("listing", dir, err)),
    };
    Ok(listing.into_iter().flatten().filter_map(|entry| {
        let entry = entry.ok()?;
        let name = entry.file_name().into_string().ok()?;
        let kind = entry.file_type().ok()?;
        Some((name, kind, entry.path()))
    }))
}

/// The session id in a rollout file name: the UUID after the timestamp
/// (`rollout-2026-11-08T13-45-33-<uuid>.jsonl`).
fn rollout_session(path: &Path) -> Option<&str> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    // A UUID is 36 characters (the last five `-`-separated groups), after
    // a `-`.
    let split = stem.len().checked_sub(37)?;
    let (head, id) = stem.split_at_checked(split)?;
    let id = id.strip_prefix('-')?;
    (head.starts_with("rollout-") && id.len() == 36).then_some(id)
}

/// A search used up its [`MAX_PEEKS`] reads.
struct OutOfReads;

/// The start time in a rollout's file name
/// (`rollout-2026-11-08T13-45-33-…`), local time read as UTC.
fn name_time(path: &Path) -> Option<Timestamp> {
    let name = path.file_name()?.to_str()?.strip_prefix("rollout-")?;
    let stamp = name.get(..19)?;
    let (date, clock) = stamp.split_once('T')?;
    parse_rfc3339(&format!("{date}T{}Z", clock.replace('-', ":")))
}

/// What a rollout's `session_meta` first line says about it.
#[derive(Debug, Clone)]
struct Meta {
    cwd: String,
    /// When the session started (`payload.timestamp`, else the record's).
    started: Option<Timestamp>,
}

/// What a rollout's first line turned out to be.
enum Peek {
    /// A `session_meta` record.
    Meta(Meta),
    /// A complete line (or one past [`MAX_META_LINE`]) that is not one:
    /// it never will be.
    Other,
    /// Unreadable, or not newline-terminated yet: Codex may still be
    /// writing it.
    Unfinished,
}

/// The first line of a rollout, read up to [`MAX_META_LINE`].
fn read_meta(path: &Path) -> Peek {
    #[derive(Deserialize)]
    struct Line {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        timestamp: Option<String>,
        payload: Payload,
    }
    #[derive(Deserialize)]
    struct Payload {
        cwd: String,
        #[serde(default)]
        timestamp: Option<String>,
    }
    let Ok(file) = File::open(path) else {
        return Peek::Unfinished;
    };
    let mut first = Vec::new();
    if BufReader::new(file.take(MAX_META_LINE))
        .read_until(b'\n', &mut first)
        .is_err()
    {
        return Peek::Unfinished;
    }
    let complete = first.ends_with(b"\n") || first.len() as u64 >= MAX_META_LINE;
    if !complete {
        return Peek::Unfinished;
    }
    let Ok(line) = serde_json::from_slice::<Line>(&first) else {
        return Peek::Other;
    };
    if line.kind != "session_meta" {
        return Peek::Other;
    }
    let started = line
        .payload
        .timestamp
        .as_deref()
        .or(line.timestamp.as_deref())
        .and_then(parse_rfc3339);
    Peek::Meta(Meta {
        cwd: line.payload.cwd,
        started,
    })
}

/// Polls for one session's rollout while it may not exist yet, searching
/// at most once per interval, and keeps it once found. A rollout found by
/// cwd ([`CodexSession::LatestIn`]) is given up only once it has stopped
/// growing and a session started in the same cwd after its last write:
/// what `/new` or a restart in the same pane leaves behind. It never
/// switches to a session that ran alongside it.
#[derive(Debug, Clone)]
pub struct CodexLookup {
    session: CodexSession,
    throttle: Throttle,
    found: Option<Found>,
}

/// The rollout held, as of the last search.
#[derive(Debug, Clone)]
struct Found {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

impl Found {
    /// `path` as it is now; `None` once it is gone (or not a regular file).
    fn stat(path: PathBuf) -> Option<Self> {
        let meta = fs::symlink_metadata(&path)
            .ok()
            .filter(fs::Metadata::is_file)?;
        Some(Self {
            path,
            len: meta.len(),
            modified: meta.modified().ok()?,
        })
    }
}

impl CodexLookup {
    /// Looks for `session`, searching at most every
    /// [`DEFAULT_SCAN_INTERVAL`](crate::DEFAULT_SCAN_INTERVAL).
    #[must_use]
    pub fn new(session: CodexSession) -> Self {
        Self {
            session,
            throttle: Throttle::default(),
            found: None,
        }
    }

    /// Overrides the minimum time between searches.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.throttle.set_interval(interval);
        self
    }

    /// The rollout held, searching first if a search is due. `now` is
    /// passed in so callers control throttling.
    pub fn poll(&mut self, locator: &CodexLocator, now: Instant) -> Result<Option<PathBuf>> {
        if self.throttle.due(now) {
            self.found = self.search(locator)?;
        }
        Ok(self.found.as_ref().map(|found| found.path.clone()))
    }

    fn search(&self, locator: &CodexLocator) -> Result<Option<Found>> {
        let held = self
            .found
            .as_ref()
            .and_then(|before| Some((before, Found::stat(before.path.clone())?)));
        let Some((before, now)) = held else {
            return Ok(locator.find(&self.session)?.and_then(Found::stat));
        };
        let CodexSession::LatestIn { cwd, .. } = &self.session else {
            return Ok(Some(now));
        };
        if now.len != before.len || now.modified != before.modified {
            return Ok(Some(now));
        }
        let since = CodexSession::LatestIn {
            cwd: cwd.clone(),
            not_before: Some(timestamp(before.modified)),
        };
        let successor = locator
            .find(&since)?
            .filter(|path| *path != now.path)
            .and_then(Found::stat);
        Ok(Some(successor.unwrap_or(now)))
    }
}

/// `at` as a protocol timestamp (milliseconds since the epoch; 0 before it).
fn timestamp(at: SystemTime) -> Timestamp {
    let millis = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    Timestamp::from_unix_millis(millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "01a1bb68-3fa5-78d3-9809-5887b47e4fcc";
    const B: &str = "01a1bb6f-45a9-7332-adc0-fd8600af2b96";

    fn rollout(root: &Path, day: &str, time: &str, id: &str, cwd: &str) -> PathBuf {
        let dir = root.join(day);
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("rollout-{time}-{id}.jsonl"));
        // The file name's time, as RFC 3339 (`2026-11-08T13-45-33`).
        let (date, clock) = time.split_once('T').expect("time");
        let started = format!("{date}T{}Z", clock.replace('-', ":"));
        let meta = serde_json::json!({
            "timestamp": "2026-11-08T13:45:34.155Z",
            "type": "session_meta",
            "payload": {"id": id, "cwd": cwd, "timestamp": started,
                        "base_instructions": {"text": "x".repeat(5000)}},
        });
        fs::write(&path, format!("{meta}\n")).expect("write");
        path
    }

    #[test]
    fn session_ids_come_from_file_names() {
        let path = PathBuf::from(format!("/s/rollout-2026-11-08T13-45-33-{A}.jsonl"));
        assert_eq!(rollout_session(&path), Some(A));
        assert_eq!(rollout_session(Path::new("/s/rollout-x.jsonl")), None);
        assert_eq!(rollout_session(Path::new("/s/x.txt")), None);
        // Exactly a UUID long (no room for the `-` before it), or shorter.
        let bare = PathBuf::from(format!("/s/{A}.jsonl"));
        assert_eq!(rollout_session(&bare), None);
        assert_eq!(rollout_session(Path::new("/s/.jsonl")), None);
        let no_dash = PathBuf::from(format!("/s/rollout-x{A}.jsonl"));
        assert_eq!(rollout_session(&no_dash), None);
    }

    #[test]
    fn finds_by_id_and_by_latest_cwd() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        let id = |s: &str| CodexSession::Id(SessionId::new(s).expect("id"));
        assert_eq!(locator.find(&id(A)).expect("empty"), None);

        let old = rollout(&sessions, "2026/10/31", "2026-10-31T09-00-00", A, "/w");
        let new = rollout(&sessions, "2026/11/08", "2026-11-08T13-53-13", B, "/w");
        let elsewhere = rollout(&sessions, "2026/11/08", "2026-11-08T14-00-00", A, "/other");
        fs::write(sessions.join("2026/11/08/notes.txt"), "").expect("write");

        assert_eq!(locator.find(&id(B)).expect("by id"), Some(new.clone()));
        // The newest file with that id.
        assert_eq!(locator.find(&id(A)).expect("by id"), Some(elsewhere));
        let latest = |cwd: &str| CodexSession::LatestIn {
            cwd: PathBuf::from(cwd),
            not_before: None,
        };
        assert_eq!(locator.find(&latest("/w")).expect("by cwd"), Some(new));
        fs::remove_file(sessions.join(format!("2026/11/08/rollout-2026-11-08T13-53-13-{B}.jsonl")))
            .expect("rm");
        assert_eq!(locator.find(&latest("/w")).expect("by cwd"), Some(old));
        assert_eq!(locator.find(&latest("/nowhere")).expect("by cwd"), None);
        assert!(matches!(
            locator.find(&latest("w")),
            Err(Error::RelativeCwd { .. })
        ));
    }

    #[test]
    fn a_rollout_still_being_written_is_looked_at_again() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        let path = rollout(&sessions, "2026/11/08", "2026-11-08T13-00-00", A, "/w");
        let whole = fs::read_to_string(&path).expect("read");
        // Created, its first line half flushed.
        fs::write(&path, &whole[..whole.len() / 2]).expect("write");
        let mut lookup = CodexLookup::new(CodexSession::LatestIn {
            cwd: PathBuf::from("/w"),
            not_before: None,
        })
        .with_interval(Duration::ZERO);
        let now = Instant::now();
        assert_eq!(lookup.poll(&locator, now).expect("partial"), None);
        fs::write(&path, &whole).expect("write");
        assert_eq!(lookup.poll(&locator, now).expect("whole"), Some(path));
    }

    #[test]
    fn sessions_started_before_the_agent_are_not_its_own() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        rollout(&sessions, "2026/11/08", "2026-11-08T13-00-00", A, "/w");
        let since = |rfc: &str| CodexSession::LatestIn {
            cwd: PathBuf::from("/w"),
            not_before: parse_rfc3339(rfc),
        };
        let started_later = since("2026-11-08T13:30:00Z");
        assert_eq!(locator.find(&started_later).expect("search"), None);
        let own = rollout(&sessions, "2026/11/08", "2026-11-08T13-31-00", B, "/w");
        assert_eq!(locator.find(&started_later).expect("search"), Some(own));
        assert!(
            locator
                .find(&since("2026-11-08T12:00:00Z"))
                .expect("search")
                .is_some()
        );
    }

    #[test]
    fn a_lookup_finds_a_match_behind_many_others_and_reads_each_once() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        let target = rollout(&sessions, "2026/11/01", "2026-11-01T09-00-00", A, "/w");
        for n in 0..(2 * MAX_PEEKS + 10) {
            let id = format!("01a1bb68-3fa5-78d3-9809-{n:012}");
            rollout(
                &sessions,
                "2026/11/08",
                &format!("2026-11-08T10-{:02}-{:02}", n / 60, n % 60),
                &id,
                "/other",
            );
        }
        let mut lookup = CodexLookup::new(CodexSession::LatestIn {
            cwd: PathBuf::from("/w"),
            not_before: None,
        })
        .with_interval(Duration::ZERO);
        let now = Instant::now();
        assert_eq!(lookup.poll(&locator, now).expect("first"), None);
        assert_eq!(lookup.poll(&locator, now).expect("second"), None);
        assert_eq!(
            lookup.poll(&locator, now).expect("third"),
            Some(target.clone())
        );
        // Everything is known now: no file is read again.
        fs::remove_dir_all(sessions.join("2026/11/01")).expect("rm");
        assert_eq!(lookup.poll(&locator, now).expect("listing only"), None);
        assert_eq!(
            locator.peeked.lock().expect("lock").len(),
            2 * MAX_PEEKS + 10,
            "forgot the removed day"
        );
    }

    #[test]
    fn with_a_known_start_the_earliest_session_since_is_the_agents() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        rollout(&sessions, "2026/11/07", "2026-11-07T09-00-00", B, "/w");
        let own = rollout(&sessions, "2026/11/08", "2026-11-08T13-00-05", A, "/w");
        // Another pane's session, started later in the same repo.
        let later = "01a1bb70-0000-7000-8000-000000000001";
        rollout(&sessions, "2026/11/08", "2026-11-08T14-00-00", later, "/w");
        let session = CodexSession::LatestIn {
            cwd: PathBuf::from("/w"),
            not_before: parse_rfc3339("2026-11-08T13:00:00Z"),
        };
        assert_eq!(locator.find(&session).expect("search"), Some(own));
        // Without a start: the most recent.
        let latest = CodexSession::LatestIn {
            cwd: PathBuf::from("/w"),
            not_before: None,
        };
        let found = locator.find(&latest).expect("search").expect("found");
        assert!(found.to_string_lossy().contains(later), "{found:?}");
    }

    #[test]
    fn the_cache_forgets_deleted_rollouts_even_when_a_search_stops_early() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        let old = rollout(&sessions, "2026/11/01", "2026-11-01T09-00-00", B, "/other");
        let new = rollout(&sessions, "2026/11/08", "2026-11-08T13-00-00", A, "/w");
        let latest = |cwd: &str| CodexSession::LatestIn {
            cwd: PathBuf::from(cwd),
            not_before: None,
        };
        assert_eq!(locator.find(&latest("/nowhere")).expect("all read"), None);
        assert_eq!(locator.peeked().len(), 2);
        fs::remove_file(&old).expect("rm");
        // Returns on its first (newest) match, yet forgets the deleted one.
        assert_eq!(locator.find(&latest("/w")).expect("found"), Some(new));
        assert_eq!(locator.peeked().len(), 1);
    }

    #[test]
    fn unfinished_rollouts_are_reread_only_once_grown() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        let target = rollout(&sessions, "2026/11/01", "2026-11-01T09-00-00", A, "/w");
        let day = sessions.join("2026/11/08");
        fs::create_dir_all(&day).expect("mkdir");
        // More truncated heads (no newline, e.g. after a crash) than one
        // search may read.
        for n in 0..(MAX_PEEKS + 5) {
            let name = format!(
                "rollout-2026-11-08T10-{:02}-{:02}-01a1bb68-3fa5-78d3-9809-{n:012}.jsonl",
                n / 60,
                n % 60
            );
            fs::write(day.join(name), "{\"type\":\"session_meta\",\"pay").expect("write");
        }
        let latest = CodexSession::LatestIn {
            cwd: PathBuf::from("/w"),
            not_before: None,
        };
        assert_eq!(locator.find(&latest).expect("first"), None);
        // Unchanged since: skipped without a read, so the budget reaches
        // the older match.
        assert_eq!(locator.find(&latest).expect("second"), Some(target));
    }

    #[test]
    fn symlinks_and_odd_directories_are_ignored() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let real = rollout(root.path(), "elsewhere", "2026-11-08T13-00-00", A, "/w");
        let day = sessions.join("2026/11/08");
        fs::create_dir_all(&day).expect("mkdir");
        std::os::unix::fs::symlink(
            &real,
            day.join(format!("rollout-2026-11-08T13-00-00-{A}.jsonl")),
        )
        .expect("symlink");
        rollout(&sessions, "archive/11/08", "2026-11-08T12-00-00", A, "/w");
        let locator = CodexLocator::new(&sessions);
        let session = CodexSession::Id(SessionId::new(A).expect("id"));
        assert_eq!(locator.find(&session).expect("search"), None);
    }

    /// Sets `path`'s last write to `rfc` (RFC 3339), appending a line
    /// first when `grow`.
    fn written_at(path: &Path, rfc: &str, grow: bool) {
        use std::io::Write;
        let file = File::options().append(true).open(path).expect("open");
        if grow {
            (&file).write_all(b"{}\n").expect("append");
        }
        let at = parse_rfc3339(rfc).expect("time").unix_millis();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(at))
            .expect("mtime");
    }

    #[test]
    fn a_rollout_found_by_cwd_is_kept_until_a_later_session_replaces_it() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        let mut lookup = CodexLookup::new(CodexSession::LatestIn {
            cwd: PathBuf::from("/w"),
            not_before: None,
        })
        .with_interval(Duration::ZERO);
        let now = Instant::now();
        let first = rollout(&sessions, "2026/11/08", "2026-11-08T13-00-00", A, "/w");
        written_at(&first, "2026-11-08T13:10:00Z", false);
        assert_eq!(
            lookup.poll(&locator, now).expect("found"),
            Some(first.clone())
        );
        // Another pane's session, started while the first was being
        // written: never taken, though it is newer.
        rollout(&sessions, "2026/11/08", "2026-11-08T13-05-00", B, "/w");
        assert_eq!(
            lookup.poll(&locator, now).expect("kept"),
            Some(first.clone())
        );
        // `/new`: a session started after the first's last write. While
        // the first still grows it is kept; once quiet, it is replaced.
        let later = "01a1bb70-0000-7000-8000-000000000001";
        let next = rollout(&sessions, "2026/11/08", "2026-11-08T13-20-00", later, "/w");
        written_at(&first, "2026-11-08T13:15:00Z", true);
        assert_eq!(lookup.poll(&locator, now).expect("growing"), Some(first));
        assert_eq!(
            lookup.poll(&locator, now).expect("replaced"),
            Some(next.clone())
        );
        assert_eq!(
            CodexLocator::session_of(&next)
                .map(|s| s.to_string())
                .as_deref(),
            Some(later)
        );
    }

    #[test]
    fn lookup_throttles_searches() {
        let root = tempfile::tempdir().expect("temp dir");
        let sessions = root.path().join("sessions");
        let locator = CodexLocator::new(&sessions);
        let t0 = Instant::now();
        let mut lookup = CodexLookup::new(CodexSession::LatestIn {
            cwd: PathBuf::from("/w"),
            not_before: None,
        })
        .with_interval(Duration::from_secs(10));
        assert_eq!(lookup.poll(&locator, t0).expect("search"), None);
        let path = rollout(&sessions, "2026/11/08", "2026-11-08T13-00-00", A, "/w");
        let soon = t0 + Duration::from_secs(1);
        assert_eq!(lookup.poll(&locator, soon).expect("throttled"), None);
        let later = t0 + Duration::from_secs(11);
        assert_eq!(lookup.poll(&locator, later).expect("search"), Some(path));
    }
}
