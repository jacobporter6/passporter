//! Reading the set of worktrees git currently tracks.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Paths of every worktree git tracks for the repo at `root`, main checkout included.
pub(crate) fn live(root: &Path) -> io::Result<BTreeSet<PathBuf>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(parse_porcelain_z(&out.stdout))
}

/// Parses `git worktree list --porcelain -z`.
///
/// Fields are NUL-terminated, so paths containing newlines survive intact, and
/// they are kept as raw bytes, so non-UTF-8 paths still match their directories.
/// Getting this wrong would make a live worktree look orphaned.
pub(crate) fn parse_porcelain_z(bytes: &[u8]) -> BTreeSet<PathBuf> {
    bytes
        .split(|&b| b == 0)
        .filter_map(|field| field.strip_prefix(b"worktree "))
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_main_and_linked_worktrees() {
        let out = b"worktree /repo\0HEAD 4b7b534\0branch refs/heads/main\0\0\
worktree /repo/.worktrees/T11\0HEAD 09ff5c3\0branch refs/heads/task/T11\0\0\
worktree /repo/.worktrees/T12\0HEAD 09ff5c3\0detached\0prunable gitdir points nowhere\0\0";
        let expected: BTreeSet<PathBuf> = ["/repo", "/repo/.worktrees/T11", "/repo/.worktrees/T12"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(parse_porcelain_z(out), expected);
    }

    #[test]
    fn keeps_spaces_newlines_and_non_utf8_bytes() {
        let out = b"worktree /w/my repo\0\0worktree /w/line\nbreak\0\0worktree /w/\xff\xfe\0\0";
        let paths = parse_porcelain_z(out);
        assert!(paths.contains(Path::new("/w/my repo")));
        assert!(paths.contains(Path::new("/w/line\nbreak")));
        assert!(paths.contains(&PathBuf::from(OsStr::from_bytes(b"/w/\xff\xfe"))));
    }

    #[test]
    fn empty_output_is_empty_set() {
        assert_eq!(parse_porcelain_z(b""), BTreeSet::new());
    }
}
