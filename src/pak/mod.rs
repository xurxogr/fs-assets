use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use repak::PakBuilder;

/// Lists every file path recorded in `pak`'s index.
pub fn list(pak: &Path) -> anyhow::Result<Vec<String>> {
    let mut file =
        BufReader::new(File::open(pak).with_context(|| format!("opening {}", pak.display()))?);
    let reader = PakBuilder::new().reader(&mut file)?;
    let mut files = reader.files();
    files.sort();
    Ok(files)
}

/// Extracts every file under `pak` whose path contains `include` (or every
/// file, if `include` is `None`) into `output_dir`, preserving the pak's
/// internal directory structure.
pub fn extract(
    pak: &Path,
    include: Option<&str>,
    output_dir: &Path,
) -> anyhow::Result<Vec<String>> {
    extract_with_progress(pak, include, output_dir, &|_, _| Ok(()))
}

/// Same as [`extract`], reporting progress as
/// [`extract_matching_with_progress`] does.
pub fn extract_with_progress(
    pak: &Path,
    include: Option<&str>,
    output_dir: &Path,
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<String>> {
    extract_matching_with_progress(
        pak,
        |path| include.is_none_or(|filter| path.contains(filter)),
        output_dir,
        progress,
    )
}

/// Extracts every file under `pak` whose path `keep` accepts into
/// `output_dir`, preserving the pak's internal directory structure. `keep`
/// receives the path exactly as the pak's index records it.
pub fn extract_matching<F>(pak: &Path, keep: F, output_dir: &Path) -> anyhow::Result<Vec<String>>
where
    F: Fn(&str) -> bool,
{
    extract_matching_with_progress(pak, keep, output_dir, &|_, _| Ok(()))
}

/// Same as [`extract_matching`], reporting `(done, total)` over the kept
/// files to `progress`: once before the first file and once after each. An
/// error `progress` returns stops the extraction and is returned.
pub fn extract_matching_with_progress<F>(
    pak: &Path,
    keep: F,
    output_dir: &Path,
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<String>>
where
    F: Fn(&str) -> bool,
{
    let pak_len = fs::metadata(pak)
        .with_context(|| format!("reading the size of {}", pak.display()))?
        .len();
    extract_matching_within(
        pak,
        keep,
        output_dir,
        progress,
        &mut ExtractBudget::unpak(pak_len),
    )
}

/// How much a pak extraction may write. The compression headers come from
/// the pak itself, so a small entry from an untrusted mod could otherwise
/// inflate without bound, and many such entries could fill the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractBudget {
    /// Largest decompressed size accepted for one entry.
    pub max_entry_bytes: u64,
    /// Bytes still allowed in total, shrinking as entries are written; one
    /// budget can be shared by the extractions of several paks.
    pub remaining_bytes: u64,
}

impl ExtractBudget {
    /// For a plain `unpak` of a pak `pak_len` bytes long, where the user picks
    /// what to extract: entries of up to 4 GiB (real game entries are far
    /// smaller), and in total 32 times the pak's size, or 16 GiB for a small
    /// pak. Game assets inflate far less than 32 times, while a few MB of
    /// crafted pak, many entries sharing one compressed block, could otherwise
    /// write terabytes.
    pub fn unpak(pak_len: u64) -> Self {
        Self {
            max_entry_bytes: 4 << 30,
            remaining_bytes: pak_len.saturating_mul(32).max(16 << 30),
        }
    }
}

/// Same as [`extract_matching_with_progress`], writing no more than
/// `budget` allows and charging what it writes to it.
pub fn extract_matching_within<F>(
    pak: &Path,
    keep: F,
    output_dir: &Path,
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
    budget: &mut ExtractBudget,
) -> anyhow::Result<Vec<String>>
where
    F: Fn(&str) -> bool,
{
    let mut file =
        BufReader::new(File::open(pak).with_context(|| format!("opening {}", pak.display()))?);
    let reader = PakBuilder::new().reader(&mut file)?;

    // Every destination is checked before anything is written. On a
    // case-insensitive filesystem (Windows, macOS) two entries differing only
    // in case land on the same file, the second silently replacing the first;
    // real paks have none, so that is refused too.
    let mut wanted: Vec<(String, PathBuf)> = Vec::new();
    let mut destinations: HashMap<String, usize> = HashMap::new();
    for path in reader.files().into_iter().filter(|path| keep(path)) {
        let dest = contained_path(output_dir, &path)?;
        if let Some(other) =
            destinations.insert(dest.to_string_lossy().to_lowercase(), wanted.len())
        {
            anyhow::bail!(
                "{} and {path} would be extracted to the same file",
                wanted[other].0
            );
        }
        wanted.push((path, dest));
    }
    let total = wanted.len();
    progress(0, total)?;

    let mut extracted = Vec::with_capacity(total);
    for (path, dest) in wanted {
        if has_symlink_below(output_dir, &dest) {
            anyhow::bail!(
                "{path} would be written through a symlink in {}",
                output_dir.display()
            );
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }

        // `read_file` holds the entry's stored bytes in memory, so an entry
        // whose header is already over the budget is refused before it is
        // read. The header comes from the pak and may understate the sizes;
        // `CappedWriter` holds the decompressed output to the budget anyway,
        // writing it straight to disk rather than through `reader.get`.
        let size = reader
            .entry_size(&path)
            .with_context(|| format!("{path} is missing from the pak index"))?;
        if size.stored.max(size.uncompressed) > budget.max_entry_bytes {
            anyhow::bail!("{path} is larger than {} bytes", budget.max_entry_bytes);
        }
        if size.uncompressed > budget.remaining_bytes {
            anyhow::bail!(
                "extracting {path} would exceed the total size limit ({} bytes were left)",
                budget.remaining_bytes
            );
        }
        let mut out = CappedWriter {
            inner: BufWriter::new(
                File::create(&dest).with_context(|| format!("creating {}", dest.display()))?,
            ),
            written: 0,
            budget: *budget,
        };
        let written = reader
            .read_file(&path, &mut file, &mut out)
            .map_err(anyhow::Error::from)
            .and_then(|()| Ok(out.flush()?));
        if let Err(e) = written {
            drop(out);
            let _ = fs::remove_file(&dest);
            return Err(e.context(format!("failed to read {path}")));
        }
        budget.remaining_bytes -= out.written;
        extracted.push(path);
        progress(extracted.len(), total)?;
    }

    extracted.sort();
    Ok(extracted)
}

/// A writer that fails once more than `budget` allows for one entry has gone
/// through it.
struct CappedWriter<W> {
    inner: W,
    written: u64,
    budget: ExtractBudget,
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Checked against what this call could write, but only what `inner`
        // actually wrote is counted: `write_all` passes the rest of a short
        // write back in, and it must not be counted twice.
        let after = self.written + buf.len() as u64;
        if after > self.budget.max_entry_bytes {
            return Err(io::Error::other(format!(
                "entry is larger than {} bytes",
                self.budget.max_entry_bytes
            )));
        }
        if after > self.budget.remaining_bytes {
            return Err(io::Error::other(format!(
                "extraction is larger than its total size limit ({} bytes were left)",
                self.budget.remaining_bytes
            )));
        }
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Whether reaching `path` from `root` goes through a symlink: any existing
/// component below `root` (`root` itself may be one) that is a link. A link
/// in a directory the user hands in — an extraction directory, `unpak`'s
/// output — could otherwise point reads or writes anywhere on the system,
/// which [`contained_path`]'s purely textual check can't see. A `path` not
/// under `root` counts as one, to fail closed.
pub(crate) fn has_symlink_below(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            // Nothing exists from here on, so nothing below can be a link.
            Err(_) => return false,
        }
    }
    false
}

/// `path` under `root` if it is a regular file reached without following a
/// symlink below `root` (see [`has_symlink_below`]).
pub(crate) fn real_file_under(root: &Path, path: PathBuf) -> Option<PathBuf> {
    (!has_symlink_below(root, &path) && path.is_file()).then_some(path)
}

/// Whether `name` is usable as a file or directory name on Windows too: not
/// a reserved device name (`CON`, `NUL`, `COM1`…, with any extension, which
/// Windows opens as the device instead of a file) and not ending in `.` or a
/// space (which Windows drops, so `a.uasset.` would overwrite `a.uasset`).
pub(crate) fn is_portable_name(name: &str) -> bool {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let stem = name.split('.').next().unwrap_or("");
    let numbered_device = ["COM", "LPT"].iter().any(|prefix| {
        stem.get(..3)
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
            && matches!(
                &stem[3..],
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
    });
    let reserved = numbered_device || RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r));
    !reserved && !name.ends_with(['.', ' '])
}

/// `output_dir` joined with `entry`, an untrusted relative path such as a pak
/// index entry or a catalog object path. Only plain name components are
/// accepted, split on both `/` and `\\`, so an entry can't climb out with
/// `..` or replace `output_dir` with an absolute or drive-prefixed path. A `:` is refused on every host, since on Windows it
/// starts a drive prefix (`C:`) or an alternate data stream. Empty and `.`
/// components are skipped.
pub(crate) fn contained_path(output_dir: &Path, entry: &str) -> anyhow::Result<PathBuf> {
    let mut dest = output_dir.to_path_buf();
    for part in entry
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
    {
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None)
                if !part.contains(':') && is_portable_name(part) =>
            {
                dest.push(name)
            }
            _ => anyhow::bail!("path {entry:?} escapes {}", output_dir.display()),
        }
    }
    if dest == output_dir {
        anyhow::bail!("path {entry:?} has no file name");
    }
    Ok(dest)
}

/// The pak versions [`pack`] is offered with, by the name the CLI and the
/// Python API take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PakVersion {
    V8a,
    V8b,
    V11,
}

impl From<PakVersion> for repak::Version {
    fn from(value: PakVersion) -> Self {
        match value {
            PakVersion::V8a => repak::Version::V8A,
            PakVersion::V8b => repak::Version::V8B,
            PakVersion::V11 => repak::Version::V11,
        }
    }
}

/// Packs every file under `input_dir` into a new `.pak` at `output`. `mount_point` and `version` should match whatever pak
/// this mod is meant to sit alongside — for Foxhole itself that's
/// `"../../../"` and `Version::V8B`, the values of the game's own
/// `War-WindowsNoEditor.pak`. Returns every packed file's path, relative to
/// `input_dir` with `/` separators, sorted for stable output. A directory
/// that can't be read (or a missing `input_dir`), or a file name that isn't
/// UTF-8 (pak paths are text), fails the whole pack before `output` is
/// touched, rather than producing a pak missing or misnaming its files.
///
/// The pak is written to a temporary file next to `output` and renamed over
/// it only once complete, so a failed pack leaves an existing `output` as it
/// was. An `output` inside `input_dir` (re-running `pack mod -o mod/X.pak`)
/// isn't packed into itself.
pub fn pack(
    input_dir: &Path,
    output: &Path,
    mount_point: &str,
    version: repak::Version,
) -> anyhow::Result<Vec<String>> {
    pack_with_progress(input_dir, output, mount_point, version, &|_, _| Ok(()))
}

/// Same as [`pack`], reporting `(done, total)` over the packed files to
/// `progress`: once before the first file and once after each. An error
/// `progress` returns stops the pack, leaving `output` as it was, and is
/// returned.
pub fn pack_with_progress(
    input_dir: &Path,
    output: &Path,
    mount_point: &str,
    version: repak::Version,
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<String>> {
    let output_file = output.canonicalize().ok();
    let mut files = Vec::new();
    // Sorted, so the same directory packs to the same bytes whatever order
    // the filesystem lists it in.
    for entry in walkdir::WalkDir::new(input_dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file()
            || output_file.is_some() && entry.path().canonicalize().ok() == output_file
        {
            continue;
        }
        let rel = entry.path().strip_prefix(input_dir)?;
        // repak paths always use `/`, even on Windows hosts.
        let rel_str = rel
            .components()
            .map(|c| {
                c.as_os_str().to_str().ok_or_else(|| {
                    anyhow::anyhow!("{} is not a UTF-8 file name", entry.path().display())
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .join("/");
        files.push((entry.into_path(), rel_str));
    }

    let mut temp = crate::atomic_write::temp_next_to(output, ".pak.tmp")?;
    let mut writer = PakBuilder::new().writer(
        BufWriter::new(temp.as_file_mut()),
        version,
        mount_point.to_string(),
        None,
    );

    let total = files.len();
    progress(0, total)?;
    let mut written = Vec::with_capacity(total);
    for (path, rel_str) in files {
        let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        writer
            .write_file(&rel_str, false, data)
            .map_err(|e| anyhow::anyhow!("failed to write {rel_str}: {e}"))?;
        written.push(rel_str);
        progress(written.len(), total)?;
    }

    writer.write_index()?.flush()?;
    crate::atomic_write::persist(temp.into_temp_path(), output)?;
    written.sort();
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use repak::PakBuilder as Builder;
    use std::io::Cursor;

    fn build_test_pak() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut writer =
            Builder::new().writer(&mut buf, repak::Version::V11, "../../../".to_string(), None);
        writer
            .write_file("dir/a.txt", false, b"hello".as_slice())
            .unwrap();
        writer
            .write_file("b.txt", false, b"world".as_slice())
            .unwrap();
        writer.write_index().unwrap();
        buf.into_inner()
    }

    #[test]
    fn lists_and_extracts_files() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("test.pak");
        fs::write(&pak_path, build_test_pak()).unwrap();

        let mut files = list(&pak_path).unwrap();
        files.sort();
        assert_eq!(files, vec!["b.txt".to_string(), "dir/a.txt".to_string()]);

        let out_dir = tmp.path().join("out");
        let extracted = extract(&pak_path, None, &out_dir).unwrap();
        assert_eq!(extracted.len(), 2);
        assert_eq!(
            fs::read_to_string(out_dir.join("dir/a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(fs::read_to_string(out_dir.join("b.txt")).unwrap(), "world");

        let filtered = extract(&pak_path, Some("dir/"), &tmp.path().join("out2")).unwrap();
        assert_eq!(filtered, vec!["dir/a.txt".to_string()]);
    }

    #[test]
    fn extracts_only_paths_accepted_by_the_predicate() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("test.pak");
        fs::write(&pak_path, build_test_pak()).unwrap();

        let out_dir = tmp.path().join("out");
        let extracted =
            extract_matching(&pak_path, |path| path.ends_with("b.txt"), &out_dir).unwrap();

        assert_eq!(extracted, vec!["b.txt".to_string()]);
        assert!(out_dir.join("b.txt").exists());
        assert!(!out_dir.join("dir").exists());
    }

    #[test]
    fn reports_progress_over_the_kept_files_only() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("test.pak");
        fs::write(&pak_path, build_test_pak()).unwrap();

        let reports = std::cell::RefCell::new(Vec::new());
        extract_matching_with_progress(
            &pak_path,
            |path| path.starts_with("dir/"),
            &tmp.path().join("out"),
            &|done, total| {
                reports.borrow_mut().push((done, total));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(reports.into_inner(), vec![(0, 1), (1, 1)]);
    }

    #[test]
    fn a_failing_progress_report_stops_the_extraction() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("test.pak");
        fs::write(&pak_path, build_test_pak()).unwrap();
        let out = tmp.path().join("out");

        let err = extract_matching_with_progress(&pak_path, |_| true, &out, &|_, _| {
            anyhow::bail!("stop")
        })
        .unwrap_err();

        assert_eq!(err.to_string(), "stop");
        assert!(!out.exists());
    }

    fn build_pak_with(entries: &[&str]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut writer =
            Builder::new().writer(&mut buf, repak::Version::V11, "../../../".to_string(), None);
        for entry in entries {
            writer.write_file(entry, false, b"evil".as_slice()).unwrap();
        }
        writer.write_index().unwrap();
        buf.into_inner()
    }

    #[test]
    fn refuses_entries_that_escape_the_output_dir() {
        for entry in [
            "../escape.txt",
            "dir/../../escape.txt",
            "dir\\..\\..\\escape.txt",
            "C:/escape.txt",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let pak_path = tmp.path().join("evil.pak");
            fs::write(&pak_path, build_pak_with(&[entry])).unwrap();

            let out_dir = tmp.path().join("nested/out");
            let err = extract(&pak_path, None, &out_dir).unwrap_err();
            assert!(err.to_string().contains("escapes"), "{entry}: {err}");
            assert!(!tmp.path().join("nested/escape.txt").exists(), "{entry}");
            assert!(!tmp.path().join("escape.txt").exists(), "{entry}");
        }
    }

    #[test]
    fn refuses_entries_that_decompress_past_the_cap() {
        let mut buf = Cursor::new(Vec::new());
        let mut writer = Builder::new()
            .compression([repak::Compression::Zlib])
            .writer(&mut buf, repak::Version::V11, "../../../".to_string(), None);
        writer
            .write_file("bomb.bin", true, vec![0u8; 1 << 20].as_slice())
            .unwrap();
        writer
            .write_file("small.txt", false, b"hi".as_slice())
            .unwrap();
        writer.write_index().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("bomb.pak");
        fs::write(&pak_path, buf.into_inner()).unwrap();
        let out_dir = tmp.path().join("out");

        let budget = ExtractBudget {
            max_entry_bytes: 1024,
            remaining_bytes: u64::MAX,
        };
        let err = extract_matching_within(
            &pak_path,
            |p| p == "bomb.bin",
            &out_dir,
            &|_, _| Ok(()),
            &mut budget.clone(),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("larger than 1024 bytes"),
            "{err:#}"
        );
        assert!(!out_dir.join("bomb.bin").exists());

        let ok = extract_matching_within(
            &pak_path,
            |p| p == "small.txt",
            &out_dir,
            &|_, _| Ok(()),
            &mut budget.clone(),
        );
        assert_eq!(ok.unwrap(), vec!["small.txt".to_string()]);
    }

    #[test]
    fn refuses_entries_differing_only_in_case_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("case.pak");
        fs::write(&pak_path, build_pak_with(&["War/A.txt", "war/a.txt"])).unwrap();
        let out_dir = tmp.path().join("out");

        let err = extract(&pak_path, None, &out_dir).unwrap_err();
        assert!(err.to_string().contains("same file"), "{err}");
        assert!(!out_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn real_file_under_refuses_links_below_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.uasset"), b"x").unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("War")).unwrap();
        fs::write(root.join("War/real.uasset"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("War/Linked")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.uasset"), root.join("War/file.uasset"))
            .unwrap();

        let real = root.join("War/real.uasset");
        assert_eq!(real_file_under(&root, real.clone()), Some(real));
        assert_eq!(
            real_file_under(&root, root.join("War/Linked/secret.uasset")),
            None
        );
        assert_eq!(real_file_under(&root, root.join("War/file.uasset")), None);
        assert_eq!(real_file_under(&root, outside.join("secret.uasset")), None);
        assert_eq!(
            real_file_under(&root, root.join("War/missing.uasset")),
            None
        );

        // A symlinked root itself is fine: only links below it count.
        let linked_root = tmp.path().join("linked_root");
        std::os::unix::fs::symlink(&root, &linked_root).unwrap();
        assert!(real_file_under(&linked_root, linked_root.join("War/real.uasset")).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn extraction_refuses_to_write_through_a_symlink_in_the_output_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("test.pak");
        fs::write(&pak_path, build_pak_with(&["War/escape.txt"])).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&out_dir).unwrap();
        std::os::unix::fs::symlink(&outside, out_dir.join("War")).unwrap();

        let err = extract(&pak_path, None, &out_dir).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!outside.join("escape.txt").exists());
    }

    #[test]
    fn unpak_caps_the_total_at_32_times_the_pak_or_16_gib() {
        assert_eq!(ExtractBudget::unpak(1 << 20).remaining_bytes, 16 << 30);
        assert_eq!(ExtractBudget::unpak(24 << 30).remaining_bytes, 768 << 30);
        assert_eq!(ExtractBudget::unpak(u64::MAX).remaining_bytes, u64::MAX);
    }

    /// The header check stops honest oversized entries; this is what stops
    /// one whose header understates its size.
    #[test]
    fn capped_writer_fails_past_either_budget() {
        let write = |max_entry_bytes, remaining_bytes| {
            let mut out = CappedWriter {
                inner: Vec::new(),
                written: 0,
                budget: ExtractBudget {
                    max_entry_bytes,
                    remaining_bytes,
                },
            };
            out.write_all(&[0; 10])
                .and_then(|()| out.write_all(&[0; 10]))
        };
        assert!(write(20, 20).is_ok());
        let entry = write(15, 100).unwrap_err().to_string();
        assert!(entry.contains("larger than 15 bytes"), "{entry}");
        let total = write(100, 15).unwrap_err().to_string();
        assert!(total.contains("total size limit"), "{total}");
    }

    /// A writer that takes at most 3 bytes per call, like a `write(2)` that
    /// stops short of a huge buffer.
    struct ShortWrites(Vec<u8>);

    impl Write for ShortWrites {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = buf.len().min(3);
            self.0.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn capped_writer_counts_short_writes_once() {
        let mut out = CappedWriter {
            inner: ShortWrites(Vec::new()),
            written: 0,
            budget: ExtractBudget {
                max_entry_bytes: 10,
                remaining_bytes: 10,
            },
        };
        out.write_all(&[0; 10]).unwrap();
        assert_eq!(out.written, 10);
        assert_eq!(out.inner.0.len(), 10);
    }

    #[test]
    fn stops_once_the_total_budget_is_spent_across_entries_and_paks() {
        let mut buf = Cursor::new(Vec::new());
        let mut writer =
            Builder::new().writer(&mut buf, repak::Version::V11, "../../../".to_string(), None);
        for name in ["a.bin", "b.bin"] {
            writer
                .write_file(name, false, vec![0u8; 600].as_slice())
                .unwrap();
        }
        writer.write_index().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("two.pak");
        fs::write(&pak_path, buf.into_inner()).unwrap();
        let out_dir = tmp.path().join("out");
        let mut budget = ExtractBudget {
            max_entry_bytes: 1024,
            remaining_bytes: 1500,
        };

        // 1200 bytes fit; the budget is then down to 300.
        let ok =
            extract_matching_within(&pak_path, |_| true, &out_dir, &|_, _| Ok(()), &mut budget);
        assert_eq!(ok.unwrap().len(), 2);
        assert_eq!(budget.remaining_bytes, 300);

        // A second extraction sharing the budget can't write another 600.
        let err = extract_matching_within(
            &pak_path,
            |p| p == "a.bin",
            &tmp.path().join("again"),
            &|_, _| Ok(()),
            &mut budget,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("total size limit"), "{err:#}");
        assert!(!tmp.path().join("again/a.bin").exists());
    }

    #[test]
    fn rejects_a_footer_index_size_past_the_file() {
        let mut pak = build_test_pak();
        // Footer: magic, version, index offset, index size.
        let magic = pak
            .windows(4)
            .rposition(|w| w == 0x5A6F12E1u32.to_le_bytes())
            .unwrap();
        let index_size = magic + 4 + 4 + 8;
        pak[index_size..index_size + 8].copy_from_slice(&(1u64 << 40).to_le_bytes());

        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("corrupt.pak");
        fs::write(&pak_path, pak).unwrap();

        assert!(list(&pak_path).is_err());
    }

    #[test]
    fn contained_path_refuses_names_windows_would_not_keep() {
        let out = Path::new("out");
        for entry in [
            "War/CON",
            "War/nul.uasset",
            "War/Com1/a.uasset",
            "War/LPT9.txt",
            "War/a.uasset.",
            "War/dir /a.uasset",
        ] {
            assert!(contained_path(out, entry).is_err(), "{entry}");
        }
        for entry in [
            "War/CONSOLE.uasset",
            "War/COM10.uasset",
            "War/Nullable",
            "War/COM.uasset",
        ] {
            assert!(contained_path(out, entry).is_ok(), "{entry}");
        }
    }

    #[test]
    fn contained_path_keeps_plain_entries_under_the_output_dir() {
        let out = Path::new("out");
        assert_eq!(
            contained_path(out, "/War/./Content/a.uasset").unwrap(),
            out.join("War").join("Content").join("a.uasset")
        );
        assert!(contained_path(out, "/").is_err());
    }

    #[test]
    fn packs_a_directory_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let input_dir = tmp.path().join("mod");
        fs::create_dir_all(input_dir.join("dir")).unwrap();
        fs::write(input_dir.join("dir/a.txt"), b"hello").unwrap();
        fs::write(input_dir.join("b.txt"), b"world").unwrap();

        let pak_path = tmp.path().join("out.pak");
        let written = pack(&input_dir, &pak_path, "../../../", repak::Version::V8B).unwrap();
        assert_eq!(written, vec!["b.txt".to_string(), "dir/a.txt".to_string()]);

        let mut files = list(&pak_path).unwrap();
        files.sort();
        assert_eq!(files, vec!["b.txt".to_string(), "dir/a.txt".to_string()]);

        let out_dir = tmp.path().join("extracted");
        extract(&pak_path, None, &out_dir).unwrap();
        assert_eq!(
            fs::read_to_string(out_dir.join("dir/a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(fs::read_to_string(out_dir.join("b.txt")).unwrap(), "world");
    }

    #[test]
    fn packs_a_locres_file_into_a_mod() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = crate::locres::LocresDocument {
            version: 2,
            namespaces: vec![crate::locres::LocresNamespace {
                namespace_hash: 1,
                namespace: "STActions".to_string(),
                entries: vec![crate::locres::LocresEntry {
                    key_hash: 1,
                    key: "ActionAim".to_string(),
                    source_hash: 1,
                    text: "Mirar - Segurar".to_string(),
                }],
            }],
        };

        let input_dir = tmp.path().join("mod");
        let locres_rel = "War/Content/Localization/Foxhole-Content/pt/Foxhole-Content.locres";
        let locres_path = input_dir.join(locres_rel);
        fs::create_dir_all(locres_path.parent().unwrap()).unwrap();
        crate::locres::write(&doc, &locres_path).unwrap();

        let pak_path = tmp.path().join("out.pak");
        pack(&input_dir, &pak_path, "../../../", repak::Version::V8B).unwrap();

        let files = list(&pak_path).unwrap();
        assert_eq!(files, vec![locres_rel.to_string()]);
    }

    #[test]
    fn packing_a_missing_directory_fails_without_writing_a_pak() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = tmp.path().join("out.pak");
        assert!(pack(
            &tmp.path().join("missing"),
            &pak_path,
            "../../../",
            repak::Version::V8B
        )
        .is_err());
        assert!(!pak_path.exists());
    }

    #[test]
    fn repacking_into_the_input_dir_skips_the_previous_pak() {
        let tmp = tempfile::tempdir().unwrap();
        let input_dir = tmp.path().join("mod");
        fs::create_dir_all(&input_dir).unwrap();
        fs::write(input_dir.join("a.txt"), b"hello").unwrap();
        let pak_path = input_dir.join("Mod.pak");

        for _ in 0..2 {
            let written = pack(&input_dir, &pak_path, "../../../", repak::Version::V8B).unwrap();
            assert_eq!(written, vec!["a.txt".to_string()]);
        }
        assert_eq!(list(&pak_path).unwrap(), vec!["a.txt".to_string()]);
        let leftovers: Vec<_> = fs::read_dir(&input_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 2, "{leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_pack_leaves_the_existing_pak_alone() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempfile::tempdir().unwrap();
        let input_dir = tmp.path().join("mod");
        fs::create_dir_all(&input_dir).unwrap();
        fs::write(input_dir.join("a.txt"), b"hello").unwrap();
        let pak_path = tmp.path().join("out.pak");
        pack(&input_dir, &pak_path, "../../../", repak::Version::V8B).unwrap();
        let before = fs::read(&pak_path).unwrap();

        fs::write(
            input_dir.join(std::ffi::OsStr::from_bytes(b"bad\xff.txt")),
            b"x",
        )
        .unwrap();
        let err = pack(&input_dir, &pak_path, "../../../", repak::Version::V8B).unwrap_err();
        assert!(err.to_string().contains("not a UTF-8 file name"), "{err}");
        assert_eq!(fs::read(&pak_path).unwrap(), before);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 2);
    }
}
