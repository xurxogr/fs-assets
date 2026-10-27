//! `extract-icons`: writes the icon of every catalog item a source provides
//! as `<CodeName>.png`, by default with its subtype icon drawn over it the
//! way the templates get it, or without it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use image::DynamicImage;

use super::{load_catalog, AssetSource, Progress, Stage};
use crate::database::pipeline::{decode_icon, filesystem_icon_loader_with_fallback};
use crate::database::{self, CatalogIconItem};

/// Everything one `extract-icons` run needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractIconsRequest {
    /// Where the icons are read from.
    pub source: AssetSource,
    /// The base game's assets, used as a fallback for the subtype icons
    /// `source` doesn't provide itself; never for main icons.
    pub vanilla: Option<AssetSource>,
    /// Existing catalog JSON listing the items whose icons are extracted.
    pub catalog: PathBuf,
    /// Directory the PNGs are written into.
    pub output: PathBuf,
    /// Whether to draw each item's subtype icon over its icon.
    pub subicons: bool,
}

/// What one `extract-icons` run wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExtractIconsStats {
    /// PNGs written.
    pub written: usize,
    /// Items with an icon the source doesn't provide.
    pub icons_missing: usize,
    /// Items whose icon failed to decode.
    pub decode_errors: usize,
    /// Items written without the subtype icon they have, because it was
    /// missing or failed to decode.
    pub subicons_missing: usize,
}

/// Writes `request.catalog`'s icons found in `request.source` into
/// `request.output`, one `<CodeName>.png` per item, reporting each stage's
/// progress to `progress`. An error `progress` returns stops the run and is
/// returned.
pub fn extract_icons(
    request: &ExtractIconsRequest,
    progress: &dyn Fn(Progress) -> anyhow::Result<()>,
) -> anyhow::Result<ExtractIconsStats> {
    let report =
        |stage: Stage| move |done: usize, total: usize| progress(Progress { stage, done, total });
    let catalog = load_catalog(&request.catalog)?;
    let items: Vec<_> = catalog
        .iter()
        .filter_map(database::derive_catalog_icon_item)
        .filter(|item| !item.icon_path.is_empty())
        .collect();
    check_codes(&items)?;
    let subicon_paths: HashSet<String> = items
        .iter()
        .filter(|_| request.subicons)
        .filter_map(|item| item.subicon_path.clone())
        .filter(|path| !path.is_empty())
        .collect();
    let mut wanted: HashSet<String> = items.iter().map(|item| item.icon_path.clone()).collect();
    wanted.extend(subicon_paths.iter().cloned());

    let workspace = tempfile::tempdir()?;
    let source_dir = request.source.icons_dir(
        &wanted,
        workspace.path().join("source"),
        true,
        &report(Stage::ExtractingIcons),
    )?;
    let vanilla_dir = request
        .vanilla
        .as_ref()
        .filter(|_| !subicon_paths.is_empty())
        .map(|vanilla| {
            vanilla.icons_dir(
                &subicon_paths,
                workspace.path().join("vanilla"),
                false,
                &report(Stage::ExtractingVanillaIcons),
            )
        })
        .transpose()?;
    let resolve =
        filesystem_icon_loader_with_fallback(&source_dir, vanilla_dir.as_deref(), &subicon_paths);

    write_icons(
        &items,
        resolve,
        request.subicons,
        &request.output,
        &report(Stage::WritingIcons),
    )
}

/// Fails if a code of `items` isn't a plain file name, or if two codes differ
/// only in case (so would overwrite each other's file on Windows and macOS),
/// since codes come from the (possibly hostile) catalog and name the PNGs.
fn check_codes(items: &[CatalogIconItem]) -> anyhow::Result<()> {
    if let Some(item) = items.iter().find(|item| !is_plain_file_name(&item.code)) {
        anyhow::bail!("catalog CodeName {:?} is not a plain file name", item.code);
    }
    let mut codes: HashMap<String, &str> = HashMap::with_capacity(items.len());
    for item in items {
        if let Some(other) = codes.insert(item.code.to_lowercase(), &item.code) {
            anyhow::bail!(
                "catalog CodeNames {other:?} and {:?} would be written to the same file",
                item.code
            );
        }
    }
    Ok(())
}

/// Decodes each of `items`' icons through `resolve` and writes it into
/// `output` as `<code>.png`, with its subtype icon drawn over it when
/// `subicons` is set. `items`' codes must have passed [`check_codes`].
fn write_icons<F>(
    items: &[CatalogIconItem],
    resolve: F,
    subicons: bool,
    output: &Path,
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<ExtractIconsStats>
where
    F: Fn(&str) -> Option<PathBuf>,
{
    std::fs::create_dir_all(output)?;
    let mut stats = ExtractIconsStats::default();
    let mut subicon_cache: HashMap<String, Option<DynamicImage>> = HashMap::new();
    // Reported before each item rather than after, since a missing or
    // undecodable icon skips the rest of its iteration.
    for (done, item) in items.iter().enumerate() {
        progress(done, items.len())?;
        let Some(path) = resolve(&item.icon_path) else {
            stats.icons_missing += 1;
            continue;
        };
        let icon = match decode_icon(&path) {
            Ok(icon) => icon,
            Err(e) => {
                eprintln!("extract-icons: {e}");
                stats.decode_errors += 1;
                continue;
            }
        };

        let subicon_path = item
            .subicon_path
            .as_ref()
            .filter(|path| subicons && !path.is_empty());
        let png = match subicon_path {
            None => icon.to_rgba8(),
            Some(subicon_path) => {
                let subicon = subicon_cache
                    .entry(subicon_path.clone())
                    .or_insert_with(|| load_subicon(&resolve, subicon_path));
                match subicon {
                    Some(subicon) => database::composite_subicon(&icon, subicon),
                    None => {
                        stats.subicons_missing += 1;
                        icon.to_rgba8()
                    }
                }
            }
        };
        png.save(output.join(format!("{}.png", item.code)))?;
        stats.written += 1;
    }
    progress(items.len(), items.len())?;

    Ok(stats)
}

/// Whether `name` names a file directly inside a directory on every host:
/// not empty, `.` or `..`, free of path separators, drive or stream colons
/// and NULs, and not a name Windows reserves or rewrites (see
/// [`crate::pak::is_portable_name`]).
fn is_plain_file_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..")
        && !name.contains(['/', '\\', ':', '\0'])
        && crate::pak::is_portable_name(name)
}

/// Resolves and decodes a subtype icon, reporting (not failing on) a decode
/// error: the item is still written, just without it.
fn load_subicon(resolve: &impl Fn(&str) -> Option<PathBuf>, path: &str) -> Option<DynamicImage> {
    decode_icon(&resolve(path)?)
        .map_err(|e| eprintln!("extract-icons: {e}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::super::test_paks::write_pak;
    use super::*;
    use serde_json::json;

    const RIFLE: &str = "War/Content/Textures/UI/ItemIcons/T_Rifle";
    const AMMO: &str = "War/Content/Textures/UI/ItemIcons/T_Ammo";
    const SUBTYPE: &str = "War/Content/Textures/UI/ItemIcons/SubtypeAmmo";

    fn catalog() -> Vec<serde_json::Value> {
        vec![
            json!({"CodeName": "Rifle", "Icon": format!("{RIFLE}.0")}),
            json!({"CodeName": "Ammo", "Icon": AMMO, "SubTypeIcon": SUBTYPE}),
            json!({"CodeName": "Pistol", "Icon": "War/Content/Textures/UI/ItemIcons/T_Pistol"}),
        ]
    }

    fn write_catalog(dir: &Path) -> PathBuf {
        let path = dir.join("catalog.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&catalog()).unwrap()).unwrap();
        path
    }

    /// Writes `color` as a 32x32 PNG standing in for the icon at
    /// `object_path` and returns the resolver entry pointing at it.
    fn write_icon(dir: &Path, object_path: &str, color: [u8; 4]) -> (String, PathBuf) {
        let path = dir.join(format!("{}.png", object_path.replace('/', "_")));
        image::RgbaImage::from_pixel(32, 32, image::Rgba(color))
            .save(&path)
            .unwrap();
        (object_path.to_string(), path)
    }

    fn items() -> Vec<CatalogIconItem> {
        catalog()
            .iter()
            .filter_map(database::derive_catalog_icon_item)
            .collect()
    }

    fn resolver(files: HashMap<String, PathBuf>) -> impl Fn(&str) -> Option<PathBuf> {
        move |path: &str| files.get(path).cloned()
    }

    #[test]
    fn writes_each_icon_with_its_subicon_drawn_over_it() {
        let tmp = tempfile::tempdir().unwrap();
        let files: HashMap<_, _> = [
            write_icon(tmp.path(), RIFLE, [0, 0, 0, 255]),
            write_icon(tmp.path(), AMMO, [0, 0, 0, 255]),
            write_icon(tmp.path(), SUBTYPE, [255, 255, 255, 255]),
        ]
        .into_iter()
        .collect();
        let out = tmp.path().join("out");

        let stats = write_icons(&items(), resolver(files), true, &out, &|_, _| Ok(())).unwrap();

        assert_eq!(
            stats,
            ExtractIconsStats {
                written: 2,
                icons_missing: 1,
                decode_errors: 0,
                subicons_missing: 0,
            }
        );
        let rifle = image::open(out.join("Rifle.png")).unwrap().to_rgba8();
        let ammo = image::open(out.join("Ammo.png")).unwrap().to_rgba8();
        assert_eq!(rifle.get_pixel(0, 0), &image::Rgba([0, 0, 0, 255]));
        assert_ne!(ammo.get_pixel(0, 0), &image::Rgba([0, 0, 0, 255]));
        assert_eq!(ammo.get_pixel(31, 31), &image::Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn refuses_codes_that_are_not_plain_file_names() {
        for code in [
            "../escape",
            "a/b",
            "a\\b",
            "C:escape",
            "..",
            "",
            "NUL",
            "com1",
            "Rifle.",
            "nul\0byte",
        ] {
            let items = vec![CatalogIconItem {
                code: code.to_string(),
                ..items()[0].clone()
            }];
            assert!(check_codes(&items).is_err(), "{code:?}");
        }
    }

    #[test]
    fn refuses_codes_differing_only_in_case() {
        let items: Vec<_> = ["Rifle", "rifle"]
            .into_iter()
            .map(|code| CatalogIconItem {
                code: code.to_string(),
                ..items()[0].clone()
            })
            .collect();

        let err = check_codes(&items).unwrap_err();

        assert!(err.to_string().contains("same file"), "{err}");
    }

    /// A bad code fails the run before any pak is read, not after the
    /// extraction it would otherwise waste.
    #[test]
    fn a_bad_code_fails_before_extracting() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = write_pak(tmp.path(), "a.pak", &[&format!("{RIFLE}.uasset")]);
        let mut request = request(AssetSource::Pak(vec![pak_path]), tmp.path(), false);
        std::fs::write(
            &request.catalog,
            json!([{"CodeName": "../escape", "Icon": RIFLE}]).to_string(),
        )
        .unwrap();
        request.output = tmp.path().join("nested/out");

        let reports = std::cell::Cell::new(0);
        let err = extract_icons(&request, &|_| {
            reports.set(reports.get() + 1);
            Ok(())
        })
        .unwrap_err();

        assert!(err.to_string().contains("not a plain file name"), "{err}");
        assert_eq!(reports.get(), 0);
        assert!(!request.output.exists());
    }

    #[test]
    fn without_subicons_writes_the_bare_icons() {
        let tmp = tempfile::tempdir().unwrap();
        let files: HashMap<_, _> = [
            write_icon(tmp.path(), AMMO, [0, 0, 0, 255]),
            write_icon(tmp.path(), SUBTYPE, [255, 255, 255, 255]),
        ]
        .into_iter()
        .collect();
        let out = tmp.path().join("out");

        write_icons(&items(), resolver(files), false, &out, &|_, _| Ok(())).unwrap();

        let ammo = image::open(out.join("Ammo.png")).unwrap().to_rgba8();
        assert!(ammo.pixels().all(|px| px == &image::Rgba([0, 0, 0, 255])));
    }

    #[test]
    fn missing_subicon_still_writes_the_icon() {
        let tmp = tempfile::tempdir().unwrap();
        let files: HashMap<_, _> = [write_icon(tmp.path(), AMMO, [0, 0, 0, 255])]
            .into_iter()
            .collect();
        let out = tmp.path().join("out");

        let stats = write_icons(&items(), resolver(files), true, &out, &|_, _| Ok(())).unwrap();

        assert_eq!(stats.written, 1);
        assert_eq!(stats.subicons_missing, 1);
        assert!(out.join("Ammo.png").exists());
    }

    fn request(source: AssetSource, dir: &Path, subicons: bool) -> ExtractIconsRequest {
        ExtractIconsRequest {
            source,
            vanilla: None,
            catalog: write_catalog(dir),
            output: dir.join("out"),
            subicons,
        }
    }

    #[test]
    fn pak_without_any_catalog_icon_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = write_pak(tmp.path(), "a.pak", &["War/Content/Unrelated.uasset"]);
        let request = request(AssetSource::Pak(vec![pak_path]), tmp.path(), true);

        let err = extract_icons(&request, &|_| Ok(())).unwrap_err();

        assert!(err.to_string().contains("none of the catalog's icons"));
    }

    #[test]
    fn undecodable_icons_are_counted_and_missing_ones_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = write_pak(
            tmp.path(),
            "a.pak",
            &[&format!("{RIFLE}.uasset"), &format!("{AMMO}.uasset")],
        );
        let request = request(AssetSource::Pak(vec![pak_path]), tmp.path(), false);

        let reports = std::cell::RefCell::new(Vec::new());
        let stats = extract_icons(&request, &|update| {
            reports.borrow_mut().push(update);
            Ok(())
        })
        .unwrap();

        assert_eq!(
            stats,
            ExtractIconsStats {
                written: 0,
                icons_missing: 1,
                decode_errors: 2,
                subicons_missing: 0,
            }
        );
        let reports = reports.into_inner();
        let mut stages: Vec<Stage> = reports.iter().map(|r| r.stage).collect();
        stages.dedup();
        assert_eq!(stages, vec![Stage::ExtractingIcons, Stage::WritingIcons]);
        let last = reports.last().unwrap();
        assert_eq!((last.done, last.total), (3, 3));
    }

    #[test]
    fn a_progress_error_stops_before_writing_icons() {
        let tmp = tempfile::tempdir().unwrap();
        let pak_path = write_pak(tmp.path(), "a.pak", &[&format!("{RIFLE}.uasset")]);
        let request = request(AssetSource::Pak(vec![pak_path]), tmp.path(), false);

        let err = extract_icons(&request, &|update| match update.stage {
            Stage::WritingIcons => Err(anyhow::anyhow!("interrupted")),
            _ => Ok(()),
        })
        .unwrap_err();

        assert_eq!(err.to_string(), "interrupted");
        assert!(std::fs::read_dir(&request.output)
            .map(|dir| dir.count() == 0)
            .unwrap_or(true));
    }
}
