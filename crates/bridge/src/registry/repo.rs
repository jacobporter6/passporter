//! Which repository and worktree an agent works in, from its cwd.
//!
//! Walks up from the cwd to the nearest `.git`:
//! - a `.git` **directory** marks a main checkout: the repo is that
//!   directory's name;
//! - a `.git` **file** (`gitdir: <main>/.git/worktrees/<name>`) marks a linked
//!   worktree: the repo is the main checkout's name and the worktree is the
//!   linked directory's name. Worktrees of a bare `<repo>.git` work too;
//! - any other `.git` file (a submodule) counts as a plain checkout.
//!
//! Without a `.git` the repo is the cwd's own name. Only `.git` entries are
//! read (at most [`MAX_GITFILE_BYTES`] each), never repository contents.
//!
//! Lookups are blocking file system access, so [`RepoCache`] runs them on
//! the blocking pool, a few at a time, and waits a bounded time: a cwd on a
//! hung (e.g. network) file system stays unknown instead of stalling the
//! registry. Known locations are re-checked in the background and keep
//! serving until the new answer arrives (stale-while-revalidate); a result
//! that arrives late is reported through [`RepoCache::collect`] so the
//! affected agents can be re-mapped.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// A `.git` file is one short line; anything longer is not one.
const MAX_GITFILE_BYTES: u64 = 4096;

/// An agent's repository and, for a linked worktree, the worktree's name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RepoLocation {
    /// The main checkout's directory name; empty when unknown.
    pub(crate) repo: String,
    /// The linked worktree's directory name.
    pub(crate) worktree: Option<String>,
}

/// Locates `cwd`'s repository (blocking file system access).
pub(crate) fn locate(cwd: &Path) -> RepoLocation {
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        match fs::symlink_metadata(&dot_git) {
            Ok(meta) if meta.is_dir() => return plain(dir),
            Ok(meta) if meta.is_file() => return from_gitfile(dir, &dot_git),
            _ => {}
        }
    }
    plain(cwd)
}

fn plain(dir: &Path) -> RepoLocation {
    RepoLocation {
        repo: name(dir),
        worktree: None,
    }
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Reads `gitdir: <path>` from a `.git` file.
fn read_gitdir(dot_git: &Path) -> Option<PathBuf> {
    let mut text = String::new();
    fs::File::open(dot_git)
        .ok()?
        .take(MAX_GITFILE_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    (!target.is_empty()).then(|| PathBuf::from(target))
}

fn from_gitfile(dir: &Path, dot_git: &Path) -> RepoLocation {
    let Some(gitdir) = read_gitdir(dot_git) else {
        return plain(dir);
    };
    // Relative gitdirs (`worktree.useRelativePaths`) climb with `..`, which
    // `Path::parent` would treat as a name; resolve them first.
    let gitdir = normalize(&dir.join(gitdir));
    // <main>/.git/worktrees/<name>, or <repo>.git/worktrees/<name> for a bare repo.
    let admin = gitdir
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "worktrees"))
        .and_then(Path::parent);
    let repo = admin.and_then(|git_dir| match git_dir.file_name()?.to_str()? {
        ".git" => git_dir.parent().map(name),
        bare => bare.strip_suffix(".git").map(str::to_owned),
    });
    match repo {
        Some(repo) if !repo.is_empty() => RepoLocation {
            repo,
            worktree: Some(name(dir)),
        },
        _ => plain(dir),
    }
}

/// Resolves `.` and `..` lexically, without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let at_root = matches!(out.components().next_back(), Some(Component::RootDir));
                if !at_root && !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The longest a batch of first-time lookups may hold up the registry; also
/// how long a lookup may run before it stops counting against the limit.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// At most this many (non-overdue) lookups run at once; more wait in a queue.
const MAX_IN_FLIGHT: usize = 8;

/// Where blocking lookups drop their results, and the bell they ring.
#[derive(Debug, Default)]
pub(crate) struct Completions {
    done: Mutex<Vec<(PathBuf, RepoLocation)>>,
    ready: Notify,
}

impl Completions {
    /// Resolves when a lookup has finished since the last [`RepoCache::collect`].
    pub(crate) async fn notified(&self) {
        self.ready.notified().await;
    }

    fn push(&self, cwd: PathBuf, location: RepoLocation) {
        self.done
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((cwd, location));
        self.ready.notify_one();
    }

    fn take(&self) -> Vec<(PathBuf, RepoLocation)> {
        std::mem::take(&mut *self.done.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Memoizes [`locate`] per cwd, re-checking on request.
#[derive(Debug)]
pub(crate) struct RepoCache {
    known: HashMap<PathBuf, RepoLocation>,
    /// Running lookups and when each started. One that has run longer than
    /// the timeout is overdue: it stays here (so it is never started twice)
    /// but no longer counts against [`MAX_IN_FLIGHT`].
    running: HashMap<PathBuf, Instant>,
    queued: VecDeque<PathBuf>,
    completions: Arc<Completions>,
    locate: fn(&Path) -> RepoLocation,
    timeout: Duration,
}

impl Default for RepoCache {
    fn default() -> Self {
        Self::with_locator(locate, LOOKUP_TIMEOUT)
    }
}

impl RepoCache {
    fn with_locator(locate: fn(&Path) -> RepoLocation, timeout: Duration) -> Self {
        Self {
            known: HashMap::new(),
            running: HashMap::new(),
            queued: VecDeque::new(),
            completions: Arc::new(Completions::default()),
            locate,
            timeout,
        }
    }

    /// The bell late results ring; follow it with [`collect`](Self::collect).
    pub(crate) fn completions(&self) -> Arc<Completions> {
        Arc::clone(&self.completions)
    }

    /// Makes sure every cwd gets looked up: unknown ones are waited for (at
    /// most the lookup timeout in total); with `revalidate`, known ones are
    /// re-checked in the background while their old value keeps serving.
    /// Returns the cwds whose location changed meanwhile.
    pub(crate) async fn resolve<'a>(
        &mut self,
        cwds: impl IntoIterator<Item = &'a Path>,
        revalidate: bool,
    ) -> Vec<PathBuf> {
        let mut unknown = Vec::new();
        for cwd in cwds {
            if !self.known.contains_key(cwd) {
                unknown.push(cwd.to_path_buf());
            } else if !revalidate {
                continue;
            }
            self.start_or_queue(cwd);
        }
        let deadline = Instant::now() + self.timeout;
        let mut changed = Vec::new();
        loop {
            changed.extend(self.collect());
            if unknown.iter().all(|cwd| self.known.contains_key(cwd)) {
                break;
            }
            if tokio::time::timeout_at(deadline, self.completions.notified())
                .await
                .is_err()
            {
                tracing::warn!("repository lookups are slow; leaving some unknown for now");
                break;
            }
        }
        // Lookups that just went overdue free their slots for queued cwds.
        self.pump();
        changed
    }

    /// Applies finished lookups and starts queued ones. Returns the cwds
    /// whose location changed.
    pub(crate) fn collect(&mut self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        for (cwd, location) in self.completions.take() {
            self.running.remove(&cwd);
            if self.known.get(&cwd) != Some(&location) {
                self.known.insert(cwd.clone(), location);
                changed.push(cwd);
            }
        }
        self.pump();
        changed
    }

    /// The location found for `cwd`; unknown (empty) when herdr reported no
    /// cwd or no lookup has finished yet.
    pub(crate) fn get(&self, cwd: Option<&Path>) -> RepoLocation {
        cwd.and_then(|cwd| self.known.get(cwd))
            .cloned()
            .unwrap_or_default()
    }

    fn active(&self) -> usize {
        self.running
            .values()
            .filter(|started| started.elapsed() < self.timeout)
            .count()
    }

    fn start_or_queue(&mut self, cwd: &Path) {
        if self.running.contains_key(cwd) || self.queued.iter().any(|q| q == cwd) {
            return;
        }
        if self.active() < MAX_IN_FLIGHT {
            self.start(cwd.to_path_buf());
        } else {
            self.queued.push_back(cwd.to_path_buf());
        }
    }

    fn pump(&mut self) {
        while self.active() < MAX_IN_FLIGHT {
            let Some(cwd) = self.queued.pop_front() else {
                break;
            };
            if !self.running.contains_key(&cwd) {
                self.start(cwd);
            }
        }
    }

    fn start(&mut self, cwd: PathBuf) {
        self.running.insert(cwd.clone(), Instant::now());
        let locate = self.locate;
        let completions = Arc::clone(&self.completions);
        // Detached: a lookup stuck in the kernel cannot be cancelled anyway.
        drop(tokio::task::spawn_blocking(move || {
            let location = locate(&cwd);
            completions.push(cwd, location);
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(repo: &str, worktree: Option<&str>) -> RepoLocation {
        RepoLocation {
            repo: repo.to_owned(),
            worktree: worktree.map(str::to_owned),
        }
    }

    #[test]
    fn main_checkout_from_a_subdirectory() {
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("passporter");
        fs::create_dir_all(repo.join(".git")).expect("git dir");
        fs::create_dir_all(repo.join("crates/bridge")).expect("subdir");
        assert_eq!(locate(&repo.join("crates/bridge")), loc("passporter", None));
        assert_eq!(locate(&repo), loc("passporter", None));
    }

    #[test]
    fn linked_worktree_with_absolute_gitdir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let main = tmp.path().join("passporter");
        let admin = main.join(".git/worktrees/agent-1");
        fs::create_dir_all(&admin).expect("admin");
        let wt = main.join(".claude/worktrees/agent-1");
        fs::create_dir_all(wt.join("src")).expect("wt");
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).expect("gitfile");
        assert_eq!(locate(&wt.join("src")), loc("passporter", Some("agent-1")));
    }

    #[test]
    fn linked_worktree_with_relative_gitdir_and_bare_repo() {
        let tmp = tempfile::tempdir().expect("tmp");
        fs::create_dir_all(tmp.path().join("home.git/worktrees/heat")).expect("bare");
        let wt = tmp.path().join("login");
        fs::create_dir_all(&wt).expect("wt");
        fs::write(wt.join(".git"), "gitdir: ../home.git/worktrees/heat").expect("gitfile");
        assert_eq!(locate(&wt), loc("home", Some("login")));
    }

    #[test]
    fn submodule_gitfile_is_a_plain_checkout() {
        let tmp = tempfile::tempdir().expect("tmp");
        let sub = tmp.path().join("outer/vendor/lib");
        fs::create_dir_all(&sub).expect("sub");
        fs::write(sub.join(".git"), "gitdir: ../../.git/modules/lib").expect("gitfile");
        assert_eq!(locate(&sub), loc("lib", None));
    }

    #[test]
    fn malformed_gitfile_is_a_plain_checkout() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("odd");
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(".git"), "not a gitdir line").expect("gitfile");
        assert_eq!(locate(&dir), loc("odd", None));
    }

    #[test]
    fn no_repository_uses_the_cwd_name() {
        // A temp dir may itself sit inside a checkout, so test the pure fallback.
        assert_eq!(plain(Path::new("/home/me/scratch")), loc("scratch", None));
        assert_eq!(locate(Path::new("/")), loc("", None));
    }

    #[test]
    fn linked_worktree_with_ancestor_relative_gitdir() {
        // git >= 2.48 with worktree.useRelativePaths, as this repo's agents use.
        let tmp = tempfile::tempdir().expect("tmp");
        let main = tmp.path().join("passporter");
        fs::create_dir_all(main.join(".git/worktrees/agent-1")).expect("admin");
        let wt = main.join(".claude/worktrees/agent-1");
        fs::create_dir_all(&wt).expect("wt");
        fs::write(wt.join(".git"), "gitdir: ../../../.git/worktrees/agent-1\n").expect("gitfile");
        assert_eq!(locate(&wt), loc("passporter", Some("agent-1")));
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(
            normalize(Path::new("/a/b/./c/../../d")),
            PathBuf::from("/a/d")
        );
        assert_eq!(normalize(Path::new("../x")), PathBuf::from("../x"));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
    }

    #[tokio::test]
    async fn known_values_serve_while_revalidating() {
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("r");
        fs::create_dir_all(repo.join(".git")).expect("git");
        let mut cache = RepoCache::default();
        cache.resolve([repo.as_path()], false).await;
        assert_eq!(cache.get(Some(&repo)), loc("r", None));
        fs::remove_dir(repo.join(".git")).expect("rm");
        fs::write(repo.join(".git"), "gitdir: /x/main/.git/worktrees/r").expect("gitfile");
        cache.resolve([repo.as_path()], false).await;
        assert_eq!(cache.get(Some(&repo)), loc("r", None), "cached");
        // Revalidation keeps the old value until the new one lands.
        let mut changed = cache.resolve([repo.as_path()], true).await;
        assert!(!cache.get(Some(&repo)).repo.is_empty(), "never blank");
        tokio::time::timeout(Duration::from_secs(2), async {
            while cache.get(Some(&repo)) != loc("main", Some("r")) {
                cache.completions().notified().await;
                changed.extend(cache.collect());
            }
        })
        .await
        .expect("revalidated");
        assert_eq!(changed, std::slice::from_ref(&repo));
        assert_eq!(cache.get(None), RepoLocation::default());
    }

    fn slow_locate(cwd: &Path) -> RepoLocation {
        std::thread::sleep(Duration::from_millis(150));
        plain(cwd)
    }

    fn cwds(n: usize) -> Vec<PathBuf> {
        (0..n).map(|i| PathBuf::from(format!("/w/r{i}"))).collect()
    }

    #[tokio::test]
    async fn lookups_run_concurrently() {
        let mut cache = RepoCache::with_locator(slow_locate, Duration::from_secs(5));
        let all = cwds(4);
        let started = Instant::now();
        cache.resolve(all.iter().map(PathBuf::as_path), false).await;
        assert!(
            started.elapsed() < Duration::from_millis(450),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(cache.get(Some(Path::new("/w/r3"))), loc("r3", None));
    }

    #[tokio::test]
    async fn cwds_over_the_limit_are_queued_not_dropped() {
        let mut cache = RepoCache::with_locator(slow_locate, Duration::from_secs(5));
        let all = cwds(MAX_IN_FLIGHT + 4);
        cache.resolve(all.iter().map(PathBuf::as_path), false).await;
        for cwd in &all {
            assert_eq!(cache.get(Some(cwd)).repo, name(cwd), "{}", cwd.display());
        }
    }

    #[tokio::test]
    async fn a_slow_lookup_is_left_unknown_then_reported() {
        let mut cache = RepoCache::with_locator(slow_locate, Duration::from_millis(20));
        let cwd = Path::new("/nfs/slow");
        let changed = cache.resolve([cwd], false).await;
        assert_eq!(changed, Vec::<PathBuf>::new());
        assert_eq!(
            cache.get(Some(cwd)),
            RepoLocation::default(),
            "unknown for now"
        );
        assert_eq!(cache.active(), 0, "an overdue lookup frees its slot");
        tokio::time::timeout(Duration::from_secs(1), cache.completions().notified())
            .await
            .expect("late result rings");
        assert_eq!(cache.collect(), [cwd.to_path_buf()], "reported as changed");
        assert_eq!(cache.get(Some(cwd)), loc("slow", None));
    }
}
