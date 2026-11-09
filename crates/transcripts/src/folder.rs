//! Finding a Claude agent's transcript by its working directory alone.
//!
//! herdr names a Claude agent's session through Claude Code's hooks. An
//! agent running with hooks disabled (the swarm runs agents with
//! `disableAllHooks`) never gets one, so its transcript can only be matched
//! by folder: every session started in a cwd is a `<session>.jsonl` in that
//! cwd's project directory (see [`Locator`]). [`FolderLookup`] picks one:
//!
//! - **First match:** the most recently modified session file, modified
//!   within [`DEFAULT_MAX_AGE`] (an old session is not a fresh agent's)
//!   and, when the agent's start is known, since then. A file created
//!   since the agent started wins over one that was not (its own session
//!   over another pane's in the same cwd).
//! - **Stable:** the match is kept while it lives, so it never flips
//!   between the files of two agents in one cwd.
//! - **Replaced:** it is given up for a newer file that was created after
//!   the match was last written, once the match has stopped growing: what
//!   `/clear` or a restart in the same pane leaves behind. Also for a file
//!   created since the agent started, when the match was not.
//!
//! Only regular files directly inside the cwd's project directory are
//! considered; that directory must be a real directory (not a symlink),
//! and the cwd must be absolute without `..`. The slug is made of ASCII
//! letters, digits and `-` only, so it always names a direct child of the
//! projects directory.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::error::{Error, Result};
use crate::locate::{Locator, MAX_VERBATIM_SLUG, Throttle, project_slug};
use crate::session::SessionId;

/// Default oldest last write of a first match.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_hours(24);

/// A transcript matched by folder rather than named by a session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    /// The transcript.
    pub path: PathBuf,
    /// Its session (the file name).
    pub session: SessionId,
}

/// One session file in a project directory, as listed.
#[derive(Debug, Clone)]
pub(crate) struct SessionFile {
    path: PathBuf,
    session: SessionId,
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
}

/// Fails unless `cwd` is absolute with no `..` component: a working
/// directory herdr reports is both, so anything else was crafted.
pub(crate) fn check_cwd(cwd: &Path) -> Result<()> {
    if !cwd.is_absolute() {
        return Err(Error::RelativeCwd { path: cwd.into() });
    }
    if cwd.components().any(|c| c == Component::ParentDir) {
        return Err(Error::UnsafeCwd { path: cwd.into() });
    }
    Ok(())
}

impl Locator {
    /// The session files of `cwd`'s project directory. Empty when it does
    /// not exist, is not a real directory, or has a hashed name (a slug
    /// longer than Claude Code keeps verbatim is never guessed).
    pub(crate) fn sessions_in(&self, cwd: &Path) -> Result<Vec<SessionFile>> {
        check_cwd(cwd)?;
        let slug = project_slug(cwd);
        if slug.len() > MAX_VERBATIM_SLUG {
            return Ok(Vec::new());
        }
        let dir = self.projects_dir().join(slug);
        if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            return Ok(Vec::new());
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::io("listing", &dir, err)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io("listing", &dir, e))?;
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let name = entry.file_name();
            let Some(session) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
                .and_then(|stem| SessionId::new(stem).ok())
            else {
                continue;
            };
            // `DirEntry::metadata` does not follow symlinks.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            found.push(SessionFile {
                path: entry.path(),
                session,
                len: meta.len(),
                modified,
                created: meta.created().ok(),
            });
        }
        Ok(found)
    }
}

/// Keeps one agent's transcript matched by folder (see the
/// [module docs](self)), scanning at most once per interval.
#[derive(Debug, Clone)]
pub struct FolderLookup {
    cwd: PathBuf,
    not_before: Option<SystemTime>,
    max_age: Duration,
    throttle: Throttle,
    chosen: Option<Chosen>,
    /// When each listed file came to be: its creation time, else (a
    /// filesystem without one) the time of the scan before the one that
    /// first listed it; `None` for one the first scan listed (age unknown).
    births: HashMap<PathBuf, Option<SystemTime>>,
    last_scan: Option<SystemTime>,
}

/// The current match, as of the last scan.
#[derive(Debug, Clone)]
struct Chosen {
    path: PathBuf,
    session: SessionId,
    len: u64,
    modified: SystemTime,
}

impl Chosen {
    fn of(file: &SessionFile) -> Self {
        Self {
            path: file.path.clone(),
            session: file.session.clone(),
            len: file.len,
            modified: file.modified,
        }
    }
}

impl FolderLookup {
    /// Matches a transcript in `cwd` for an agent that started at
    /// `not_before`, if known: files last written before then are not
    /// its own.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, not_before: Option<SystemTime>) -> Self {
        Self {
            cwd: cwd.into(),
            not_before,
            max_age: DEFAULT_MAX_AGE,
            throttle: Throttle::default(),
            chosen: None,
            births: HashMap::new(),
            last_scan: None,
        }
    }

    /// Overrides the minimum time between scans.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.throttle.set_interval(interval);
        self
    }

    /// Overrides [`DEFAULT_MAX_AGE`].
    #[must_use]
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// The current match, scanning first if a scan is due. `now` and
    /// `wall` (the same moment, monotonic and wall-clock) are passed in so
    /// callers control throttling and age.
    pub fn poll(
        &mut self,
        locator: &Locator,
        now: Instant,
        wall: SystemTime,
    ) -> Result<Option<Matched>> {
        if self.throttle.due(now) {
            let files = locator.sessions_in(&self.cwd)?;
            self.note_births(&files);
            self.chosen = self.choose(&files, wall);
            self.last_scan = Some(wall);
        }
        Ok(self.chosen.as_ref().map(|chosen| Matched {
            path: chosen.path.clone(),
            session: chosen.session.clone(),
        }))
    }

    fn note_births(&mut self, files: &[SessionFile]) {
        self.births
            .retain(|path, _| files.iter().any(|f| f.path == *path));
        for file in files {
            self.births
                .entry(file.path.clone())
                .or_insert_with(|| file.created.or(self.last_scan));
        }
    }

    fn birth(&self, file: &SessionFile) -> Option<SystemTime> {
        self.births.get(&file.path).copied().flatten()
    }

    /// Created since the agent started (known only when both are).
    fn born_since_start(&self, file: &SessionFile) -> bool {
        self.not_before
            .zip(self.birth(file))
            .is_some_and(|(start, born)| born >= start)
    }

    /// The match after a scan that listed `files`.
    fn choose(&self, files: &[SessionFile], wall: SystemTime) -> Option<Chosen> {
        let held = self
            .chosen
            .as_ref()
            .and_then(|chosen| Some((chosen, files.iter().find(|f| f.path == chosen.path)?)));
        let Some((before, now)) = held else {
            return self.first_match(files, wall).map(Chosen::of);
        };
        let grew = now.len != before.len || now.modified != before.modified;
        let own = self.born_since_start(now);
        let successor = files
            .iter()
            .filter(|f| f.path != now.path)
            .filter(|f| {
                let replaced = !grew
                    && f.modified > now.modified
                    && self.birth(f).is_some_and(|born| born >= before.modified);
                replaced || (!own && self.born_since_start(f))
            })
            .max_by_key(|f| f.modified);
        Some(Chosen::of(successor.unwrap_or(now)))
    }

    /// The best match among `files` with none held yet.
    fn first_match<'a>(
        &self,
        files: &'a [SessionFile],
        wall: SystemTime,
    ) -> Option<&'a SessionFile> {
        files
            .iter()
            .filter(|f| {
                // A file written "in the future" (clock skew) is recent.
                let recent = wall
                    .duration_since(f.modified)
                    .map_or(true, |age| age <= self.max_age);
                recent && self.not_before.is_none_or(|start| f.modified >= start)
            })
            .max_by_key(|f| (self.born_since_start(f), f.modified))
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    const C: &str = "33333333-3333-4333-8333-333333333333";

    struct Dir {
        tmp: tempfile::TempDir,
        locator: Locator,
        cwd: PathBuf,
        project: PathBuf,
    }

    fn dir() -> Dir {
        let tmp = tempfile::tempdir().expect("tmp");
        let projects = tmp.path().join("projects");
        let cwd = PathBuf::from("/work/repo");
        let project = projects.join(project_slug(&cwd));
        fs::create_dir_all(&project).expect("mkdir");
        Dir {
            locator: Locator::new(projects),
            tmp,
            cwd,
            project,
        }
    }

    impl Dir {
        /// Writes (appends to) session `id`, last modified `ago` before now.
        fn write(&self, id: &str, ago: Duration) -> PathBuf {
            use std::io::Write;
            let path = self.project.join(format!("{id}.jsonl"));
            let mut file = File::options()
                .create(true)
                .append(true)
                .open(&path)
                .expect("open");
            writeln!(file, "{{}}").expect("write");
            file.set_modified(SystemTime::now() - ago).expect("mtime");
            path
        }
    }

    fn lookup(d: &Dir, not_before: Option<SystemTime>) -> FolderLookup {
        FolderLookup::new(&d.cwd, not_before).with_interval(Duration::ZERO)
    }

    fn poll(lookup: &mut FolderLookup, d: &Dir) -> Option<String> {
        lookup
            .poll(&d.locator, Instant::now(), SystemTime::now())
            .expect("poll")
            .map(|m| m.session.to_string())
    }

    const MIN: Duration = Duration::from_mins(1);

    #[test]
    fn the_most_recent_session_matches() {
        let d = dir();
        let mut l = lookup(&d, None);
        assert_eq!(poll(&mut l, &d), None);
        d.write(A, 5 * MIN);
        let b = d.write(B, MIN);
        let matched = l
            .poll(&d.locator, Instant::now(), SystemTime::now())
            .expect("poll")
            .expect("a match");
        assert_eq!(matched.path, b);
        assert_eq!(matched.session.as_str(), B);
    }

    #[test]
    fn a_stale_session_does_not_match() {
        let d = dir();
        d.write(A, DEFAULT_MAX_AGE + MIN);
        let mut l = lookup(&d, None);
        assert_eq!(poll(&mut l, &d), None);
        // Nor one written before the agent started.
        d.write(B, 10 * MIN);
        let mut fresh = lookup(&d, Some(SystemTime::now() - 5 * MIN));
        assert_eq!(poll(&mut fresh, &d), None);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(B));
    }

    #[test]
    fn the_match_holds_while_another_session_writes() {
        let d = dir();
        // Another pane's session in the same folder, created first.
        d.write(B, Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        d.write(A, Duration::ZERO);
        let mut l = lookup(&d, None);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(A));
        d.write(B, Duration::ZERO);
        assert_eq!(
            poll(&mut l, &d).as_deref(),
            Some(A),
            "B predates A's last write"
        );
        d.write(A, Duration::ZERO);
        d.write(B, Duration::ZERO);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(A));
    }

    #[test]
    fn a_session_started_after_the_match_went_quiet_replaces_it() {
        let d = dir();
        d.write(A, 10 * MIN);
        let mut l = lookup(&d, None);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(A));
        // `/clear`: a new file, created after A's last write.
        d.write(B, Duration::ZERO);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(B));
        // The old one gets no say again, however it is touched.
        d.write(A, Duration::ZERO);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(B));
    }

    #[test]
    fn a_match_still_growing_is_not_replaced() {
        let d = dir();
        let a = d.write(A, 10 * MIN);
        let mut l = lookup(&d, None);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(A));
        d.write(B, Duration::ZERO);
        // A grew in the same interval (to a time before B's).
        File::options()
            .append(true)
            .open(&a)
            .and_then(|f| {
                use std::io::Write;
                (&f).write_all(b"{}\n")?;
                f.set_modified(SystemTime::now() - MIN)
            })
            .expect("grow");
        assert_eq!(poll(&mut l, &d).as_deref(), Some(A));
        // Quiet since: replaced.
        assert_eq!(poll(&mut l, &d).as_deref(), Some(B));
    }

    #[test]
    fn a_session_created_since_the_agent_started_wins() {
        let d = dir();
        // Another pane's session, created before this agent started.
        d.write(B, Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        let start = SystemTime::now();
        std::thread::sleep(Duration::from_millis(5));
        let mut l = lookup(&d, Some(start));
        assert_eq!(poll(&mut l, &d), None, "nothing written since the start");
        // Written since: the only candidate, matched for now.
        d.write(B, Duration::ZERO);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(B));
        // The agent's own session appears: it wins although B goes on.
        d.write(C, Duration::ZERO);
        d.write(B, Duration::ZERO);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(C));
        d.write(B, Duration::ZERO);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(C));
    }

    #[test]
    fn a_deleted_match_is_replaced() {
        let d = dir();
        let a = d.write(A, MIN);
        d.write(B, 2 * MIN);
        let mut l = lookup(&d, None);
        assert_eq!(poll(&mut l, &d).as_deref(), Some(A));
        fs::remove_file(a).expect("rm");
        assert_eq!(poll(&mut l, &d).as_deref(), Some(B));
    }

    #[test]
    fn scans_are_throttled_and_the_match_kept_between() {
        let d = dir();
        d.write(A, MIN);
        let mut l = FolderLookup::new(&d.cwd, None).with_interval(Duration::from_secs(10));
        let t0 = Instant::now();
        let at = |l: &mut FolderLookup, t| {
            l.poll(&d.locator, t, SystemTime::now())
                .expect("poll")
                .map(|m| m.session.to_string())
        };
        assert_eq!(at(&mut l, t0).as_deref(), Some(A));
        fs::remove_file(d.project.join(format!("{A}.jsonl"))).expect("rm");
        assert_eq!(at(&mut l, t0 + Duration::from_secs(1)).as_deref(), Some(A));
        assert_eq!(at(&mut l, t0 + Duration::from_secs(11)), None);
    }

    #[test]
    fn only_regular_session_files_in_a_real_directory_count() {
        let d = dir();
        let elsewhere = d.tmp.path().join("secret.jsonl");
        fs::write(&elsewhere, "{}\n").expect("write");
        std::os::unix::fs::symlink(&elsewhere, d.project.join(format!("{A}.jsonl")))
            .expect("symlink");
        fs::create_dir(d.project.join(format!("{B}.jsonl"))).expect("dir");
        fs::write(d.project.join("notes.txt"), "").expect("write");
        fs::write(d.project.join("bad id.jsonl"), "").expect("write");
        let mut l = lookup(&d, None);
        assert_eq!(poll(&mut l, &d), None);

        // A project directory that is a symlink is not followed.
        let real = d.tmp.path().join("real");
        fs::create_dir(&real).expect("mkdir");
        fs::write(real.join(format!("{C}.jsonl")), "{}\n").expect("write");
        let linked = Path::new("/work/linked");
        std::os::unix::fs::symlink(&real, d.locator.projects_dir().join(project_slug(linked)))
            .expect("symlink");
        let mut l = FolderLookup::new(linked, None).with_interval(Duration::ZERO);
        assert_eq!(poll(&mut l, &d), None);
    }

    #[test]
    fn a_crafted_cwd_is_refused() {
        let d = dir();
        for cwd in ["/work/../../etc", "relative/repo", "/work/repo/.."] {
            let mut l = FolderLookup::new(cwd, None).with_interval(Duration::ZERO);
            let refused = l.poll(&d.locator, Instant::now(), SystemTime::now());
            assert!(
                matches!(
                    refused,
                    Err(Error::UnsafeCwd { .. } | Error::RelativeCwd { .. })
                ),
                "{cwd}: {refused:?}"
            );
        }
        // Whatever the cwd, its slug names a direct child of the projects
        // directory.
        let slug = project_slug(Path::new("/a/../../b/./c\\d"));
        assert!(
            slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "{slug}"
        );
    }

    #[test]
    fn a_long_cwd_is_never_guessed() {
        let d = dir();
        let cwd = PathBuf::from(format!("/{}", "d".repeat(300)));
        let mut l = FolderLookup::new(&cwd, None).with_interval(Duration::ZERO);
        assert_eq!(
            l.poll(&d.locator, Instant::now(), SystemTime::now())
                .expect("poll"),
            None
        );
    }
}
