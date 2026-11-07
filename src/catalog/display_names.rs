//! The in-game name, per language, of every structure type and vehicle fs can
//! receive: those holding a stockpile, and the structures a player can pin on
//! the map (each pin goes into the `.sav`).
//!
//! Both are told from the pak, so a type a game update adds comes out on its
//! own, with no list of codes to maintain:
//!
//! - A stockpile has a map icon (`MapIconType`) and a stockpile or factory
//!   component (`GenericStockpileComponent`, `SpecializedFactoryComponent`).
//!   Ships holding one count; the `Factory` has only the factory component.
//! - A pinnable structure has a map icon and a native class from
//!   [`PINNABLE_CLASSES`]. Which native classes those are isn't in the pak, so
//!   that list is the one thing kept by hand; the native class of each
//!   blueprint is read from the pak, at the end of its parent chain.
//!
//! These aren't catalog entries, so they get their own table. A name is
//! resolved through the localization key of the `DisplayName` of the blueprint
//! with that `CodeName`, never through its English text: one English text can
//! have several keys with different translations.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::catalog::blueprint_parser::{self, filesystem_loader, Blueprints};
use crate::catalog::catalog_assembler::{enrich_locales, extract_localization_keys, sort_catalog};
use crate::catalog::import_resolution;
use crate::catalog::localization::LocalizationLookup;
use crate::catalog::pipeline::{self, CatalogSource};
use crate::catalog::utils::{find_default_export, import_at};
use crate::convert;

/// Subdirectories of `Blueprints/` scanned, recursively, for named blueprints.
/// The catalog's own search directories cover them, links refused included.
const NAMED_DIRECTORIES: &[&str] = &["Structures", "Vehicles"];

/// Native classes (`/Script/War`) whose structures a player can pin on the
/// map, given a map icon. A native class added by a game update is left out
/// until it is added here.
pub const PINNABLE_CLASSES: &[&str] = &[
    "AssemblyStation",
    "Base",
    "BorderBase",
    "ConstructionSite",
    "EngineRoom",
    "FacilityRefinery",
    "FortForwardBase",
    "FortGarrisonStation",
    "ForwardBase",
    "GarrisonStation",
    "HomeRegionBase",
    "Hospital",
    "Keep",
    "LiquidTransferStation",
    "MaintenanceTunnel",
    "MassProductionFactory",
    "Outpost",
    "PowerPlant",
    "Refinery",
    "RelicBase",
    "ResourceField",
    "ResourceMine",
    "ResourceTransferStation",
    "RocketFacility",
    "RocketTarget",
    "Seaport",
    "SpecializedFactory",
    "StaticBase",
    "StorageFacility",
    "TownBase",
    "TownHall",
    "VehicleFactory",
    "WeatherStation",
];

/// Components that make a blueprint with a map icon a stockpile.
const STOCKPILE_COMPONENTS: &[&str] = &["GenericStockpileComponent", "SpecializedFactoryComponent"];

/// Package of the game's native classes.
const NATIVE_PACKAGE: &str = "/Script/War";

/// Longest parent chain walked looking for a native class.
const MAX_PARENT_DEPTH: usize = 32;

/// What a display-names build did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DisplayNamesStats {
    /// Blueprint files looked at.
    pub total_files: usize,
    /// Blueprint files that failed to parse: a code they define can't come
    /// out in the table.
    pub errors: usize,
    /// Codes in the table.
    pub resolved: usize,
    /// Codes in the table that hold a stockpile.
    pub stockpiles: usize,
    /// Codes in the table that can be pinned.
    pub pinnable: usize,
    /// Why each stockpile or pinnable code is out of the table, for want of a
    /// usable name. Build sites are expected here: they have no name of their
    /// own.
    pub unnamed: BTreeMap<String, String>,
    /// Blueprints with a map icon whose native class couldn't be found, so
    /// whether they can be pinned is unknown.
    pub unknown_class: Vec<String>,
    /// Why each of the required codes is out of the table.
    pub missing: BTreeMap<String, String>,
}

/// A blueprint found defining a code.
struct Definition {
    object_path: String,
    display_name: Option<Value>,
    stockpile: bool,
    pinnable: bool,
}

/// Whether the merged blueprint data `data` has a map icon.
fn has_map_icon(data: &Map<String, Value>) -> bool {
    data.get("MapIconType")
        .and_then(Value::as_str)
        .is_some_and(|icon| !icon.is_empty() && !icon.ends_with("::None"))
}

/// Whether the merged blueprint data `data` holds a stockpile.
fn is_stockpile(data: &Map<String, Value>) -> bool {
    has_map_icon(data) && STOCKPILE_COMPONENTS.iter().any(|c| data.contains_key(*c))
}

/// Where the parent class of `asset` comes from.
enum Parent {
    /// A native class, by name.
    Native(String),
    /// A blueprint, by `/Game/...` object path.
    Blueprint(String),
}

/// The parent class of the blueprint in `asset`.
fn parent_of(asset: &Value) -> Option<Parent> {
    let default_export = find_default_export(asset)?;
    let class_name = default_export
        .get("object_name")
        .and_then(Value::as_str)?
        .strip_prefix("Default__")?;
    let class_export = asset
        .get("exports")?
        .as_array()?
        .iter()
        .find(|e| e.get("object_name").and_then(Value::as_str) == Some(class_name))?;
    let super_index = class_export.get("super_index").and_then(Value::as_i64)?;
    let imports: Vec<Value> = asset
        .get("imports")?
        .as_array()?
        .iter()
        .map(import_resolution::to_pascalcase_import)
        .collect();
    let parent = import_at(super_index, &imports)?;
    if let Some(path) = import_resolution::resolve_import_full_path(parent, &imports) {
        return Some(Parent::Blueprint(path));
    }
    let outer = import_at(parent.get("OuterIndex").and_then(Value::as_i64)?, &imports)?;
    if outer.get("ObjectName").and_then(Value::as_str) != Some(NATIVE_PACKAGE) {
        return None;
    }
    let name = parent.get("ObjectName").and_then(Value::as_str)?;
    Some(Parent::Native(name.to_string()))
}

/// Finds native classes up blueprint parent chains, remembering each
/// blueprint's.
struct NativeClasses<'a> {
    loader: &'a dyn Fn(&str) -> Option<Value>,
    known: HashMap<String, Option<String>>,
}

impl NativeClasses<'_> {
    /// The native class at the end of the parent chain of `asset`.
    fn of(&mut self, asset: &Value) -> Option<String> {
        let mut parent = parent_of(asset)?;
        let mut walked = Vec::new();
        let found = loop {
            let path = match parent {
                Parent::Native(name) => break Some(name),
                Parent::Blueprint(path) => path,
            };
            if let Some(known) = self.known.get(&path) {
                break known.clone();
            }
            if walked.len() >= MAX_PARENT_DEPTH || walked.contains(&path) {
                break None;
            }
            let next = (self.loader)(&path).as_ref().and_then(parent_of);
            walked.push(path);
            match next {
                Some(next) => parent = next,
                None => break None,
            }
        };
        for path in walked {
            self.known.insert(path, found.clone());
        }
        found
    }
}

/// The output entry of `code` out of the blueprints defining it, or why it
/// has none.
fn resolve_code(
    code: &str,
    definitions: &[Definition],
    loc_service: &LocalizationLookup,
) -> Result<Value, String> {
    if definitions.is_empty() {
        return Err("no blueprint has this CodeName".to_string());
    }

    // Blueprints of one code normally share a key. One of them having none
    // is no conflict; two different keys can't be told apart by this code.
    let mut resolved: Vec<(String, Map<String, Value>, &str)> = Vec::new();
    let mut without_key = Vec::new();
    for definition in definitions {
        let mut data = Map::new();
        if let Some(display_name) = &definition.display_name {
            data.insert("DisplayName".to_string(), display_name.clone());
        }
        extract_localization_keys(&mut data, loc_service);
        match data
            .get("DisplayNameKey")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            Some(key) if !key.is_empty() => {
                if !resolved.iter().any(|(known, _, _)| *known == key) {
                    resolved.push((key, data, &definition.object_path));
                }
            }
            _ => without_key.push(definition.object_path.as_str()),
        }
    }

    let (key, mut data, object_path) = match resolved.len() {
        0 => {
            return Err(format!(
                "no localization key in the DisplayName of {}",
                without_key.join(", ")
            ))
        }
        1 => resolved.remove(0),
        _ => {
            let found: Vec<String> = resolved
                .iter()
                .map(|(key, _, object_path)| format!("{object_path} ({key})"))
                .collect();
            return Err(format!(
                "blueprints disagree on the DisplayName key: {}",
                found.join(", ")
            ));
        }
    };

    let display_name = data
        .get("DisplayName")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && *text != key)
        .map(str::to_string)
        .ok_or_else(|| format!("key {key} of {object_path} has no text"))?;

    enrich_locales(&mut data, &["DisplayName"], loc_service);
    let locales = data
        .remove("DisplayNameLocales")
        .unwrap_or_else(|| json!({}));
    Ok(json!({
        "CodeName": code,
        "DisplayName": display_name,
        "DisplayNameLocales": locales,
        "Stockpile": definitions.iter().any(|d| d.stockpile),
        "Pinnable": definitions.iter().any(|d| d.pinnable),
    }))
}

/// Builds the table from an extraction directory, reporting `(done, total)`
/// over the blueprint files to `progress`. A code in `required` that doesn't
/// come out in the table doesn't fail the build: it is logged and reported in
/// [`DisplayNamesStats::missing`].
pub fn build_display_names_from_extract_dir(
    extract_dir: &Path,
    required: &[String],
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<(Vec<Value>, DisplayNamesStats)> {
    let dirs = pipeline::content_dirs(extract_dir)?;

    let loader = pipeline::catching_loader(filesystem_loader(&dirs.blueprints));
    let blueprints = Blueprints::new(&loader, true);
    let mut native_classes = NativeClasses {
        loader: &loader,
        known: HashMap::new(),
    };
    let pinnable_classes: HashSet<&str> = PINNABLE_CLASSES.iter().copied().collect();
    let loc_service = LocalizationLookup::new(&dirs.localization, None);

    let mut files = Vec::new();
    for directory in NAMED_DIRECTORIES {
        files.extend(pipeline::collect_blueprint_files(
            &dirs.blueprints.join(directory),
        )?);
    }

    let mut stats = DisplayNamesStats {
        total_files: files.len(),
        ..Default::default()
    };
    let mut definitions: BTreeMap<String, Vec<Definition>> = BTreeMap::new();

    progress(0, files.len())?;
    for (index, file) in files.iter().enumerate() {
        if index > 0 {
            progress(index, files.len())?;
        }
        let object_path = blueprint_parser::compute_object_path(&dirs.blueprints, file);
        let extracted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let asset = convert::uasset_to_full_json(file)?;
            let data = blueprints.extract(&object_path, &asset);
            let native_class = native_classes.of(&asset);
            anyhow::Ok(data.map(|data| (data, native_class)))
        }));
        let data = match extracted {
            Ok(Ok(data)) => data,
            Ok(Err(e)) => {
                log::warn!("display-names: failed to parse {}: {e}", file.display());
                stats.errors += 1;
                continue;
            }
            Err(_) => {
                log::warn!("display-names: panicked while parsing {}", file.display());
                stats.errors += 1;
                continue;
            }
        };
        let Some((Value::Object(data), native_class)) = data else {
            continue;
        };
        let Some(code) = data.get("CodeName").and_then(Value::as_str) else {
            continue;
        };
        let map_icon = has_map_icon(&data);
        if map_icon && native_class.is_none() {
            log::warn!("display-names: no native class found for {object_path}");
            stats.unknown_class.push(object_path.clone());
        }
        let pinnable = map_icon
            && native_class
                .as_deref()
                .is_some_and(|class| pinnable_classes.contains(class));
        definitions
            .entry(code.to_string())
            .or_default()
            .push(Definition {
                object_path: object_path.clone(),
                display_name: data.get("DisplayName").cloned(),
                stockpile: is_stockpile(&data),
                pinnable,
            });
    }
    progress(files.len(), files.len())?;

    let mut table = Vec::new();
    for (code, found) in &definitions {
        if !found.iter().any(|d| d.stockpile || d.pinnable) {
            continue;
        }
        match resolve_code(code, found, &loc_service) {
            Ok(entry) => table.push(entry),
            Err(reason) => {
                log::debug!("display-names: {code} left out: {reason}");
                stats.unnamed.insert(code.clone(), reason);
            }
        }
    }
    sort_catalog(&mut table);
    stats.resolved = table.len();
    stats.stockpiles = table.iter().filter(|e| e["Stockpile"] == true).count();
    stats.pinnable = table.iter().filter(|e| e["Pinnable"] == true).count();

    for code in required {
        if table.iter().any(|entry| entry["CodeName"] == code.as_str()) {
            continue;
        }
        let reason = stats.unnamed.get(code).cloned().unwrap_or_else(|| {
            if definitions.contains_key(code) {
                "neither a stockpile nor pinnable".to_string()
            } else {
                "no blueprint has this CodeName".to_string()
            }
        });
        log::warn!("display-names: {code} left out: {reason}");
        stats.missing.insert(code.clone(), reason);
    }
    Ok((table, stats))
}

/// Builds the table from `source` and writes it to `output` as JSON. Progress
/// is reported over a pak's extracted files to `extracting` and over the
/// blueprint files to `building`.
pub fn build_display_names_to_file(
    source: CatalogSource<'_>,
    output: &Path,
    required: &[String],
    extracting: &dyn Fn(usize, usize) -> anyhow::Result<()>,
    building: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<DisplayNamesStats> {
    let (table, stats) = match source {
        CatalogSource::ExtractDir(dir) => {
            build_display_names_from_extract_dir(dir, required, building)?
        }
        CatalogSource::Pak(pak) => {
            let workspace = pipeline::extract_catalog_inputs(pak, extracting)?;
            build_display_names_from_extract_dir(workspace.path(), required, building)?
        }
    };
    crate::atomic_write::write_json(output, &table)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A `Localization` directory holding `languages` as (language, key,
    /// text) rows.
    fn localization(root: &Path, rows: &[(&str, &str, &str)]) -> LocalizationLookup {
        for language in ["en", "de", "ru", "pt"] {
            let entries = rows
                .iter()
                .filter(|(row_language, _, _)| *row_language == language)
                .map(|(_, key, text)| crate::locres::LocresEntry {
                    key_hash: 0,
                    key: key.to_string(),
                    source_hash: 0,
                    text: text.to_string(),
                })
                .collect();
            let path = root.join(format!("Foxhole-Content/{language}/game.locres"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            crate::locres::write(
                &crate::locres::LocresDocument {
                    version: 2,
                    namespaces: vec![crate::locres::LocresNamespace {
                        namespace_hash: 0,
                        namespace: String::new(),
                        entries,
                    }],
                },
                &path,
            )
            .unwrap();
        }
        LocalizationLookup::new(root, None)
    }

    fn definition(object_path: &str, display_name: Value) -> Definition {
        Definition {
            object_path: object_path.to_string(),
            display_name: Some(display_name),
            stockpile: true,
            pinnable: false,
        }
    }

    fn named(text: &str, key: &str) -> Value {
        json!({"Text": text, "Guid": key})
    }

    #[test]
    fn translates_by_the_key_not_by_the_english_text() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(
            tmp.path(),
            &[
                ("en", "TENT", "Encampment"),
                ("ru", "TENT", "Палатка"),
                ("en", "BASE", "Encampment"),
                ("ru", "BASE", "Лагерь"),
            ],
        );

        let entry = resolve_code(
            "ForwardBase1",
            &[definition("BPForwardBase1", named("Encampment", "BASE"))],
            &loc,
        )
        .unwrap();

        assert_eq!(
            entry,
            json!({
                "CodeName": "ForwardBase1",
                "DisplayName": "Encampment",
                "DisplayNameLocales": {"en": "Encampment", "ru": "Лагерь"},
                "Stockpile": true,
                "Pinnable": false,
            })
        );
    }

    #[test]
    fn leaves_out_a_language_without_a_translation() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(
            tmp.path(),
            &[
                ("en", "H", "Hospital"),
                ("de", "H", "Krankenhaus"),
                ("pt", "H", ""),
            ],
        );

        let entry = resolve_code(
            "Hospital",
            &[definition("BPHospital", named("Hospital", "H"))],
            &loc,
        )
        .unwrap();

        assert_eq!(
            entry["DisplayNameLocales"],
            json!({"de": "Krankenhaus", "en": "Hospital"})
        );
    }

    #[test]
    fn keeps_the_blueprint_text_when_no_language_has_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(tmp.path(), &[]);

        let entry =
            resolve_code("Ship", &[definition("BPShip", named("Ship", "S"))], &loc).unwrap();

        assert_eq!(entry["DisplayName"], "Ship");
        assert_eq!(entry["DisplayNameLocales"], json!({}));
    }

    #[test]
    fn reports_a_code_no_blueprint_has() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(tmp.path(), &[]);

        assert_eq!(
            resolve_code("Nope", &[], &loc).unwrap_err(),
            "no blueprint has this CodeName"
        );
    }

    #[test]
    fn reports_a_blueprint_without_a_display_name() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(tmp.path(), &[]);
        let definitions = [Definition {
            object_path: "BPNoName".to_string(),
            display_name: None,
            stockpile: true,
            pinnable: true,
        }];

        let reason = resolve_code("NoName", &definitions, &loc).unwrap_err();

        assert!(reason.contains("no localization key"), "{reason}");
        assert!(reason.contains("BPNoName"), "{reason}");
    }

    #[test]
    fn reports_a_display_name_without_a_key_instead_of_using_its_text() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(tmp.path(), &[("en", "K", "Keep")]);

        let reason =
            resolve_code("Keep", &[definition("BPKeep", json!("Keep"))], &loc).unwrap_err();

        assert!(reason.contains("no localization key"), "{reason}");
    }

    #[test]
    fn reports_a_bare_key_that_has_no_text() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(tmp.path(), &[]);
        let key = "8BB336F4459740A6ADA7B28B2D91748B";

        let reason = resolve_code("Bare", &[definition("BPBare", json!(key))], &loc).unwrap_err();

        assert!(reason.contains("has no text"), "{reason}");
    }

    #[test]
    fn reports_blueprints_that_disagree_on_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(tmp.path(), &[]);
        let definitions = [
            definition("BPKeepA", named("Keep", "A")),
            definition("BPKeepB", named("Keep", "B")),
        ];

        let reason = resolve_code("Keep", &definitions, &loc).unwrap_err();

        assert!(reason.contains("BPKeepA (A)"), "{reason}");
        assert!(reason.contains("BPKeepB (B)"), "{reason}");
    }

    #[test]
    fn accepts_blueprints_that_share_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(tmp.path(), &[("en", "K", "Keep")]);
        let definitions = [
            definition("BPKeepA", named("Keep", "K")),
            definition("BPKeepB", named("Keep", "K")),
        ];

        let entry = resolve_code("Keep", &definitions, &loc).unwrap();

        assert_eq!(entry["DisplayNameLocales"], json!({"en": "Keep"}));
    }

    #[test]
    fn a_code_is_pinnable_or_a_stockpile_if_any_of_its_blueprints_is() {
        let tmp = tempfile::tempdir().unwrap();
        let loc = localization(tmp.path(), &[("en", "K", "Keep")]);
        let mut pinned = definition("BPKeepB", named("Keep", "K"));
        pinned.stockpile = false;
        pinned.pinnable = true;
        let mut stockpile = definition("BPKeepA", named("Keep", "K"));
        stockpile.stockpile = true;

        let entry = resolve_code("Keep", &[stockpile, pinned], &loc).unwrap();

        assert_eq!(entry["Stockpile"], true);
        assert_eq!(entry["Pinnable"], true);
    }

    fn data(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_stockpile_needs_a_map_icon_and_a_stockpile_or_factory_component() {
        let icon = "EMapIconType::StorageFacility";
        assert!(is_stockpile(&data(
            json!({"MapIconType": icon, "GenericStockpileComponent": {}})
        )));
        assert!(is_stockpile(&data(
            json!({"MapIconType": icon, "SpecializedFactoryComponent": {}})
        )));
        assert!(!is_stockpile(&data(json!({"MapIconType": icon}))));
        assert!(!is_stockpile(&data(
            json!({"GenericStockpileComponent": {}})
        )));
        assert!(!is_stockpile(&data(json!({
            "MapIconType": "EMapIconType::None",
            "GenericStockpileComponent": {},
        }))));
    }

    /// A blueprint asset named `name` whose class's parent is the import
    /// `parent` (`object_name`, `class_name`) inside the package `package`.
    fn asset(name: &str, parent: &str, parent_class: &str, package: &str) -> Value {
        json!({
            "imports": [
                {"object_name": package, "class_name": "Package", "outer_index": 0},
                {"object_name": parent, "class_name": parent_class, "outer_index": -1},
            ],
            "exports": [
                {"object_name": format!("{name}_C"), "super_index": -2},
                {"object_name": format!("Default__{name}_C")},
            ],
        })
    }

    #[test]
    fn finds_the_native_class_up_the_parent_chain() {
        let parent = asset("BPBase", "StorageFacility", "Class", NATIVE_PACKAGE);
        let loader = move |path: &str| (path == "/Game/Blueprints/BPBase").then(|| parent.clone());
        let mut classes = NativeClasses {
            loader: &loader,
            known: HashMap::new(),
        };
        let child = asset(
            "BPChild",
            "BPBase_C",
            "BlueprintGeneratedClass",
            "/Game/Blueprints/BPBase",
        );

        assert_eq!(classes.of(&child).as_deref(), Some("StorageFacility"));
        assert_eq!(
            classes.known["/Game/Blueprints/BPBase"].as_deref(),
            Some("StorageFacility")
        );
    }

    #[test]
    fn a_parent_outside_the_game_package_has_no_native_class() {
        let loader = |_: &str| None;
        let mut classes = NativeClasses {
            loader: &loader,
            known: HashMap::new(),
        };

        let engine = asset("BPActor", "Actor", "Class", "/Script/Engine");
        assert_eq!(classes.of(&engine), None);
        let unloadable = asset(
            "BPChild",
            "BPGone_C",
            "BlueprintGeneratedClass",
            "/Game/Blueprints/BPGone",
        );
        assert_eq!(classes.of(&unloadable), None);
    }

    #[test]
    fn stops_on_a_parent_cycle() {
        let looped = asset(
            "BPLoop",
            "BPLoop_C",
            "BlueprintGeneratedClass",
            "/Game/Blueprints/BPLoop",
        );
        let copy = looped.clone();
        let loader = move |_: &str| Some(copy.clone());
        let mut classes = NativeClasses {
            loader: &loader,
            known: HashMap::new(),
        };

        assert_eq!(classes.of(&looped), None);
    }

    fn empty_extraction() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["Blueprints/Data", "Localization"] {
            fs::create_dir_all(tmp.path().join("War/Content").join(dir)).unwrap();
        }
        tmp
    }

    #[test]
    fn an_extraction_without_blueprints_yields_an_empty_table() {
        let tmp = empty_extraction();

        let (table, stats) =
            build_display_names_from_extract_dir(tmp.path(), &[], &|_, _| Ok(())).unwrap();

        assert!(table.is_empty());
        assert_eq!(stats, DisplayNamesStats::default());
    }

    #[test]
    fn reports_a_required_code_that_is_not_in_the_table() {
        let tmp = empty_extraction();
        let required = ["NewBase".to_string()];

        let (_, stats) =
            build_display_names_from_extract_dir(tmp.path(), &required, &|_, _| Ok(())).unwrap();

        assert_eq!(
            stats.missing["NewBase"],
            "no blueprint has this CodeName".to_string()
        );
    }

    #[test]
    fn fails_on_an_invalid_extraction() {
        let tmp = tempfile::tempdir().unwrap();

        let error =
            build_display_names_from_extract_dir(tmp.path(), &[], &|_, _| Ok(())).unwrap_err();

        assert!(error.to_string().contains("directory not found"), "{error}");
    }

    #[test]
    fn writes_the_table_even_with_missing_codes() {
        let tmp = empty_extraction();
        let output = tmp.path().join("display_names.json");
        let required = ["NewBase".to_string()];

        let stats = build_display_names_to_file(
            CatalogSource::ExtractDir(tmp.path()),
            &output,
            &required,
            &|_, _| Ok(()),
            &|_, _| Ok(()),
        )
        .unwrap();

        assert_eq!(stats.missing.len(), 1);
        assert_eq!(fs::read_to_string(&output).unwrap().trim(), "[]");
    }
}
