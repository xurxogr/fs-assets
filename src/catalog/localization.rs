//! Localization service for resolving text GUIDs to localized strings.
//!
//! Parses UE4 `.locres` files (`FTextLocalizationResource` format) for
//! multi-language text lookup.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::locres::TextBudget;

/// Magic number for modern locres files (UE4 `FTextLocalizationResource`).
pub const LOCRES_MAGIC: [u8; 16] = [
    0x0e, 0x14, 0x74, 0x75, 0x67, 0x4a, 0x03, 0xfc, 0x4a, 0x15, 0x90, 0x9d, 0xc3, 0x37, 0x7f, 0x1b,
];

/// Supported languages and their folder names.
pub const SUPPORTED_LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "German"),
    ("fr", "French"),
    ("pt", "Portuguese"),
    ("ru", "Russian"),
    ("zh", "Chinese"),
];

/// Service for loading and querying localized strings.
///
/// Parses UE4 `.locres` files and provides methods to look up localized
/// strings by their GUID keys.
pub struct LocalizationLookup {
    localization_dir: PathBuf,
    default_language: String,
    // {language: {guid: text}}
    cache: RefCell<HashMap<String, HashMap<String, String>>>,
    loaded_files: RefCell<HashSet<String>>,
}

fn resolve_dir(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

fn try_read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => return Ok(false),
            n => filled += n,
        }
    }
    Ok(true)
}

fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_i32<R: Read>(r: &mut R) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}

/// Read `len` bytes, or `None` if the file ends first. The buffer grows as
/// data actually arrives instead of reserving `len` up front: `len` comes
/// from the file, so a hostile length can't make us allocate more than the
/// file really holds.
fn read_bytes<R: Read>(r: &mut R, len: usize) -> io::Result<Option<Vec<u8>>> {
    let mut data = Vec::new();
    r.take(len as u64).read_to_end(&mut data)?;
    Ok((data.len() == len).then_some(data))
}

/// Read a UE4 `FString` — handles both ANSI and Unicode. Returns `Ok(None)`
/// if the read was truncated (not an error — truncation is detected by
/// length rather than raised).
fn read_fstring<R: Read>(r: &mut R) -> io::Result<Option<String>> {
    let mut len_bytes = [0u8; 4];
    if !try_read_exact(r, &mut len_bytes)? {
        return Ok(None);
    }
    let length = i32::from_le_bytes(len_bytes);
    if length == 0 {
        return Ok(Some(String::new()));
    }

    if length < 0 {
        // Unicode (UTF-16)
        // `unsigned_abs` so `i32::MIN` doesn't overflow the negation.
        let Some(data) = read_bytes(r, length.unsigned_abs() as usize * 2)? else {
            return Ok(None);
        };
        let units: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        let s = String::from_utf16_lossy(&units);
        Ok(Some(s.trim_end_matches('\0').to_string()))
    } else {
        // ANSI (Latin-1)
        let Some(data) = read_bytes(r, length as usize)? else {
            return Ok(None);
        };
        let s: String = data.iter().map(|&b| b as char).collect();
        Ok(Some(s.trim_end_matches('\0').to_string()))
    }
}

/// Read an `FTextKey` — hash + string.
fn read_text_key<R: Read>(r: &mut R) -> io::Result<(u32, Option<String>)> {
    let str_hash = read_u32(r)?;
    let string = read_fstring(r)?;
    Ok((str_hash, string))
}

/// Read the compact string array table from a locres file.
fn read_string_array<R: Read + Seek>(r: &mut R, str_array_offset: i64) -> io::Result<Vec<String>> {
    let mut string_array = Vec::new();
    if str_array_offset == -1 {
        return Ok(string_array);
    }

    let current_pos = r.stream_position()?;
    let file_len = r.seek(SeekFrom::End(0))?;
    let offset = u64::try_from(str_array_offset)
        .ok()
        .filter(|&offset| offset < file_len)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "string array offset {str_array_offset} is outside the file ({file_len} bytes)"
                ),
            )
        })?;
    r.seek(SeekFrom::Start(offset))?;

    let str_count = read_i32(r)?;
    for _ in 0..str_count {
        match read_fstring(r)? {
            Some(s) => {
                read_u32(r)?; // source string hash
                string_array.push(s);
            }
            None => break,
        }
    }

    r.seek(SeekFrom::Start(current_pos))?;
    Ok(string_array)
}

/// Read all namespace/key/string entries following the string array.
fn read_namespace_strings<R: Read>(
    r: &mut R,
    version: u8,
    string_array: &[String],
    text_budget: &mut TextBudget,
) -> io::Result<HashMap<String, String>> {
    let mut strings = HashMap::new();

    let namespace_count = read_u32(r)?;
    for _ in 0..namespace_count {
        read_text_key(r)?; // namespace key (hash + string), discarded

        let key_count = read_u32(r)?;
        for _ in 0..key_count {
            let (_, key_string) = read_text_key(r)?;
            read_u32(r)?; // source string hash

            let value = if version >= 1 {
                let str_index = read_i32(r)?;
                let text = usize::try_from(str_index)
                    .ok()
                    .and_then(|index| string_array.get(index))
                    .map_or("", String::as_str);
                text_budget.spend(text)?;
                text.to_string()
            } else {
                read_fstring(r)?.unwrap_or_default()
            };

            if let Some(key) = key_string {
                if !key.is_empty() {
                    strings.insert(key, value);
                }
            }
        }
    }

    Ok(strings)
}

fn parse_locres_inner(filepath: &Path) -> io::Result<HashMap<String, String>> {
    let mut f = File::open(filepath)?;

    let mut magic = [0u8; 16];
    f.read_exact(&mut magic)?;
    if magic != LOCRES_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad magic (legacy .locres format is not supported)",
        ));
    }

    let version = read_u8(&mut f)?;
    let str_array_offset = read_i64(&mut f)?;
    let string_array = read_string_array(&mut f, str_array_offset)?;

    if version >= 2 {
        read_u32(&mut f)?; // entries count, unused
    }

    let mut text_budget = TextBudget::new(f.metadata()?.len());
    read_namespace_strings(&mut f, version, &string_array, &mut text_budget)
}

/// Parse a UE4 `.locres` file. A parse error (malformed or legacy file,
/// truncated data) is reported on stderr and yields an empty map, so one bad
/// locale file never aborts a catalog build but doesn't go unnoticed either.
fn parse_locres(filepath: &Path) -> HashMap<String, String> {
    parse_locres_inner(filepath).unwrap_or_else(|e| {
        eprintln!("build-catalog: failed to parse {}: {e}", filepath.display());
        HashMap::new()
    })
}

impl LocalizationLookup {
    /// Initialize the localization service.
    ///
    /// `localization_dir` is the path to the Localization directory
    /// containing language subdirectories (e.g. `War/Content/Localization`).
    /// `default_language` defaults to `"en"` when `None`.
    pub fn new(localization_dir: &Path, default_language: Option<&str>) -> Self {
        Self {
            localization_dir: resolve_dir(localization_dir),
            default_language: default_language.unwrap_or("en").to_string(),
            cache: RefCell::new(HashMap::new()),
            loaded_files: RefCell::new(HashSet::new()),
        }
    }

    /// Load all locres files for a language, from cache if already loaded.
    fn load_language(&self, language: &str) -> HashMap<String, String> {
        if let Some(cached) = self.cache.borrow().get(language) {
            return cached.clone();
        }

        let mut strings: HashMap<String, String> = HashMap::new();

        if !self.localization_dir.exists() {
            self.cache
                .borrow_mut()
                .insert(language.to_string(), strings.clone());
            return strings;
        }

        if let Ok(entries) = fs::read_dir(&self.localization_dir) {
            for entry in entries.flatten() {
                let loc_subdir = entry.path();
                if !loc_subdir.is_dir() {
                    continue;
                }

                let lang_dir = loc_subdir.join(language);
                if !lang_dir.exists() {
                    continue;
                }

                let Ok(lang_entries) = fs::read_dir(&lang_dir) else {
                    continue;
                };
                for file_entry in lang_entries.flatten() {
                    let path = file_entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("locres") {
                        continue;
                    }

                    let file_key = format!("{}:{}", language, path.display());
                    if self.loaded_files.borrow().contains(&file_key) {
                        continue;
                    }

                    let parsed = parse_locres(&path);
                    strings.extend(parsed);
                    self.loaded_files.borrow_mut().insert(file_key);
                }
            }
        }

        self.cache
            .borrow_mut()
            .insert(language.to_string(), strings.clone());
        strings
    }

    /// Get the localized string for a GUID. `language` defaults to
    /// `self.default_language` when `None`.
    pub fn get(&self, guid: &str, language: Option<&str>) -> Option<String> {
        let language = language.unwrap_or(&self.default_language).to_string();
        let strings = self.load_language(&language);
        strings.get(guid).cloned()
    }

    /// Get a localized string with fallback to English, then to the raw GUID
    /// itself — never fails.
    pub fn get_with_fallback(&self, guid: &str, language: Option<&str>) -> String {
        let language = language.unwrap_or(&self.default_language).to_string();

        if let Some(result) = self.get(guid, Some(&language)) {
            if !result.is_empty() {
                return result;
            }
        }

        if language != "en" {
            if let Some(result) = self.get(guid, Some("en")) {
                if !result.is_empty() {
                    return result;
                }
            }
        }

        guid.to_string()
    }

    /// Get the localized string in all available languages.
    pub fn get_all_languages(&self, guid: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();
        for (lang_code, _) in SUPPORTED_LANGUAGES {
            if let Some(text) = self.get(guid, Some(lang_code)) {
                if !text.is_empty() {
                    result.insert((*lang_code).to_string(), text);
                }
            }
        }
        result
    }

    /// Check if a value looks like a localization GUID (exactly 32 hex chars).
    pub fn is_guid(&self, value: &Value) -> bool {
        match value.as_str() {
            Some(s) => s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit()),
            None => false,
        }
    }

    /// Get statistics about loaded localizations: counts per language.
    pub fn get_stats(&self) -> HashMap<String, usize> {
        self.cache
            .borrow()
            .iter()
            .map(|(lang, strings)| (lang.clone(), strings.len()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a mock `.locres` file matching the format `parse_locres` reads.
    fn create_mock_locres(filepath: &Path, strings: &[(&str, &str)]) {
        fs::create_dir_all(filepath.parent().unwrap()).unwrap();

        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&LOCRES_MAGIC);
        buf.push(2); // version 2 (Optimized_CRC32)

        let str_array_offset_pos = buf.len();
        buf.extend_from_slice(&(-1i64).to_le_bytes());

        buf.extend_from_slice(&(strings.len() as u32).to_le_bytes()); // entries count
        buf.extend_from_slice(&1u32.to_le_bytes()); // namespace count

        buf.extend_from_slice(&0u32.to_le_bytes()); // namespace key hash
        buf.extend_from_slice(&0i32.to_le_bytes()); // empty namespace string

        buf.extend_from_slice(&(strings.len() as u32).to_le_bytes()); // key count

        let string_list: Vec<&str> = strings.iter().map(|(_, v)| *v).collect();

        for (guid, text) in strings {
            buf.extend_from_slice(&0u32.to_le_bytes()); // key hash

            let mut guid_bytes = guid.as_bytes().to_vec();
            guid_bytes.push(0);
            buf.extend_from_slice(&(guid_bytes.len() as i32).to_le_bytes());
            buf.extend_from_slice(&guid_bytes);

            buf.extend_from_slice(&0u32.to_le_bytes()); // source string hash

            let str_index = string_list.iter().position(|s| s == text).unwrap();
            buf.extend_from_slice(&(str_index as i32).to_le_bytes());
        }

        let str_array_offset = buf.len() as i64;
        buf.extend_from_slice(&(string_list.len() as i32).to_le_bytes());
        for text in &string_list {
            let mut text_bytes = text.as_bytes().to_vec();
            text_bytes.push(0);
            buf.extend_from_slice(&(text_bytes.len() as i32).to_le_bytes());
            buf.extend_from_slice(&text_bytes);
            buf.extend_from_slice(&0u32.to_le_bytes()); // source string hash
        }

        buf[str_array_offset_pos..str_array_offset_pos + 8]
            .copy_from_slice(&str_array_offset.to_le_bytes());

        fs::write(filepath, buf).unwrap();
    }

    mod init {
        use super::*;

        #[test]
        fn sets_localization_dir() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(
                lookup.localization_dir,
                fs::canonicalize(tmp.path()).unwrap()
            );
        }

        #[test]
        fn sets_default_language() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.default_language, "en");
        }

        #[test]
        fn sets_custom_default_language() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), Some("de"));
            assert_eq!(lookup.default_language, "de");
        }

        #[test]
        fn creates_empty_cache() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert!(lookup.cache.borrow().is_empty());
        }
    }

    mod get {
        use super::*;

        #[test]
        fn returns_none_for_missing_dir() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(&tmp.path().join("nonexistent"), None);
            assert_eq!(lookup.get("SOMEGUID", None), None);
        }

        #[test]
        fn returns_none_for_missing_guid() {
            let tmp = tempfile::tempdir().unwrap();
            let locres_path = tmp.path().join("Foxhole-Content/en/test.locres");
            create_mock_locres(&locres_path, &[("ABC123", "Hello")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get("NONEXISTENT", None), None);
        }

        #[test]
        fn returns_string_for_existing_guid() {
            let tmp = tempfile::tempdir().unwrap();
            let locres_path = tmp.path().join("Foxhole-Content/en/test.locres");
            create_mock_locres(&locres_path, &[("ABC123", "Hello World")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get("ABC123", None), Some("Hello World".to_string()));
        }

        #[test]
        fn uses_specified_language() {
            let tmp = tempfile::tempdir().unwrap();
            let en_path = tmp.path().join("Foxhole-Content/en/test.locres");
            let de_path = tmp.path().join("Foxhole-Content/de/test.locres");
            create_mock_locres(&en_path, &[("GREETING", "Hello")]);
            create_mock_locres(&de_path, &[("GREETING", "Hallo")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(
                lookup.get("GREETING", Some("en")),
                Some("Hello".to_string())
            );
            assert_eq!(
                lookup.get("GREETING", Some("de")),
                Some("Hallo".to_string())
            );
        }

        #[test]
        fn caches_loaded_language() {
            let tmp = tempfile::tempdir().unwrap();
            let locres_path = tmp.path().join("Foxhole-Content/en/test.locres");
            create_mock_locres(&locres_path, &[("ABC123", "Hello")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            lookup.get("ABC123", None);

            assert!(lookup.cache.borrow().contains_key("en"));
            assert_eq!(
                lookup.cache.borrow()["en"].get("ABC123"),
                Some(&"Hello".to_string())
            );
        }
    }

    mod get_with_fallback {
        use super::*;

        #[test]
        fn returns_string_for_existing_guid() {
            let tmp = tempfile::tempdir().unwrap();
            let locres_path = tmp.path().join("Foxhole-Content/en/test.locres");
            create_mock_locres(&locres_path, &[("ABC123", "Hello")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get_with_fallback("ABC123", None), "Hello");
        }

        #[test]
        fn returns_english_when_language_missing() {
            let tmp = tempfile::tempdir().unwrap();
            let en_path = tmp.path().join("Foxhole-Content/en/test.locres");
            create_mock_locres(&en_path, &[("GREETING", "Hello")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get_with_fallback("GREETING", Some("de")), "Hello");
        }

        #[test]
        fn returns_guid_when_not_found() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(
                lookup.get_with_fallback("NONEXISTENT123", None),
                "NONEXISTENT123"
            );
        }
    }

    mod get_all_languages {
        use super::*;

        #[test]
        fn returns_all_available_translations() {
            let tmp = tempfile::tempdir().unwrap();
            for lang in ["en", "de", "fr"] {
                let path = tmp
                    .path()
                    .join(format!("Foxhole-Content/{lang}/test.locres"));
                create_mock_locres(&path, &[("GREETING", &format!("Hello-{lang}"))]);
            }

            let lookup = LocalizationLookup::new(tmp.path(), None);
            let result = lookup.get_all_languages("GREETING");

            assert_eq!(result.get("en"), Some(&"Hello-en".to_string()));
            assert_eq!(result.get("de"), Some(&"Hello-de".to_string()));
            assert_eq!(result.get("fr"), Some(&"Hello-fr".to_string()));
        }

        #[test]
        fn returns_empty_for_missing_guid() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert!(lookup.get_all_languages("NONEXISTENT").is_empty());
        }
    }

    mod is_guid {
        use super::*;

        #[test]
        fn returns_true_for_valid_guid() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert!(lookup.is_guid(&json!("8BB336F4459740A6ADA7B28B2D91748B")));
            assert!(lookup.is_guid(&json!("abc123def456abc123def456abc12345")));
        }

        #[test]
        fn returns_false_for_non_guid() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert!(!lookup.is_guid(&json!("ABC123")));
            assert!(!lookup.is_guid(&json!("8BB336F4459740A6ADA7B28B2D91748B1")));
            assert!(!lookup.is_guid(&json!("8BB336F4459740A6ADA7B28B2D91748G")));
            assert!(!lookup.is_guid(&json!("Hello World")));
            assert!(!lookup.is_guid(&json!(12345)));
            assert!(!lookup.is_guid(&Value::Null));
        }
    }

    mod get_stats {
        use super::*;

        #[test]
        fn returns_counts() {
            let tmp = tempfile::tempdir().unwrap();
            let en_path = tmp.path().join("Foxhole-Content/en/test.locres");
            let de_path = tmp.path().join("Foxhole-Content/de/test.locres");
            create_mock_locres(&en_path, &[("A", "Hello"), ("B", "World")]);
            create_mock_locres(&de_path, &[("A", "Hallo")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            lookup.get("A", Some("en"));
            lookup.get("A", Some("de"));

            let stats = lookup.get_stats();
            assert_eq!(stats.get("en"), Some(&2));
            assert_eq!(stats.get("de"), Some(&1));
        }

        #[test]
        fn returns_empty_for_no_loaded() {
            let tmp = tempfile::tempdir().unwrap();
            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert!(lookup.get_stats().is_empty());
        }
    }

    mod supported_languages {
        use super::*;

        #[test]
        fn has_expected_entries() {
            let langs: HashMap<&str, &str> = SUPPORTED_LANGUAGES.iter().copied().collect();
            assert!(langs.contains_key("en"));
            assert!(langs.contains_key("de"));
            assert!(langs.contains_key("fr"));
        }

        #[test]
        fn maps_to_folder_names() {
            let langs: HashMap<&str, &str> = SUPPORTED_LANGUAGES.iter().copied().collect();
            assert_eq!(langs["en"], "English");
            assert_eq!(langs["de"], "German");
            assert_eq!(langs["fr"], "French");
        }
    }

    mod edge_cases {
        use super::*;

        #[test]
        fn handles_legacy_locres_format() {
            let tmp = tempfile::tempdir().unwrap();
            let locres_path = tmp.path().join("Foxhole-Content/en/legacy.locres");
            fs::create_dir_all(locres_path.parent().unwrap()).unwrap();
            fs::write(&locres_path, b"WRONG_MAGIC_BYTES").unwrap();

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get("SOMEGUID", Some("en")), None);
        }

        #[test]
        fn legacy_and_truncated_files_are_parse_errors_not_empty_maps() {
            let tmp = tempfile::tempdir().unwrap();
            let legacy = tmp.path().join("legacy.locres");
            fs::write(&legacy, b"WRONG_MAGIC_BYTES").unwrap();
            let err = parse_locres_inner(&legacy).unwrap_err();
            assert!(err.to_string().contains("bad magic"), "{err}");

            let truncated = tmp.path().join("truncated.locres");
            fs::write(&truncated, LOCRES_MAGIC).unwrap();
            assert!(parse_locres_inner(&truncated).is_err());
        }

        #[test]
        fn handles_missing_language_dir() {
            let tmp = tempfile::tempdir().unwrap();
            let en_path = tmp.path().join("Foxhole-Content/en/test.locres");
            create_mock_locres(&en_path, &[("GUID", "Hello")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get("GUID", Some("de")), None);
        }

        #[test]
        fn handles_non_directory_in_localization_dir() {
            let tmp = tempfile::tempdir().unwrap();
            fs::write(tmp.path().join("not_a_dir.txt"), b"").unwrap();

            let en_path = tmp.path().join("Foxhole-Content/en/test.locres");
            create_mock_locres(&en_path, &[("GUID", "Hello")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get("GUID", Some("en")), Some("Hello".to_string()));
        }

        #[test]
        fn skips_already_loaded_files() {
            let tmp = tempfile::tempdir().unwrap();
            let en_path = tmp.path().join("Foxhole-Content/en/test.locres");
            create_mock_locres(&en_path, &[("GUID", "Hello")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);

            assert_eq!(lookup.get("GUID", Some("en")), Some("Hello".to_string()));

            lookup.cache.borrow_mut().clear();

            // File won't be reloaded since it's tracked in loaded_files, so
            // the freshly-cleared cache stays empty for this GUID.
            assert_eq!(lookup.get("GUID", Some("en")), None);
        }

        #[test]
        fn handles_error_parsing_locres() {
            let tmp = tempfile::tempdir().unwrap();
            let locres_path = tmp.path().join("Foxhole-Content/en/corrupt.locres");
            fs::create_dir_all(locres_path.parent().unwrap()).unwrap();
            fs::write(&locres_path, LOCRES_MAGIC).unwrap();

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get("SOMEGUID", Some("en")), None);
        }

        /// A version-2 header with `str_array_offset`, followed by `rest`.
        fn header(str_array_offset: i64, rest: &[u8]) -> Vec<u8> {
            let mut buf = LOCRES_MAGIC.to_vec();
            buf.push(2);
            buf.extend_from_slice(&str_array_offset.to_le_bytes());
            buf.extend_from_slice(rest);
            buf
        }

        #[test]
        fn hostile_counts_and_lengths_error_instead_of_allocating() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("hostile.locres");
            for length in [i32::MAX, i32::MIN, -1_000_000_000] {
                let mut rest = 0u32.to_le_bytes().to_vec(); // entries count
                rest.extend_from_slice(&u32::MAX.to_le_bytes()); // namespace count
                rest.extend_from_slice(&0u32.to_le_bytes()); // namespace hash
                rest.extend_from_slice(&length.to_le_bytes());
                fs::write(&path, header(-1, &rest)).unwrap();
                assert!(parse_locres_inner(&path).is_err(), "{length}");
            }
        }

        #[test]
        fn rejects_a_string_array_offset_outside_the_file() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("offset.locres");
            for offset in [-2, 1 << 40] {
                fs::write(&path, header(offset, &[0; 8])).unwrap();
                let err = parse_locres_inner(&path).unwrap_err();
                assert!(
                    err.to_string().contains("outside the file"),
                    "{offset}: {err}"
                );
            }
        }

        #[test]
        fn rejects_entries_that_resolve_to_far_more_text_than_the_file() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("shared.locres");

            fs::write(&path, crate::locres::shared_text_locres(4, 100)).unwrap();
            assert_eq!(parse_locres_inner(&path).unwrap().len(), 4);

            fs::write(&path, crate::locres::shared_text_locres(2000, 4096)).unwrap();
            let err = parse_locres_inner(&path).unwrap_err();
            assert!(err.to_string().contains("the file's size in text"), "{err}");
        }
    }

    mod unicode_strings {
        use super::*;

        #[test]
        fn handles_unicode_strings() {
            let tmp = tempfile::tempdir().unwrap();
            let locres_path = tmp.path().join("Foxhole-Content/zh/test.locres");
            fs::create_dir_all(locres_path.parent().unwrap()).unwrap();

            let mut buf: Vec<u8> = Vec::new();
            buf.extend_from_slice(&LOCRES_MAGIC);
            buf.push(2); // version 2

            let str_array_offset_pos = buf.len();
            buf.extend_from_slice(&(-1i64).to_le_bytes());

            buf.extend_from_slice(&1u32.to_le_bytes()); // entries count
            buf.extend_from_slice(&1u32.to_le_bytes()); // namespace count

            buf.extend_from_slice(&0u32.to_le_bytes()); // namespace hash
            buf.extend_from_slice(&0i32.to_le_bytes()); // empty namespace string

            buf.extend_from_slice(&1u32.to_le_bytes()); // key count

            buf.extend_from_slice(&0u32.to_le_bytes()); // key hash
            let guid = "UNICODEGUID12345678901234567890";
            let mut guid_bytes = guid.as_bytes().to_vec();
            guid_bytes.push(0);
            buf.extend_from_slice(&(guid_bytes.len() as i32).to_le_bytes());
            buf.extend_from_slice(&guid_bytes);

            buf.extend_from_slice(&0u32.to_le_bytes()); // source string hash
            buf.extend_from_slice(&0i32.to_le_bytes()); // string index

            let str_array_offset = buf.len() as i64;
            buf.extend_from_slice(&1i32.to_le_bytes()); // string array count

            let chinese_text = "你好世界";
            let units: Vec<u16> = chinese_text.encode_utf16().collect();
            buf.extend_from_slice(&(-(units.len() as i32 + 1)).to_le_bytes());
            for u in &units {
                buf.extend_from_slice(&u.to_le_bytes());
            }
            buf.extend_from_slice(&0u16.to_le_bytes()); // null terminator

            buf.extend_from_slice(&0u32.to_le_bytes()); // source string hash

            buf[str_array_offset_pos..str_array_offset_pos + 8]
                .copy_from_slice(&str_array_offset.to_le_bytes());

            fs::write(&locres_path, buf).unwrap();

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(
                lookup.get("UNICODEGUID12345678901234567890", Some("zh")),
                Some("你好世界".to_string())
            );
        }
    }

    mod multiple_files {
        use super::*;

        #[test]
        fn merges_multiple_locres_files() {
            let tmp = tempfile::tempdir().unwrap();
            let path1 = tmp.path().join("Foxhole-Content/en/file1.locres");
            let path2 = tmp.path().join("Foxhole-CodeStrings/en/file2.locres");
            create_mock_locres(&path1, &[("GUID1", "Text1")]);
            create_mock_locres(&path2, &[("GUID2", "Text2")]);

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get("GUID1", Some("en")), Some("Text1".to_string()));
            assert_eq!(lookup.get("GUID2", Some("en")), Some("Text2".to_string()));
        }
    }
}
