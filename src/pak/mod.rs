use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use repak::PakBuilder;

/// Lists every file path recorded in `pak`'s index.
pub fn list(pak: &Path) -> anyhow::Result<Vec<String>> {
    let mut file = BufReader::new(File::open(pak)?);
    let reader = PakBuilder::new()
        .reader(&mut file)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
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
    extract_matching(
        pak,
        |path| include.is_none_or(|filter| path.contains(filter)),
        output_dir,
    )
}

/// Extracts every file under `pak` whose path `keep` accepts into
/// `output_dir`, preserving the pak's internal directory structure. `keep`
/// receives the path exactly as the pak's index records it.
pub fn extract_matching<F>(pak: &Path, keep: F, output_dir: &Path) -> anyhow::Result<Vec<String>>
where
    F: Fn(&str) -> bool,
{
    extract_matching_with_progress(pak, keep, output_dir, &|_, _| {})
}

/// Same as [`extract_matching`], reporting `(done, total)` over the kept
/// files to `progress`: once before the first file and once after each.
pub fn extract_matching_with_progress<F>(
    pak: &Path,
    keep: F,
    output_dir: &Path,
    progress: &dyn Fn(usize, usize),
) -> anyhow::Result<Vec<String>>
where
    F: Fn(&str) -> bool,
{
    extract_capped(pak, keep, output_dir, progress, MAX_ENTRY_BYTES)
}

/// Largest decompressed size accepted for one pak entry. The compression
/// headers come from the pak itself, so a small entry from an untrusted mod
/// could otherwise inflate without bound; real game entries are far smaller.
const MAX_ENTRY_BYTES: u64 = 4 << 30;

fn extract_capped<F>(
    pak: &Path,
    keep: F,
    output_dir: &Path,
    progress: &dyn Fn(usize, usize),
    max_entry_bytes: u64,
) -> anyhow::Result<Vec<String>>
where
    F: Fn(&str) -> bool,
{
    let mut file = BufReader::new(File::open(pak)?);
    let reader = PakBuilder::new()
        .reader(&mut file)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let wanted: Vec<String> = reader
        .files()
        .into_iter()
        .filter(|path| keep(path))
        .collect();
    let total = wanted.len();
    progress(0, total);

    let mut extracted = Vec::with_capacity(total);
    for path in wanted {
        let dest = contained_path(output_dir, &path)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        // Streamed straight to disk rather than through `reader.get`, which
        // would hold the whole decompressed entry in memory.
        let mut out = CappedWriter {
            inner: BufWriter::new(File::create(&dest)?),
            written: 0,
            limit: max_entry_bytes,
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
        extracted.push(path);
        progress(extracted.len(), total);
    }

    extracted.sort();
    Ok(extracted)
}

/// A writer that fails once more than `limit` bytes have gone through it.
struct CappedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written += buf.len() as u64;
        if self.written > self.limit {
            return Err(io::Error::other(format!(
                "entry is larger than {} bytes",
                self.limit
            )));
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
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

/// Packs every file under `input_dir` into a new `.pak` at `output`. `mount_point` and `version` should match whatever pak
/// this mod is meant to sit alongside — for Foxhole itself that's
/// `"../../../"` and `Version::V8B`, the values of the game's own
/// `War-WindowsNoEditor.pak`. Returns every packed file's path, relative to
/// `input_dir` with `/` separators, sorted for stable output.
pub fn pack(
    input_dir: &Path,
    output: &Path,
    mount_point: &str,
    version: repak::Version,
) -> anyhow::Result<Vec<String>> {
    let file = File::create(output)?;
    let mut writer =
        PakBuilder::new().writer(BufWriter::new(file), version, mount_point.to_string(), None);

    let mut written = Vec::new();
    for entry in walkdir::WalkDir::new(input_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(input_dir)?;
        // repak paths always use `/`, even on Windows hosts.
        let rel_str = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let data = fs::read(entry.path())?;
        writer
            .write_file(&rel_str, false, data)
            .map_err(|e| anyhow::anyhow!("failed to write {rel_str}: {e}"))?;
        written.push(rel_str);
    }

    writer.write_index().map_err(|e| anyhow::anyhow!("{e}"))?;
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
            &|done, total| reports.borrow_mut().push((done, total)),
        )
        .unwrap();

        assert_eq!(reports.into_inner(), vec![(0, 1), (1, 1)]);
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

        let err =
            extract_capped(&pak_path, |p| p == "bomb.bin", &out_dir, &|_, _| {}, 1024).unwrap_err();
        assert!(
            format!("{err:#}").contains("larger than 1024 bytes"),
            "{err:#}"
        );
        assert!(!out_dir.join("bomb.bin").exists());

        let ok = extract_capped(&pak_path, |p| p == "small.txt", &out_dir, &|_, _| {}, 1024);
        assert_eq!(ok.unwrap(), vec!["small.txt".to_string()]);
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
}
