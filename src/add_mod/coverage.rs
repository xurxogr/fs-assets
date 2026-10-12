//! Which of a catalog's icons a set of paks provides, read from the pak
//! indexes alone (nothing is extracted).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{icon_object_path, load_catalog};
use crate::database::pipeline::overlay_icon_paths;
use crate::{database, pak};

/// How much of a catalog's icon set some paks contain.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PakCoverage {
    /// Distinct main icons of the catalog found in the paks.
    pub main_icons_found: usize,
    /// Distinct main icons the catalog references.
    pub main_icons_total: usize,
    /// Distinct overlays (subtype icons and the crate icon) found.
    pub overlays_found: usize,
    /// Distinct overlays the catalog references.
    pub overlays_total: usize,
    /// Whether the shared crate icon is among the overlays found.
    pub has_crate_icon: bool,
}

/// Reports which of `catalog_path`'s icons `paks` contain.
pub fn pak_coverage(catalog_path: &Path, paks: &[PathBuf]) -> anyhow::Result<PakCoverage> {
    let catalog = load_catalog(catalog_path)?;
    let main_icons: HashSet<String> = catalog
        .iter()
        .filter_map(database::derive_catalog_icon_item)
        .map(|item| item.icon_path)
        .filter(|path| !path.is_empty())
        .collect();
    let overlays = overlay_icon_paths(&catalog);

    let mut present = HashSet::new();
    for pak_path in paks {
        for file in pak::list(pak_path)? {
            present.insert(icon_object_path(&file).to_string());
        }
    }

    Ok(PakCoverage {
        main_icons_found: main_icons.intersection(&present).count(),
        main_icons_total: main_icons.len(),
        overlays_found: overlays.intersection(&present).count(),
        overlays_total: overlays.len(),
        has_crate_icon: present.contains(database::CRATE_ICON_OBJECT_PATH),
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_paks::write_pak;
    use super::*;
    use crate::database::CRATE_ICON_OBJECT_PATH;
    use serde_json::json;

    const RIFLE: &str = "War/Content/Textures/UI/ItemIcons/T_Rifle";
    const AMMO: &str = "War/Content/Textures/UI/ItemIcons/T_Ammo";
    const SUBTYPE: &str = "War/Content/Textures/UI/ItemIcons/SubtypeAmmo";

    fn write_catalog(dir: &Path) -> PathBuf {
        let path = dir.join("catalog.json");
        let catalog = json!([
            {"CodeName": "Rifle", "Icon": format!("{RIFLE}.0"), "bIsCratable": true},
            {"CodeName": "Ammo", "Icon": AMMO, "SubTypeIcon": SUBTYPE},
        ]);
        std::fs::write(&path, serde_json::to_vec_pretty(&catalog).unwrap()).unwrap();
        path
    }

    #[test]
    fn counts_main_icons_and_overlays_across_every_pak() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = write_catalog(tmp.path());
        let first = write_pak(tmp.path(), "a.pak", &[&format!("{RIFLE}.uasset")]);
        let second = write_pak(
            tmp.path(),
            "b.pak",
            &[
                &format!("{SUBTYPE}.uasset"),
                &format!("{CRATE_ICON_OBJECT_PATH}.uasset"),
                "War/Content/Unrelated.uasset",
            ],
        );

        let coverage = pak_coverage(&catalog, &[first, second]).unwrap();

        assert_eq!(
            coverage,
            PakCoverage {
                main_icons_found: 1,
                main_icons_total: 2,
                overlays_found: 2,
                overlays_total: 2,
                has_crate_icon: true,
            }
        );
    }

    #[test]
    fn paks_without_catalog_icons_cover_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = write_catalog(tmp.path());
        let pak_path = write_pak(tmp.path(), "a.pak", &["War/Content/Unrelated.uasset"]);

        let coverage = pak_coverage(&catalog, &[pak_path]).unwrap();

        assert_eq!(coverage.main_icons_found, 0);
        assert_eq!(coverage.overlays_found, 0);
        assert!(!coverage.has_crate_icon);
        assert_eq!(coverage.main_icons_total, 2);
    }

    #[test]
    fn missing_pak_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = write_catalog(tmp.path());

        assert!(pak_coverage(&catalog, &[tmp.path().join("missing.pak")]).is_err());
    }
}
