//! Replacing a file so that a crash or power loss leaves either the old file
//! or the complete new one, never a truncated mix: the new contents go to a
//! temporary file next to the destination, which is flushed to disk and only
//! then renamed over it.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
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

/// Replaces `dest` with `value` as pretty-printed JSON.
pub(crate) fn write_json<T: serde::Serialize + ?Sized>(
    dest: &Path,
    value: &T,
) -> anyhow::Result<()> {
    let mut temp = tempfile::Builder::new()
        .suffix(".json.tmp")
        .tempfile_in(parent_dir(dest))?;
    let mut writer = BufWriter::new(temp.as_file_mut());
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.flush()?;
    drop(writer);
    persist(temp.into_temp_path(), dest)
}

/// The directory holding `path`, `.` for a bare file name.
fn parent_dir(path: &Path) -> &Path {
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
    fn writes_pretty_json_leaving_no_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.json");
        std::fs::write(&dest, b"old").unwrap();

        write_json(&dest, &serde_json::json!({"a": 1})).unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "{\n  \"a\": 1\n}");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
