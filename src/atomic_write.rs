//! Replacing a file so that a crash or power loss leaves either the old file
//! or the complete new one, never a truncated mix: the new contents go to a
//! temporary file next to the destination, which is flushed to disk and only
//! then renamed over it.
//!
//! The replaced file keeps its permissions (a new one gets the umask's, as a
//! plain create would), and a symlink at the destination is followed, so its
//! target is what gets replaced, not the link.

use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use tempfile::{NamedTempFile, TempPath};

/// Flushes the finished temporary file `temp` to disk, renames it to `dest`
/// and flushes the rename. Without the first flush, a filesystem may commit
/// the rename before the data and leave an empty `dest` after a power loss.
pub(crate) fn persist(temp: TempPath, dest: &Path) -> anyhow::Result<()> {
    let dest = resolve_dest(dest);
    keep_permissions(&temp, &dest)?;
    // Opened for writing: Windows only flushes a handle with write access.
    OpenOptions::new().write(true).open(&temp)?.sync_all()?;
    temp.persist(&dest)?;
    sync_parent_dir(&dest)
}

/// A new temporary file named with `suffix` in the directory of the file
/// `dest` names, where [`persist`] can rename it over that file.
pub(crate) fn temp_next_to(dest: &Path, suffix: &str) -> std::io::Result<NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.suffix(suffix);
    // `tempfile` creates owner-only (0600) files; asked for 0666, the umask
    // narrows it as it does for any newly created file.
    #[cfg(unix)]
    builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
    builder.tempfile_in(parent_dir(&resolve_dest(dest)))
}

/// The file a write to `dest` should replace: the target of a symlink at
/// `dest` (renaming over the link would swap it for a plain file and leave
/// its target stale), or `dest` itself, also for a dangling link.
fn resolve_dest(dest: &Path) -> Cow<'_, Path> {
    match fs::symlink_metadata(dest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(dest).map_or(Cow::Borrowed(dest), Cow::Owned)
        }
        _ => Cow::Borrowed(dest),
    }
}

/// Gives `temp` the permissions of the file at `dest` it is about to replace.
#[cfg(unix)]
fn keep_permissions(temp: &Path, dest: &Path) -> std::io::Result<()> {
    match fs::metadata(dest) {
        Ok(meta) => fs::set_permissions(temp, meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Windows permissions are only a read-only flag, and a read-only `dest`
/// can't be renamed over anyway.
#[cfg(not(unix))]
fn keep_permissions(_temp: &Path, _dest: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Replaces `dest` with `value` as pretty-printed JSON.
pub(crate) fn write_json<T: serde::Serialize + ?Sized>(
    dest: &Path,
    value: &T,
) -> anyhow::Result<()> {
    let mut temp = temp_next_to(dest, ".json.tmp")?;
    let mut writer = BufWriter::new(temp.as_file_mut());
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.flush()?;
    drop(writer);
    persist(temp.into_temp_path(), dest)
}

/// Replaces `dest` with `bytes`.
pub(crate) fn write_bytes(dest: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut temp = temp_next_to(dest, ".tmp")?;
    temp.write_all(bytes)?;
    persist(temp.into_temp_path(), dest)
}

/// The directory holding `path`, `.` for a bare file name.
pub(crate) fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Flushes the directory entry of `path`, which a rename changed.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> anyhow::Result<()> {
    File::open(parent_dir(path))?.sync_all()?;
    Ok(())
}

/// Windows can't open a directory as a file; NTFS journals the rename itself.
#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn replaces_the_destination_with_the_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.bin");
        std::fs::write(&dest, b"old").unwrap();
        let mut temp = tempfile::NamedTempFile::new_in(tmp.path()).unwrap();
        temp.write_all(b"new").unwrap();

        persist(temp.into_temp_path(), &dest).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn writes_bytes_leaving_no_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.bin");
        std::fs::write(&dest, b"old").unwrap();

        write_bytes(&dest, b"new").unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn writes_pretty_json_leaving_no_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.json");
        std::fs::write(&dest, b"old").unwrap();

        write_json(&dest, &serde_json::json!({"a": 1})).unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "{\n  \"a\": 1\n}");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_permissions_of_the_replaced_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.bin");
        std::fs::write(&dest, b"old").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o640)).unwrap();

        write_bytes(&dest, b"new").unwrap();

        assert_eq!(mode(&dest), 0o640);
    }

    /// Compared with a file `fs::write` creates, which gets the umask's mode.
    #[cfg(unix)]
    #[test]
    fn a_new_file_gets_the_same_mode_as_a_plain_create() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain.bin");
        std::fs::write(&plain, b"").unwrap();
        let dest = tmp.path().join("out.bin");

        write_bytes(&dest, b"new").unwrap();

        assert_eq!(mode(&dest), mode(&plain));
    }

    #[cfg(unix)]
    #[test]
    fn replaces_the_target_of_a_symlinked_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().join("game");
        std::fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("real.locres");
        std::fs::write(&target, b"old").unwrap();
        let link = tmp.path().join("link.locres");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_bytes(&link, b"new").unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&target_dir).unwrap().count(), 1);
    }
}
