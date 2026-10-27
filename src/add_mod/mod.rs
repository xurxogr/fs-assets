//! `add-mod` orchestration shared by `crate::cli` and the Python bindings:
//! reads an existing catalog (see `build-catalog`), resolves each asset
//! source (an already-extracted directory, or a `.pak` read directly), then
//! builds the icon templates of the catalog's items from that source and
//! merges them into the existing database under the mod's name. The catalog
//! is only read, never written: a mod adds icon options to the database, it
//! doesn't change which items exist.
//!
//! A `.pak` source is extracted into a temporary workspace that is removed
//! when the run ends, keeping only the icon textures the catalog references
//! (see [`referenced_icon_paths`]).

mod coverage;
mod extract_icons;
mod progress;
#[cfg(test)]
mod test_paks;

pub use coverage::{pak_coverage, PakCoverage};
pub use extract_icons::{extract_icons, ExtractIconsRequest, ExtractIconsStats};
pub use progress::{Progress, Stage};

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::database::pipeline::{overlay_icon_paths, DatabaseBuildStats};
use crate::{database, pak};

/// Where one side of an `add-mod` run (the mod itself, or the vanilla
/// fallback) reads its assets from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetSource {
    /// A directory a pak was already extracted into.
    ExtractDir(PathBuf),
    /// One or more `.pak` archives, extracted on the fly, in order, into a
    /// temporary workspace: a file several of them contain is taken from the
    /// last one.
    Pak(Vec<PathBuf>),
}

impl AssetSource {
    /// The directory this source's `icons` are read from: an extraction
    /// directory as is, or `dir` with the `icons` its paks contain extracted
    /// into it, reporting progress to `progress`. When `required`, paks that
    /// contain none of `icons` are an error.
    fn icons_dir(
        &self,
        icons: &HashSet<String>,
        dir: PathBuf,
        required: bool,
        progress: &dyn Fn(usize, usize),
    ) -> anyhow::Result<PathBuf> {
        match self {
            AssetSource::ExtractDir(source_dir) => Ok(source_dir.clone()),
            AssetSource::Pak(paks) => {
                let extracted =
                    extract_paks(paks, |path| is_referenced_icon(path, icons), &dir, progress)?;
                if required && extracted == 0 {
                    anyhow::bail!("{} has none of the catalog's icons", describe_paks(paks));
                }
                Ok(dir)
            }
        }
    }
}

/// Everything one `add-mod` run needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddModRequest {
    /// The mod's (or the base game's) assets.
    pub source: AssetSource,
    /// The base game's assets, used as a fallback for the subtype and crate
    /// overlays `source` doesn't provide itself; never for main icons.
    pub vanilla: Option<AssetSource>,
    /// Existing catalog JSON listing the items whose icons are imported.
    /// Only read.
    pub catalog: PathBuf,
    /// Icon-template HDF5 database to create or merge into.
    pub db: PathBuf,
    /// Name recorded against every template this run adds.
    pub mod_name: String,
    /// Resolutions to (re)build; empty means every supported resolution.
    /// The mod's templates at the other resolutions are kept.
    pub resolutions: Vec<i32>,
}

/// Every icon object path `catalog` references (main icons and subtype
/// overlays), plus the shared crate icon — the set of textures the database
/// build may try to decode.
pub fn referenced_icon_paths(catalog: &[Value]) -> HashSet<String> {
    let mut paths = overlay_icon_paths(catalog);
    paths.extend(
        catalog
            .iter()
            .filter_map(database::derive_catalog_icon_item)
            .map(|item| item.icon_path)
            .filter(|path| !path.is_empty()),
    );
    paths
}

/// Whether `pak_path` (as a pak index records it) is one of the files
/// backing an object path in `icons`, whatever its extension.
pub fn is_referenced_icon(pak_path: &str, icons: &HashSet<String>) -> bool {
    icons.contains(icon_object_path(pak_path))
}

/// The object path a pak file backs: its path without the leading slash and
/// without any extension (`.uasset`, `.uexp`, `.ubulk`).
fn icon_object_path(pak_path: &str) -> &str {
    let path = pak_path.trim_start_matches('/');
    let file_start = path.rfind('/').map_or(0, |slash| slash + 1);
    match path[file_start..].find('.') {
        Some(dot) => &path[..file_start + dot],
        None => path,
    }
}

/// Extracts the files `keep` selects from every pak in `paks`, in order, into
/// `output_dir`, so a file several paks contain ends up with the last one's
/// copy. Reports `(done, total)` across all the paks to `progress` and returns
/// how many files were written.
pub fn extract_paks<F>(
    paks: &[PathBuf],
    keep: F,
    output_dir: &Path,
    progress: &dyn Fn(usize, usize),
) -> anyhow::Result<usize>
where
    F: Fn(&str) -> bool,
{
    let counts = paks
        .iter()
        .map(|pak_path| {
            Ok(pak::list(pak_path)?
                .iter()
                .filter(|path| keep(path))
                .count())
        })
        .collect::<anyhow::Result<Vec<usize>>>()?;
    let total: usize = counts.iter().sum();

    let mut offset = 0;
    for (pak_path, count) in paks.iter().zip(&counts) {
        pak::extract_matching_with_progress(pak_path, &keep, output_dir, &|done, _| {
            progress(offset + done, total)
        })?;
        offset += count;
    }
    Ok(total)
}

/// The paks of a source, listed for an error message.
fn describe_paks(paks: &[PathBuf]) -> String {
    paks.iter()
        .map(|pak_path| pak_path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reads the catalog `add-mod` imports icons for, failing when it doesn't
/// exist yet or lists no items.
fn load_catalog(path: &Path) -> anyhow::Result<Vec<Value>> {
    if !path.exists() {
        anyhow::bail!(
            "catalog not found: {} (generate it first with build-catalog)",
            path.display()
        );
    }
    let entries: Vec<Value> = serde_json::from_slice(&std::fs::read(path)?)?;
    if entries.is_empty() {
        anyhow::bail!("catalog {} has no entries", path.display());
    }
    Ok(entries)
}

/// Runs one full `add-mod`: builds the templates of every item in
/// `request.catalog` from the mod's icons and merges them into
/// `request.db`, reporting each stage's progress to `progress` along the
/// way. `request.catalog` is never written.
pub fn add_mod(
    request: &AddModRequest,
    progress: &dyn Fn(Progress),
) -> anyhow::Result<DatabaseBuildStats> {
    let resolutions = database::resolve_resolutions(&request.resolutions)?;
    let catalog = load_catalog(&request.catalog)?;
    let icons = referenced_icon_paths(&catalog);
    let workspace = tempfile::tempdir()?;
    let report =
        |stage: Stage| move |done: usize, total: usize| progress(Progress { stage, done, total });

    let mod_dir = request.source.icons_dir(
        &icons,
        workspace.path().join("mod"),
        true,
        &report(Stage::ExtractingIcons),
    )?;
    let vanilla_dir = request
        .vanilla
        .as_ref()
        .map(|vanilla| {
            vanilla.icons_dir(
                &overlay_icon_paths(&catalog),
                workspace.path().join("vanilla"),
                false,
                &report(Stage::ExtractingVanillaIcons),
            )
        })
        .transpose()?;

    database::pipeline::add_mod_to_database_with_progress(
        &mod_dir,
        &catalog,
        &request.mod_name,
        &request.db,
        vanilla_dir.as_deref(),
        &resolutions,
        &report(Stage::BuildingTemplates),
    )
}

#[cfg(test)]
mod tests {
    use super::test_paks::{write_pak, write_pak_with};
    use super::*;
    use serde_json::json;

    const RIFLE_ICON: &str = "War/Content/Textures/UI/ItemIcons/T_Rifle";

    /// Writes a one-item catalog referencing [`RIFLE_ICON`] and returns its path.
    fn write_catalog(dir: &Path) -> PathBuf {
        let path = dir.join("catalog.json");
        let catalog = json!([{"CodeName": "Rifle", "Icon": RIFLE_ICON}]);
        std::fs::write(&path, serde_json::to_vec_pretty(&catalog).unwrap()).unwrap();
        path
    }

    fn request(source: AssetSource, catalog: PathBuf, db: PathBuf) -> AddModRequest {
        AddModRequest {
            source,
            vanilla: None,
            catalog,
            db,
            mod_name: "icons".to_string(),
            resolutions: Vec::new(),
        }
    }

    #[test]
    fn later_paks_override_earlier_ones_and_progress_spans_every_pak() {
        let tmp = tempfile::tempdir().unwrap();
        let rifle_file = format!("{RIFLE_ICON}.uasset");
        let pistol_file = "War/Content/Textures/UI/ItemIcons/T_Pistol.uasset";
        let first = write_pak_with(tmp.path(), "a.pak", &[&rifle_file, pistol_file], b"first");
        let second = write_pak_with(tmp.path(), "b.pak", &[&rifle_file], b"second");
        let out = tmp.path().join("out");

        let reports = std::cell::RefCell::new(Vec::new());
        let extracted = extract_paks(&[first, second], |_| true, &out, &|done, total| {
            reports.borrow_mut().push((done, total))
        })
        .unwrap();

        assert_eq!(extracted, 3);
        assert_eq!(std::fs::read(out.join(&rifle_file)).unwrap(), b"second");
        assert_eq!(std::fs::read(out.join(pistol_file)).unwrap(), b"first");
        let reports = reports.into_inner();
        assert_eq!(reports.first(), Some(&(0, 3)));
        assert_eq!(reports.last(), Some(&(3, 3)));
        assert!(reports.iter().all(|&(_, total)| total == 3));
    }

    #[test]
    fn several_mod_paks_contribute_their_icons_together() {
        let tmp = tempfile::tempdir().unwrap();
        let pistol_icon = "War/Content/Textures/UI/ItemIcons/T_Pistol";
        let catalog = tmp.path().join("catalog.json");
        let entries = json!([
            {"CodeName": "Rifle", "Icon": RIFLE_ICON},
            {"CodeName": "Pistol", "Icon": pistol_icon}
        ]);
        std::fs::write(&catalog, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();
        let rifle_pak = write_pak(tmp.path(), "rifle.pak", &[&format!("{RIFLE_ICON}.uasset")]);
        let pistol_pak = write_pak(
            tmp.path(),
            "pistol.pak",
            &[&format!("{pistol_icon}.uasset")],
        );
        let request = request(
            AssetSource::Pak(vec![rifle_pak, pistol_pak]),
            catalog,
            tmp.path().join("database.h5"),
        );

        let stats = add_mod(&request, &|_| {}).unwrap();

        // Both (placeholder) icons are found, one from each pak.
        assert_eq!(stats.icons_missing, 0);
        assert_eq!(stats.decode_errors, 2);
    }

    #[test]
    fn referenced_icons_include_subicons_and_the_crate_icon() {
        let catalog = vec![
            json!({"CodeName": "Rifle", "Icon": "War/Content/Textures/UI/ItemIcons/T_Rifle.0"}),
            json!({
                "CodeName": "RifleAmmo",
                "Icon": "War/Content/Textures/UI/ItemIcons/T_Ammo",
                "SubTypeIcon": "War/Content/Textures/UI/ItemIcons/Subtype_Ammo"
            }),
            json!({"Icon": "War/Content/Textures/UI/ItemIcons/T_NoCode"}),
            json!({"CodeName": "NoIcon"}),
        ];

        let icons = referenced_icon_paths(&catalog);

        let expected: HashSet<String> = [
            "War/Content/Textures/UI/ItemIcons/T_Rifle",
            "War/Content/Textures/UI/ItemIcons/T_Ammo",
            "War/Content/Textures/UI/ItemIcons/Subtype_Ammo",
            database::CRATE_ICON_OBJECT_PATH,
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(icons, expected);
    }

    #[test]
    fn referenced_icon_matches_any_extension_of_the_object_path() {
        let icons: HashSet<String> = [RIFLE_ICON.to_string()].into_iter().collect();

        assert!(is_referenced_icon(
            "War/Content/Textures/UI/ItemIcons/T_Rifle.uasset",
            &icons
        ));
        assert!(is_referenced_icon(
            "/War/Content/Textures/UI/ItemIcons/T_Rifle.uexp",
            &icons
        ));
        assert!(!is_referenced_icon(
            "War/Content/Textures/UI/ItemIcons/T_Rifle2.uasset",
            &icons
        ));
        assert!(!is_referenced_icon(
            "War/Content/Textures/UI/Other/T_Rifle.uasset",
            &icons
        ));
    }

    #[test]
    fn missing_catalog_errors_before_touching_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = write_pak(tmp.path(), "mod.pak", &[&format!("{RIFLE_ICON}.uasset")]);
        let request = request(
            AssetSource::Pak(vec![pak_path]),
            tmp.path().join("missing.json"),
            tmp.path().join("database.h5"),
        );

        let err = add_mod(&request, &|_| {}).unwrap_err();

        assert!(err.to_string().contains("build-catalog"));
        assert!(!request.db.exists());
    }

    #[test]
    fn empty_catalog_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog.json");
        std::fs::write(&catalog, b"[]").unwrap();
        let request = request(
            AssetSource::ExtractDir(tmp.path().to_path_buf()),
            catalog,
            tmp.path().join("database.h5"),
        );

        let err = add_mod(&request, &|_| {}).unwrap_err();

        assert!(err.to_string().contains("no entries"));
        assert!(!request.db.exists());
    }

    #[test]
    fn missing_pak_source_errors_before_writing_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let request = request(
            AssetSource::Pak(vec![tmp.path().join("missing.pak")]),
            write_catalog(tmp.path()),
            tmp.path().join("database.h5"),
        );

        assert!(add_mod(&request, &|_| {}).is_err());
        assert!(!request.db.exists());
    }

    #[test]
    fn pak_without_any_catalog_icon_is_rejected_without_touching_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = write_pak(
            tmp.path(),
            "unrelated.pak",
            &["War/Content/Textures/UI/Other/T_Unrelated.uasset"],
        );
        let request = request(
            AssetSource::Pak(vec![pak_path]),
            write_catalog(tmp.path()),
            tmp.path().join("database.h5"),
        );

        let err = add_mod(&request, &|_| {}).unwrap_err();

        assert!(err.to_string().contains("none of the catalog's icons"));
        assert!(!request.db.exists());
    }

    #[test]
    fn vanilla_never_supplies_a_main_icon_the_mod_lacks() {
        let tmp = tempfile::tempdir().unwrap();
        let pistol_icon = "War/Content/Textures/UI/ItemIcons/T_Pistol";
        let catalog = tmp.path().join("catalog.json");
        let entries = json!([
            {"CodeName": "Rifle", "Icon": RIFLE_ICON},
            {"CodeName": "Pistol", "Icon": pistol_icon}
        ]);
        std::fs::write(&catalog, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();
        let rifle_file = format!("{RIFLE_ICON}.uasset");
        let pistol_file = format!("{pistol_icon}.uasset");
        let mod_pak = write_pak(tmp.path(), "mod.pak", &[&rifle_file]);
        let vanilla_pak = write_pak(tmp.path(), "vanilla.pak", &[&rifle_file, &pistol_file]);
        let request = AddModRequest {
            vanilla: Some(AssetSource::Pak(vec![vanilla_pak])),
            ..request(
                AssetSource::Pak(vec![mod_pak]),
                catalog,
                tmp.path().join("database.h5"),
            )
        };

        let stats = add_mod(&request, &|_| {}).unwrap();

        // The mod's own (placeholder) rifle icon is tried; the pistol icon
        // only vanilla has is not borrowed.
        assert_eq!(stats.decode_errors, 1);
        assert_eq!(stats.icons_missing, 1);
    }

    #[test]
    fn icon_only_pak_reports_every_stage_and_leaves_the_catalog_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let icon_file = format!("{RIFLE_ICON}.uasset");
        let mod_pak = write_pak(tmp.path(), "mod.pak", &[&icon_file]);
        let vanilla_pak = write_pak(tmp.path(), "vanilla.pak", &[&icon_file]);
        let catalog = write_catalog(tmp.path());
        let catalog_before = std::fs::read(&catalog).unwrap();
        let request = AddModRequest {
            vanilla: Some(AssetSource::Pak(vec![vanilla_pak])),
            ..request(
                AssetSource::Pak(vec![mod_pak]),
                catalog,
                tmp.path().join("database.h5"),
            )
        };

        let reports = std::cell::RefCell::new(Vec::new());
        let stats = add_mod(&request, &|report| reports.borrow_mut().push(report)).unwrap();
        let reports = reports.into_inner();

        let mut stages: Vec<Stage> = reports.iter().map(|r| r.stage).collect();
        stages.dedup();
        assert_eq!(
            stages,
            vec![
                Stage::ExtractingIcons,
                Stage::ExtractingVanillaIcons,
                Stage::BuildingTemplates,
            ]
        );
        for stage in stages {
            let of_stage: Vec<&Progress> = reports.iter().filter(|r| r.stage == stage).collect();
            assert_eq!(of_stage.first().unwrap().done, 0);
            let last = of_stage.last().unwrap();
            assert_eq!(last.done, last.total);
        }
        assert_eq!(stats.total_items, 1);
        assert_eq!(std::fs::read(&request.catalog).unwrap(), catalog_before);
        assert!(request.db.exists());
    }
}
