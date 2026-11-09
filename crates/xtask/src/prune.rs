//! `cargo xtask prune`: reclaim build output no live work needs.
//!
//! Safety rules, all covered by tests:
//! - only ever deletes `target/` of untracked `.worktrees/*` entries, or
//!   per-crate dirs inside `target/**/incremental/`;
//! - never follows a symlink (a symlinked `target/` or worktree is skipped);
//! - tolerates agents creating and deleting files mid-scan;
//! - a path it can't inspect is skipped with a warning, never a reason to stop;
//! - one failed deletion never stops the rest, and every failure is reported.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::worktrees;

const DAY: Duration = Duration::from_hours(24);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) dry_run: bool,
    /// Incremental caches untouched for longer than this are removed.
    pub(crate) max_age: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dry_run: false,
            max_age: 3 * DAY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    /// `target/` of a `.worktrees/*` directory git no longer tracks.
    Orphaned,
    /// A per-crate incremental-compilation cache older than `max_age`.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Removal {
    pub(crate) path: PathBuf,
    pub(crate) reason: Reason,
    pub(crate) bytes: u64,
}

/// What to remove, plus paths that couldn't be inspected and were left alone.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub(crate) removals: Vec<Removal>,
    pub(crate) skipped: Vec<io::Error>,
}

#[derive(Debug, Default)]
pub(crate) struct Report {
    removed: Vec<Removal>,
    failed: Vec<(Removal, io::Error)>,
    skipped: Vec<io::Error>,
    dry_run: bool,
}

impl Report {
    pub(crate) fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }
}

pub(crate) fn run(root: &Path, opts: &Options) -> io::Result<Report> {
    let live = worktrees::live(root)?;
    let plan = plan(root, &live, SystemTime::now(), opts.max_age);
    let mut report = execute(plan.removals, opts.dry_run, opts.max_age);
    report.skipped = plan.skipped;
    Ok(report)
}

/// Decides what to remove without touching anything.
pub(crate) fn plan(
    root: &Path,
    live: &BTreeSet<PathBuf>,
    now: SystemTime,
    max_age: Duration,
) -> Plan {
    let live: BTreeSet<PathBuf> = live.iter().map(|p| canonical(p)).collect();
    let mut plan = Plan::default();

    for dir in plan.ok(subdirs(&root.join(".worktrees"))) {
        let target = dir.join("target");
        if !live.contains(&canonical(&dir)) && plan.ok(is_real_dir(&target)) {
            plan.consider(target, Reason::Orphaned);
        }
    }

    for worktree in &live {
        let target = worktree.join("target");
        if !plan.ok(is_real_dir(&target)) {
            continue;
        }
        for incremental in incremental_dirs(&target, &mut plan) {
            for crate_cache in plan.ok(subdirs(&incremental)) {
                if plan.ok(is_older_than(&crate_cache, now, max_age)) {
                    plan.consider(crate_cache, Reason::Stale);
                }
            }
        }
    }

    plan
}

impl Plan {
    /// The value, or its default after recording the error as a skipped path.
    fn ok<T: Default>(&mut self, result: io::Result<T>) -> T {
        result.unwrap_or_else(|err| {
            self.skipped.push(err);
            T::default()
        })
    }

    /// Plans removing `path` unless it vanished or can't be measured.
    fn consider(&mut self, path: PathBuf, reason: Reason) {
        match size_of(&path) {
            Ok(bytes) => self.removals.push(Removal {
                path,
                reason,
                bytes,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => self.skipped.push(e),
        }
    }
}

/// Deletes each planned path, continuing past failures.
///
/// Measuring a large plan takes time, and a build may resume a crate cache
/// meanwhile, so stale caches are re-checked right before deletion.
pub(crate) fn execute(removals: Vec<Removal>, dry_run: bool, max_age: Duration) -> Report {
    let mut report = Report {
        dry_run,
        ..Report::default()
    };
    for removal in removals {
        if dry_run {
            report.removed.push(removal);
            continue;
        }
        if removal.reason == Reason::Stale {
            match is_older_than(&removal.path, SystemTime::now(), max_age) {
                Ok(true) => {}
                Ok(false) => continue, // touched since planning: in use again
                Err(e) => {
                    report.skipped.push(e);
                    continue;
                }
            }
        }
        match fs::remove_dir_all(&removal.path) {
            Ok(()) => report.removed.push(removal),
            // Gone before we got to it: nothing reclaimed by us, and not a failure.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                report.removed.push(Removal {
                    bytes: 0,
                    ..removal
                });
            }
            Err(e) => report.failed.push((removal, e)),
        }
    }
    report
}

/// `target/<profile>/incremental` and `target/<triple>/<profile>/incremental`.
/// An unreadable entry is recorded in `plan` and skipped; its siblings are still scanned.
fn incremental_dirs(target: &Path, plan: &mut Plan) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for level1 in plan.ok(subdirs(target)) {
        // List first: if level1 is unreadable, record it once and move on.
        let children = match subdirs(&level1) {
            Ok(children) => children,
            Err(e) => {
                plan.skipped.push(e);
                continue;
            }
        };
        let direct = level1.join("incremental");
        if plan.ok(is_real_dir(&direct)) {
            found.push(direct);
        }
        for level2 in children {
            let nested = level2.join("incremental");
            if plan.ok(is_real_dir(&nested)) {
                found.push(nested);
            }
        }
    }
    found
}

/// Real (non-symlink) subdirectories, sorted. A missing `dir` has none.
fn subdirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(with_path(&e, dir)),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| with_path(&e, dir))?;
        // file_type() does not follow symlinks.
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => dirs.push(entry.path()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(with_path(&e, &entry.path())),
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// True for a directory that is not a symlink; false if missing.
fn is_real_dir(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_dir()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(with_path(&e, path)),
    }
}

fn is_older_than(path: &Path, now: SystemTime, max_age: Duration) -> io::Result<bool> {
    let modified = match fs::symlink_metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => modified,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(with_path(&e, path)),
    };
    // A timestamp in the future is not stale.
    Ok(now.duration_since(modified).is_ok_and(|age| age > max_age))
}

/// Total size of a tree, not following symlinks. Entries that vanish mid-walk
/// count as zero; any other error names the exact entry that failed.
fn size_of(path: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(path).map_err(|e| with_path(&e, path))?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let entries = fs::read_dir(path).map_err(|e| with_path(&e, path))?;
    let mut total = 0;
    for entry in entries {
        match entry
            .map_err(|e| with_path(&e, path))
            .and_then(|e| size_of(&e.path()))
        {
            Ok(bytes) => total += bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn with_path(err: &io::Error, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {err}", path.display()))
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = if self.dry_run {
            "would remove"
        } else {
            "removed"
        };
        for r in &self.removed {
            let size = human(r.bytes);
            writeln!(f, "{verb} {size:>10}  {}  {}", r.reason, r.path.display())?;
        }
        for err in &self.skipped {
            writeln!(f, "skipped (could not inspect): {err}")?;
        }
        for (r, err) in &self.failed {
            let size = human(r.bytes);
            writeln!(
                f,
                "FAILED {size:>10}  {}  {}: {err}",
                r.reason,
                r.path.display()
            )?;
        }
        if self.removed.is_empty() && self.failed.is_empty() {
            return writeln!(f, "prune: nothing to remove");
        }
        let total = human(self.removed.iter().map(|r| r.bytes).sum());
        write!(f, "prune: {verb} {total} in {} paths", self.removed.len())?;
        if self.failed.is_empty() {
            writeln!(f)
        } else {
            writeln!(f, "; {} failed", self.failed.len())
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reason::Orphaned => "orphaned worktree build",
            Reason::Stale => "stale incremental cache",
        })
    }
}

#[allow(clippy::cast_precision_loss)] // Display only; two decimals is plenty.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::{PermissionsExt, symlink};

    /// A scratch directory (canonical path) removed on drop.
    struct Scratch(
        PathBuf,
        #[expect(dead_code, reason = "held only to delete the directory on drop")]
        tempfile::TempDir,
    );

    impl Scratch {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create scratch dir");
            Self(canonical(dir.path()), dir)
        }

        fn mkdir(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(&path).expect("mkdir");
            path
        }

        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir parent");
            fs::write(&path, vec![0_u8; bytes]).expect("write file");
            path
        }
    }

    fn age(path: &Path, by: Duration) {
        let when = SystemTime::now() - by;
        File::open(path)
            .expect("open")
            .set_modified(when)
            .expect("set mtime");
    }

    fn live(paths: &[&Path]) -> BTreeSet<PathBuf> {
        paths.iter().map(|p| p.to_path_buf()).collect()
    }

    fn paths(removals: &[Removal]) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = removals.iter().map(|r| r.path.clone()).collect();
        paths.sort();
        paths
    }

    fn removals(list: &[&PathBuf], reason: Reason) -> Vec<Removal> {
        list.iter()
            .map(|p| Removal {
                path: (*p).clone(),
                reason,
                bytes: 0,
            })
            .collect()
    }

    #[test]
    fn removes_target_of_untracked_worktree_dirs_only() {
        let s = Scratch::new();
        let tracked = s.mkdir(".worktrees/T11");
        s.file(".worktrees/T11/target/debug/app", 10);
        s.file(".worktrees/T12/target/debug/app", 2048);
        s.file(".worktrees/T12/src/main.rs", 5);

        let plan = plan(&s.0, &live(&[&s.0, &tracked]), SystemTime::now(), DAY);

        let expected = Removal {
            path: s.0.join(".worktrees/T12/target"),
            reason: Reason::Orphaned,
            bytes: 2048,
        };
        assert_eq!(plan.removals, vec![expected]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn untracked_dir_without_target_is_left_alone() {
        let s = Scratch::new();
        s.file(".worktrees/notes/todo.md", 3);
        let plan = plan(&s.0, &live(&[&s.0]), SystemTime::now(), DAY);
        assert_eq!(plan.removals, Vec::new());
    }

    #[test]
    fn removes_only_stale_per_crate_incremental_caches() {
        let s = Scratch::new();
        let old_host = s.mkdir("target/debug/incremental/bridge-old");
        let old_wasm = s.mkdir("target/wasm32-unknown-unknown/debug/incremental/web-old");
        let fresh = s.mkdir("target/debug/incremental/bridge-fresh");
        s.file("target/debug/incremental/bridge-old/dep-graph.bin", 100);
        age(&old_host, 5 * DAY);
        age(&old_wasm, 5 * DAY);
        age(&fresh, DAY / 2);

        let plan = plan(&s.0, &live(&[&s.0]), SystemTime::now(), 3 * DAY);

        assert!(plan.removals.iter().all(|r| r.reason == Reason::Stale));
        assert_eq!(paths(&plan.removals), vec![old_host, old_wasm]);
    }

    #[test]
    fn never_follows_a_symlinked_target() {
        let s = Scratch::new();
        // A tree outside the repo with a stale incremental cache in it.
        let elsewhere = s.mkdir("elsewhere/debug/incremental/precious");
        age(&elsewhere, 30 * DAY);
        let repo = s.mkdir("repo");
        symlink(s.0.join("elsewhere"), repo.join("target")).expect("symlink");
        // And an orphaned worktree whose target is a symlink to the same place.
        s.mkdir("repo/.worktrees/T13");
        symlink(s.0.join("elsewhere"), repo.join(".worktrees/T13/target")).expect("symlink");

        let plan = plan(&repo, &live(&[&repo]), SystemTime::now(), DAY);

        assert_eq!(plan.removals, Vec::new());
    }

    #[test]
    fn live_worktree_given_by_a_non_canonical_path_is_not_orphaned() {
        let s = Scratch::new();
        s.file(".worktrees/T11/target/debug/app", 10);
        let alias = s.0.join("alias");
        symlink(s.0.join(".worktrees"), &alias).expect("symlink");
        let dotted = s.0.join(".worktrees/./T11/../T11");

        for spelling in [alias.join("T11"), dotted] {
            let plan = plan(&s.0, &live(&[&s.0, &spelling]), SystemTime::now(), DAY);
            assert_eq!(
                plan.removals,
                Vec::new(),
                "{} treated as orphaned",
                spelling.display()
            );
        }
    }

    #[test]
    fn missing_dirs_are_not_errors() {
        let s = Scratch::new();
        let gone = s.0.join(".worktrees/prunable");
        let plan = plan(&s.0, &live(&[&s.0, &gone]), SystemTime::now(), DAY);
        assert_eq!(plan.removals, Vec::new());
        assert!(plan.skipped.is_empty());
    }

    /// Makes a directory unreadable until dropped (restored even if the test panics).
    struct Locked(PathBuf);

    impl Locked {
        /// `None` when permissions aren't enforced (running as root), so the test can bail.
        fn new(path: &Path) -> Option<Self> {
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("chmod");
            let guard = Self(path.to_path_buf());
            fs::read_dir(path).is_err().then_some(guard)
        }
    }

    impl Drop for Locked {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    #[test]
    fn unreadable_paths_are_skipped_not_fatal() {
        let s = Scratch::new();
        let locked = s.mkdir("target/debug/incremental/locked-crate");
        s.file(
            "target/debug/incremental/locked-crate/inner/dep-graph.bin",
            1,
        );
        let open = s.mkdir("target/debug/incremental/open-crate");
        age(&locked, 10 * DAY);
        age(&open, 10 * DAY);
        let Some(_guard) = Locked::new(&locked.join("inner")) else {
            return; // root ignores mode 000; nothing to test
        };

        let plan = plan(&s.0, &live(&[&s.0]), SystemTime::now(), DAY);

        assert_eq!(paths(&plan.removals), vec![open]);
        assert_eq!(plan.skipped.len(), 1);
        let err = plan.skipped[0].to_string();
        assert!(
            err.contains("locked-crate/inner"),
            "error names the failing entry: {err}"
        );
    }

    #[test]
    fn unreadable_profile_dir_does_not_hide_its_siblings() {
        let s = Scratch::new();
        s.mkdir("target/root-owned/incremental");
        let stale = s.mkdir("target/debug/incremental/bridge-old");
        age(&stale, 10 * DAY);
        let Some(_guard) = Locked::new(&s.0.join("target/root-owned")) else {
            return;
        };

        let plan = plan(&s.0, &live(&[&s.0]), SystemTime::now(), DAY);

        assert_eq!(paths(&plan.removals), vec![stale]);
        assert_eq!(plan.skipped.len(), 1);
    }

    #[test]
    fn live_worktree_replaced_by_a_file_is_skipped() {
        let s = Scratch::new();
        let file = s.file("was-a-worktree", 1);
        let plan = plan(&s.0, &live(&[&s.0, &file]), SystemTime::now(), DAY);
        assert_eq!(plan.removals, Vec::new());
        assert_eq!(plan.skipped.len(), 1, "ENOTDIR becomes a warning");
    }

    #[test]
    fn stale_cache_touched_after_planning_is_kept() {
        let s = Scratch::new();
        let cache = s.mkdir("target/debug/incremental/bridge-abc");
        age(&cache, 10 * DAY);
        let plan = plan(&s.0, &live(&[&s.0]), SystemTime::now(), DAY);
        assert_eq!(paths(&plan.removals), vec![cache.clone()]);

        age(&cache, Duration::ZERO); // a build resumed this crate
        let report = execute(plan.removals, false, DAY);

        assert!(cache.exists());
        assert_eq!(report.removed, Vec::new());
    }

    #[test]
    fn execute_deletes_and_dry_run_does_not() {
        let s = Scratch::new();
        let a = s.mkdir("a");
        let b = s.mkdir("b");

        let dry = execute(removals(&[&a, &b], Reason::Orphaned), true, DAY);
        assert!(a.exists() && b.exists());
        assert_eq!(dry.removed.len(), 2);

        let real = execute(removals(&[&a, &b], Reason::Orphaned), false, DAY);
        assert!(!a.exists() && !b.exists());
        assert_eq!(real.removed.len(), 2);
        assert!(!real.has_failures());
    }

    #[test]
    fn execute_continues_past_failures_and_reports_them() {
        let s = Scratch::new();
        let file = s.file("not-a-dir", 1); // remove_dir_all fails on a plain file
        let dir = s.mkdir("dir");
        let vanished = s.0.join("already-gone");
        let mut planned = removals(&[&file, &dir, &vanished], Reason::Orphaned);
        planned[2].bytes = 4096;

        let report = execute(planned, false, DAY);

        assert!(!dir.exists(), "later removals still run after a failure");
        assert_eq!(paths(&report.removed), vec![vanished.clone(), dir]);
        let gone = report
            .removed
            .iter()
            .find(|r| r.path == vanished)
            .expect("reported");
        assert_eq!(
            gone.bytes, 0,
            "nothing reclaimed for a path that was already gone"
        );
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0.path, file);
        assert!(report.to_string().contains("1 failed"));
    }

    #[test]
    fn report_formats_sizes_verb_and_skips() {
        let report = Report {
            removed: vec![Removal {
                path: PathBuf::from("/r/.worktrees/T12/target"),
                reason: Reason::Orphaned,
                bytes: 3 * 1024 * 1024,
            }],
            skipped: vec![io::Error::other("/r/target/x: denied")],
            dry_run: true,
            ..Report::default()
        };
        let text = report.to_string();
        assert!(text.contains("would remove"));
        assert!(text.contains("3.00 MiB"));
        assert!(text.contains("in 1 paths"));
        assert!(text.contains("skipped (could not inspect): /r/target/x: denied"));
    }

    #[test]
    fn errors_name_the_path() {
        let err = with_path(
            &io::Error::from(io::ErrorKind::PermissionDenied),
            Path::new("/x/y"),
        );
        assert!(err.to_string().starts_with("/x/y: "));
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1536), "1.50 KiB");
        assert_eq!(human(5 * 1024_u64.pow(3)), "5.00 GiB");
    }
}
