//! Finding a session's transcript under `~/.claude/projects`.
//!
//! Claude Code stores each session at
//! `<config>/projects/<cwd-slug>/<session>.jsonl`, and the transcripts of
//! subagents it spawned under `<config>/projects/<cwd-slug>/<session>/subagents/`
//! (directly, or in `workflows/wf_*/` for workflow agents) as `agent-*.jsonl`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::session::SessionId;

/// Claude Code keeps slugs up to this length verbatim; longer ones are cut
/// to it and get a hash suffix that is not reproducible here, so they are
/// only ever found by scanning (see [`Locator::find`]). This also keeps every
/// path this module builds well under the 255-byte file name limit.
pub(crate) const MAX_VERBATIM_SLUG: usize = 200;

/// How deep [`Locator::subagents`] looks below `subagents/`
/// (`workflows/wf_*/agent-*.jsonl` is two levels down).
const MAX_SUBAGENT_DEPTH: usize = 4;

/// Default minimum time between directory scans in a [`Lookup`].
pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(5);

/// The project directory name Claude Code derives from a working directory.
///
/// Every character other than an ASCII letter or digit becomes `-`, one per
/// UTF-16 code unit (Claude Code is JavaScript), so
/// `/home/dev/git/gadget/.worktrees/login` becomes
/// `-home-dev-git-gadget--worktrees-login`. Slugs longer than
/// 200 characters are stored under a hashed name instead; see
/// [`Locator::transcript_path`].
#[must_use]
pub fn project_slug(cwd: &Path) -> String {
    let mut slug = String::new();
    for c in cwd.to_string_lossy().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else {
            slug.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    slug
}

/// `<$var>/<sub>` when the environment variable `var` is set and not
/// empty, else `<$HOME>/<home_dir>/<sub>`: how Claude Code
/// (`CLAUDE_CONFIG_DIR`, `~/.claude`) and Codex (`CODEX_HOME`, `~/.codex`)
/// find their own directories.
pub(crate) fn config_dir(var: &str, home_dir: &str, sub: &str) -> Result<PathBuf> {
    let non_empty = |key| std::env::var_os(key).filter(|v: &OsString| !v.is_empty());
    if let Some(config) = non_empty(var) {
        return Ok(PathBuf::from(config).join(sub));
    }
    let home = non_empty("HOME").ok_or(Error::NoConfigDir)?;
    Ok(PathBuf::from(home).join(home_dir).join(sub))
}

/// Resolves transcript paths inside one Claude projects directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locator {
    projects: PathBuf,
}

impl Locator {
    /// A locator over an explicit projects directory (e.g. a test fixture).
    #[must_use]
    pub fn new(projects_dir: impl Into<PathBuf>) -> Self {
        Self {
            projects: projects_dir.into(),
        }
    }

    /// The projects directory Claude Code itself uses:
    /// `$CLAUDE_CONFIG_DIR/projects`, else `$HOME/.claude/projects`.
    pub fn from_env() -> Result<Self> {
        config_dir("CLAUDE_CONFIG_DIR", ".claude", "projects").map(Self::new)
    }

    /// The projects directory this locator searches.
    #[must_use]
    pub fn projects_dir(&self) -> &Path {
        &self.projects
    }

    /// Where the transcript for `session` started in `cwd` lives, whether or
    /// not it exists yet (a fresh session writes its file lazily).
    ///
    /// `None` when the slug is too long to be stored verbatim: Claude Code
    /// then uses a hashed directory name that only [`find`](Self::find) can
    /// discover.
    pub fn transcript_path(&self, cwd: &Path, session: &SessionId) -> Result<Option<PathBuf>> {
        if !cwd.is_absolute() {
            return Err(Error::RelativeCwd { path: cwd.into() });
        }
        let slug = project_slug(cwd);
        Ok((slug.len() <= MAX_VERBATIM_SLUG)
            .then(|| self.projects.join(slug).join(file_name(session))))
    }

    /// The existing transcript for `session`, if any.
    ///
    /// Tries the derived path first (when there is one), then scans the
    /// project directories for `<session>.jsonl`, preferring names that share
    /// the slug's first 200 characters (hashed long slugs, renamed
    /// checkouts). Session ids are UUIDs, so a hit is unambiguous.
    ///
    /// A scan lists every project directory; a caller waiting for a file to
    /// appear should poll through a [`Lookup`], which throttles scans.
    pub fn find(&self, cwd: &Path, session: &SessionId) -> Result<Option<PathBuf>> {
        if let Some(direct) = self.direct(cwd, session)? {
            return Ok(Some(direct));
        }
        self.scan(cwd, session)
    }

    /// The derived path, if it exists.
    fn direct(&self, cwd: &Path, session: &SessionId) -> Result<Option<PathBuf>> {
        Ok(self
            .transcript_path(cwd, session)?
            .filter(|path| is_file(path)))
    }

    fn scan(&self, cwd: &Path, session: &SessionId) -> Result<Option<PathBuf>> {
        let slug = project_slug(cwd);
        let prefix = slug.get(..MAX_VERBATIM_SLUG).unwrap_or(&slug);
        let entries = match fs::read_dir(&self.projects) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::io("listing", &self.projects, err)),
        };
        let mut fallback = None;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io("listing", &self.projects, e))?;
            let candidate = entry.path().join(file_name(session));
            if !is_file(&candidate) {
                continue;
            }
            if entry.file_name().to_string_lossy().starts_with(prefix) {
                return Ok(Some(candidate));
            }
            fallback.get_or_insert(candidate);
        }
        Ok(fallback)
    }

    /// Transcripts of subagents spawned by the session at `transcript`:
    /// every `agent-*.jsonl` under `<session>/subagents/`, including
    /// workflow agents in `workflows/wf_*/`. Other files there (`*.meta.json`,
    /// a workflow's `journal.jsonl`) are not transcripts and are skipped, as
    /// are symlinks. Sorted by path.
    pub fn subagents(transcript: &Path) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        collect_agents(
            &transcript.with_extension("").join("subagents"),
            MAX_SUBAGENT_DEPTH,
            &mut found,
        )?;
        found.sort();
        Ok(found)
    }
}

fn collect_agents(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::io("listing", dir, err)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("listing", dir, e))?;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && depth > 1 {
            collect_agents(&path, depth - 1, found)?;
        } else if kind.is_file() && is_agent_transcript(&entry.file_name().to_string_lossy()) {
            found.push(path);
        }
    }
    Ok(())
}

fn is_agent_transcript(name: &str) -> bool {
    name.strip_prefix("agent-")
        .and_then(|rest| rest.strip_suffix(".jsonl"))
        .is_some_and(|id| !id.is_empty() && !id.contains('.'))
}

/// Polls for one session's transcript while it may not exist yet, without
/// rescanning the projects directory on every call: the derived path is
/// checked each time (one `lstat`), a full scan at most once per interval.
/// Once found, the transcript is kept.
#[derive(Debug, Clone)]
pub struct Lookup {
    cwd: PathBuf,
    session: SessionId,
    throttle: Throttle,
    found: Option<PathBuf>,
}

/// At most one scan per interval, for [`Lookup`] and
/// [`CodexLookup`](crate::CodexLookup).
#[derive(Debug, Clone)]
pub(crate) struct Throttle {
    interval: Duration,
    last: Option<Instant>,
}

impl Default for Throttle {
    fn default() -> Self {
        Self {
            interval: DEFAULT_SCAN_INTERVAL,
            last: None,
        }
    }
}

impl Throttle {
    /// Sets the minimum time between scans.
    pub(crate) fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Whether a scan may run at `now`; if so, it counts as run.
    pub(crate) fn due(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < self.interval)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

impl Lookup {
    /// Looks for `session`'s transcript, started in `cwd`, scanning at most
    /// every [`DEFAULT_SCAN_INTERVAL`].
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, session: SessionId) -> Self {
        Self {
            cwd: cwd.into(),
            session,
            throttle: Throttle::default(),
            found: None,
        }
    }

    /// Overrides the minimum time between scans.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.throttle.set_interval(interval);
        self
    }

    /// The transcript, if it has been found (it is then kept) or can be
    /// found now. `now` is passed in so tests (and callers with their own
    /// clock) control throttling.
    pub fn poll(&mut self, locator: &Locator, now: Instant) -> Result<Option<PathBuf>> {
        if self.found.is_none() {
            self.found = match locator.direct(&self.cwd, &self.session)? {
                Some(direct) => Some(direct),
                None if self.throttle.due(now) => locator.scan(&self.cwd, &self.session)?,
                None => None,
            };
        }
        Ok(self.found.clone())
    }
}

fn file_name(session: &SessionId) -> String {
    format!("{session}.jsonl")
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> SessionId {
        SessionId::new(id).expect("valid id")
    }

    #[test]
    fn slug_matches_claude_directory_names() {
        let cases = [
            (
                "/home/dev/git/gadget/.worktrees/login",
                "-home-dev-git-gadget--worktrees-login",
            ),
            ("/home/dev/git/passporter", "-home-dev-git-passporter"),
            (
                "/home/dev/git/gadget/docs_site",
                "-home-dev-git-gadget-docs-site",
            ),
            ("/tmp/café", "-tmp-caf-"),
            ("/tmp/🦀", "-tmp---"),
        ];
        for (cwd, slug) in cases {
            assert_eq!(project_slug(Path::new(cwd)), slug, "{cwd}");
        }
    }

    #[test]
    fn transcript_path_requires_an_absolute_cwd() {
        let locator = Locator::new("/p");
        let id = session("abc");
        assert_eq!(
            locator
                .transcript_path(Path::new("/w/x"), &id)
                .expect("absolute"),
            Some(PathBuf::from("/p/-w-x/abc.jsonl"))
        );
        let long = PathBuf::from(format!("/{}", "d".repeat(400)));
        assert_eq!(
            locator.transcript_path(&long, &id).expect("long"),
            None,
            "a hashed slug is never guessed"
        );
        assert!(matches!(
            locator.transcript_path(Path::new("w/x"), &id),
            Err(Error::RelativeCwd { .. })
        ));
    }

    #[test]
    fn find_falls_back_to_scanning() {
        let root = tempfile::tempdir().expect("temp dir");
        let projects = root.path().join("projects");
        let id = session("1111-2222");
        let locator = Locator::new(&projects);

        assert_eq!(locator.find(Path::new("/w"), &id).expect("scan"), None);

        fs::create_dir_all(projects.join("-w")).expect("mkdir");
        fs::create_dir_all(projects.join("-elsewhere")).expect("mkdir");
        fs::write(projects.join("-elsewhere/1111-2222.jsonl"), "").expect("write");
        assert_eq!(
            locator.find(Path::new("/w"), &id).expect("scan"),
            Some(projects.join("-elsewhere/1111-2222.jsonl"))
        );

        fs::write(projects.join("-w/1111-2222.jsonl"), "").expect("write");
        assert_eq!(
            locator.find(Path::new("/w"), &id).expect("direct"),
            Some(projects.join("-w/1111-2222.jsonl"))
        );
    }

    #[test]
    fn find_prefers_the_slug_prefix_for_long_paths() {
        let root = tempfile::tempdir().expect("temp dir");
        let projects = root.path().join("projects");
        let cwd = PathBuf::from(format!("/{}", "d".repeat(300)));
        let slug = project_slug(&cwd);
        let hashed = format!("{}-1a2b3c", &slug[..MAX_VERBATIM_SLUG]);
        for dir in ["-other", hashed.as_str()] {
            fs::create_dir_all(projects.join(dir)).expect("mkdir");
            fs::write(projects.join(dir).join("s.jsonl"), "").expect("write");
        }
        let found = Locator::new(&projects)
            .find(&cwd, &session("s"))
            .expect("scan");
        assert_eq!(found, Some(projects.join(hashed).join("s.jsonl")));
    }

    #[test]
    fn lists_subagent_transcripts() {
        let root = tempfile::tempdir().expect("temp dir");
        let transcript = root.path().join("sess.jsonl");
        assert_eq!(
            Locator::subagents(&transcript).expect("missing ok"),
            Vec::<PathBuf>::new()
        );

        let dir = root.path().join("sess/subagents");
        let wf = dir.join("workflows/wf_02bf-44f");
        fs::create_dir_all(&wf).expect("mkdir");
        for name in ["agent-b.jsonl", "agent-a.jsonl", "agent-a.meta.json"] {
            fs::write(dir.join(name), "").expect("write");
        }
        for name in ["agent-c.jsonl", "journal.jsonl"] {
            fs::write(wf.join(name), "").expect("write");
        }
        std::os::unix::fs::symlink(dir.join("agent-a.jsonl"), dir.join("agent-z.jsonl"))
            .expect("symlink");
        assert_eq!(
            Locator::subagents(&transcript).expect("list"),
            vec![
                dir.join("agent-a.jsonl"),
                dir.join("agent-b.jsonl"),
                wf.join("agent-c.jsonl")
            ]
        );
    }

    #[test]
    fn lookup_throttles_scans() {
        let root = tempfile::tempdir().expect("temp dir");
        let projects = root.path().join("projects");
        let locator = Locator::new(&projects);
        let t0 = Instant::now();
        let mut lookup = Lookup::new("/w", session("s1")).with_interval(Duration::from_secs(10));
        assert_eq!(lookup.poll(&locator, t0).expect("scan"), None);

        // Appears under another project dir: only a scan can find it.
        fs::create_dir_all(projects.join("-renamed")).expect("mkdir");
        fs::write(projects.join("-renamed/s1.jsonl"), "").expect("write");
        let soon = t0 + Duration::from_secs(1);
        assert_eq!(lookup.poll(&locator, soon).expect("throttled"), None);
        let later = t0 + Duration::from_secs(11);
        assert_eq!(
            lookup.poll(&locator, later).expect("scan"),
            Some(projects.join("-renamed/s1.jsonl"))
        );

        // The derived path is checked on every poll, throttle or not.
        let mut direct = Lookup::new("/w", session("s2")).with_interval(Duration::from_secs(10));
        assert_eq!(direct.poll(&locator, t0).expect("scan"), None);
        fs::create_dir_all(projects.join("-w")).expect("mkdir");
        fs::write(projects.join("-w/s2.jsonl"), "").expect("write");
        assert_eq!(
            direct.poll(&locator, soon).expect("direct"),
            Some(projects.join("-w/s2.jsonl"))
        );
    }
}
