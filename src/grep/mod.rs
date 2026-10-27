//! Binary-safe literal-text search across `.uasset`/`.uexp` content, for
//! finding strings baked directly into a Blueprint/Widget asset instead of
//! routed through a `.locres` key.
//!
//! Every `FString` on disk is either single-byte (ASCII/Latin-1) or
//! UTF-16LE, per the same two encodings `crate::locres`'s `read_fstring`/
//! `write_fstring` already decode/encode — this searches for both byte
//! representations of `pattern` since a given literal could use either.

use std::path::{Path, PathBuf};

use memchr::memmem::Finder;

/// Which on-disk `FString` encoding a match was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Single-byte ASCII/Latin-1.
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
/// `pattern`, checked as both raw ASCII/Latin-1 bytes and UTF-16LE bytes. A
/// file that matches in both encodings is reported once per encoding, sorted
/// by path then encoding.
pub fn search_content_dir(content_dir: &Path, pattern: &str) -> anyhow::Result<Vec<GrepMatch>> {
    let utf16_needle: Vec<u8> = pattern
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let ascii = Finder::new(pattern.as_bytes());
    let utf16 = Finder::new(&utf16_needle);

    let mut matches = Vec::new();
    for entry in walkdir::WalkDir::new(content_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
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
        if contains_bytes(&data, &ascii) {
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
}
