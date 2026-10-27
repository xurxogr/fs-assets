//! Filesystem-backed catalog build pipeline: the integration/wiring step
//! that ties the four pure catalog modules (`blueprint_parser`,
//! `data_table_lookup`, `damage_type_resolver`, `catalog_assembler`) to a
//! real `war/` extraction directory.
//!
//! It walks `.uasset` files directly and parses them with
//! `crate::convert::uasset_to_full_json` — see the module docs on
//! `blueprint_parser`/`data_table_lookup` for the resulting JSON shape.
//!
//! One layout detail that's easy to get wrong: the `Data` directory is
//! nested *inside* `Blueprints` (`Blueprints/Data`), not a sibling of it
//! under `Content` — consistent with
//! `data_table_lookup::table_object_path` (`"War/Content/Blueprints/Data/{table_name}"`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::catalog::blueprint_parser::{self, filesystem_loader};
use crate::catalog::catalog_assembler::{self, AssemblerContext, DataTables};
use crate::catalog::data_table_lookup::{
    self, filesystem_blueprint_loader, filesystem_data_loader,
};
use crate::catalog::localization::LocalizationLookup;
use crate::convert;

/// Subdirectories of `Blueprints/` recursively scanned for catalog
/// candidates.
const SEARCH_DIRECTORIES: &[&str] = &["ItemPickups", "Vehicles", "Structures"];

/// Filename-stem substrings that exclude a blueprint file from the catalog
/// build (substring match against the file stem).
const EXCLUDE_PATTERNS: &[&str] = &["ItemComponent", "VehicleProxy", "Ghost", "Destroyed"];

/// Processing statistics for a catalog build. Note that `parsed` is
/// incremented for every discovered file regardless of outcome, so it always
/// equals `total_files` by the end of a run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CatalogStats {
    pub total_files: usize,
    pub parsed: usize,
    pub stockpilable: usize,
    pub errors: usize,
}

/// Recursively collects every `*.uasset` file under `dir`, excluding any
/// whose file stem contains one of `EXCLUDE_PATTERNS` (substring match).
/// Used for each `SEARCH_DIRECTORIES` entry. Returns an empty list (rather
/// than erroring) if `dir` doesn't exist, but fails on a directory under it
/// that can't be read: skipping it would silently drop its items from the
/// catalog.
pub fn collect_blueprint_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // `walkdir` doesn't follow symlinks (so a link back up the tree can't
    // loop) and walks without recursing.
    let mut results: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| {
            if path.extension().and_then(|e| e.to_str()) != Some("uasset") {
                return false;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            !EXCLUDE_PATTERNS
                .iter()
                .any(|pattern| stem.contains(pattern))
        })
        .collect();
    results.sort();
    Ok(results)
}

/// `raw` with every result remembered by object path, failures included. A
/// load that panics (a malformed asset the parser chokes on) is reported and
/// remembered as missing, like one that fails to parse: otherwise the panic
/// would skip the cache, and every blueprint with that ancestor would parse
/// it, and panic, again.
fn cached_loader(raw: impl Fn(&str) -> Option<Value>) -> impl Fn(&str) -> Option<Value> {
    let cache: RefCell<HashMap<String, Option<Value>>> = RefCell::new(HashMap::new());
    move |object_path: &str| {
        if let Some(cached) = cache.borrow().get(object_path) {
            return cached.clone();
        }
        let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| raw(object_path)))
            .unwrap_or_else(|_| {
                log::warn!("build-catalog: panicked while parsing {object_path}");
                None
            });
        cache
            .borrow_mut()
            .insert(object_path.to_string(), loaded.clone());
        loaded
    }
}

/// Build the complete catalog from a real extraction directory (e.g. the
/// output of `fs-assets unpak`).
///
/// Validates that `extract_dir/War/Content/{Blueprints,Blueprints/Data,
/// Localization}` all exist and are directories, reporting a single
/// `anyhow::Error` per missing/invalid path. Missing individual data-table
/// `.uasset` files under `Data/` are tolerated (left as `None`, so lookups
/// against that table return `None`) rather than failing the whole build.
///
/// A parse error or panic while processing any single blueprint file is
/// caught and counted in [`CatalogStats::errors`] rather than aborting the
/// batch.
pub fn build_catalog_from_extract_dir(
    extract_dir: &Path,
) -> anyhow::Result<(Vec<Value>, CatalogStats)> {
    build_catalog_from_extract_dir_with_progress(extract_dir, &|_, _| Ok(()))
}

/// Pak path prefixes [`build_catalog_from_extract_dir`] reads from.
pub const CATALOG_INPUT_PREFIXES: [&str; 2] =
    ["War/Content/Blueprints/", "War/Content/Localization/"];

/// Whether `pak_path` (as a pak index records it) is a catalog input.
pub fn is_catalog_input(pak_path: &str) -> bool {
    let path = pak_path.trim_start_matches('/');
    CATALOG_INPUT_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// Builds the catalog straight from a `.pak`: only its catalog inputs
/// ([`CATALOG_INPUT_PREFIXES`]) are extracted, into a temporary directory
/// removed afterwards, then built as [`build_catalog_from_extract_dir`] does.
pub fn build_catalog_from_pak(pak: &Path) -> anyhow::Result<(Vec<Value>, CatalogStats)> {
    build_catalog_from_pak_with_progress(pak, &|_, _| Ok(()), &|_, _| Ok(()))
}

/// Same as [`build_catalog_from_pak`], reporting `(done, total)` over the
/// extracted files to `extracting` and over the blueprint files to
/// `building` (see [`build_catalog_from_extract_dir_with_progress`]).
pub fn build_catalog_from_pak_with_progress(
    pak: &Path,
    extracting: &dyn Fn(usize, usize) -> anyhow::Result<()>,
    building: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<(Vec<Value>, CatalogStats)> {
    let workspace = tempfile::tempdir()?;
    // The game's catalog inputs add up to ~1.7 GB, the largest ~55 MB.
    let mut budget = crate::pak::ExtractBudget {
        max_entry_bytes: 256 << 20,
        remaining_bytes: 8 << 30,
    };
    let extracted = crate::pak::extract_matching_within(
        pak,
        is_catalog_input,
        workspace.path(),
        extracting,
        &mut budget,
    )?;
    if extracted.is_empty() {
        anyhow::bail!(
            "{} has no catalog inputs (nothing under {})",
            pak.display(),
            CATALOG_INPUT_PREFIXES.join(" or ")
        );
    }
    build_catalog_from_extract_dir_with_progress(workspace.path(), building)
}

/// Same as [`build_catalog_from_extract_dir`], reporting `(done, total)`
/// over the blueprint files to `progress`: once before the first file and
/// once after the last. An error `progress` returns stops the build and is
/// returned.
pub fn build_catalog_from_extract_dir_with_progress(
    extract_dir: &Path,
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<(Vec<Value>, CatalogStats)> {
    if !extract_dir.exists() {
        anyhow::bail!("extraction directory not found: {}", extract_dir.display());
    }
    if !extract_dir.is_dir() {
        anyhow::bail!("not a directory: {}", extract_dir.display());
    }

    let content_dir = extract_dir.join("War").join("Content");
    let blueprints_dir = content_dir.join("Blueprints");
    // NOTE: Data lives under Blueprints/, not directly under Content/ — see
    // the module-level doc comment.
    let data_dir = blueprints_dir.join("Data");
    let localization_dir = content_dir.join("Localization");

    for (name, path) in [
        ("Blueprints", &blueprints_dir),
        ("Data", &data_dir),
        ("Localization", &localization_dir),
    ] {
        if !path.exists() {
            anyhow::bail!("{name} directory not found: {}", path.display());
        }
        if !path.is_dir() {
            anyhow::bail!("{name} is not a directory: {}", path.display());
        }
    }

    let loc_service = LocalizationLookup::new(&localization_dir, None);

    let data_loader = filesystem_data_loader(&data_dir);
    let item_dynamic_data = data_loader(data_table_lookup::ITEM_DYNAMIC_DATA);
    let weapon_dynamic_data = data_loader(data_table_lookup::WEAPON_DYNAMIC_DATA);
    let grenade_dynamic_data = data_loader(data_table_lookup::GRENADE_DYNAMIC_DATA);
    let melee_dynamic_data = data_loader(data_table_lookup::MELEE_DYNAMIC_DATA);
    let structure_dynamic_data = data_loader(data_table_lookup::STRUCTURE_DYNAMIC_DATA);
    let vehicle_dynamic_data = data_loader(data_table_lookup::VEHICLE_DYNAMIC_DATA);
    let ship_dynamic_data = data_loader(data_table_lookup::SHIP_DYNAMIC_DATA);
    let ammo_dynamic_data = data_loader(data_table_lookup::AMMO_DYNAMIC_DATA);
    let item_profile_table = data_loader(data_table_lookup::ITEM_PROFILE_TABLE);
    let structure_profile_table = data_loader(data_table_lookup::STRUCTURE_PROFILE_TABLE);
    let vehicle_profile_table = data_loader(data_table_lookup::VEHICLE_PROFILE_TABLE);
    let vehicle_movement_profile = data_loader(data_table_lookup::VEHICLE_MOVEMENT_PROFILE);

    for (name, table) in [
        (data_table_lookup::ITEM_DYNAMIC_DATA, &item_dynamic_data),
        (data_table_lookup::WEAPON_DYNAMIC_DATA, &weapon_dynamic_data),
        (
            data_table_lookup::GRENADE_DYNAMIC_DATA,
            &grenade_dynamic_data,
        ),
        (data_table_lookup::MELEE_DYNAMIC_DATA, &melee_dynamic_data),
        (
            data_table_lookup::STRUCTURE_DYNAMIC_DATA,
            &structure_dynamic_data,
        ),
        (
            data_table_lookup::VEHICLE_DYNAMIC_DATA,
            &vehicle_dynamic_data,
        ),
        (data_table_lookup::SHIP_DYNAMIC_DATA, &ship_dynamic_data),
        (data_table_lookup::AMMO_DYNAMIC_DATA, &ammo_dynamic_data),
        (data_table_lookup::ITEM_PROFILE_TABLE, &item_profile_table),
        (
            data_table_lookup::STRUCTURE_PROFILE_TABLE,
            &structure_profile_table,
        ),
        (
            data_table_lookup::VEHICLE_PROFILE_TABLE,
            &vehicle_profile_table,
        ),
        (
            data_table_lookup::VEHICLE_MOVEMENT_PROFILE,
            &vehicle_movement_profile,
        ),
    ] {
        if table.is_none() {
            log::warn!("build-catalog: data table not found, skipping: {name}");
        }
    }

    let tables = DataTables {
        item_dynamic_data: item_dynamic_data.as_ref(),
        weapon_dynamic_data: weapon_dynamic_data.as_ref(),
        grenade_dynamic_data: grenade_dynamic_data.as_ref(),
        melee_dynamic_data: melee_dynamic_data.as_ref(),
        structure_dynamic_data: structure_dynamic_data.as_ref(),
        vehicle_dynamic_data: vehicle_dynamic_data.as_ref(),
        ship_dynamic_data: ship_dynamic_data.as_ref(),
        ammo_dynamic_data: ammo_dynamic_data.as_ref(),
        item_profile_table: item_profile_table.as_ref(),
        structure_profile_table: structure_profile_table.as_ref(),
        vehicle_profile_table: vehicle_profile_table.as_ref(),
        vehicle_movement_profile: vehicle_movement_profile.as_ref(),
    };

    let blueprint_file_loader = filesystem_blueprint_loader(&blueprints_dir);
    let factory_asset = blueprint_file_loader(data_table_lookup::FACTORY_BLUEPRINT);
    let mass_production_asset = blueprint_file_loader(data_table_lookup::MASS_PRODUCTION_BLUEPRINT);
    let production_categories = data_table_lookup::load_production_categories(
        factory_asset.as_ref(),
        mass_production_asset.as_ref(),
    );

    // `blueprint_parser::AssetLoader`'s doc comment is explicit that this
    // module keeps no cross-call cache and that memoizing
    // repeated loads across a whole catalog build is the caller's
    // responsibility. Without this, every blueprint that walks its parent
    // chain re-reads and re-parses shared ancestors (e.g. a common base
    // item) from scratch via `uasset_to_full_json` once per descendant,
    // which is quadratic-ish in practice and was measured to turn a ~5
    // minute build into 56+ minutes over the real ~24k-file corpus.
    let blueprint_loader = cached_loader(filesystem_loader(&blueprints_dir));

    let ctx = AssemblerContext {
        full_extraction: true,
        blueprint_loader: &blueprint_loader,
        tables,
        production_categories: &production_categories,
        loc_service: &loc_service,
    };

    let mut files = Vec::new();
    for search_dir in SEARCH_DIRECTORIES {
        files.extend(collect_blueprint_files(&blueprints_dir.join(search_dir))?);
    }

    let mut stats = CatalogStats {
        total_files: files.len(),
        ..Default::default()
    };
    let mut catalog: Vec<Value> = Vec::new();

    progress(0, files.len())?;
    for (index, file) in files.iter().enumerate() {
        // Reported up front for the previous file, so every `continue`
        // below still counts as a finished step.
        if index > 0 {
            progress(index, files.len())?;
        }
        stats.parsed += 1;

        let asset = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            convert::uasset_to_full_json(file)
        })) {
            Ok(Ok(asset)) => asset,
            Ok(Err(e)) => {
                log::warn!("build-catalog: failed to parse {}: {e}", file.display());
                stats.errors += 1;
                continue;
            }
            Err(_) => {
                log::warn!("build-catalog: panicked while parsing {}", file.display());
                stats.errors += 1;
                continue;
            }
        };

        let object_path = blueprint_parser::compute_object_path(&blueprints_dir, file);

        let assembled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            catalog_assembler::assemble_catalog_entry(&object_path, &asset, &ctx)
        }));

        let entry = match assembled {
            Ok(entry) => entry,
            Err(_) => {
                log::warn!("build-catalog: panicked while assembling {object_path}");
                stats.errors += 1;
                continue;
            }
        };

        let Some(entry) = entry else { continue };
        if !catalog_assembler::filter_blueprint_by_content(&entry) {
            continue;
        }
        catalog.push(catalog_assembler::clean_entry(&entry));
        stats.stockpilable += 1;
    }
    progress(files.len(), files.len())?;

    catalog_assembler::sort_catalog(&mut catalog);

    Ok((catalog, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    mod cached_loader_tests {
        use super::*;
        use std::cell::Cell;

        #[test]
        fn loads_each_path_once_failures_and_panics_included() {
            let calls = Cell::new(0);
            let loader = cached_loader(|path: &str| {
                calls.set(calls.get() + 1);
                match path {
                    "/Game/Blueprints/Ok" => Some(serde_json::json!({"ok": true})),
                    "/Game/Blueprints/Panics" => panic!("malformed asset"),
                    _ => None,
                }
            });

            for _ in 0..3 {
                assert_eq!(
                    loader("/Game/Blueprints/Ok"),
                    Some(serde_json::json!({"ok": true}))
                );
                assert_eq!(loader("/Game/Blueprints/Missing"), None);
                assert_eq!(loader("/Game/Blueprints/Panics"), None);
            }
            assert_eq!(calls.get(), 3);
        }
    }

    mod collect_blueprint_files_tests {
        use super::*;
        use std::fs;

        fn touch(path: &Path) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, b"").unwrap();
        }

        #[test]
        fn finds_uasset_files_recursively() {
            let tmp = tempfile::tempdir().unwrap();
            touch(&tmp.path().join("Rifle.uasset"));
            touch(&tmp.path().join("Sub/Pistol.uasset"));
            touch(&tmp.path().join("Sub/notes.txt"));

            let mut files = collect_blueprint_files(tmp.path()).unwrap();
            files.sort();
            assert_eq!(files.len(), 2);
            assert!(files.iter().any(|p| p.ends_with("Rifle.uasset")));
            assert!(files.iter().any(|p| p.ends_with("Sub/Pistol.uasset")));
        }

        #[test]
        fn excludes_matching_stems() {
            let tmp = tempfile::tempdir().unwrap();
            touch(&tmp.path().join("BPRifleItemComponent.uasset"));
            touch(&tmp.path().join("BPVehicleProxyTank.uasset"));
            touch(&tmp.path().join("BPGhostStructure.uasset"));
            touch(&tmp.path().join("BPDestroyedWall.uasset"));
            touch(&tmp.path().join("BPRifle.uasset"));

            let files = collect_blueprint_files(tmp.path()).unwrap();
            assert_eq!(files.len(), 1);
            assert!(files[0].ends_with("BPRifle.uasset"));
        }

        #[cfg(unix)]
        #[test]
        fn does_not_follow_a_symlink_loop() {
            let tmp = tempfile::tempdir().unwrap();
            touch(&tmp.path().join("Sub/Rifle.uasset"));
            std::os::unix::fs::symlink(tmp.path(), tmp.path().join("Sub/loop")).unwrap();

            let files = collect_blueprint_files(tmp.path()).unwrap();
            assert_eq!(files, vec![tmp.path().join("Sub/Rifle.uasset")]);
        }

        #[test]
        fn missing_directory_returns_empty() {
            let tmp = tempfile::tempdir().unwrap();
            let missing = tmp.path().join("DoesNotExist");
            assert_eq!(
                collect_blueprint_files(&missing).unwrap(),
                Vec::<PathBuf>::new()
            );
        }

        #[cfg(unix)]
        #[test]
        fn fails_on_a_subdirectory_it_cannot_read() {
            use std::os::unix::fs::PermissionsExt;

            let tmp = tempfile::tempdir().unwrap();
            touch(&tmp.path().join("Rifle.uasset"));
            let locked = tmp.path().join("Locked");
            touch(&locked.join("Pistol.uasset"));
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

            let result = collect_blueprint_files(tmp.path());
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(result.is_err());
        }
    }

    mod build_catalog_from_extract_dir_tests {
        use super::*;
        use std::fs;

        fn fixture_uasset() -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/BPFlameBackpackW.uasset")
        }

        #[test]
        fn missing_extract_dir_errors() {
            let tmp = tempfile::tempdir().unwrap();
            let missing = tmp.path().join("nope");
            let err = build_catalog_from_extract_dir(&missing).unwrap_err();
            assert!(err.to_string().contains("not found"));
        }

        #[test]
        fn missing_required_subdirectory_errors() {
            let tmp = tempfile::tempdir().unwrap();
            // Only create Blueprints — Data/Localization stay missing.
            fs::create_dir_all(tmp.path().join("War/Content/Blueprints")).unwrap();
            let err = build_catalog_from_extract_dir(tmp.path()).unwrap_err();
            assert!(err.to_string().contains("Data"));
        }

        #[test]
        fn reports_start_and_end_progress_for_an_empty_corpus() {
            let tmp = tempfile::tempdir().unwrap();
            for dir in ["Blueprints/Data", "Localization"] {
                fs::create_dir_all(tmp.path().join("War/Content").join(dir)).unwrap();
            }

            let reports = std::cell::RefCell::new(Vec::new());
            build_catalog_from_extract_dir_with_progress(tmp.path(), &|done, total| {
                reports.borrow_mut().push((done, total));
                Ok(())
            })
            .unwrap();

            assert_eq!(reports.into_inner(), vec![(0, 0), (0, 0)]);
        }

        /// End-to-end plumbing test: walk + filter + missing-data-tables
        /// tolerated + output collection, using a real fixture `.uasset` so
        /// `convert::uasset_to_full_json` has something genuine to parse.
        /// `BPFlameBackpackW` isn't shaped like a stockpilable item (its
        /// item profile carries no stockpile/crate flags in this fixture
        /// context), so it's expected to be parsed but filtered out
        /// — this test proves the pipeline plumbing works, not catalog
        /// business logic (already covered by `catalog_assembler`'s own
        /// unit tests).
        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn walks_and_processes_real_fixture_without_data_tables() {
            let tmp = tempfile::tempdir().unwrap();
            let blueprints_dir = tmp.path().join("War/Content/Blueprints");
            fs::create_dir_all(blueprints_dir.join("ItemPickups")).unwrap();
            fs::create_dir_all(blueprints_dir.join("Data")).unwrap();
            fs::create_dir_all(tmp.path().join("War/Content/Localization")).unwrap();

            fs::copy(
                fixture_uasset(),
                blueprints_dir.join("ItemPickups/BPFlameBackpackW.uasset"),
            )
            .unwrap();

            let (catalog, stats) = build_catalog_from_extract_dir(tmp.path()).unwrap();
            assert_eq!(stats.total_files, 1);
            assert_eq!(stats.parsed, 1);
            assert_eq!(stats.errors, 0);
            // Not stockpilable in isolation (no data tables/required fields
            // present beyond what the raw blueprint itself carries).
            assert_eq!(stats.stockpilable, catalog.len());
        }

        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn excluded_and_unrelated_files_are_skipped() {
            let tmp = tempfile::tempdir().unwrap();
            let blueprints_dir = tmp.path().join("War/Content/Blueprints");
            fs::create_dir_all(blueprints_dir.join("ItemPickups")).unwrap();
            fs::create_dir_all(blueprints_dir.join("Data")).unwrap();
            fs::create_dir_all(tmp.path().join("War/Content/Localization")).unwrap();

            fs::copy(
                fixture_uasset(),
                blueprints_dir.join("ItemPickups/BPFlameBackpackWItemComponent.uasset"),
            )
            .unwrap();
            // Outside the scanned search directories entirely.
            fs::create_dir_all(blueprints_dir.join("Other")).unwrap();
            fs::copy(
                fixture_uasset(),
                blueprints_dir.join("Other/BPIgnored.uasset"),
            )
            .unwrap();

            let (catalog, stats) = build_catalog_from_extract_dir(tmp.path()).unwrap();
            assert_eq!(stats.total_files, 0);
            assert_eq!(catalog.len(), 0);
        }
    }

    mod build_catalog_from_pak_tests {
        use super::*;
        use std::fs;

        /// Packs `files` (pak paths, with empty contents) into a pak.
        fn write_pak(dir: &Path, files: &[&str]) -> PathBuf {
            let content = dir.join("content");
            for file in files {
                let path = content.join(file);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"").unwrap();
            }
            let pak_path = dir.join("test.pak");
            crate::pak::pack(&content, &pak_path, "../../../", repak::Version::V8B).unwrap();
            pak_path
        }

        #[test]
        fn catalog_inputs_are_blueprints_and_localization_only() {
            assert!(is_catalog_input(
                "War/Content/Blueprints/ItemPickups/Rifle.uasset"
            ));
            assert!(is_catalog_input(
                "/War/Content/Localization/Game/en/Game.locres"
            ));
            assert!(!is_catalog_input(
                "War/Content/Textures/UI/ItemIcons/T_Rifle.uasset"
            ));
            assert!(!is_catalog_input("War/Content/BlueprintsExtra/Foo.uasset"));
        }

        #[test]
        fn missing_pak_errors() {
            let tmp = tempfile::tempdir().unwrap();
            assert!(build_catalog_from_pak(&tmp.path().join("missing.pak")).is_err());
        }

        #[test]
        fn pak_without_catalog_inputs_is_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let pak_path = write_pak(
                tmp.path(),
                &["War/Content/Textures/UI/ItemIcons/T_Rifle.uasset"],
            );

            let err = build_catalog_from_pak(&pak_path).unwrap_err();

            assert!(err.to_string().contains("has no catalog inputs"));
        }

        #[test]
        fn extracts_only_the_catalog_inputs_and_reports_both_steps() {
            let tmp = tempfile::tempdir().unwrap();
            let pak_path = write_pak(
                tmp.path(),
                &[
                    "War/Content/Blueprints/Data/placeholder.txt",
                    "War/Content/Localization/placeholder.txt",
                    "War/Content/Textures/UI/ItemIcons/T_Rifle.uasset",
                ],
            );

            let extracting = RefCell::new(Vec::new());
            let building = RefCell::new(Vec::new());
            let (catalog, stats) = build_catalog_from_pak_with_progress(
                &pak_path,
                &|done, total| {
                    extracting.borrow_mut().push((done, total));
                    Ok(())
                },
                &|done, total| {
                    building.borrow_mut().push((done, total));
                    Ok(())
                },
            )
            .unwrap();

            assert_eq!(extracting.into_inner(), vec![(0, 2), (1, 2), (2, 2)]);
            assert_eq!(building.into_inner(), vec![(0, 0), (0, 0)]);
            assert!(catalog.is_empty());
            assert_eq!(stats.total_files, 0);
        }
    }
}
