//! Files only the bridge's user may read (design §8, secrets).
//!
//! `OpenOptions::mode` applies only when a file is created, so an existing
//! file keeps whatever mode it had. [`write_atomically`] therefore never
//! reuses a leftover temp file, and [`tighten`] makes an opened file private
//! or refuses it.

use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Replaces `path` with `bytes`, atomically and durably, as a `0600` file
/// (creating a missing parent `0700`): write a fresh temp file, sync,
/// rename, sync the directory.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    // A leftover from a crash could carry wider permissions (or be a
    // link): never reuse it, always create the file afresh.
    match std::fs::remove_file(&tmp) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    // The umask can only narrow the mode, but say it exactly.
    file.set_permissions(Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
}

/// Makes `file`, just opened from `path`, private: it must be a regular
/// file reached without a symlink, and a group or world bit is cleared.
/// Refuses (`PermissionDenied`, naming `path`) a link, a non-regular file,
/// or one whose mode the bridge cannot change (another user's file).
pub(crate) fn tighten(file: &File, path: &Path) -> io::Result<()> {
    let opened = file.metadata()?;
    let linked = std::fs::symlink_metadata(path)?;
    let refuse = |why: &str| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {why}", path.display()),
        )
    };
    if linked.file_type().is_symlink()
        || linked.dev() != opened.dev()
        || linked.ino() != opened.ino()
    {
        return Err(refuse("is a symlink; refusing to use it as a private file"));
    }
    if !opened.file_type().is_file() {
        return Err(refuse("is not a regular file"));
    }
    if opened.mode() & 0o077 != 0 {
        file.set_permissions(Permissions::from_mode(0o600))
            .map_err(|error| {
                refuse(&format!(
                    "is readable by others and cannot be made private ({error})"
                ))
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("meta").permissions().mode() & 0o777
    }

    #[test]
    fn a_leftover_world_readable_tmp_file_is_not_reused() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("state/file.json");
        std::fs::create_dir(dir.path().join("state")).expect("mkdir");
        let leftover = dir.path().join("state/file.json.tmp");
        std::fs::write(&leftover, "stale").expect("plant");
        std::fs::set_permissions(&leftover, Permissions::from_mode(0o644)).expect("chmod");
        write_atomically(&path, b"fresh").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"fresh");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn an_open_file_is_tightened() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("log");
        std::fs::write(&path, "").expect("plant");
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).expect("chmod");
        tighten(&File::open(&path).expect("open"), &path).expect("tighten");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn links_and_special_files_are_refused() {
        let dir = tempfile::tempdir().expect("tmp");
        let target = dir.path().join("target");
        std::fs::write(&target, "").expect("plant");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let error = tighten(&File::open(&link).expect("open"), &link).expect_err("a link");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("symlink"), "{error}");

        let null = Path::new("/dev/null");
        let error = tighten(&File::open(null).expect("open"), null).expect_err("not a file");
        assert!(error.to_string().contains("not a regular file"), "{error}");
    }
}
