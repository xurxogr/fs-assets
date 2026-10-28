//! Lossless `.locres` (UE4 `FTextLocalizationResource`, version 2) dump/pack.
//!
//! It round-trips a `.locres` byte-for-byte-equivalent (modulo `ref_count`,
//! which is a non-authoritative size hint — see below), which the
//! translation workflow requires. The catalog build reads `.locres` files
//! through [`parse`] too, flattening every namespace into one key->text map
//! (see `catalog::localization`).
//!
//! Hashes (`key_hash`/`namespace_hash`/`source_hash`) are never recomputed:
//! they are carried over from the input. The `en` file holds every namespace
//! and key the other languages use, which is why `--reference` takes it.

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Magic number for version-2 `.locres` files.
pub const LOCRES_MAGIC: [u8; 16] = [
    0x0e, 0x14, 0x74, 0x75, 0x67, 0x4a, 0x03, 0xfc, 0x4a, 0x15, 0x90, 0x9d, 0xc3, 0x37, 0x7f, 0x1b,
];

/// One (key, text) entry within a namespace, preserving every hash field.
#[derive(Debug, Clone, PartialEq)]
pub struct LocresEntry {
    pub key_hash: u32,
    pub key: String,
    /// Hash of the *English* source text for this (namespace, key) — see
    /// module doc. Copied verbatim on round-trip, never recomputed.
    pub source_hash: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocresNamespace {
    pub namespace_hash: u32,
    pub namespace: String,
    pub entries: Vec<LocresEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocresDocument {
    /// Always 2 in practice; `parse`/`write` reject anything else.
    pub version: u8,
    pub namespaces: Vec<LocresNamespace>,
}

fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_i32<R: Read>(r: &mut R) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}

fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}

/// Read exactly `len` bytes, growing the buffer as data actually arrives
/// instead of reserving `len` up front: `len` comes from the file, so a
/// hostile length can't make us allocate more than the file really holds.
fn read_bytes<R: Read>(r: &mut R, len: usize) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    r.take(len as u64).read_to_end(&mut data)?;
    if data.len() != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("string of {len} bytes runs past end of file"),
        ));
    }
    Ok(data)
}

/// Read a UE4 `FString`: `i32` length prefix, empty/UTF-16/Latin-1 body per
/// the sign of the length, trailing NUL stripped. Truncation is an error:
/// this reader is meant to faithfully round-trip real files, not to shrug
/// off malformed ones.
fn read_fstring<R: Read>(r: &mut R) -> io::Result<String> {
    let length = read_i32(r)?;
    if length == 0 {
        return Ok(String::new());
    }

    if length < 0 {
        // `unsigned_abs` so `i32::MIN` doesn't overflow the negation.
        let data = read_bytes(r, length.unsigned_abs() as usize * 2)?;
        let units: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        let s = String::from_utf16_lossy(&units);
        Ok(s.trim_end_matches('\0').to_string())
    } else {
        let data = read_bytes(r, length as usize)?;
        let s: String = data.iter().map(|&b| b as char).collect();
        Ok(s.trim_end_matches('\0').to_string())
    }
}

/// `len` as the integer type a `.locres` stores it in, or an error naming
/// `what` when it doesn't fit, rather than writing a wrapped, corrupt length.
fn stored_len<T: TryFrom<usize>>(len: usize, what: &str) -> io::Result<T> {
    T::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{what} ({len}) doesn't fit in a .locres"),
        )
    })
}

/// Write a UE4 `FString`, using the single-byte/ANSI path only for pure
/// ASCII strings and UTF-16LE for everything else — matching how the base
/// game's own non-English `.locres` files (e.g. German ü/ö/ä) are encoded.
fn write_fstring<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    if s.is_empty() {
        return w.write_all(&0i32.to_le_bytes());
    }

    if s.is_ascii() {
        let mut bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
        bytes.push(0);
        w.write_all(&stored_len::<i32>(bytes.len(), "string length")?.to_le_bytes())?;
        w.write_all(&bytes)
    } else {
        let mut units: Vec<u16> = s.encode_utf16().collect();
        units.push(0);
        // Never overflows: a stored length is at most `i32::MAX`.
        w.write_all(&(-stored_len::<i32>(units.len(), "string length")?).to_le_bytes())?;
        for u in &units {
            w.write_all(&u.to_le_bytes())?;
        }
        Ok(())
    }
}

/// Bound on the text a `.locres` file may resolve to, as a multiple of the
/// file's size. Entries reference a shared string array, so a small file can
/// point thousands of keys at one long string and make every entry copy it;
/// real game files resolve to under 1x their size.
const MAX_TEXT_PER_FILE_BYTE: u64 = 16;

/// Running total of the text resolved from one `.locres` file, failing once
/// it passes [`MAX_TEXT_PER_FILE_BYTE`] times the file's size.
struct TextBudget {
    remaining: u64,
}

impl TextBudget {
    fn new(file_len: u64) -> Self {
        Self {
            remaining: file_len.saturating_mul(MAX_TEXT_PER_FILE_BYTE),
        }
    }

    fn spend(&mut self, text: &str) -> io::Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(text.len() as u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "entries resolve to more than {MAX_TEXT_PER_FILE_BYTE}x the file's size in text"
                    ),
                )
            })?;
        Ok(())
    }
}

/// Reads an edited JSON translation file written by [`dump`].
pub fn read_dump(path: &Path) -> Result<DumpDocument> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Parse a version-2 `.locres` file, preserving every namespace, key, hash,
/// and string exactly.
pub fn parse(path: &Path) -> Result<LocresDocument> {
    let mut f =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);

    let mut magic = [0u8; 16];
    f.read_exact(&mut magic)
        .with_context(|| format!("reading magic from {}", path.display()))?;
    if magic != LOCRES_MAGIC {
        bail!("{}: not a .locres file (bad magic)", path.display());
    }

    let version = read_u8(&mut f)?;
    if version != 2 {
        bail!(
            "{}: unsupported .locres version {version} (only version 2 is supported)",
            path.display()
        );
    }

    let string_array_offset = read_i64(&mut f)?;
    let entries_count = read_u32(&mut f)?;
    let _ = entries_count; // informational; recomputed on write, not trusted here

    let namespace_count = read_u32(&mut f)?;
    struct PendingEntry {
        key_hash: u32,
        key: String,
        source_hash: u32,
        string_index: i32,
    }
    struct PendingNamespace {
        namespace_hash: u32,
        namespace: String,
        entries: Vec<PendingEntry>,
    }

    // Counts come from the file: grow as entries are read, never reserve them.
    let mut pending = Vec::new();
    for _ in 0..namespace_count {
        let namespace_hash = read_u32(&mut f)?;
        let namespace = read_fstring(&mut f)?;
        let key_count = read_u32(&mut f)?;

        let mut entries = Vec::new();
        for _ in 0..key_count {
            let key_hash = read_u32(&mut f)?;
            let key = read_fstring(&mut f)?;
            let source_hash = read_u32(&mut f)?;
            let string_index = read_i32(&mut f)?;
            entries.push(PendingEntry {
                key_hash,
                key,
                source_hash,
                string_index,
            });
        }

        pending.push(PendingNamespace {
            namespace_hash,
            namespace,
            entries,
        });
    }

    let file_len = f
        .get_ref()
        .metadata()
        .with_context(|| format!("reading size of {}", path.display()))?
        .len();
    let string_array_offset = u64::try_from(string_array_offset)
        .ok()
        .filter(|&offset| offset < file_len)
        .with_context(|| {
            format!(
                "{}: string array offset {string_array_offset} is outside the file ({file_len} bytes)",
                path.display()
            )
        })?;
    f.seek(SeekFrom::Start(string_array_offset))
        .with_context(|| format!("seeking to string array in {}", path.display()))?;
    let string_count = read_i32(&mut f)?;
    let mut string_array = Vec::new();
    for _ in 0..string_count {
        let text = read_fstring(&mut f)?;
        let _ref_count = read_u32(&mut f)?; // advisory size hint, not needed to read
        string_array.push(text);
    }

    let mut text_budget = TextBudget::new(file_len);
    let mut namespaces = Vec::new();
    for ns in pending {
        let mut entries = Vec::new();
        for e in ns.entries {
            // An entry pointing outside the string array is a corrupt file;
            // reading it as "" would only fail later, confusingly, when the
            // dump is packed back ("empty text field").
            let text = usize::try_from(e.string_index)
                .ok()
                .and_then(|index| string_array.get(index))
                .cloned()
                .with_context(|| {
                    format!(
                        "{}: {}/{} points at string {}, but there are only {}",
                        path.display(),
                        ns.namespace,
                        e.key,
                        e.string_index,
                        string_array.len()
                    )
                })?;
            text_budget
                .spend(&text)
                .with_context(|| path.display().to_string())?;
            entries.push(LocresEntry {
                key_hash: e.key_hash,
                key: e.key,
                source_hash: e.source_hash,
                text,
            });
        }
        namespaces.push(LocresNamespace {
            namespace_hash: ns.namespace_hash,
            namespace: ns.namespace,
            entries,
        });
    }

    Ok(LocresDocument {
        version,
        namespaces,
    })
}

/// Write a `LocresDocument` back out as a version-2 `.locres` file.
///
/// Doesn't dedupe the string array (one `StringArray` entry per
/// `LocresEntry`, `ref_count` always `1`) — the format doesn't require
/// dedup for validity, and it's simplest-correct per the translation guide.
pub fn write(doc: &LocresDocument, path: &Path) -> Result<()> {
    if doc.version != 2 {
        bail!(
            "refusing to write unsupported .locres version {}",
            doc.version
        );
    }

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&LOCRES_MAGIC);
    buf.push(doc.version);

    let string_array_offset_pos = buf.len();
    buf.extend_from_slice(&0i64.to_le_bytes()); // placeholder, backpatched below

    let entries_count_pos = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // placeholder, backpatched below

    buf.extend_from_slice(
        &stored_len::<u32>(doc.namespaces.len(), "namespace count")?.to_le_bytes(),
    );

    let mut total_entries: u32 = 0;
    let mut texts: Vec<&str> = Vec::new();

    for ns in &doc.namespaces {
        buf.extend_from_slice(&ns.namespace_hash.to_le_bytes());
        write_fstring(&mut buf, &ns.namespace)?;
        buf.extend_from_slice(&stored_len::<u32>(ns.entries.len(), "entry count")?.to_le_bytes());

        for entry in &ns.entries {
            buf.extend_from_slice(&entry.key_hash.to_le_bytes());
            write_fstring(&mut buf, &entry.key)?;
            buf.extend_from_slice(&entry.source_hash.to_le_bytes());

            let string_index = stored_len::<i32>(texts.len(), "string count")?;
            buf.extend_from_slice(&string_index.to_le_bytes());
            texts.push(&entry.text);
            total_entries = total_entries
                .checked_add(1)
                .context("entry count doesn't fit in a .locres")?;
        }
    }

    let string_array_offset = stored_len::<i64>(buf.len(), "string array offset")?;
    buf.extend_from_slice(&stored_len::<i32>(texts.len(), "string count")?.to_le_bytes());
    for text in &texts {
        write_fstring(&mut buf, text)?;
        buf.extend_from_slice(&1u32.to_le_bytes()); // ref_count: advisory, always 1 (no dedup)
    }

    buf[string_array_offset_pos..string_array_offset_pos + 8]
        .copy_from_slice(&string_array_offset.to_le_bytes());
    buf[entries_count_pos..entries_count_pos + 4].copy_from_slice(&total_entries.to_le_bytes());

    crate::atomic_write::write_bytes(path, &buf)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// A `.locres` whose `keys` entries all point at one `text_len`-byte string,
/// so the file is small but resolves to `keys * text_len` bytes of text.
#[cfg(test)]
pub(crate) fn shared_text_locres(keys: u32, text_len: usize) -> Vec<u8> {
    let mut buf = LOCRES_MAGIC.to_vec();
    buf.push(2);
    let string_array_offset_pos = buf.len();
    buf.extend_from_slice(&0i64.to_le_bytes());
    buf.extend_from_slice(&keys.to_le_bytes()); // entries count
    buf.extend_from_slice(&1u32.to_le_bytes()); // namespace count
    buf.extend_from_slice(&0u32.to_le_bytes()); // namespace hash
    write_fstring(&mut buf, "").unwrap();
    buf.extend_from_slice(&keys.to_le_bytes()); // key count
    for i in 0..keys {
        buf.extend_from_slice(&0u32.to_le_bytes()); // key hash
        write_fstring(&mut buf, &format!("k{i}")).unwrap();
        buf.extend_from_slice(&0u32.to_le_bytes()); // source hash
        buf.extend_from_slice(&0i32.to_le_bytes()); // string index
    }
    let string_array_offset = buf.len() as i64;
    buf.extend_from_slice(&1i32.to_le_bytes());
    write_fstring(&mut buf, &"x".repeat(text_len)).unwrap();
    buf.extend_from_slice(&keys.to_le_bytes()); // ref count
    buf[string_array_offset_pos..string_array_offset_pos + 8]
        .copy_from_slice(&string_array_offset.to_le_bytes());
    buf
}

// ---------------------------------------------------------------------
// JSON dump/pack — the `locres-dump`/`locres-pack` CLI commands' data model.
// ---------------------------------------------------------------------

/// One entry in a dump JSON file. `input_text` is read-only/informational
/// (never consumed by `pack`); only `text` round-trips into the written
/// `.locres`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpEntry {
    pub key: String,
    pub key_hash: u32,
    pub source_hash: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input_text: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpNamespace {
    pub namespace: String,
    pub namespace_hash: u32,
    pub entries: Vec<DumpEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpDocument {
    pub version: u8,
    pub namespaces: Vec<DumpNamespace>,
}

/// Build a `DumpDocument` from a parsed `input` `.locres`, optionally
/// structured against a `reference` (typically `en`, the structural
/// superset of namespaces and keys).
///
/// Without a reference, `input`'s own namespace/key set and hashes are used
/// verbatim, `input_text == text` for every entry.
///
/// With a reference, the reference's namespace/key set and hash fields
/// become authoritative: entries present in `input` get `input_text`/`text`
/// seeded from `input`'s text; entries missing from `input` get
/// `input_text: null` and `text` seeded from the reference's own text (the
/// English fallback).
pub fn dump(input: &Path, reference: Option<&Path>) -> Result<DumpDocument> {
    let input_doc = parse(input)?;

    let Some(reference_path) = reference else {
        let namespaces = input_doc
            .namespaces
            .into_iter()
            .map(|ns| DumpNamespace {
                namespace: ns.namespace,
                namespace_hash: ns.namespace_hash,
                entries: ns
                    .entries
                    .into_iter()
                    .map(|e| DumpEntry {
                        key: e.key,
                        key_hash: e.key_hash,
                        source_hash: e.source_hash,
                        input_text: Some(e.text.clone()),
                        text: e.text,
                    })
                    .collect(),
            })
            .collect();
        return Ok(DumpDocument {
            version: input_doc.version,
            namespaces,
        });
    };

    let reference_doc = parse(reference_path)?;

    let mut input_lookup: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    for ns in &input_doc.namespaces {
        for entry in &ns.entries {
            input_lookup.insert(
                (ns.namespace.clone(), entry.key.clone()),
                entry.text.clone(),
            );
        }
    }

    let namespaces = reference_doc
        .namespaces
        .into_iter()
        .map(|ns| {
            let entries = ns
                .entries
                .into_iter()
                .map(|e| {
                    let input_text = input_lookup
                        .get(&(ns.namespace.clone(), e.key.clone()))
                        .cloned();
                    let text = input_text.clone().unwrap_or_else(|| e.text.clone());
                    DumpEntry {
                        key: e.key,
                        key_hash: e.key_hash,
                        source_hash: e.source_hash,
                        input_text,
                        text,
                    }
                })
                .collect();
            DumpNamespace {
                namespace: ns.namespace,
                namespace_hash: ns.namespace_hash,
                entries,
            }
        })
        .collect();

    Ok(DumpDocument {
        version: reference_doc.version,
        namespaces,
    })
}

/// Convert a `DumpDocument` (as edited by a translator) into a
/// `LocresDocument` ready for `write`. Ignores `input_text` entirely.
/// Errors loudly (rather than silently shipping empty strings into the
/// game) if the version isn't 2 or any entry's `text` is empty.
pub fn pack(doc: &DumpDocument) -> Result<LocresDocument> {
    if doc.version != 2 {
        bail!("unsupported .locres version {} in dump JSON", doc.version);
    }

    let namespaces = doc
        .namespaces
        .iter()
        .map(|ns| {
            let entries = ns
                .entries
                .iter()
                .map(|e| {
                    if e.text.is_empty() {
                        bail!(
                            "namespace {:?} key {:?} has an empty text field — fill it in before packing",
                            ns.namespace,
                            e.key
                        );
                    }
                    Ok(LocresEntry {
                        key_hash: e.key_hash,
                        key: e.key.clone(),
                        source_hash: e.source_hash,
                        text: e.text.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(LocresNamespace {
                namespace_hash: ns.namespace_hash,
                namespace: ns.namespace.clone(),
                entries,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(LocresDocument {
        version: doc.version,
        namespaces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc() -> LocresDocument {
        LocresDocument {
            version: 2,
            namespaces: vec![
                LocresNamespace {
                    namespace_hash: 111,
                    namespace: "STActions".to_string(),
                    entries: vec![
                        LocresEntry {
                            key_hash: 1,
                            key: "ActionAim".to_string(),
                            source_hash: 999,
                            text: "Aim - Hold".to_string(),
                        },
                        LocresEntry {
                            key_hash: 2,
                            key: "ActionFire".to_string(),
                            source_hash: 998,
                            text: "Fire".to_string(),
                        },
                    ],
                },
                LocresNamespace {
                    namespace_hash: 222,
                    namespace: "".to_string(),
                    entries: vec![LocresEntry {
                        key_hash: 3,
                        key: "8BB336F4459740A6ADA7B28B2D91748".to_string(),
                        source_hash: 997,
                        text: "你好世界".to_string(),
                    }],
                },
            ],
        }
    }

    mod round_trip {
        use super::*;

        #[test]
        fn parse_write_parse_is_identical() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("test.locres");
            let doc = sample_doc();

            write(&doc, &path).unwrap();
            let reparsed = parse(&path).unwrap();

            assert_eq!(reparsed, doc);
        }

        #[test]
        fn preserves_accented_text_as_utf16() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("test.locres");
            let doc = LocresDocument {
                version: 2,
                namespaces: vec![LocresNamespace {
                    namespace_hash: 1,
                    namespace: "STItems".to_string(),
                    entries: vec![LocresEntry {
                        key_hash: 1,
                        key: "ItemRifle".to_string(),
                        source_hash: 1,
                        text: "Fusil de asalto - ¡Atención, señor! ¿Listo?".to_string(),
                    }],
                }],
            };

            write(&doc, &path).unwrap();
            let reparsed = parse(&path).unwrap();
            assert_eq!(reparsed, doc);

            // Non-ASCII strings must be stored as UTF-16LE on disk (negative
            // length prefix, 2 bytes/char), not Latin-1 (1 byte/char) — the
            // game's locres reader only accepts single-byte strings when
            // they're pure ASCII.
            let mut buf = Vec::new();
            write_fstring(&mut buf, "CRÉDITOS").unwrap();
            let len_prefix = i32::from_le_bytes(buf[0..4].try_into().unwrap());
            assert!(
                len_prefix < 0,
                "expected negative (UTF-16) length prefix, got {len_prefix}"
            );
            // "CRÉDITOS\0" = 9 UTF-16 units, 2 bytes each, after the 4-byte prefix.
            assert_eq!(buf.len(), 4 + 9 * 2);
            let e_bytes = &buf[4 + 2 * 2..4 + 3 * 2];
            assert_eq!(e_bytes, &[0xC9, 0x00]);
        }

        #[test]
        fn rejects_bad_magic() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("bad.locres");
            fs::write(&path, b"not a locres file at all!!").unwrap();
            assert!(parse(&path).is_err());
        }

        #[test]
        fn rejects_unsupported_version() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("v1.locres");
            let mut buf = LOCRES_MAGIC.to_vec();
            buf.push(1); // version 1, unsupported
            fs::write(&path, buf).unwrap();
            assert!(parse(&path).is_err());
        }

        /// A version-2 header with `string_array_offset` and
        /// `namespace_count` as given, followed by `rest`.
        fn header(string_array_offset: i64, namespace_count: u32, rest: &[u8]) -> Vec<u8> {
            let mut buf = LOCRES_MAGIC.to_vec();
            buf.push(2);
            buf.extend_from_slice(&string_array_offset.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&namespace_count.to_le_bytes());
            buf.extend_from_slice(rest);
            buf
        }

        #[test]
        fn hostile_counts_and_lengths_error_instead_of_allocating() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("hostile.locres");
            for length in [i32::MAX, i32::MIN, -1_000_000_000] {
                let mut rest = 0u32.to_le_bytes().to_vec(); // namespace_hash
                rest.extend_from_slice(&length.to_le_bytes());
                fs::write(&path, header(0, u32::MAX, &rest)).unwrap();
                let err = parse(&path).unwrap_err();
                assert!(
                    format!("{err:#}").contains("past end of file"),
                    "{length}: {err:#}"
                );
            }
        }

        #[test]
        fn rejects_a_string_array_offset_outside_the_file() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("offset.locres");
            for offset in [-1, 1 << 40] {
                fs::write(&path, header(offset, 0, &[])).unwrap();
                let err = parse(&path).unwrap_err();
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

            fs::write(&path, shared_text_locres(4, 100)).unwrap();
            assert_eq!(parse(&path).unwrap().namespaces[0].entries.len(), 4);

            fs::write(&path, shared_text_locres(2000, 4096)).unwrap();
            let err = parse(&path).unwrap_err();
            assert!(
                format!("{err:#}").contains("the file's size in text"),
                "{err:#}"
            );
        }

        #[test]
        fn rejects_an_entry_pointing_outside_the_string_array() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("index.locres");
            let mut bytes = shared_text_locres(1, 3);
            // The one entry's string index sits just before the string array.
            let offset_bytes: [u8; 8] = bytes[17..25].try_into().unwrap();
            let string_array_offset = i64::from_le_bytes(offset_bytes) as usize;
            bytes[string_array_offset - 4..string_array_offset]
                .copy_from_slice(&5i32.to_le_bytes());
            fs::write(&path, bytes).unwrap();

            let err = parse(&path).unwrap_err();

            assert!(
                format!("{err:#}").contains("/k0 points at string 5, but there are only 1"),
                "{err:#}"
            );
        }

        #[test]
        fn lengths_that_do_not_fit_are_errors_not_wrapped_values() {
            assert_eq!(stored_len::<u8>(255, "x").unwrap(), 255);
            let err = stored_len::<u8>(256, "string length").unwrap_err();
            assert!(err.to_string().contains("string length (256)"), "{err}");
        }

        #[test]
        fn write_rejects_unsupported_version() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("v3.locres");
            let doc = LocresDocument {
                version: 3,
                namespaces: vec![],
            };
            assert!(write(&doc, &path).is_err());
        }
    }

    mod dump_pack {
        use super::*;

        #[test]
        fn dump_without_reference_mirrors_input() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("test.locres");
            let doc = sample_doc();
            write(&doc, &path).unwrap();

            let dumped = dump(&path, None).unwrap();
            assert_eq!(dumped.namespaces.len(), 2);
            let ns = &dumped.namespaces[0];
            assert_eq!(ns.entries[0].input_text.as_deref(), Some("Aim - Hold"));
            assert_eq!(ns.entries[0].text, "Aim - Hold");
        }

        #[test]
        fn dump_with_reference_fills_missing_keys_from_reference() {
            let tmp = tempfile::tempdir().unwrap();
            let en_path = tmp.path().join("en.locres");
            let pt_path = tmp.path().join("pt.locres");

            let en_doc = LocresDocument {
                version: 2,
                namespaces: vec![LocresNamespace {
                    namespace_hash: 1,
                    namespace: "STActions".to_string(),
                    entries: vec![
                        LocresEntry {
                            key_hash: 1,
                            key: "ActionAim".to_string(),
                            source_hash: 10,
                            text: "Aim - Hold".to_string(),
                        },
                        LocresEntry {
                            key_hash: 2,
                            key: "ActionFire".to_string(),
                            source_hash: 20,
                            text: "Fire".to_string(),
                        },
                    ],
                }],
            };
            let pt_doc = LocresDocument {
                version: 2,
                namespaces: vec![LocresNamespace {
                    // Different hash on purpose: reference's hash must win.
                    namespace_hash: 999,
                    namespace: "STActions".to_string(),
                    entries: vec![LocresEntry {
                        key_hash: 999,
                        key: "ActionAim".to_string(),
                        source_hash: 999,
                        text: "Mirar - Segurar".to_string(),
                    }],
                }],
            };
            write(&en_doc, &en_path).unwrap();
            write(&pt_doc, &pt_path).unwrap();

            let dumped = dump(&pt_path, Some(&en_path)).unwrap();
            let ns = &dumped.namespaces[0];
            assert_eq!(ns.namespace_hash, 1); // from reference (en), not pt's 999

            let aim = ns.entries.iter().find(|e| e.key == "ActionAim").unwrap();
            assert_eq!(aim.input_text.as_deref(), Some("Mirar - Segurar"));
            assert_eq!(aim.text, "Mirar - Segurar");
            assert_eq!(aim.source_hash, 10); // from reference, not pt's 999

            let fire = ns.entries.iter().find(|e| e.key == "ActionFire").unwrap();
            assert_eq!(fire.input_text, None); // pt doesn't have this key
            assert_eq!(fire.text, "Fire"); // seeded from reference's English text
        }

        #[test]
        fn pack_ignores_input_text_and_uses_text() {
            let doc = DumpDocument {
                version: 2,
                namespaces: vec![DumpNamespace {
                    namespace: "STActions".to_string(),
                    namespace_hash: 1,
                    entries: vec![DumpEntry {
                        key: "ActionAim".to_string(),
                        key_hash: 1,
                        source_hash: 1,
                        input_text: Some("stale english text".to_string()),
                        text: "Mirar - Segurar".to_string(),
                    }],
                }],
            };

            let packed = pack(&doc).unwrap();
            assert_eq!(packed.namespaces[0].entries[0].text, "Mirar - Segurar");
        }

        #[test]
        fn pack_rejects_empty_text() {
            let doc = DumpDocument {
                version: 2,
                namespaces: vec![DumpNamespace {
                    namespace: "STActions".to_string(),
                    namespace_hash: 1,
                    entries: vec![DumpEntry {
                        key: "ActionAim".to_string(),
                        key_hash: 1,
                        source_hash: 1,
                        input_text: None,
                        text: String::new(),
                    }],
                }],
            };

            assert!(pack(&doc).is_err());
        }

        #[test]
        fn dump_then_pack_then_write_round_trips() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("test.locres");
            let out_path = tmp.path().join("out.locres");
            let doc = sample_doc();
            write(&doc, &path).unwrap();

            let dumped = dump(&path, None).unwrap();
            let packed = pack(&dumped).unwrap();
            write(&packed, &out_path).unwrap();

            let reparsed = parse(&out_path).unwrap();
            assert_eq!(reparsed, doc);
        }
    }
}
