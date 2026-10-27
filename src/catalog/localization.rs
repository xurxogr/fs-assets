//! Localization service for resolving text GUIDs to localized strings.
//!
//! Reads UE4 `.locres` files (through [`crate::locres::parse`]) for
//! multi-language text lookup.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

/// The key -> text strings of a `.locres` file, every namespace flattened
/// into one map (keys are GUIDs, unique across namespaces), read with
/// [`crate::locres::parse`]. A file that fails to parse (malformed, legacy
/// or truncated) is reported on stderr and yields an empty map, so one bad
/// locale file never aborts a catalog build but doesn't go unnoticed either.
fn parse_locres(filepath: &Path) -> HashMap<String, String> {
    match crate::locres::parse(filepath) {
        Ok(document) => document
            .namespaces
            .into_iter()
            .flat_map(|namespace| namespace.entries)
            .filter(|entry| !entry.key.is_empty())
            .map(|entry| (entry.key, entry.text))
            .collect(),
        Err(e) => {
            eprintln!(
                "build-catalog: failed to parse {}: {e:#}",
                filepath.display()
            );
            HashMap::new()
        }
    }
}

/// The paths of `dir`'s entries, reporting (and skipping) the directory or
/// any entry that can't be read.
fn read_dir_or_warn(dir: &Path) -> Vec<PathBuf> {
    let warn = |e: io::Error| eprintln!("build-catalog: failed to read {}: {e}", dir.display());
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.map_err(warn).ok())
            .map(|entry| entry.path())
            .collect(),
        Err(e) => {
            warn(e);
            Vec::new()
        }
    }
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
        }
    }

    /// Load all locres files for a language into the cache, unless it is
    /// already there.
    fn ensure_loaded(&self, language: &str) {
        if self.cache.borrow().contains_key(language) {
            return;
        }

        let mut strings: HashMap<String, String> = HashMap::new();

        if !self.localization_dir.exists() {
            self.cache
                .borrow_mut()
                .insert(language.to_string(), strings);
            return;
        }

        // Like a `.locres` that fails to parse, a directory that can't be
        // read is reported and skipped: lookups can't fail, so the language
        // just comes out without those strings.
        for loc_subdir in read_dir_or_warn(&self.localization_dir) {
            if !loc_subdir.is_dir() {
                continue;
            }

            let lang_dir = loc_subdir.join(language);
            if !lang_dir.exists() {
                continue;
            }

            for path in read_dir_or_warn(&lang_dir) {
                if path.extension().and_then(|e| e.to_str()) != Some("locres")
                    || crate::pak::real_file_under(&self.localization_dir, path.clone()).is_none()
                {
                    continue;
                }

                strings.extend(parse_locres(&path));
            }
        }

        self.cache
            .borrow_mut()
            .insert(language.to_string(), strings);
    }

    /// Get the localized string for a GUID. `language` defaults to
    /// `self.default_language` when `None`.
    ///
    /// Only the one string is copied out: a language's table holds every
    /// string of the game and is read once per GUID and language.
    pub fn get(&self, guid: &str, language: Option<&str>) -> Option<String> {
        let language = language.unwrap_or(&self.default_language);
        self.ensure_loaded(language);
        self.cache.borrow().get(language)?.get(guid).cloned()
    }

    /// Get a localized string with fallback to English, then to the raw GUID
    /// itself — never fails.
    pub fn get_with_fallback(&self, guid: &str, language: Option<&str>) -> String {
        let language = language.unwrap_or(&self.default_language);

        if let Some(result) = self.get(guid, Some(language)) {
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
}

/// Whether `text` looks like a localization GUID: exactly 32 hex digits.
pub fn is_guid(text: &str) -> bool {
    text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a `.locres` holding `strings` as (key, text) pairs.
    fn create_mock_locres(filepath: &Path, strings: &[(&str, &str)]) {
        fs::create_dir_all(filepath.parent().unwrap()).unwrap();
        let document = crate::locres::LocresDocument {
            version: 2,
            namespaces: vec![crate::locres::LocresNamespace {
                namespace_hash: 0,
                namespace: String::new(),
                entries: strings
                    .iter()
                    .map(|(key, text)| crate::locres::LocresEntry {
                        key_hash: 0,
                        key: key.to_string(),
                        source_hash: 0,
                        text: text.to_string(),
                    })
                    .collect(),
            }],
        };
        crate::locres::write(&document, filepath).unwrap();
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
            assert!(is_guid("8BB336F4459740A6ADA7B28B2D91748B"));
            assert!(is_guid("abc123def456abc123def456abc12345"));
        }

        #[test]
        fn returns_false_for_non_guid() {
            assert!(!is_guid("ABC123"));
            assert!(!is_guid("8BB336F4459740A6ADA7B28B2D91748B1"));
            assert!(!is_guid("8BB336F4459740A6ADA7B28B2D91748G"));
            assert!(!is_guid("Hello World"));
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

        #[cfg(unix)]
        #[test]
        fn skips_an_unreadable_language_dir_and_loads_the_rest() {
            use std::os::unix::fs::PermissionsExt;

            let tmp = tempfile::tempdir().unwrap();
            create_mock_locres(
                &tmp.path().join("Foxhole-Content/en/game.locres"),
                &[("GUID", "Hello")],
            );
            create_mock_locres(
                &tmp.path().join("Locked-Content/en/other.locres"),
                &[("OTHER", "Hidden")],
            );
            let locked = tmp.path().join("Locked-Content/en");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

            let lookup = LocalizationLookup::new(tmp.path(), None);
            let (found, hidden) = (lookup.get("GUID", None), lookup.get("OTHER", None));
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

            assert_eq!(found, Some("Hello".to_string()));
            assert_eq!(hidden, None);
        }

        #[test]
        fn handles_error_parsing_locres() {
            let tmp = tempfile::tempdir().unwrap();
            let locres_path = tmp.path().join("Foxhole-Content/en/corrupt.locres");
            fs::create_dir_all(locres_path.parent().unwrap()).unwrap();
            fs::write(&locres_path, crate::locres::LOCRES_MAGIC).unwrap();

            let lookup = LocalizationLookup::new(tmp.path(), None);
            assert_eq!(lookup.get("SOMEGUID", Some("en")), None);
        }
    }

    mod unicode_strings {
        use super::*;

        #[test]
        fn handles_unicode_strings() {
            let tmp = tempfile::tempdir().unwrap();
            create_mock_locres(
                &tmp.path().join("Foxhole-Content/zh/test.locres"),
                &[("UNICODEGUID12345678901234567890", "你好世界")],
            );

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
