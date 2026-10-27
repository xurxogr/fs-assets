//! Replacing a file so that a crash or power loss leaves either the old file
//! or the complete new one, never a truncated mix: the new contents go to a
//! temporary file next to the destination, which is flushed to disk and only
//! then renamed over it.

use std::fs::{File, OpenOptions};
use std::path::Path;

use tempfile::TempPath;

/// Flushes the finished temporary file `temp` to disk, renames it to `dest`
/// and flushes the rename. Without the first flush, a filesystem may commit
/// the rename before the data and leave an empty `dest` after a power loss.
pub(crate) fn persist(temp: TempPath, dest: &Path) -> anyhow::Result<()> {
    // Opened for writing: Windows only flushes a handle with write access.
    OpenOptions::new().write(true).open(&temp)?.sync_all()?;
    temp.persist(dest)?;
    sync_parent_dir(dest)
}

/// Flushes the directory entry of `path`, which a rename changed.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> anyhow::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()?;
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
}
