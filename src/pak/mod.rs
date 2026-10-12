use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Seek};
use std::path::Path;

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
        file.rewind()?;
        let data = reader
            .get(&path, &mut file)
            .map_err(|e| anyhow::anyhow!("failed to read {path}: {e}"))?;

        let dest = output_dir.join(path.trim_start_matches('/'));
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, data)?;
        extracted.push(path);
        progress(extracted.len(), total);
    }

    extracted.sort();
    Ok(extracted)
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
