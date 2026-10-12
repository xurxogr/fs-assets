//! Test helpers that build small `.pak` archives.

use std::path::{Path, PathBuf};

use crate::pak;

/// Packs `files` (pak paths, with placeholder contents) into `name`.
pub fn write_pak(dir: &Path, name: &str, files: &[&str]) -> PathBuf {
    write_pak_with(dir, name, files, b"not a real texture")
}

/// Packs `files` into `name`, every one holding `contents`.
pub fn write_pak_with(dir: &Path, name: &str, files: &[&str], contents: &[u8]) -> PathBuf {
    let content = dir.join(format!("{name}_content"));
    for file in files {
        let path = content.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    let pak_path = dir.join(name);
    pak::pack(&content, &pak_path, "../../../", repak::Version::V8B).unwrap();
    pak_path
}
