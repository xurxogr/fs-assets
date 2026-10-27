//! Binary-safe literal-text search across `.uasset`/`.uexp` content, for
//! finding strings baked directly into a Blueprint/Widget asset instead of
//! routed through a `.locres` key.
//!
//! Every `FString` on disk is either single-byte or UTF-16LE, per the same
//! two encodings `crate::locres`'s `read_fstring`/`write_fstring` already
//! decode/encode. Unreal only stores a string single-byte when it is pure
//! ASCII (anything else goes out as UTF-16), so an ASCII `pattern` is
//! searched in both byte representations and any other only as UTF-16LE.

use std::path::{Path, PathBuf};

use memchr::memmem::Finder;

/// Which on-disk `FString` encoding a match was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Single-byte ASCII.
    Ascii,
    /// UTF-16LE (2 bytes/char, matching how non-ASCII `FString`s are stored).
    Utf16Le,
}

impl std::fmt::Display for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Encoding::Ascii => "ascii",
            Encoding::Utf16Le => "utf16le",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: PathBuf,
    pub encoding: Encoding,
}

/// Recursively search every `.uasset`/`.uexp` file under `content_dir` for
/// `pattern`, checked as UTF-16LE bytes and, when it is ASCII, as raw
/// single-byte ASCII too (see the module docs). A
/// file that matches in both encodings is reported once per encoding, sorted
/// by path then encoding. A directory or file that can't be read (or a
/// missing `content_dir`) fails the search rather than silently narrowing it.
pub fn search_content_dir(content_dir: &Path, pattern: &str) -> anyhow::Result<Vec<GrepMatch>> {
    let utf16_needle: Vec<u8> = pattern
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let ascii = pattern.is_ascii().then(|| Finder::new(pattern.as_bytes()));
    let utf16 = Finder::new(&utf16_needle);

    let mut matches = Vec::new();
    for entry in walkdir::WalkDir::new(content_dir) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext != "uasset" && ext != "uexp" {
            continue;
        }

        let data = std::fs::read(path)?;
        if ascii
            .as_ref()
            .is_some_and(|ascii| contains_bytes(&data, ascii))
        {
            matches.push(GrepMatch {
                path: path.to_path_buf(),
                encoding: Encoding::Ascii,
            });
        }
        if contains_bytes(&data, &utf16) {
            matches.push(GrepMatch {
                path: path.to_path_buf(),
                encoding: Encoding::Utf16Le,
            });
        }
    }

    matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.encoding.cmp(&b.encoding)));
    Ok(matches)
}

impl PartialOrd for Encoding {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Encoding {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn rank(e: &Encoding) -> u8 {
            match e {
                Encoding::Ascii => 0,
                Encoding::Utf16Le => 1,
            }
        }
        rank(self).cmp(&rank(other))
    }
}

/// Whether `haystack` contains `needle`'s needle; an empty needle never
/// matches.
fn contains_bytes(haystack: &[u8], needle: &Finder<'_>) -> bool {
    !needle.needle().is_empty() && needle.find(haystack).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_ascii_and_utf16_matches_in_separate_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ascii_path = tmp.path().join("Ascii.uasset");
        fs::write(
            &ascii_path,
            b"junk before Export Empty Stockpile Entries junk after",
        )
        .unwrap();

        let utf16_path = tmp.path().join("Utf16.uexp");
        let mut bytes: Vec<u8> = b"junk".to_vec();
        bytes.extend(
            "Export Empty Stockpile Entries"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes()),
        );
        bytes.extend(b"junk");
        fs::write(&utf16_path, bytes).unwrap();

        let matches = search_content_dir(tmp.path(), "Export Empty Stockpile Entries").unwrap();

        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].path, ascii_path);
        assert_eq!(matches[0].encoding, Encoding::Ascii);
        assert_eq!(matches[1].path, utf16_path);
        assert_eq!(matches[1].encoding, Encoding::Utf16Le);
    }

    #[test]
    fn ignores_non_uasset_uexp_files_and_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Notes.txt"),
            b"Export Empty Stockpile Entries",
        )
        .unwrap();
        fs::write(tmp.path().join("Other.uasset"), b"nothing interesting here").unwrap();

        let matches = search_content_dir(tmp.path(), "Export Empty Stockpile Entries").unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn recurses_into_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("Blueprints/WBP");
        fs::create_dir_all(&sub).unwrap();
        let path = sub.join("WBP_Stockpile.uasset");
        fs::write(&path, b"Export Empty Stockpile Entries").unwrap();

        let matches = search_content_dir(tmp.path(), "Export Empty Stockpile Entries").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, path);
    }

    #[test]
    fn searches_a_non_ascii_pattern_only_as_utf16() {
        let tmp = tempfile::tempdir().unwrap();
        // Its UTF-8 bytes can't be a single-byte `FString`, so they're no match.
        fs::write(tmp.path().join("Utf8.uasset"), "Munición".as_bytes()).unwrap();
        let utf16_path = tmp.path().join("Utf16.uasset");
        let bytes: Vec<u8> = "Munición"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        fs::write(&utf16_path, bytes).unwrap();

        let matches = search_content_dir(tmp.path(), "Munición").unwrap();
        assert_eq!(
            matches,
            vec![GrepMatch {
                path: utf16_path,
                encoding: Encoding::Utf16Le,
            }]
        );
    }

    #[test]
    fn searching_a_missing_directory_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(search_content_dir(&tmp.path().join("missing"), "anything").is_err());
    }
}
