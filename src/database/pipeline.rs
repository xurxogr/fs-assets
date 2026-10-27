//! Filesystem-backed `add-mod` pipeline: resolves catalog icon paths to real
//! `.uasset` files under an extraction directory, decodes them via
//! `crate::convert::uasset_to_image` (the decode behind `uasset_to_png`,
//! already verified pixel-exact against the full real icon corpus — see that
//! module's docs), then resizes/hashes/writes the result to HDF5 via
//! `crate::database::hdf5_writer`.
//!
//! There is deliberately no separate "build a database from scratch" entry
//! point: creating the very first mod's database and merging in the Nth
//! mod's are the same operation — [`add_mod_to_database_with_progress`]
//! reads whatever already exists at the output path (an empty per-resolution
//! skeleton if nothing does yet) and merges this mod's templates into it,
//! upserting by `code`. `crate::cli`'s `add-mod` command is the only caller.
//!
//! Mirrors the split in `crate::catalog::pipeline`: the icon-path -> file
//! resolution is injected as a closure (so
//! [`build_database_from_catalog_with_progress`] is testable with an
//! in-memory/fixture map, matching `blueprint_parser`'s loader-closure
//! pattern) while the actual `.uasset` decode goes straight through
//! `crate::convert` (consistent with how `catalog::pipeline` calls
//! `convert::uasset_to_full_json` directly rather than injecting it).
//!
//! # Real end-to-end validation against `fs-ocr`
//!
//! Built a catalog + database from
//! `~/code/foxhole-stockpiles/data/War-WindowsNoEditor` and confirmed
//! `fs-ocr`'s own `TemplateDatabase::load` can open every resolution group
//! it wrote (see the crate's top-level report for exact counts and the
//! verification method used — a temporary `#[test]` added to, run in, and
//! removed from `fs-ocr` afterward, not a permanent change to that crate).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use image::DynamicImage;
use serde_json::Value;

use crate::database::{
    self, hdf5_writer, CatalogIconItem, IconTemplate, ItemCategory, ItemFaction,
};

/// Statistics for a single `add-mod` run, mirroring the shape of
/// `crate::catalog::pipeline::CatalogStats` for consistency with the
/// existing high-level command's reporting style.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseBuildStats {
    /// Total entries in the input catalog.
    pub total_items: usize,
    /// Catalog entries with no `CodeName` (skipped entirely).
    pub missing_code: usize,
    /// Entries with an icon path that doesn't resolve to a file under the
    /// extraction directory.
    pub icons_missing: usize,
    /// Entries whose icon `.uasset` failed to decode (parse error or panic,
    /// caught the same way `catalog::pipeline` catches per-file failures).
    pub decode_errors: usize,
    /// Entries that successfully produced templates for every supported
    /// resolution.
    pub items_embedded: usize,
}

/// The `.uasset` file backing `icon_path` under `dir`, if there is one. An
/// `icon_path` that would resolve outside `dir` (it comes from the catalog,
/// which a mod controls) resolves to nothing.
fn resolve_icon_file(dir: &Path, icon_path: &str) -> Option<PathBuf> {
    if icon_path.is_empty() {
        return None;
    }
    let candidate = crate::pak::contained_path(dir, &format!("{icon_path}.uasset")).ok()?;
    crate::pak::real_file_under(dir, candidate)
}

/// Resolves a catalog `icon_path` (e.g. `War/Content/Textures/UI/ItemIcons/T_Rifle`,
/// already normalized/version-suffix-stripped by
/// `crate::database::normalize_icon_path`) to a real file on disk under
/// `extract_dir`, mirroring how `crate::catalog::pipeline` locates blueprint
/// assets: the object path is relative to the extraction root and the
/// `.uasset` extension is appended directly (icon paths, unlike blueprint
/// paths, don't need `find_blueprint_path`'s one-level-recursive fallback
/// search since they're already fully-qualified object paths).
///
/// When one of `overlay_paths` doesn't resolve under `extract_dir` and
/// `vanilla_extract_dir` is given, it falls back to resolving it there
/// instead. A mod's own icon set often omits the subicon overlays and/or the
/// shared crate-icon texture, so vanilla's copies of those are borrowed; an
/// item's main icon never is, since vanilla's own templates already cover
/// any item the mod doesn't re-skin.
pub fn filesystem_icon_loader_with_fallback<'a>(
    extract_dir: &'a Path,
    vanilla_extract_dir: Option<&'a Path>,
    overlay_paths: &'a HashSet<String>,
) -> impl Fn(&str) -> Option<PathBuf> + 'a {
    move |icon_path: &str| {
        resolve_icon_file(extract_dir, icon_path).or_else(|| {
            let vanilla_dir = vanilla_extract_dir.filter(|_| overlay_paths.contains(icon_path))?;
            resolve_icon_file(vanilla_dir, icon_path)
        })
    }
}

/// Every main icon object path `catalog`'s items use. Unlike overlays,
/// these always come from the mod itself, never from vanilla.
pub fn main_icon_paths(catalog: &[Value]) -> HashSet<String> {
    catalog
        .iter()
        .filter_map(database::derive_catalog_icon_item)
        .map(|item| item.icon_path)
        .filter(|path| !path.is_empty())
        .collect()
}

/// Every overlay icon object path `catalog` may blend into a template: the
/// items' subtype icons plus the shared crate icon. These, unlike main
/// icons, may come from vanilla when a mod doesn't ship them.
pub fn overlay_icon_paths(catalog: &[Value]) -> HashSet<String> {
    let mut paths: HashSet<String> = catalog
        .iter()
        .filter_map(database::derive_catalog_icon_item)
        .filter_map(|item| item.subicon_path)
        .filter(|path| !path.is_empty())
        .collect();
    paths.insert(database::CRATE_ICON_OBJECT_PATH.to_string());
    paths
}

/// Decodes one icon file to an in-memory image: a `.png` is read as is, a
/// `.uasset` texture is converted first (catching a panicking decoder).
/// Any other extension is rejected.
pub fn decode_icon(path: &Path) -> anyhow::Result<image::DynamicImage> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => std::fs::File::open(path)
            .map_err(image::ImageError::IoError)
            .and_then(|file| crate::convert::decode_png(std::io::BufReader::new(file)))
            .map_err(|e| anyhow::anyhow!("failed to read icon {}: {e}", path.display())),
        Some("uasset") => decode_uasset_icon(path),
        _ => anyhow::bail!(
            "unsupported icon {}: expected a .png or .uasset file",
            path.display()
        ),
    }
}

/// Decodes a `.uasset` texture, turning a decoder panic into an error.
fn decode_uasset_icon(path: &Path) -> anyhow::Result<image::DynamicImage> {
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::convert::uasset_to_image(path)
    }));

    match decoded {
        Ok(Ok(image)) => Ok(image),
        Ok(Err(e)) => anyhow::bail!("failed to decode icon {}: {e}", path.display()),
        Err(_) => anyhow::bail!("panicked while decoding icon {}", path.display()),
    }
}

/// Resolves, decodes, and caches (by `path`) an icon that's allowed to be
/// missing or fail to decode without counting against the run's stats —
/// used for the subicon overlay and the shared crate-icon texture, which
/// (unlike an item's own main icon) don't have to exist for a template to
/// still be built, just for that particular overlay to be skipped. A miss is
/// cached too, so a broken overlay is only tried (and reported) once.
fn load_optional_image<F>(
    path: &str,
    resolve_icon: &F,
    cache: &mut HashMap<String, Option<Rc<image::DynamicImage>>>,
) -> Option<Rc<image::DynamicImage>>
where
    F: Fn(&str) -> Option<PathBuf>,
{
    if let Some(cached) = cache.get(path) {
        return cached.clone();
    }
    let decoded = resolve_icon(path).and_then(|resolved| match decode_icon(&resolved) {
        Ok(img) => Some(Rc::new(img)),
        Err(e) => {
            eprintln!("add-mod: {e}");
            None
        }
    });
    cache.insert(path.to_string(), decoded.clone());
    decoded
}

/// Builds the [`IconTemplate`]s for every catalog item with a resolvable
/// icon, using `resolve_icon` to map a catalog icon path to a real `.uasset`
/// file path. Each item gets one `crated=false` template per
/// [`database::SUPPORTED_RESOLUTIONS`] entry (with its `SubTypeIcon` overlay
/// blended in when present and resolvable), plus a second `crated=true`
/// template when the item is cratable and the shared crate-icon texture
/// ([`database::CRATE_ICON_OBJECT_PATH`]) resolves. The overlays (subicons
/// and the crate icon, a few dozen textures shared by many items) are decoded
/// once and cached for the call; each main icon is decoded for its item and
/// dropped after, since the game's items almost never share one and keeping
/// them all would hold every decoded texture until the end.
///
/// Only `target_resolutions` are built, and `(done, total)` over the catalog
/// entries is reported to `progress`: once before the first entry and once
/// after each. An error `progress` returns stops the build and is returned.
pub fn build_database_from_catalog_with_progress<F>(
    catalog: &[Value],
    resolve_icon: F,
    mod_name: &str,
    target_resolutions: &[i32],
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<(BTreeMap<i32, Vec<IconTemplate>>, DatabaseBuildStats)>
where
    F: Fn(&str) -> Option<PathBuf>,
{
    let mut resolutions: BTreeMap<i32, Vec<IconTemplate>> = target_resolutions
        .iter()
        .map(|r| (*r, Vec::new()))
        .collect();
    let mut stats = DatabaseBuildStats::default();
    let mut overlay_cache: HashMap<String, Option<Rc<image::DynamicImage>>> = HashMap::new();

    progress(0, catalog.len())?;
    for (index, entry) in catalog.iter().enumerate() {
        // Reported up front for the previous entry, so every `continue`
        // below still counts as a finished step.
        if index > 0 {
            progress(index, catalog.len())?;
        }
        stats.total_items += 1;

        let Some(item) = database::derive_catalog_icon_item(entry) else {
            stats.missing_code += 1;
            continue;
        };

        if item.icon_path.is_empty() {
            stats.icons_missing += 1;
            continue;
        }

        let Some(path) = resolve_icon(&item.icon_path) else {
            stats.icons_missing += 1;
            continue;
        };
        let image = match decode_icon(&path) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("add-mod: {e}");
                stats.decode_errors += 1;
                continue;
            }
        };

        let subicon = item
            .subicon_path
            .as_ref()
            .and_then(|path| load_optional_image(path, &resolve_icon, &mut overlay_cache));
        let crate_icon = item
            .cratable
            .then(|| {
                load_optional_image(
                    database::CRATE_ICON_OBJECT_PATH,
                    &resolve_icon,
                    &mut overlay_cache,
                )
            })
            .flatten();

        // Each resolution resizes the icon from scratch, so they're built
        // in parallel from the one prepared copy.
        let prepared =
            database::PreparedIcon::new(&image, subicon.as_deref(), crate_icon.as_deref());
        let built: Vec<Vec<IconTemplate>> = std::thread::scope(|scope| {
            let (prepared, item) = (&prepared, &item);
            let handles: Vec<_> = target_resolutions
                .iter()
                .map(|&resolution| {
                    scope.spawn(move || prepared.templates(item, mod_name, resolution))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|e| std::panic::resume_unwind(e))
                })
                .collect()
        });
        for (resolution, templates) in target_resolutions.iter().zip(built) {
            resolutions
                .get_mut(resolution)
                .expect("all resolutions preallocated")
                .extend(templates);
        }
        stats.items_embedded += 1;
    }
    progress(catalog.len(), catalog.len())?;

    Ok((resolutions, stats))
}

/// Merges `new` templates into `existing`, per resolution, upserting by
/// `(code, crated, mod_name)`: a template whose code, crated flag, *and* mod
/// all match one already present replaces it in place, and anything else is
/// appended. The database is expected to hold
/// multiple icons sharing a `(code, crated)` pair contributed by *different*
/// mods (e.g. vanilla's rifle icon alongside a re-skin mod's rifle icon, both
/// valid templates for OCR matching); only re-adding the *same* mod's own
/// entry (e.g. re-extracting after a game patch) replaces it.
pub fn merge_templates(
    mut existing: BTreeMap<i32, Vec<IconTemplate>>,
    new: BTreeMap<i32, Vec<IconTemplate>>,
) -> BTreeMap<i32, Vec<IconTemplate>> {
    for (resolution, new_templates) in new {
        let bucket = existing.entry(resolution).or_default();
        for template in new_templates {
            match bucket.iter().position(|t| {
                t.code == template.code
                    && t.crated == template.crated
                    && t.mod_name == template.mod_name
            }) {
                Some(pos) => bucket[pos] = template,
                None => bucket.push(template),
            }
        }
    }
    existing
}

/// Replaces every template `mod_name` contributed to `existing` with `new`,
/// in the resolutions `new` covers: the mod's previous templates there are
/// dropped first (so an item a game patch removed doesn't linger as a stale
/// OCR candidate), then `new` is merged in. Other mods' templates, and the
/// mod's own templates at resolutions `new` doesn't cover, are untouched.
pub fn replace_mod_templates(
    mut existing: BTreeMap<i32, Vec<IconTemplate>>,
    new: BTreeMap<i32, Vec<IconTemplate>>,
    mod_name: &str,
) -> BTreeMap<i32, Vec<IconTemplate>> {
    for (resolution, bucket) in existing.iter_mut() {
        if new.contains_key(resolution) {
            bucket.retain(|t| t.mod_name != mod_name);
        }
    }
    merge_templates(existing, new)
}

/// Full `add-mod` pipeline: builds templates for `catalog` (the entries
/// `build-catalog` produced) from the icons under `extract_dir`, tagged with
/// `mod_name`, replaces that mod's templates in whatever database already
/// exists at `output_path` with them (see [`replace_mod_templates`]), and
/// writes the result back. If `output_path`
/// doesn't exist yet, merging starts from an empty skeleton (one empty
/// group per `database::SUPPORTED_RESOLUTIONS` entry) — the same code path
/// creates a database from nothing and adds a mod to an existing one.
///
/// When `vanilla_extract_dir` is given, an overlay (subicon or the shared
/// crate icon) that doesn't resolve under `extract_dir` falls back to
/// resolving there — most third-party icon sets replace only the item icons
/// themselves and omit the subtype/crate overlays. Main icons are only ever
/// read from `extract_dir`: an item the mod doesn't ship counts as
/// `icons_missing`, and vanilla's own templates keep covering it. See
/// [`filesystem_icon_loader_with_fallback`].
///
/// Only `target_resolutions` are rebuilt (the mod's templates at every other
/// resolution are kept), and template-build progress is reported to
/// `progress` (see [`build_database_from_catalog_with_progress`]).
pub fn add_mod_to_database_with_progress(
    extract_dir: &Path,
    catalog: &[Value],
    mod_name: &str,
    output_path: &Path,
    vanilla_extract_dir: Option<&Path>,
    target_resolutions: &[i32],
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<DatabaseBuildStats> {
    if !extract_dir.exists() {
        anyhow::bail!("extraction directory not found: {}", extract_dir.display());
    }
    let _lock = hdf5_writer::lock_database(output_path)?;

    let existing = if output_path.exists() {
        hdf5_writer::read_database(output_path)?
    } else {
        database::SUPPORTED_RESOLUTIONS
            .iter()
            .map(|r| (*r, Vec::new()))
            .collect()
    };

    let overlays = overlay_icon_paths(catalog);
    let loader = filesystem_icon_loader_with_fallback(extract_dir, vanilla_extract_dir, &overlays);
    let (new_templates, stats) = build_database_from_catalog_with_progress(
        catalog,
        loader,
        mod_name,
        target_resolutions,
        progress,
    )?;
    // Replacing the mod's templates with nothing would silently wipe it: a
    // wrong `extract_dir` or a pak whose icons all fail to decode builds no
    // templates at all.
    if stats.items_embedded == 0
        && target_resolutions.iter().any(|r| {
            existing
                .get(r)
                .is_some_and(|t| t.iter().any(|t| t.mod_name == mod_name))
        })
    {
        anyhow::bail!(
            "no icon of {} catalog items could be read (missing {}, decode errors {}); \
             refusing to replace {mod_name}'s existing templates with none, {} left unchanged",
            stats.total_items,
            stats.icons_missing,
            stats.decode_errors,
            output_path.display()
        );
    }
    let merged = replace_mod_templates(existing, new_templates, mod_name);
    hdf5_writer::write_database(&merged, output_path)?;
    Ok(stats)
}

/// Identity of one icon template: code, faction, category, crated and mod
/// name — not code alone, since one mod can intentionally supply distinct
/// crated/faction variants sharing a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconIdentity {
    pub code: String,
    pub faction: ItemFaction,
    pub category: ItemCategory,
    pub crated: bool,
    pub mod_name: String,
}

impl IconIdentity {
    /// Whether `template` is this icon.
    pub fn matches(&self, template: &IconTemplate) -> bool {
        template.code == self.code
            && template.faction == self.faction
            && template.category == self.category
            && template.crated == self.crated
            && template.mod_name == self.mod_name
    }
}

impl std::fmt::Display for IconIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "icon {} (faction {:?}, category {:?}, crated {}, mod {})",
            self.code, self.faction, self.category, self.crated, self.mod_name
        )
    }
}

/// Adds (or, with `replace`, overwrites in place) one icon's template in
/// every one of `resolutions` (`--replace` on the `modify-icon` CLI
/// command). Errors if a matching template already exists at any requested
/// resolution and `replace` is `false`.
pub fn add_icon_to_templates(
    mut existing: BTreeMap<i32, Vec<IconTemplate>>,
    resolutions: &[i32],
    image: &DynamicImage,
    identity: &IconIdentity,
    replace: bool,
) -> anyhow::Result<BTreeMap<i32, Vec<IconTemplate>>> {
    let item = CatalogIconItem {
        code: identity.code.clone(),
        faction: identity.faction,
        category: identity.category,
        icon_path: String::new(),
        subicon_path: None,
        cratable: false,
    };

    for &resolution in resolutions {
        let bucket = existing.entry(resolution).or_default();
        // A non-cratable item yields exactly the plain composited template.
        let mut template = database::build_icon_templates(
            &item,
            &identity.mod_name,
            image,
            None,
            None,
            resolution,
        )
        .swap_remove(0);
        template.crated = identity.crated;

        match bucket.iter().position(|t| identity.matches(t)) {
            Some(pos) if replace => bucket[pos] = template,
            Some(_) => anyhow::bail!(
                "{identity} already exists at {resolution}p; pass --replace to overwrite"
            ),
            None => bucket.push(template),
        }
    }

    Ok(existing)
}

/// Removes one icon's template from every one of `resolutions`, matching the
/// same 5-field identity as [`add_icon_to_templates`]. Errors if no matching
/// template exists at any requested resolution (`modify-icon --remove` on
/// the CLI).
pub fn remove_icon_from_templates(
    mut existing: BTreeMap<i32, Vec<IconTemplate>>,
    resolutions: &[i32],
    identity: &IconIdentity,
) -> anyhow::Result<BTreeMap<i32, Vec<IconTemplate>>> {
    for &resolution in resolutions {
        let bucket = existing.entry(resolution).or_default();
        match bucket.iter().position(|t| identity.matches(t)) {
            Some(pos) => {
                bucket.remove(pos);
            }
            None => anyhow::bail!("{identity} not found at {resolution}p"),
        }
    }
    Ok(existing)
}

/// Full `modify-icon` pipeline: reads whatever database already exists at
/// `output_path` (an empty per-resolution skeleton if nothing does yet),
/// adds/replaces or removes one icon's template in the requested
/// resolutions, and writes the result back. `icon` is required unless
/// `remove` is set.
pub fn modify_icon_in_database(
    identity: &IconIdentity,
    icon: Option<&Path>,
    resolutions: &[i32],
    replace: bool,
    remove: bool,
    output_path: &Path,
) -> anyhow::Result<()> {
    database::check_template_name("code", &identity.code)?;
    database::check_template_name("mod name", &identity.mod_name)?;
    let _lock = hdf5_writer::lock_database(output_path)?;
    let existing = if output_path.exists() {
        hdf5_writer::read_database(output_path)?
    } else {
        database::SUPPORTED_RESOLUTIONS
            .iter()
            .map(|r| (*r, Vec::new()))
            .collect()
    };

    let updated = if remove {
        remove_icon_from_templates(existing, resolutions, identity)?
    } else {
        let icon_path =
            icon.ok_or_else(|| anyhow::anyhow!("--icon is required unless --remove is set"))?;
        let image = decode_icon(icon_path)?;
        add_icon_to_templates(existing, resolutions, &image, identity, replace)?
    };

    hdf5_writer::write_database(&updated, output_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap as StdHashMap;

    fn filesystem_icon_loader(extract_dir: &Path) -> impl Fn(&str) -> Option<PathBuf> + '_ {
        move |icon_path: &str| resolve_icon_file(extract_dir, icon_path)
    }

    /// [`build_database_from_catalog_with_progress`] at every supported
    /// resolution, without progress.
    fn build_database_from_catalog<F>(
        catalog: &[Value],
        resolve_icon: F,
        mod_name: &str,
    ) -> (BTreeMap<i32, Vec<IconTemplate>>, DatabaseBuildStats)
    where
        F: Fn(&str) -> Option<PathBuf>,
    {
        build_database_from_catalog_with_progress(
            catalog,
            resolve_icon,
            mod_name,
            &database::SUPPORTED_RESOLUTIONS,
            &|_, _| Ok(()),
        )
        .unwrap()
    }

    /// [`add_mod_to_database_with_progress`] at every supported resolution,
    /// without progress.
    fn add_mod_to_database(
        extract_dir: &Path,
        catalog: &[Value],
        mod_name: &str,
        output_path: &Path,
        vanilla_extract_dir: Option<&Path>,
    ) -> anyhow::Result<DatabaseBuildStats> {
        add_mod_to_database_with_progress(
            extract_dir,
            catalog,
            mod_name,
            output_path,
            vanilla_extract_dir,
            &database::SUPPORTED_RESOLUTIONS,
            &|_, _| Ok(()),
        )
    }

    /// A path-keyed in-memory loader standing in for real files on disk,
    /// mirroring `blueprint_parser`'s own `map_loader` test helper pattern.
    fn map_loader(files: StdHashMap<String, PathBuf>) -> impl Fn(&str) -> Option<PathBuf> {
        move |path: &str| files.get(path).cloned()
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/textures")
            .join(name)
    }

    mod build_database_from_catalog_tests {
        use super::*;

        #[test]
        fn empty_catalog_produces_empty_groups_for_every_resolution() {
            let catalog: Vec<Value> = vec![];
            let (resolutions, stats) =
                build_database_from_catalog(&catalog, map_loader(StdHashMap::new()), "vanilla");

            assert_eq!(stats.total_items, 0);
            assert_eq!(resolutions.len(), database::SUPPORTED_RESOLUTIONS.len());
            for templates in resolutions.values() {
                assert!(templates.is_empty());
            }
        }

        #[test]
        fn builds_only_the_requested_resolutions() {
            let (resolutions, _) = build_database_from_catalog_with_progress(
                &[],
                map_loader(StdHashMap::new()),
                "vanilla",
                &[1080, 1440],
                &|_, _| Ok(()),
            )
            .unwrap();
            assert_eq!(
                resolutions.keys().copied().collect::<Vec<_>>(),
                vec![1080, 1440]
            );
        }

        #[test]
        fn reports_one_step_per_catalog_entry() {
            let catalog = vec![json!({"CodeName": "A"}), json!({"Icon": "x"})];

            let reports = std::cell::RefCell::new(Vec::new());
            build_database_from_catalog_with_progress(
                &catalog,
                map_loader(StdHashMap::new()),
                "vanilla",
                &database::SUPPORTED_RESOLUTIONS,
                &|done, total| {
                    reports.borrow_mut().push((done, total));
                    Ok(())
                },
            )
            .unwrap();

            assert_eq!(reports.into_inner(), vec![(0, 2), (1, 2), (2, 2)]);
        }

        #[test]
        fn missing_code_name_is_skipped() {
            let catalog = vec![json!({"Icon": "War/Content/Icon.0"})];
            let (resolutions, stats) =
                build_database_from_catalog(&catalog, map_loader(StdHashMap::new()), "vanilla");

            assert_eq!(stats.total_items, 1);
            assert_eq!(stats.missing_code, 1);
            assert_eq!(stats.items_embedded, 0);
            assert!(resolutions.values().all(|t| t.is_empty()));
        }

        #[test]
        fn missing_icon_file_is_counted_and_skipped() {
            let catalog = vec![json!({
                "CodeName": "Ghost",
                "Icon": "War/Content/Textures/DoesNotExist.0",
            })];
            let (_, stats) =
                build_database_from_catalog(&catalog, map_loader(StdHashMap::new()), "vanilla");

            assert_eq!(stats.icons_missing, 1);
            assert_eq!(stats.items_embedded, 0);
        }

        #[test]
        fn empty_icon_path_is_counted_as_missing() {
            let catalog = vec![json!({"CodeName": "NoIcon"})];
            let (_, stats) =
                build_database_from_catalog(&catalog, map_loader(StdHashMap::new()), "vanilla");

            assert_eq!(stats.icons_missing, 1);
        }

        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn decode_error_on_non_texture_asset_is_counted() {
            // A real .uasset that isn't a Texture2D (a blueprint fixture) —
            // uasset_to_png should fail to decode it as an icon.
            let bogus_icon =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/STMounts.uasset");
            let mut files = StdHashMap::new();
            files.insert("War/Content/Icon".to_string(), bogus_icon);

            let catalog = vec![json!({"CodeName": "BadIcon", "Icon": "War/Content/Icon.0"})];
            let (_, stats) = build_database_from_catalog(&catalog, map_loader(files), "vanilla");

            assert_eq!(stats.decode_errors, 1);
            assert_eq!(stats.items_embedded, 0);
        }

        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn real_texture_fixture_produces_templates_for_every_resolution() {
            let icon = fixture("BlueprintIcon.uasset");
            let mut files = StdHashMap::new();
            files.insert("War/Content/Icon".to_string(), icon);

            let catalog = vec![json!({"CodeName": "Blueprint", "Icon": "War/Content/Icon.0"})];
            let (resolutions, stats) =
                build_database_from_catalog(&catalog, map_loader(files), "vanilla");

            assert_eq!(stats.items_embedded, 1);
            assert_eq!(stats.decode_errors, 0);
            for (resolution, templates) in &resolutions {
                assert_eq!(
                    templates.len(),
                    1,
                    "resolution {resolution} missing its template"
                );
                let expected_size = database::icon_size_for_resolution(*resolution) as usize;
                assert_eq!(templates[0].image.len(), expected_size * expected_size * 3);
                assert_eq!(templates[0].code, "Blueprint");
                assert_eq!(templates[0].mod_name, "vanilla");
                assert!(!templates[0].crated);
            }
        }

        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn duplicate_icon_path_across_items_is_decoded_once_but_both_embedded() {
            let icon = fixture("BlueprintIcon.uasset");
            let mut files = StdHashMap::new();
            files.insert("War/Content/Icon".to_string(), icon);

            let catalog = vec![
                json!({"CodeName": "ItemA", "Icon": "War/Content/Icon.0"}),
                json!({"CodeName": "ItemB", "Icon": "War/Content/Icon.0"}),
            ];
            let (resolutions, stats) =
                build_database_from_catalog(&catalog, map_loader(files), "vanilla");

            assert_eq!(stats.items_embedded, 2);
            let templates_1080 = &resolutions[&1080];
            assert_eq!(templates_1080.len(), 2);
            let codes: Vec<&str> = templates_1080.iter().map(|t| t.code.as_str()).collect();
            assert!(codes.contains(&"ItemA"));
            assert!(codes.contains(&"ItemB"));
            // Same source image -> identical phash for both.
            assert_eq!(templates_1080[0].phash, templates_1080[1].phash);
        }
    }

    mod filesystem_icon_loader_tests {
        use super::*;

        #[test]
        fn resolves_existing_uasset_file() {
            let tmp = tempfile::tempdir().unwrap();
            let icon_dir = tmp.path().join("War/Content/Textures");
            std::fs::create_dir_all(&icon_dir).unwrap();
            std::fs::write(icon_dir.join("T_Icon.uasset"), b"").unwrap();

            let loader = filesystem_icon_loader(tmp.path());
            let resolved = loader("War/Content/Textures/T_Icon");
            assert_eq!(resolved, Some(icon_dir.join("T_Icon.uasset")));
        }

        #[test]
        fn returns_none_for_missing_file() {
            let tmp = tempfile::tempdir().unwrap();
            let loader = filesystem_icon_loader(tmp.path());
            assert_eq!(loader("War/Content/Textures/Nope"), None);
        }

        #[test]
        fn returns_none_for_empty_path() {
            let tmp = tempfile::tempdir().unwrap();
            let loader = filesystem_icon_loader(tmp.path());
            assert_eq!(loader(""), None);
        }

        #[test]
        fn returns_none_for_a_path_escaping_the_extract_dir() {
            let tmp = tempfile::tempdir().unwrap();
            let extract_dir = tmp.path().join("extract");
            std::fs::create_dir_all(&extract_dir).unwrap();
            std::fs::write(tmp.path().join("Outside.uasset"), b"").unwrap();

            let loader = filesystem_icon_loader(&extract_dir);
            assert_eq!(loader("../Outside"), None);
            assert_eq!(loader("War/../../Outside"), None);
        }

        #[test]
        fn fallback_resolves_only_overlay_icons_from_vanilla() {
            let tmp = tempfile::tempdir().unwrap();
            let mod_dir = tmp.path().join("mod");
            let vanilla_dir = tmp.path().join("vanilla");
            let main = "War/Content/Textures/T_Rifle";
            let overlay = "War/Content/Textures/Subtype_Ammo";
            for icon in [main, overlay] {
                let file = vanilla_dir.join(format!("{icon}.uasset"));
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, b"").unwrap();
            }
            let overlays: HashSet<String> = [overlay.to_string()].into_iter().collect();

            let loader =
                filesystem_icon_loader_with_fallback(&mod_dir, Some(&vanilla_dir), &overlays);

            assert_eq!(loader(main), None);
            assert_eq!(
                loader(overlay),
                Some(vanilla_dir.join(format!("{overlay}.uasset")))
            );
        }

        #[test]
        fn fallback_prefers_the_mod_own_copy() {
            let tmp = tempfile::tempdir().unwrap();
            let overlay = "War/Content/Textures/Subtype_Ammo";
            let mut files = Vec::new();
            for dir in ["mod", "vanilla"] {
                let file = tmp.path().join(dir).join(format!("{overlay}.uasset"));
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, b"").unwrap();
                files.push(file);
            }
            let overlays: HashSet<String> = [overlay.to_string()].into_iter().collect();
            let mod_dir = tmp.path().join("mod");
            let vanilla_dir = tmp.path().join("vanilla");

            let loader =
                filesystem_icon_loader_with_fallback(&mod_dir, Some(&vanilla_dir), &overlays);

            assert_eq!(loader(overlay), Some(files[0].clone()));
        }

        #[test]
        fn overlay_icon_paths_lists_subicons_and_the_crate_icon_only() {
            let catalog = vec![
                json!({"CodeName": "Rifle", "Icon": "War/Content/Textures/UI/ItemIcons/T_Rifle"}),
                json!({
                    "CodeName": "RifleAmmo",
                    "Icon": "War/Content/Textures/UI/ItemIcons/T_Ammo",
                    "SubTypeIcon": "War/Content/Textures/UI/ItemIcons/Subtype_Ammo"
                }),
            ];

            let overlays = overlay_icon_paths(&catalog);

            let expected: HashSet<String> = [
                "War/Content/Textures/UI/ItemIcons/Subtype_Ammo",
                database::CRATE_ICON_OBJECT_PATH,
            ]
            .into_iter()
            .map(String::from)
            .collect();
            assert_eq!(overlays, expected);
        }
    }

    mod merge_templates_tests {
        use super::*;

        fn template(code: &str, mod_name: &str) -> IconTemplate {
            template_crated(code, mod_name, false)
        }

        fn template_crated(code: &str, mod_name: &str, crated: bool) -> IconTemplate {
            IconTemplate {
                image: vec![],
                code: code.to_string(),
                mod_name: mod_name.to_string(),
                faction: database::ItemFaction::Neutral,
                category: database::ItemCategory::Item,
                crated,
                phash: 0,
            }
        }

        /// Regression test: a cratable item contributes two templates
        /// sharing one `code` (crated=false and crated=true) within the same
        /// `new` batch. Upserting by `code` alone would make the second
        /// overwrite the first instead of both surviving the merge.
        #[test]
        fn same_code_crated_and_uncreated_variants_both_survive() {
            let mut new = BTreeMap::new();
            new.insert(
                1080,
                vec![
                    template_crated("Rifle", "vanilla", false),
                    template_crated("Rifle", "vanilla", true),
                ],
            );

            let merged = merge_templates(BTreeMap::new(), new);
            assert_eq!(merged[&1080].len(), 2);
            assert!(merged[&1080].iter().any(|t| !t.crated));
            assert!(merged[&1080].iter().any(|t| t.crated));
        }

        #[test]
        fn appends_new_codes_alongside_existing() {
            let mut existing = BTreeMap::new();
            existing.insert(1080, vec![template("Rifle", "vanilla")]);
            let mut new = BTreeMap::new();
            new.insert(1080, vec![template("ModItem", "cool_mod")]);

            let merged = merge_templates(existing, new);
            let codes: Vec<&str> = merged[&1080].iter().map(|t| t.code.as_str()).collect();
            assert_eq!(codes, vec!["Rifle", "ModItem"]);
        }

        #[test]
        fn overwrites_matching_code_crated_and_mod_in_place() {
            let mut existing = BTreeMap::new();
            existing.insert(1080, vec![template("Rifle", "vanilla")]);
            let mut new = BTreeMap::new();
            new.insert(1080, vec![template("Rifle", "vanilla")]);

            let merged = merge_templates(existing, new);
            assert_eq!(merged[&1080].len(), 1);
        }

        /// The same `(code, crated)` pair contributed by two *different* mods
        /// isn't a duplicate — e.g. vanilla's rifle icon and a re-skin mod's
        /// rifle icon are both valid OCR templates and must both survive.
        /// Regression test for a bug where merging upserted by `code` alone
        /// (or `(code, crated)` alone), silently dropping one mod's template.
        #[test]
        fn same_code_and_crated_from_different_mods_both_survive() {
            let mut existing = BTreeMap::new();
            existing.insert(1080, vec![template("Rifle", "vanilla")]);
            let mut new = BTreeMap::new();
            new.insert(1080, vec![template("Rifle", "cool_mod")]);

            let merged = merge_templates(existing, new);
            assert_eq!(merged[&1080].len(), 2);
            let mods: Vec<&str> = merged[&1080].iter().map(|t| t.mod_name.as_str()).collect();
            assert!(mods.contains(&"vanilla"));
            assert!(mods.contains(&"cool_mod"));
        }

        /// Re-importing a mod after a game patch must drop the templates of
        /// items the patch removed, while leaving every other mod alone.
        #[test]
        fn replacing_a_mod_drops_its_stale_templates_only() {
            let mut existing = BTreeMap::new();
            existing.insert(
                1080,
                vec![
                    template("Rifle", "vanilla"),
                    template("RemovedItem", "vanilla"),
                    template("Rifle", "cool_mod"),
                ],
            );
            let mut new = BTreeMap::new();
            new.insert(1080, vec![template("Rifle", "vanilla")]);

            let replaced = replace_mod_templates(existing, new, "vanilla");
            let entries: Vec<(&str, &str)> = replaced[&1080]
                .iter()
                .map(|t| (t.code.as_str(), t.mod_name.as_str()))
                .collect();
            assert_eq!(entries, vec![("Rifle", "cool_mod"), ("Rifle", "vanilla")]);
        }

        /// Rebuilding only some resolutions leaves the mod's templates at
        /// every other resolution as they were.
        #[test]
        fn replacing_a_mod_leaves_resolutions_it_did_not_rebuild() {
            let mut existing = BTreeMap::new();
            existing.insert(1080, vec![template("Old", "vanilla")]);
            existing.insert(1440, vec![template("Kept", "vanilla")]);
            let mut new = BTreeMap::new();
            new.insert(1080, vec![template("New", "vanilla")]);

            let replaced = replace_mod_templates(existing, new, "vanilla");
            assert_eq!(replaced[&1080][0].code, "New");
            assert_eq!(replaced[&1080].len(), 1);
            assert_eq!(replaced[&1440][0].code, "Kept");
        }

        #[test]
        fn merging_into_empty_existing_just_takes_new() {
            let merged = merge_templates(BTreeMap::new(), {
                let mut new = BTreeMap::new();
                new.insert(1080, vec![template("Rifle", "vanilla")]);
                new
            });
            assert_eq!(merged[&1080].len(), 1);
        }
    }

    mod add_mod_to_database_tests {
        use super::*;

        fn stored(code: &str, resolution: i32) -> IconTemplate {
            let size = database::icon_size_for_resolution(resolution) as usize;
            IconTemplate {
                image: vec![0; size * size * 3],
                code: code.to_string(),
                mod_name: "vanilla".to_string(),
                faction: database::ItemFaction::Neutral,
                category: database::ItemCategory::Item,
                crated: false,
                phash: 0,
            }
        }

        /// A run that reads no icon (wrong extraction dir, every icon failing
        /// to decode) must not wipe the mod's existing templates.
        #[test]
        fn building_nothing_leaves_the_mods_templates_untouched() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let mut existing: BTreeMap<i32, Vec<IconTemplate>> = database::SUPPORTED_RESOLUTIONS
                .iter()
                .map(|r| (*r, Vec::new()))
                .collect();
            existing.insert(1080, vec![stored("Rifle", 1080)]);
            hdf5_writer::write_database(&existing, &output).unwrap();

            let catalog = vec![json!({
                "CodeName": "Rifle",
                "Icon": "War/Content/Textures/DoesNotExist.0",
            })];
            let err = add_mod_to_database_with_progress(
                tmp.path(),
                &catalog,
                "vanilla",
                &output,
                None,
                &[1080],
                &|_, _| Ok(()),
            )
            .unwrap_err();
            assert!(err.to_string().contains("left unchanged"), "{err}");

            let written = hdf5_writer::read_database(&output).unwrap();
            assert_eq!(written[&1080].len(), 1);
        }

        /// Building nothing for a mod the database doesn't hold yet loses
        /// nothing, so it isn't an error.
        #[test]
        fn building_nothing_for_a_new_mod_is_allowed() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let mut existing: BTreeMap<i32, Vec<IconTemplate>> = database::SUPPORTED_RESOLUTIONS
                .iter()
                .map(|r| (*r, Vec::new()))
                .collect();
            existing.insert(1080, vec![stored("Rifle", 1080)]);
            hdf5_writer::write_database(&existing, &output).unwrap();

            add_mod_to_database(tmp.path(), &[], "cool_mod", &output, None).unwrap();

            let written = hdf5_writer::read_database(&output).unwrap();
            assert_eq!(written[&1080].len(), 1);
        }

        #[test]
        fn missing_extract_dir_errors() {
            let tmp = tempfile::tempdir().unwrap();
            let missing = tmp.path().join("nope");
            let output = tmp.path().join("out.h5");
            let err = add_mod_to_database(&missing, &[], "vanilla", &output, None).unwrap_err();
            assert!(err.to_string().contains("not found"));
        }

        #[test]
        fn empty_catalog_creates_a_fresh_valid_database() {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(tmp.path().join("War/Content")).unwrap();
            let output = tmp.path().join("out.h5");

            let stats = add_mod_to_database(tmp.path(), &[], "vanilla", &output, None).unwrap();
            assert_eq!(stats.total_items, 0);
            assert!(output.exists());

            let file = hdf5::File::open(&output).unwrap();
            let group = file.group("1080").unwrap();
            let count: i64 = group.attr("template_count").unwrap().read_scalar().unwrap();
            assert_eq!(count, 0);
        }

        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn real_extract_dir_and_catalog_round_trip_into_hdf5() {
            let tmp = tempfile::tempdir().unwrap();
            let icon_dir = tmp.path().join("War/Content/Textures");
            std::fs::create_dir_all(&icon_dir).unwrap();
            std::fs::copy(
                fixture("BlueprintIcon.uasset"),
                icon_dir.join("T_Icon.uasset"),
            )
            .unwrap();

            let catalog = vec![json!({
                "CodeName": "Blueprint",
                "Icon": "War/Content/Textures/T_Icon.0",
            })];
            let output = tmp.path().join("out.h5");

            let stats =
                add_mod_to_database(tmp.path(), &catalog, "vanilla", &output, None).unwrap();
            assert_eq!(stats.items_embedded, 1);

            let file = hdf5::File::open(&output).unwrap();
            let group = file.group("1080").unwrap();
            let codes: Vec<hdf5::types::VarLenUnicode> =
                group.dataset("codes").unwrap().read_raw().unwrap();
            assert_eq!(codes[0].to_string(), "Blueprint");
        }

        /// The core of the unified command: calling `add_mod_to_database`
        /// twice — once per mod — against the *same* output file merges
        /// both mods' items into one database instead of the second call
        /// wiping out the first's contribution.
        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn second_call_merges_into_first_instead_of_overwriting() {
            let tmp = tempfile::tempdir().unwrap();
            let vanilla_icons = tmp.path().join("War/Content/Textures");
            std::fs::create_dir_all(&vanilla_icons).unwrap();
            std::fs::copy(
                fixture("BlueprintIcon.uasset"),
                vanilla_icons.join("T_Rifle.uasset"),
            )
            .unwrap();
            let output = tmp.path().join("out.h5");

            let vanilla_catalog = vec![json!({
                "CodeName": "Rifle",
                "Icon": "War/Content/Textures/T_Rifle.0",
            })];
            add_mod_to_database(tmp.path(), &vanilla_catalog, "vanilla", &output, None).unwrap();

            let mod_extract_dir = tmp.path().join("mod_extract");
            let mod_icons = mod_extract_dir.join("War/Content/Textures");
            std::fs::create_dir_all(&mod_icons).unwrap();
            std::fs::copy(
                fixture("BlueprintIcon.uasset"),
                mod_icons.join("T_ModRifle.uasset"),
            )
            .unwrap();
            let mod_catalog = vec![json!({
                "CodeName": "ModRifle",
                "Icon": "War/Content/Textures/T_ModRifle.0",
            })];
            let stats =
                add_mod_to_database(&mod_extract_dir, &mod_catalog, "cool_mod", &output, None)
                    .unwrap();
            assert_eq!(stats.items_embedded, 1);

            let file = hdf5::File::open(&output).unwrap();
            let group = file.group("1080").unwrap();
            let count: i64 = group.attr("template_count").unwrap().read_scalar().unwrap();
            assert_eq!(count, 2);
            let codes: Vec<hdf5::types::VarLenUnicode> =
                group.dataset("codes").unwrap().read_raw().unwrap();
            let codes: Vec<String> = codes.into_iter().map(|c| c.to_string()).collect();
            assert!(codes.contains(&"Rifle".to_string()));
            assert!(codes.contains(&"ModRifle".to_string()));
        }

        /// Re-running the same mod (e.g. re-extracting after a game patch)
        /// overwrites its own entries rather than duplicating them.
        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn rerunning_same_mod_overwrites_rather_than_duplicates() {
            let tmp = tempfile::tempdir().unwrap();
            let icon_dir = tmp.path().join("War/Content/Textures");
            std::fs::create_dir_all(&icon_dir).unwrap();
            std::fs::copy(
                fixture("BlueprintIcon.uasset"),
                icon_dir.join("T_Icon.uasset"),
            )
            .unwrap();
            let output = tmp.path().join("out.h5");

            let catalog = vec![json!({
                "CodeName": "Rifle",
                "Icon": "War/Content/Textures/T_Icon.0",
            })];
            add_mod_to_database(tmp.path(), &catalog, "vanilla", &output, None).unwrap();
            add_mod_to_database(tmp.path(), &catalog, "vanilla", &output, None).unwrap();

            let file = hdf5::File::open(&output).unwrap();
            let group = file.group("1080").unwrap();
            let count: i64 = group.attr("template_count").unwrap().read_scalar().unwrap();
            assert_eq!(count, 1);
        }
    }

    fn identity(
        code: &str,
        faction: ItemFaction,
        category: ItemCategory,
        crated: bool,
        mod_name: &str,
    ) -> IconIdentity {
        IconIdentity {
            code: code.to_string(),
            faction,
            category,
            crated,
            mod_name: mod_name.to_string(),
        }
    }

    mod icon_identity_tests {
        use super::*;
        use image::{Rgb, RgbImage};

        fn template(crated: bool) -> IconTemplate {
            let item = CatalogIconItem {
                code: "Rifle".to_string(),
                faction: ItemFaction::Neutral,
                category: ItemCategory::Item,
                icon_path: String::new(),
                subicon_path: None,
                cratable: false,
            };
            let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, Rgb([100, 150, 200])));
            let mut template =
                database::build_icon_templates(&item, "vanilla", &img, None, None, 1080)
                    .swap_remove(0);
            template.crated = crated;
            template
        }

        #[test]
        fn matches_same_identity() {
            let id = identity(
                "Rifle",
                ItemFaction::Neutral,
                ItemCategory::Item,
                false,
                "vanilla",
            );
            assert!(id.matches(&template(false)));
        }

        #[test]
        fn does_not_match_other_crated_variant() {
            let id = identity(
                "Rifle",
                ItemFaction::Neutral,
                ItemCategory::Item,
                true,
                "vanilla",
            );
            assert!(!id.matches(&template(false)));
        }

        #[test]
        fn does_not_match_other_mod() {
            let id = identity(
                "Rifle",
                ItemFaction::Neutral,
                ItemCategory::Item,
                false,
                "airborne",
            );
            assert!(!id.matches(&template(false)));
        }

        #[test]
        fn displays_every_field() {
            let id = identity(
                "Rifle",
                ItemFaction::Neutral,
                ItemCategory::Item,
                true,
                "vanilla",
            );
            assert_eq!(
                id.to_string(),
                "icon Rifle (faction Neutral, category Item, crated true, mod vanilla)"
            );
        }
    }

    mod add_icon_to_templates_tests {
        use super::*;
        use image::{Rgb, RgbImage};

        fn image() -> DynamicImage {
            DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, Rgb([100, 150, 200])))
        }

        #[test]
        fn adds_to_an_empty_bucket() {
            let img = image();
            let existing = add_icon_to_templates(
                BTreeMap::new(),
                &[1080],
                &img,
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                false,
            )
            .unwrap();
            assert_eq!(existing[&1080].len(), 1);
            assert_eq!(existing[&1080][0].code, "Rifle");
        }

        #[test]
        fn errors_on_duplicate_without_replace() {
            let img = image();
            let existing = add_icon_to_templates(
                BTreeMap::new(),
                &[1080],
                &img,
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                false,
            )
            .unwrap();

            let err = add_icon_to_templates(
                existing,
                &[1080],
                &img,
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                false,
            )
            .unwrap_err();
            assert!(err.to_string().contains("already exists"));
        }

        #[test]
        fn replace_overwrites_the_matching_entry_in_place() {
            let existing = add_icon_to_templates(
                BTreeMap::new(),
                &[1080],
                &image(),
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                false,
            )
            .unwrap();

            let different_image =
                DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, Rgb([0, 0, 0])));
            let updated = add_icon_to_templates(
                existing,
                &[1080],
                &different_image,
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                true,
            )
            .unwrap();
            assert_eq!(updated[&1080].len(), 1);
            assert_eq!(updated[&1080][0].phash, 0);
        }

        #[test]
        fn distinct_faction_is_not_treated_as_a_duplicate() {
            let img = image();
            let existing = add_icon_to_templates(
                BTreeMap::new(),
                &[1080],
                &img,
                &identity(
                    "Rifle",
                    ItemFaction::Colonials,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                false,
            )
            .unwrap();

            let updated = add_icon_to_templates(
                existing,
                &[1080],
                &img,
                &identity(
                    "Rifle",
                    ItemFaction::Wardens,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                false,
            )
            .unwrap();
            assert_eq!(updated[&1080].len(), 2);
        }
    }

    mod remove_icon_from_templates_tests {
        use super::*;
        use image::{Rgb, RgbImage};

        #[test]
        fn removes_a_matching_entry() {
            let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, Rgb([1, 2, 3])));
            let existing = add_icon_to_templates(
                BTreeMap::new(),
                &[1080],
                &img,
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                false,
            )
            .unwrap();

            let updated = remove_icon_from_templates(
                existing,
                &[1080],
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
            )
            .unwrap();
            assert!(updated[&1080].is_empty());
        }

        #[test]
        fn errors_when_no_match_found() {
            let err = remove_icon_from_templates(
                BTreeMap::new(),
                &[1080],
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
            )
            .unwrap_err();
            assert!(err.to_string().contains("not found"));
        }
    }

    mod modify_icon_in_database_tests {
        use super::*;

        fn rifle() -> IconIdentity {
            identity(
                "Rifle",
                ItemFaction::Neutral,
                ItemCategory::Item,
                false,
                "vanilla",
            )
        }

        #[test]
        fn a_database_another_run_is_updating_is_left_alone() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let icon = tmp.path().join("rifle.png");
            image::RgbaImage::from_pixel(64, 64, image::Rgba([200, 100, 50, 255]))
                .save(&icon)
                .unwrap();

            let lock = hdf5_writer::lock_database(&output).unwrap();
            let err =
                modify_icon_in_database(&rifle(), Some(&icon), &[1080], false, false, &output)
                    .unwrap_err();
            assert!(err.to_string().contains("another run"), "{err}");
            let err = add_mod_to_database(tmp.path(), &[], "vanilla", &output, None).unwrap_err();
            assert!(err.to_string().contains("another run"), "{err}");
            assert!(!output.exists());

            drop(lock);
            modify_icon_in_database(&rifle(), Some(&icon), &[1080], false, false, &output).unwrap();
            assert!(output.exists());
        }

        #[test]
        fn unstorable_names_are_refused_before_touching_the_database() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let empty_mod = IconIdentity {
                mod_name: String::new(),
                ..rifle()
            };
            let nul_code = IconIdentity {
                code: "Ri\0fle".to_string(),
                ..rifle()
            };
            for identity in [empty_mod, nul_code] {
                let result =
                    modify_icon_in_database(&identity, None, &[1080], false, true, &output);
                assert!(result.is_err(), "{identity:?}");
                assert!(!output.exists());
            }
        }

        #[test]
        fn png_icon_is_added_like_a_uasset_one() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let icon = tmp.path().join("rifle.png");
            image::RgbaImage::from_pixel(64, 64, image::Rgba([200, 100, 50, 255]))
                .save(&icon)
                .unwrap();

            modify_icon_in_database(&rifle(), Some(&icon), &[1080], false, false, &output).unwrap();

            let written = hdf5_writer::read_database(&output).unwrap();
            let template = &written[&1080][0];
            assert_eq!(template.code, "Rifle");
            // Opaque orange, stored as BGR.
            assert_eq!(&template.image[..3], &[50, 100, 200]);
        }

        #[test]
        fn unsupported_icon_format_is_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let icon = tmp.path().join("rifle.jpg");
            std::fs::write(&icon, b"not an icon").unwrap();

            let err =
                modify_icon_in_database(&rifle(), Some(&icon), &[1080], false, false, &output)
                    .unwrap_err()
                    .to_string();

            assert!(err.contains(".png or .uasset"), "{err}");
            assert!(!output.exists());
        }

        #[test]
        fn remove_without_icon_needs_no_uasset_and_errors_when_missing() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let err = modify_icon_in_database(
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                None,
                &[1080],
                false,
                true,
                &output,
            )
            .unwrap_err();
            assert!(err.to_string().contains("not found"));
        }

        #[test]
        fn add_without_icon_path_errors() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let err = modify_icon_in_database(
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                None,
                &[1080],
                false,
                false,
                &output,
            )
            .unwrap_err();
            assert!(err.to_string().contains("--icon is required"));
        }

        #[test]
        #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
        fn real_icon_add_then_remove_round_trips_through_hdf5() {
            let tmp = tempfile::tempdir().unwrap();
            let output = tmp.path().join("out.h5");
            let icon = fixture("BlueprintIcon.uasset");

            modify_icon_in_database(
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                Some(&icon),
                &[1080],
                false,
                false,
                &output,
            )
            .unwrap();

            let file = hdf5::File::open(&output).unwrap();
            let group = file.group("1080").unwrap();
            let count: i64 = group.attr("template_count").unwrap().read_scalar().unwrap();
            assert_eq!(count, 1);
            drop(file);

            modify_icon_in_database(
                &identity(
                    "Rifle",
                    ItemFaction::Neutral,
                    ItemCategory::Item,
                    false,
                    "vanilla",
                ),
                None,
                &[1080],
                false,
                true,
                &output,
            )
            .unwrap();

            let file = hdf5::File::open(&output).unwrap();
            let group = file.group("1080").unwrap();
            let count: i64 = group.attr("template_count").unwrap().read_scalar().unwrap();
            assert_eq!(count, 0);
        }
    }
}
