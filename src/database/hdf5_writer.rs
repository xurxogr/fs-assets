//! Real HDF5 I/O for the template database. Kept deliberately thin (a single
//! `write_database` entry point) so all the actual template-construction
//! logic stays in `crate::database` where it's testable without touching a
//! real HDF5 file.
//!
//! # Schema (must stay in lockstep with `fs-ocr`'s reader)
//!
//! This matches what `fs-ocr`'s `TemplateDatabase::load` reads, exactly:
//!
//! ```text
//! database.h5
//! ├── Attributes: version (2), format ("hdf5"), resolutions ([str, ...])
//! └── /{resolution}                  one group per SUPPORTED_RESOLUTIONS entry
//!     ├── Attributes: resolution (str), template_count (i64), icon_size (i64),
//!     │               version (2), format ("hdf5")
//!     └── Datasets:
//!         ├── images   (N, H, W, 3) u8, gzip-compressed (level 4)   — BGR
//!         ├── codes    (N,) variable-length UTF-8 string
//!         ├── mods     (N,) variable-length UTF-8 string
//!         ├── crated   (N,) bool
//!         ├── faction  (N,) u8   (index into ItemFaction)
//!         ├── category (N,) u8   (index into ItemCategory)
//!         └── phash    (N,) u64
//! ```
//!
//! `fs-ocr`'s loader (`TemplateDatabase::load`) only reads the per-group
//! `version`/`template_count` attributes plus the seven datasets above by
//! name — it never touches the file-level attributes — so those two
//! per-group attributes and the dataset names/dtypes are the load-bearing
//! part of this schema; the rest is written for tooling completeness.
//!
//! **Verified against fs-ocr**: see `crate::database::pipeline` module docs
//! for how this was exercised end-to-end against a real `fs-ocr` read.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use hdf5::types::VarLenUnicode;

use crate::database::{IconTemplate, ItemCategory, ItemFaction};

/// Database format version. Must match `fs-ocr`'s `DATABASE_VERSION`
/// (`fs-ocr/src/template/database.rs`).
/// Stored as `int64`.
pub const DATABASE_VERSION: i64 = 2;

/// `s` as an HDF5 variable-length string. Fails for a string holding `\0`,
/// which HDF5 strings can't store (a code or mod name from a catalog or a
/// caller could).
fn vlen(s: &str) -> anyhow::Result<VarLenUnicode> {
    VarLenUnicode::from_str(s)
        .map_err(|e| anyhow::anyhow!("{s:?} can't be stored as an HDF5 string: {e}"))
}

/// Writes a full template database (one group per resolution) to `output_path`,
/// overwriting any existing file. `resolutions` maps a resolution height
/// (e.g. `1080`) to the list of templates generated for it; an empty list
/// still produces a (metadata-only) group.
///
/// The database is written to a temporary file next to `output_path` and
/// renamed over it only once complete, so a failure partway through leaves
/// any existing database untouched.
pub fn write_database(
    resolutions: &BTreeMap<i32, Vec<IconTemplate>>,
    output_path: &Path,
) -> anyhow::Result<()> {
    let parent = match output_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent)?;

    let temp_path = tempfile::Builder::new()
        .suffix(".h5.tmp")
        .tempfile_in(parent)?
        .into_temp_path();
    let file = hdf5::File::create(&temp_path)?;
    write_file(&file, resolutions)?;
    file.close()?;
    temp_path.persist(output_path)?;
    Ok(())
}

fn write_file(
    file: &hdf5::File,
    resolutions: &BTreeMap<i32, Vec<IconTemplate>>,
) -> anyhow::Result<()> {
    file.new_attr::<i64>()
        .create("version")?
        .write_scalar(&DATABASE_VERSION)?;
    file.new_attr::<VarLenUnicode>()
        .create("format")?
        .write_scalar(&vlen("hdf5")?)?;

    let resolution_strs = resolutions
        .keys()
        .map(|r| vlen(&r.to_string()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if !resolution_strs.is_empty() {
        file.new_attr::<VarLenUnicode>()
            .shape(resolution_strs.len())
            .create("resolutions")?
            .write_raw(resolution_strs.as_slice())?;
    }

    for (resolution, templates) in resolutions {
        let group = file.create_group(&resolution.to_string())?;
        write_group(&group, *resolution, templates)?;
    }

    Ok(())
}

/// Reads back every [`IconTemplate`] from a database previously written by
/// [`write_database`], one `Vec` per [`crate::database::SUPPORTED_RESOLUTIONS`]
/// entry (a resolution absent from the file — shouldn't happen for a file
/// this writer produced, but is tolerated defensively — comes back as an
/// empty `Vec`, same as an explicit empty group). This is the inverse of
/// `write_database`, used by `add-mod` to load the existing database before
/// merging a new mod's templates into it.
pub fn read_database(path: &Path) -> anyhow::Result<BTreeMap<i32, Vec<IconTemplate>>> {
    let file = hdf5::File::open(path)?;
    let mut resolutions = BTreeMap::new();

    for &resolution in &crate::database::SUPPORTED_RESOLUTIONS {
        let Ok(group) = file.group(&resolution.to_string()) else {
            resolutions.insert(resolution, Vec::new());
            continue;
        };
        resolutions.insert(resolution, read_group(&group, resolution)?);
    }

    Ok(resolutions)
}

/// Reads the templates of one supported `resolution` (none if the database
/// has no group for it).
pub fn read_resolution(path: &Path, resolution: i32) -> anyhow::Result<Vec<IconTemplate>> {
    if !crate::database::SUPPORTED_RESOLUTIONS.contains(&resolution) {
        anyhow::bail!("unsupported resolution {resolution}");
    }
    let file = hdf5::File::open(path)?;
    match file.group(&resolution.to_string()) {
        Ok(group) => read_group(&group, resolution),
        Err(_) => Ok(Vec::new()),
    }
}

/// How many templates each mod has at each resolution of a database.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DatabaseStatistics {
    /// Supported resolutions the database has a group for, ascending.
    pub resolutions: Vec<i32>,
    /// Mod name -> resolution -> template count; a resolution where the mod
    /// has no template is absent.
    pub per_mod: BTreeMap<String, BTreeMap<i32, usize>>,
}

/// Counts the templates per mod and resolution, reading only the mod names
/// (no images).
pub fn read_statistics(path: &Path) -> anyhow::Result<DatabaseStatistics> {
    let file = hdf5::File::open(path)?;
    let mut stats = DatabaseStatistics::default();

    for &resolution in &crate::database::SUPPORTED_RESOLUTIONS {
        let Ok(group) = file.group(&resolution.to_string()) else {
            continue;
        };
        stats.resolutions.push(resolution);
        let count: i64 = group.attr("template_count")?.read_scalar()?;
        if count <= 0 {
            continue;
        }
        let mods: Vec<VarLenUnicode> = group.dataset("mods")?.read_raw()?;
        for mod_name in &mods {
            *stats
                .per_mod
                .entry(mod_name.to_string())
                .or_default()
                .entry(resolution)
                .or_default() += 1;
        }
    }
    Ok(stats)
}

fn read_group(group: &hdf5::Group, resolution: i32) -> anyhow::Result<Vec<IconTemplate>> {
    let count: i64 = group.attr("template_count")?.read_scalar()?;
    if count <= 0 {
        return Ok(Vec::new());
    }
    let n = count as usize;
    let icon_size = crate::database::icon_size_for_resolution(resolution) as usize;
    let image_len = icon_size * icon_size * 3;

    let images: Vec<u8> = group.dataset("images")?.read_raw()?;
    let codes: Vec<VarLenUnicode> = group.dataset("codes")?.read_raw()?;
    let mods: Vec<VarLenUnicode> = group.dataset("mods")?.read_raw()?;
    let crated: Vec<bool> = group.dataset("crated")?.read_raw()?;
    let faction: Vec<u8> = group.dataset("faction")?.read_raw()?;
    let category: Vec<u8> = group.dataset("category")?.read_raw()?;
    let phash: Vec<u64> = group.dataset("phash")?.read_raw()?;

    // `template_count` is only an attribute of the file: check it against
    // what the datasets really hold before indexing them with it.
    let lengths = [
        ("images", images.len() / image_len),
        ("codes", codes.len()),
        ("mods", mods.len()),
        ("crated", crated.len()),
        ("faction", faction.len()),
        ("category", category.len()),
        ("phash", phash.len()),
    ];
    if !images.len().is_multiple_of(image_len) {
        anyhow::bail!(
            "group {resolution}: images holds {} bytes, not a whole number of {image_len}-byte icons",
            images.len()
        );
    }
    if let Some((name, len)) = lengths.iter().find(|(_, len)| *len != n) {
        anyhow::bail!("group {resolution}: template_count is {n} but {name} holds {len} entries");
    }

    let mut templates = Vec::with_capacity(n);
    for i in 0..n {
        templates.push(IconTemplate {
            image: images[i * image_len..(i + 1) * image_len].to_vec(),
            code: codes[i].to_string(),
            mod_name: mods[i].to_string(),
            faction: ItemFaction::from_index(faction[i]),
            category: ItemCategory::from_index(category[i]),
            crated: crated[i],
            phash: phash[i],
        });
    }
    Ok(templates)
}

fn write_group(
    group: &hdf5::Group,
    resolution: i32,
    templates: &[IconTemplate],
) -> anyhow::Result<()> {
    let n = templates.len();

    if n == 0 {
        group
            .new_attr::<VarLenUnicode>()
            .create("resolution")?
            .write_scalar(&vlen(&resolution.to_string())?)?;
        group
            .new_attr::<i64>()
            .create("template_count")?
            .write_scalar(&0i64)?;
        group
            .new_attr::<i64>()
            .create("icon_size")?
            .write_scalar(&0i64)?;
        group
            .new_attr::<i64>()
            .create("version")?
            .write_scalar(&DATABASE_VERSION)?;
        group
            .new_attr::<VarLenUnicode>()
            .create("format")?
            .write_scalar(&vlen("hdf5")?)?;
        return Ok(());
    }

    let icon_size = crate::database::icon_size_for_resolution(resolution) as usize;

    let mut images = Vec::with_capacity(n * icon_size * icon_size * 3);
    let mut codes = Vec::with_capacity(n);
    let mut mods = Vec::with_capacity(n);
    let mut crated = Vec::with_capacity(n);
    let mut faction = Vec::with_capacity(n);
    let mut category = Vec::with_capacity(n);
    let mut phash = Vec::with_capacity(n);

    for template in templates {
        images.extend_from_slice(&template.image);
        codes.push(vlen(&template.code)?);
        mods.push(vlen(&template.mod_name)?);
        crated.push(template.crated);
        faction.push(template.faction.as_index());
        category.push(template.category.as_index());
        phash.push(template.phash);
    }

    group
        .new_dataset::<u8>()
        .deflate(4)
        .shape((n, icon_size, icon_size, 3))
        .create("images")?
        .write_raw(images.as_slice())?;
    group
        .new_dataset::<VarLenUnicode>()
        .shape(n)
        .create("codes")?
        .write_raw(codes.as_slice())?;
    group
        .new_dataset::<VarLenUnicode>()
        .shape(n)
        .create("mods")?
        .write_raw(mods.as_slice())?;
    group
        .new_dataset::<bool>()
        .shape(n)
        .create("crated")?
        .write_raw(crated.as_slice())?;
    group
        .new_dataset::<u8>()
        .shape(n)
        .create("faction")?
        .write_raw(faction.as_slice())?;
    group
        .new_dataset::<u8>()
        .shape(n)
        .create("category")?
        .write_raw(category.as_slice())?;
    group
        .new_dataset::<u64>()
        .shape(n)
        .create("phash")?
        .write_raw(phash.as_slice())?;

    group
        .new_attr::<VarLenUnicode>()
        .create("resolution")?
        .write_scalar(&vlen(&resolution.to_string())?)?;
    group
        .new_attr::<i64>()
        .create("template_count")?
        .write_scalar(&(n as i64))?;
    group
        .new_attr::<i64>()
        .create("icon_size")?
        .write_scalar(&(icon_size as i64))?;
    group
        .new_attr::<i64>()
        .create("version")?
        .write_scalar(&DATABASE_VERSION)?;
    group
        .new_attr::<VarLenUnicode>()
        .create("format")?
        .write_scalar(&vlen("hdf5")?)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{ItemCategory, ItemFaction};

    fn sample_template(code: &str, size: usize) -> IconTemplate {
        IconTemplate {
            image: vec![0u8; size * size * 3],
            code: code.to_string(),
            mod_name: "vanilla".to_string(),
            faction: ItemFaction::Neutral,
            category: ItemCategory::Item,
            crated: false,
            phash: 42,
        }
    }

    fn sample_from(code: &str, mod_name: &str, resolution: i32) -> IconTemplate {
        let size = crate::database::icon_size_for_resolution(resolution) as usize;
        IconTemplate {
            mod_name: mod_name.to_string(),
            ..sample_template(code, size)
        }
    }

    /// Writes a database with two mods spread over 1080p and 1440p.
    fn write_two_mod_database(output: &Path) {
        let mut resolutions: BTreeMap<i32, Vec<IconTemplate>> = BTreeMap::new();
        resolutions.insert(
            1080,
            vec![
                sample_from("Rifle", "vanilla", 1080),
                sample_from("Pistol", "vanilla", 1080),
                sample_from("Rifle", "clean", 1080),
            ],
        );
        resolutions.insert(1440, vec![sample_from("Rifle", "vanilla", 1440)]);
        write_database(&resolutions, output).unwrap();
    }

    #[test]
    fn a_failed_write_leaves_the_existing_database_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("db.h5");
        write_two_mod_database(&path);
        let before = read_database(&path).unwrap();

        let mut bad: BTreeMap<i32, Vec<IconTemplate>> = BTreeMap::new();
        bad.insert(1080, vec![sample_from("Rifle\0", "vanilla", 1080)]);
        let err = write_database(&bad, &path).unwrap_err();
        assert!(err.to_string().contains("HDF5 string"), "{err}");

        let after = read_database(&path).unwrap();
        assert_eq!(after.len(), before.len());
        for (resolution, templates) in &before {
            let codes = |t: &[IconTemplate]| t.iter().map(|t| t.code.clone()).collect::<Vec<_>>();
            assert_eq!(codes(&after[resolution]), codes(templates));
        }
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("db.h5")]);
    }

    #[test]
    fn rejects_a_template_count_the_datasets_do_not_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("db.h5");
        write_two_mod_database(&path);
        {
            let file = hdf5::File::open_rw(&path).unwrap();
            let group = file.group("1080").unwrap();
            group
                .attr("template_count")
                .unwrap()
                .write_scalar(&i64::MAX)
                .unwrap();
        }

        let err = read_database(&path).unwrap_err();
        assert!(err.to_string().contains("template_count"), "{err}");
    }

    #[test]
    fn reads_the_templates_of_one_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("db.h5");
        write_two_mod_database(&output);

        let templates = read_resolution(&output, 1080).unwrap();

        let codes: Vec<(&str, &str)> = templates
            .iter()
            .map(|t| (t.code.as_str(), t.mod_name.as_str()))
            .collect();
        assert_eq!(
            codes,
            vec![
                ("Rifle", "vanilla"),
                ("Pistol", "vanilla"),
                ("Rifle", "clean")
            ]
        );
        assert_eq!(templates[0].image.len(), 32 * 32 * 3);
        assert!(read_resolution(&output, 720).unwrap().is_empty());
    }

    #[test]
    fn reading_an_unsupported_resolution_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("db.h5");
        write_two_mod_database(&output);

        assert!(read_resolution(&output, 1234).is_err());
    }

    #[test]
    fn statistics_count_templates_per_mod_and_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("db.h5");
        write_two_mod_database(&output);

        let stats = read_statistics(&output).unwrap();

        assert_eq!(stats.resolutions, vec![1080, 1440]);
        assert_eq!(stats.per_mod["vanilla"][&1080], 2);
        assert_eq!(stats.per_mod["vanilla"][&1440], 1);
        assert_eq!(stats.per_mod["clean"][&1080], 1);
        assert!(!stats.per_mod["clean"].contains_key(&1440));
    }

    #[test]
    fn writes_empty_database_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("empty.h5");
        let resolutions: BTreeMap<i32, Vec<IconTemplate>> = BTreeMap::new();
        write_database(&resolutions, &output).unwrap();

        let file = hdf5::File::open(&output).unwrap();
        let version: u32 = file.attr("version").unwrap().read_scalar().unwrap();
        assert_eq!(version, DATABASE_VERSION as u32);
    }

    #[test]
    fn version_attributes_are_int64() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("db.h5");
        let mut resolutions: BTreeMap<i32, Vec<IconTemplate>> = BTreeMap::new();
        resolutions.insert(1080, vec![sample_template("Rifle", 32)]);
        write_database(&resolutions, &output).unwrap();

        let file = hdf5::File::open(&output).unwrap();
        let int64 = hdf5::types::TypeDescriptor::Integer(hdf5::types::IntSize::U8);
        let root = file.attr("version").unwrap().dtype().unwrap();
        assert_eq!(root.to_descriptor().unwrap(), int64);
        let group = file.group("1080").unwrap();
        let grouped = group.attr("version").unwrap().dtype().unwrap();
        assert_eq!(grouped.to_descriptor().unwrap(), int64);
    }

    #[test]
    fn writes_group_with_empty_template_list() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("db.h5");
        let mut resolutions: BTreeMap<i32, Vec<IconTemplate>> = BTreeMap::new();
        resolutions.insert(1080, vec![]);
        write_database(&resolutions, &output).unwrap();

        let file = hdf5::File::open(&output).unwrap();
        let group = file.group("1080").unwrap();
        let count: i64 = group.attr("template_count").unwrap().read_scalar().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn round_trips_a_single_template() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("db.h5");
        let size = crate::database::icon_size_for_resolution(1080) as usize;
        let mut resolutions: BTreeMap<i32, Vec<IconTemplate>> = BTreeMap::new();
        resolutions.insert(1080, vec![sample_template("Rifle", size)]);
        write_database(&resolutions, &output).unwrap();

        let file = hdf5::File::open(&output).unwrap();
        let group = file.group("1080").unwrap();
        let count: i64 = group.attr("template_count").unwrap().read_scalar().unwrap();
        assert_eq!(count, 1);

        let codes: Vec<VarLenUnicode> = group.dataset("codes").unwrap().read_raw().unwrap();
        assert_eq!(codes[0].to_string(), "Rifle");

        let phashes: Vec<u64> = group.dataset("phash").unwrap().read_raw().unwrap();
        assert_eq!(phashes[0], 42);

        let images_shape = group.dataset("images").unwrap().shape();
        assert_eq!(images_shape, vec![1, size, size, 3]);
    }

    mod read_database_tests {
        use super::*;

        #[test]
        fn round_trips_empty_database() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("db.h5");
            write_database(&BTreeMap::new(), &output).unwrap();

            let resolutions = read_database(&output).unwrap();
            assert_eq!(
                resolutions.len(),
                crate::database::SUPPORTED_RESOLUTIONS.len()
            );
            assert!(resolutions.values().all(|t| t.is_empty()));
        }

        #[test]
        fn round_trips_a_populated_template() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("db.h5");
            let size = crate::database::icon_size_for_resolution(1080) as usize;
            let mut resolutions: BTreeMap<i32, Vec<IconTemplate>> = BTreeMap::new();
            let mut template = sample_template("Rifle", size);
            template.mod_name = "vanilla".to_string();
            template.faction = ItemFaction::Wardens;
            template.category = ItemCategory::Vehicle;
            resolutions.insert(1080, vec![template]);
            write_database(&resolutions, &output).unwrap();

            let read_back = read_database(&output).unwrap();
            let templates = &read_back[&1080];
            assert_eq!(templates.len(), 1);
            assert_eq!(templates[0].code, "Rifle");
            assert_eq!(templates[0].mod_name, "vanilla");
            assert_eq!(templates[0].faction, ItemFaction::Wardens);
            assert_eq!(templates[0].category, ItemCategory::Vehicle);
            assert_eq!(templates[0].phash, 42);
            assert_eq!(templates[0].image.len(), size * size * 3);
        }
    }
}
