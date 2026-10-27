//! Blueprint catalog extraction.
//!
//! This module operates on the JSON shape produced by
//! `crate::convert::uasset_to_full_json`:
//!
//! ```text
//! { "imports": [ { object_name, class_name, class_package, outer_index } ],
//!   "exports": [ { object_name, class_index, super_index, template_index,
//!                  outer_index, kind, properties?, rows? } ] }
//! ```
//!
//! `uasset_to_full_json`'s `properties` map is already the fully "processed"
//! property tree (no `$type` tags, no `Data` array-of-`{Name, ...}` needing
//! conversion to a dict) — that conversion is `convert::property_to_value`'s
//! job, run while walking the parsed `unreal_asset::Property` enum. So this
//! module only handles reference resolution and the inheritance walk.
//! `ObjectProperty` values still show up as `"Reference: N"` sentinel strings
//! (see `convert::property_to_value`), so
//! `crate::catalog::utils::parse_reference` parses those back into raw
//! indices.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::catalog::import_resolution;
use crate::catalog::utils::{parse_reference, simplify_value};

/// Callback used to load a parent blueprint's asset JSON (in
/// `uasset_to_full_json` shape) given its `/Game/...` object path.
///
/// Returns `None` if the asset can't be found/loaded (e.g. the parent file
/// doesn't exist).
/// Production code will typically compose this from a path-resolution step
/// (see [`filesystem_loader`]) plus `crate::convert::uasset_to_full_json`;
/// tests can inject an in-memory lookup instead, which is the whole point of
/// taking this as a parameter rather than hard-coding filesystem access into
/// the algorithm below.
pub type AssetLoader<'a> = dyn Fn(&str) -> Option<Value> + 'a;

/// Longest blueprint parent chain followed. The game's own chains are a
/// handful of levels; a mod could chain thousands of blueprints.
const MAX_PARENT_DEPTH: usize = 64;

/// Extract simplified catalog data from an already-parsed blueprint asset.
///
/// `object_path` is this blueprint's own `/Game/...` object path (used to
/// populate `ObjectPath` and to detect cycles while walking the parent
/// chain — a cheap visited-set costs nothing in the non-cyclic case and
/// prevents infinite recursion on malformed/cyclic input). `asset` is the
/// `uasset_to_full_json` value for this blueprint. `full_extraction`: when
/// `true`, export
/// references are resolved to object names and import references fall back
/// to raw asset-path resolution; when `false` (the catalog-builder default),
/// unresolved/empty results are dropped instead. `loader` is invoked only
/// when a parent blueprint needs to be loaded for the inheritance walk.
///
/// Returns `None` if `asset` has no `Default__` export.
///
/// No cross-call cache is kept here: this is a single pure function with no
/// persistent pipeline object, so memoization is the caller's
/// responsibility (e.g. by giving `loader` its own cache) if it's calling
/// this repeatedly across a whole catalog build. Runaway recursion is cut
/// off via the visited-set below, not via memoization.
pub fn extract_catalog_data(
    object_path: &str,
    asset: &Value,
    full_extraction: bool,
    loader: &AssetLoader,
) -> Option<Value> {
    let mut visited = HashSet::new();
    extract_catalog_data_impl(object_path, asset, full_extraction, loader, &mut visited)
}

fn extract_catalog_data_impl(
    object_path: &str,
    asset: &Value,
    full_extraction: bool,
    loader: &AssetLoader,
    visited: &mut HashSet<String>,
) -> Option<Value> {
    // `visited` holds the chain walked so far: stop on a cycle, and on a chain
    // long enough to exhaust the stack (each level recurses back into here).
    if visited.len() >= MAX_PARENT_DEPTH || !visited.insert(object_path.to_string()) {
        return None;
    }

    let exports = asset.get("exports").and_then(Value::as_array)?;
    let empty_imports = Vec::new();
    let imports = asset
        .get("imports")
        .and_then(Value::as_array)
        .unwrap_or(&empty_imports);

    let default_export = exports.iter().find(|e| {
        e.get("object_name")
            .and_then(Value::as_str)
            .map(|s| s.starts_with("Default__"))
            .unwrap_or(false)
    })?;

    let mut catalog_data = Map::new();
    catalog_data.insert("ObjectPath".to_string(), json!(object_path));

    // Step 2: ParentBlueprint via ClassIndex(-by-name) -> SuperIndex chain.
    // Adapted once per asset: every import lookup below reads this shape.
    let imports: Vec<Value> = imports
        .iter()
        .map(import_resolution::to_pascalcase_import)
        .collect();
    let parent_path = resolve_parent_blueprint(default_export, exports, &imports);
    if let Some(ref p) = parent_path {
        catalog_data.insert("ParentBlueprint".to_string(), json!(p));
    }

    // Step 3: process the Default__ export's properties. `uasset_to_full_json`
    // omits `properties` entirely for `kind == "raw"` exports (the extraction
    // tool couldn't parse them). In that case we stop here without
    // attempting the parent merge.
    let properties = match default_export.get("properties") {
        Some(v @ Value::Object(_)) | Some(v @ Value::Array(_)) => v,
        _ => return Some(Value::Object(catalog_data)),
    };

    let processed_imports = process_imports(&imports);
    let extracted = process_data(
        properties,
        &processed_imports,
        exports,
        &imports,
        full_extraction,
    );
    if let Value::Object(extracted_map) = extracted {
        for (k, v) in extracted_map {
            catalog_data.insert(k, v);
        }
    }

    // Step 4: merge parent data beneath the child's own (parent-first, child
    // overrides all matching keys), only when the parent is itself a
    // /Game/Blueprints/ asset.
    if let Some(p) = &parent_path {
        if p.starts_with("/Game/Blueprints/") {
            if let Some(parent_asset) = loader(p) {
                if let Some(Value::Object(parent_map)) =
                    extract_catalog_data_impl(p, &parent_asset, full_extraction, loader, visited)
                {
                    catalog_data = merge_with_parent(catalog_data, parent_map);
                }
            }
        }
    }

    Some(Value::Object(catalog_data))
}

/// Merge `parent_data` beneath `catalog_data` (parent values first, child
/// overrides), then force the child's own identity fields back on top so
/// they never leak from an ancestor. Note that `CodeName`/`ObjectPath` are always
/// force-set from the child (even to `null` if the child has none), while
/// `ParentBlueprint` is only overwritten when the child actually has one
/// (letting a grandparent's already-merged `ParentBlueprint` survive
/// otherwise — though in practice this merge is only ever invoked when the
/// child does have a `ParentBlueprint`, since that's what gates the call).
fn merge_with_parent(
    catalog_data: Map<String, Value>,
    parent_data: Map<String, Value>,
) -> Map<String, Value> {
    let mut merged = parent_data;
    for (k, v) in catalog_data.iter() {
        merged.insert(k.clone(), v.clone());
    }
    merged.insert(
        "ObjectPath".to_string(),
        catalog_data
            .get("ObjectPath")
            .cloned()
            .unwrap_or(Value::Null),
    );
    merged.insert(
        "CodeName".to_string(),
        catalog_data.get("CodeName").cloned().unwrap_or(Value::Null),
    );
    if let Some(pb) = catalog_data.get("ParentBlueprint") {
        merged.insert("ParentBlueprint".to_string(), pb.clone());
    }
    merged
}

/// Resolve parent blueprint path from the class export's `super_index`.
///
/// Finds the class export by NAME (the `Default__` export's name minus the
/// `Default__` prefix) — not by indexing `default_export`'s `class_index`
/// into `exports`. `class_index` typically *does* point at the same export,
/// but the name lookup doesn't rely on that.
fn resolve_parent_blueprint(
    default_export: &Value,
    exports: &[Value],
    imports: &[Value],
) -> Option<String> {
    let default_name = default_export
        .get("object_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let class_name = default_name.strip_prefix("Default__")?;

    let class_export = exports
        .iter()
        .find(|e| e.get("object_name").and_then(Value::as_str) == Some(class_name))?;

    let super_index = class_export
        .get("super_index")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if super_index >= 0 {
        return None;
    }

    let import_obj = imports.get(usize::try_from(super_index.unsigned_abs() - 1).ok()?)?;
    import_resolution::resolve_import_full_path(import_obj, imports)
}

/// Resolve each (adapted) import to a blueprint path (`None` for anything
/// that isn't a `BlueprintGeneratedClass` under a `/Game/` package).
fn process_imports(imports: &[Value]) -> Vec<Option<String>> {
    imports
        .iter()
        .map(|imp| import_resolution::resolve_import_full_path(imp, imports))
        .collect()
}

/// A processed value is worth keeping in "simple" (non-full-extraction) mode
/// only if it's non-empty — a truthiness check specialized to the only two
/// variants `process_data` ever recurses
/// into (dict/list in, same variant out).
fn is_nonempty(value: &Value) -> bool {
    match value {
        Value::Object(m) => !m.is_empty(),
        Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// Recursively resolve `"Reference: N"` sentinel strings within `data`
/// (a `properties`-shaped value) to import paths / export names.
///
/// `processed_imports` is this asset's `imports` array pre-resolved to
/// blueprint paths (or `None`); `exports` is the raw exports array and
/// `imports` the imports adapted by
/// [`import_resolution::to_pascalcase_import`], both used only
/// in `full_extraction` mode to resolve export references to object names
/// and to fall back to non-blueprint asset-path resolution.
fn process_data(
    data: &Value,
    processed_imports: &[Option<String>],
    exports: &[Value],
    imports: &[Value],
    full_extraction: bool,
) -> Value {
    match data {
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, value) in map {
                if let Some(ref_idx) = value.as_str().and_then(parse_reference) {
                    resolve_reference_into(
                        ref_idx,
                        processed_imports,
                        exports,
                        imports,
                        full_extraction,
                        |v| {
                            result.insert(key.clone(), v);
                        },
                    );
                } else if matches!(value, Value::Object(_) | Value::Array(_)) {
                    let processed =
                        process_data(value, processed_imports, exports, imports, full_extraction);
                    if full_extraction {
                        result.insert(key.clone(), simplify_value(&processed));
                    } else if is_nonempty(&processed) {
                        result.insert(key.clone(), processed);
                    }
                } else {
                    let v = if full_extraction {
                        simplify_value(value)
                    } else {
                        value.clone()
                    };
                    result.insert(key.clone(), v);
                }
            }
            Value::Object(result)
        }
        Value::Array(items) => {
            let mut result = Vec::new();
            for item in items {
                if let Some(ref_idx) = item.as_str().and_then(parse_reference) {
                    resolve_reference_into(
                        ref_idx,
                        processed_imports,
                        exports,
                        imports,
                        full_extraction,
                        |v| result.push(v),
                    );
                } else if matches!(item, Value::Object(_) | Value::Array(_)) {
                    let processed =
                        process_data(item, processed_imports, exports, imports, full_extraction);
                    // Note: unlike the dict branch, simplify_value is
                    // deliberately not applied here.
                    if full_extraction || is_nonempty(&processed) {
                        result.push(processed);
                    }
                } else {
                    // Also unlike the dict branch: plain scalars in a list
                    // are appended as-is, never through simplify_value,
                    // regardless of full_extraction.
                    result.push(item.clone());
                }
            }
            Value::Array(result)
        }
        other => other.clone(),
    }
}

/// Shared reference-resolution body for both the dict and list branches of
/// `process_data`. `emit` is called with the resolved value, if any; ref
/// index 0 (null) and unresolved references emit nothing.
fn resolve_reference_into(
    ref_idx: i64,
    processed_imports: &[Option<String>],
    exports: &[Value],
    imports: &[Value],
    full_extraction: bool,
    mut emit: impl FnMut(Value),
) {
    if ref_idx == 0 {
        return;
    }
    if ref_idx < 0 {
        let idx = (-ref_idx - 1) as usize;
        if let Some(Some(path)) = processed_imports.get(idx) {
            emit(json!(path));
        } else if full_extraction {
            if let Some(import_obj) = imports.get(idx) {
                if let Some(asset_path) =
                    import_resolution::resolve_import_asset_path(import_obj, imports)
                {
                    emit(json!(asset_path));
                }
            }
        }
    } else if full_extraction && (ref_idx as usize) <= exports.len() {
        let export_obj = &exports[(ref_idx - 1) as usize];
        if let Some(name) = export_obj.get("object_name").and_then(Value::as_str) {
            emit(json!(name));
        }
    }
}

/// Computes the catalog `ObjectPath` for a blueprint asset file, relative to
/// the war/ extraction root three directories above `blueprints_dir`. Falls
/// back to the absolute path (as a
/// string) if `asset_path` isn't under that root.
pub fn compute_object_path(blueprints_dir: &Path, asset_path: &Path) -> String {
    let extraction_root = blueprints_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent);

    if let Some(root) = extraction_root {
        if let Ok(relative) = asset_path.strip_prefix(root) {
            let mut s = relative.to_string_lossy().replace('\\', "/");
            for ext in [".json", ".uasset"] {
                if let Some(stripped) = s.strip_suffix(ext) {
                    s = stripped.to_string();
                    break;
                }
            }
            return s;
        }
    }

    asset_path.to_string_lossy().to_string()
}

/// Resolve a requested asset path to its actual on-disk location, searching
/// one level of subdirectories if the direct path doesn't exist: some
/// blueprints have `ObjectPath`s that
/// don't match their actual file location (e.g.
/// `Structures/BPEmplacedATW.uasset` actually living at
/// `Structures/Emplacements/BPEmplacedATW.uasset`).
pub fn find_blueprint_path(blueprints_dir: &Path, requested: &Path) -> Option<PathBuf> {
    if requested.exists() {
        return Some(requested.to_path_buf());
    }

    let rel_path = requested.strip_prefix(blueprints_dir).ok()?;
    let mut components = rel_path.components();
    let first = components.next()?;
    // Only search if there's at least one more component after the first
    // (i.e. the file isn't directly at the top level already).
    components.next()?;

    let filename = requested.file_name()?;
    let parent_dir = blueprints_dir.join(first);
    if parent_dir.is_dir() {
        return find_file_recursive(&parent_dir, filename);
    }

    None
}

fn find_file_recursive(dir: &Path, filename: &OsStr) -> Option<PathBuf> {
    // `walkdir` doesn't follow symlinks (so a link back up the tree can't
    // loop) and walks without recursing.
    walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .find(|entry| entry.file_type().is_file() && entry.file_name() == filename)
        .map(walkdir::DirEntry::into_path)
}

/// Builds a filesystem-backed [`AssetLoader`]: maps a `/Game/Blueprints/...`
/// object path to `<blueprints_dir>/<relative>.uasset`, applying the same
/// subdirectory search as [`find_blueprint_path`], then parses it via
/// `crate::convert::uasset_to_full_json`. Returns `None` (a cache-miss, in
/// `AssetLoader` terms) for paths outside `/Game/Blueprints/`, for files that
/// don't exist, or for files that fail to parse.
pub fn filesystem_loader(blueprints_dir: &Path) -> impl Fn(&str) -> Option<Value> + '_ {
    move |object_path: &str| {
        let candidate = blueprint_candidate(blueprints_dir, object_path)?;
        let resolved = find_blueprint_path(blueprints_dir, &candidate)?;
        crate::convert::uasset_to_full_json(&resolved).ok()
    }
}

/// `<blueprints_dir>/<relative>.uasset` for a `/Game/Blueprints/<relative>`
/// object path. The object path comes from an asset's imports, so it is kept
/// inside `blueprints_dir` like any other untrusted path: `None` for one that
/// would climb out with `..` or an absolute path.
fn blueprint_candidate(blueprints_dir: &Path, object_path: &str) -> Option<PathBuf> {
    let relative = object_path.strip_prefix("/Game/Blueprints/")?;
    crate::pak::contained_path(blueprints_dir, &format!("{relative}.uasset")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    /// A `HashMap`-backed loader standing in for real files on disk in
    /// tests, keyed by `/Game/...` object path.
    fn map_loader(assets: HashMap<String, Value>) -> impl Fn(&str) -> Option<Value> {
        move |path: &str| assets.get(path).cloned()
    }

    fn no_loader(_: &str) -> Option<Value> {
        None
    }

    /// Sample blueprint asset in `uasset_to_full_json` shape.
    fn sample_asset() -> Value {
        json!({
            "imports": [
                {"object_name": "/Script/CoreUObject", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                {"object_name": "BPItemBase_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -3},
                {"object_name": "/Game/Blueprints/Items/BPItemBase", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
            ],
            "exports": [
                {
                    "object_name": "BPTestItem_C",
                    "class_index": -2, "super_index": -2, "template_index": 0, "outer_index": 0,
                    "kind": "normal", "properties": {},
                },
                {
                    "object_name": "Default__BPTestItem_C",
                    "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "normal",
                    "properties": {
                        "CodeName": "TestItem",
                        "DisplayName": {"Text": "Test Item", "Guid": "GUID123"},
                        "bIsStockpilable": true,
                    },
                },
            ],
        })
    }

    mod extract_catalog_data_tests {
        use super::*;

        #[test]
        fn returns_none_when_no_default_export() {
            let asset = json!({
                "imports": [],
                "exports": [{"object_name": "NotDefault_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}}],
            });
            assert_eq!(
                extract_catalog_data("/Game/Blueprints/Items/BPX", &asset, false, &no_loader),
                None
            );
        }

        #[test]
        fn returns_catalog_data_with_object_path_and_properties() {
            let asset = sample_asset();
            let result = extract_catalog_data(
                "/Game/Blueprints/Items/BPTestItem",
                &asset,
                false,
                &no_loader,
            )
            .expect("expected catalog data");

            assert_eq!(
                result["ObjectPath"],
                json!("/Game/Blueprints/Items/BPTestItem")
            );
            assert_eq!(result["CodeName"], json!("TestItem"));
            assert_eq!(
                result["DisplayName"],
                json!({"Text": "Test Item", "Guid": "GUID123"})
            );
            assert_eq!(result["bIsStockpilable"], json!(true));
        }

        #[test]
        fn includes_parent_blueprint_when_super_resolves_to_blueprint_import() {
            let asset = sample_asset();
            let result = extract_catalog_data(
                "/Game/Blueprints/Items/BPTestItem",
                &asset,
                false,
                &no_loader,
            )
            .expect("expected catalog data");
            assert_eq!(
                result["ParentBlueprint"],
                json!("/Game/Blueprints/Items/BPItemBase")
            );
        }

        #[test]
        fn no_default_export_prefix_means_no_parent_and_result_is_none() {
            // No Default__ export at all -> whole call returns None,
            // independent of resolve_parent_blueprint's own None-handling of
            // non-"Default__" names, which is only reachable internally.
            let asset = json!({
                "imports": [],
                "exports": [{"object_name": "NotDefault", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}}],
            });
            assert_eq!(
                extract_catalog_data("/Game/Blueprints/X", &asset, false, &no_loader),
                None
            );
        }

        #[test]
        fn class_export_not_found_means_no_parent_blueprint() {
            let asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "SomeOther_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {"object_name": "Default__Test_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                ],
            });
            let result =
                extract_catalog_data("/Game/Blueprints/Test", &asset, false, &no_loader).unwrap();
            assert!(result.get("ParentBlueprint").is_none());
        }

        #[test]
        fn super_index_pointing_at_non_blueprint_import_means_no_parent() {
            let asset = json!({
                "imports": [
                    {"object_name": "/Script/CoreUObject", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": "Test_C", "class_index": 0, "super_index": -1, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {"object_name": "Default__Test_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                ],
            });
            let result =
                extract_catalog_data("/Game/Blueprints/Test", &asset, false, &no_loader).unwrap();
            assert!(result.get("ParentBlueprint").is_none());
        }

        #[test]
        fn parent_outside_game_blueprints_is_not_set_as_parent() {
            let asset = json!({
                "imports": [
                    {"object_name": "SomeClass_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -2},
                    {"object_name": "/Script/Engine/SomeClass", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": "Test_C", "class_index": 0, "super_index": -1, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {"object_name": "Default__Test_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                ],
            });
            let result =
                extract_catalog_data("/Game/Blueprints/Test", &asset, false, &no_loader).unwrap();
            assert!(result.get("ParentBlueprint").is_none());
        }

        #[test]
        fn raw_kind_default_export_returns_object_path_only() {
            let asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "Default__Test_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "raw"},
                ],
            });
            let result =
                extract_catalog_data("/Game/Blueprints/Test", &asset, false, &no_loader).unwrap();
            assert_eq!(result, json!({"ObjectPath": "/Game/Blueprints/Test"}));
        }

        #[test]
        fn parent_file_missing_still_returns_own_data_unmerged() {
            let asset = json!({
                "imports": [
                    {"object_name": "BPParent_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -2},
                    {"object_name": "/Game/Blueprints/Items/BPParent", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": "Test_C", "class_index": 0, "super_index": -1, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__Test_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal", "properties": {"CodeName": "Test"},
                    },
                ],
            });
            // no_loader simulates the parent file not existing on disk.
            let result =
                extract_catalog_data("/Game/Blueprints/Items/BPTest", &asset, false, &no_loader)
                    .unwrap();
            assert_eq!(result["CodeName"], json!("Test"));
            assert_eq!(
                result["ParentBlueprint"],
                json!("/Game/Blueprints/Items/BPParent")
            );
        }

        #[test]
        fn merges_parent_blueprint_data_parent_first_child_overrides() {
            let parent_asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "BPParent_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__BPParent_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"ParentProperty": 100, "SharedProperty": 50},
                    },
                ],
            });
            let child_asset = json!({
                "imports": [
                    {"object_name": "BPParent_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -2},
                    {"object_name": "/Game/Blueprints/Items/BPParent", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": "BPChild_C", "class_index": 0, "super_index": -1, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__BPChild_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"CodeName": "ChildItem", "ChildProperty": 200, "SharedProperty": 75},
                    },
                ],
            });

            let mut assets = HashMap::new();
            assets.insert("/Game/Blueprints/Items/BPParent".to_string(), parent_asset);
            let loader = map_loader(assets);

            let result = extract_catalog_data(
                "/Game/Blueprints/Items/BPChild",
                &child_asset,
                false,
                &loader,
            )
            .expect("expected catalog data");

            assert_eq!(result["CodeName"], json!("ChildItem"));
            assert_eq!(result["ChildProperty"], json!(200));
            // Inherited from parent.
            assert_eq!(result["ParentProperty"], json!(100));
            // Child overrides shared property.
            assert_eq!(result["SharedProperty"], json!(75));
            assert_eq!(
                result["ParentBlueprint"],
                json!("/Game/Blueprints/Items/BPParent")
            );
            assert_eq!(
                result["ObjectPath"],
                json!("/Game/Blueprints/Items/BPChild")
            );
        }

        /// Blueprint `BP{i}` whose parent is `BP{i + 1}`, holding property
        /// `Level{i}`.
        fn chained_asset(i: usize) -> Value {
            json!({
                "imports": [
                    {"object_name": format!("BP{}_C", i + 1), "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -2},
                    {"object_name": format!("/Game/Blueprints/Items/BP{}", i + 1), "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": format!("BP{i}_C"), "class_index": 0, "super_index": -1, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": format!("Default__BP{i}_C"), "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {format!("Level{i}"): i},
                    },
                ],
            })
        }

        #[test]
        fn stops_following_a_parent_chain_at_max_depth() {
            let chain = 10_000;
            let assets: HashMap<String, Value> = (1..chain)
                .map(|i| (format!("/Game/Blueprints/Items/BP{i}"), chained_asset(i)))
                .collect();
            let loader = map_loader(assets);

            let result = extract_catalog_data(
                "/Game/Blueprints/Items/BP0",
                &chained_asset(0),
                false,
                &loader,
            )
            .expect("expected catalog data");

            assert_eq!(result["Level0"], json!(0));
            let last = MAX_PARENT_DEPTH - 1;
            assert_eq!(result[format!("Level{last}")], json!(last));
            assert!(result.get(format!("Level{MAX_PARENT_DEPTH}")).is_none());
        }

        #[test]
        fn multi_level_inheritance_chain_merges_all_ancestors() {
            let grandparent_asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "BPGrandparent_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__BPGrandparent_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"GrandparentProperty": 1, "SharedProperty": "grandparent"},
                    },
                ],
            });
            let parent_asset = json!({
                "imports": [
                    {"object_name": "BPGrandparent_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -2},
                    {"object_name": "/Game/Blueprints/Items/BPGrandparent", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": "BPParent_C", "class_index": 0, "super_index": -1, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__BPParent_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"ParentProperty": 2, "SharedProperty": "parent"},
                    },
                ],
            });
            let child_asset = json!({
                "imports": [
                    {"object_name": "BPParent_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -2},
                    {"object_name": "/Game/Blueprints/Items/BPParent", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": "BPChild_C", "class_index": 0, "super_index": -1, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__BPChild_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"CodeName": "ChildItem"},
                    },
                ],
            });

            let mut assets = HashMap::new();
            assets.insert("/Game/Blueprints/Items/BPParent".to_string(), parent_asset);
            assets.insert(
                "/Game/Blueprints/Items/BPGrandparent".to_string(),
                grandparent_asset,
            );
            let loader = map_loader(assets);

            let result = extract_catalog_data(
                "/Game/Blueprints/Items/BPChild",
                &child_asset,
                false,
                &loader,
            )
            .expect("expected catalog data");

            assert_eq!(result["CodeName"], json!("ChildItem"));
            assert_eq!(result["GrandparentProperty"], json!(1));
            assert_eq!(result["ParentProperty"], json!(2));
            // Nearest ancestor (parent) wins over grandparent for the shared key.
            assert_eq!(result["SharedProperty"], json!("parent"));
            assert_eq!(
                result["ParentBlueprint"],
                json!("/Game/Blueprints/Items/BPParent")
            );
        }
    }

    mod full_extraction_tests {
        use super::*;

        #[test]
        fn resolves_export_references_to_object_names() {
            let asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "SomeComponent", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {"object_name": "Test_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__Test_C", "class_index": 2, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"Component": "Reference: 1"},
                    },
                ],
            });
            let result =
                extract_catalog_data("/Game/Blueprints/Test", &asset, true, &no_loader).unwrap();
            assert_eq!(result["Component"], json!("SomeComponent"));
        }

        #[test]
        fn falls_back_to_raw_import_asset_path_resolution() {
            let asset = json!({
                "imports": [
                    {"object_name": "MyIcon", "class_name": "Texture2D", "class_package": "/Script/Engine", "outer_index": -2},
                    {"object_name": "/Game/Textures/Icons", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": "Test_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__Test_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"Icon": "Reference: -1"},
                    },
                ],
            });
            let result =
                extract_catalog_data("/Game/Blueprints/Test", &asset, true, &no_loader).unwrap();
            assert!(result.get("Icon").is_some());
            assert_ne!(result["Icon"], Value::Null);
        }

        #[test]
        fn handles_list_references_skipping_null_and_resolving_both_kinds() {
            let asset = json!({
                "imports": [
                    {"object_name": "BPItem_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -2},
                    {"object_name": "/Game/Blueprints/Items/BPItem", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                ],
                "exports": [
                    {"object_name": "Component1", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {"object_name": "Test_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__Test_C", "class_index": 2, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"Items": ["Reference: -1", "Reference: 1", "Reference: 0"]},
                    },
                ],
            });
            let result =
                extract_catalog_data("/Game/Blueprints/Test", &asset, true, &no_loader).unwrap();
            let items = result["Items"].as_array().unwrap();
            assert_eq!(items.len(), 2);
            assert!(items.contains(&json!("/Game/Blueprints/Items/BPItem")));
            assert!(items.contains(&json!("Component1")));
        }
    }

    mod simple_mode_tests {
        use super::*;

        #[test]
        fn skips_empty_struct_results_but_keeps_non_empty_scalars() {
            let asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "Test_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__Test_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"EmptyStruct": {}, "NonEmpty": 42},
                    },
                ],
            });
            let result =
                extract_catalog_data("/Game/Blueprints/Test", &asset, false, &no_loader).unwrap();
            assert!(result.get("EmptyStruct").is_none());
            assert_eq!(result["NonEmpty"], json!(42));
        }
    }

    mod compute_object_path_tests {
        use super::*;

        #[test]
        fn strips_extraction_root_and_extension() {
            let blueprints_dir = Path::new("/data/War/Content/Blueprints");
            let asset_path = Path::new("/data/War/Content/Blueprints/Items/BPFoo.uasset");
            assert_eq!(
                compute_object_path(blueprints_dir, asset_path),
                "War/Content/Blueprints/Items/BPFoo"
            );
        }

        #[test]
        fn falls_back_to_absolute_path_when_not_under_root() {
            let blueprints_dir = Path::new("/data/War/Content/Blueprints");
            let asset_path = Path::new("/elsewhere/BPFoo.uasset");
            assert_eq!(
                compute_object_path(blueprints_dir, asset_path),
                "/elsewhere/BPFoo.uasset"
            );
        }
    }

    mod find_blueprint_path_tests {
        use super::*;

        #[test]
        fn returns_direct_path_when_it_exists() {
            let tmp = tempfile_dir();
            let blueprints_dir = tmp.join("Blueprints");
            std::fs::create_dir_all(blueprints_dir.join("Items")).unwrap();
            let direct = blueprints_dir.join("Items/BPFoo.uasset");
            std::fs::write(&direct, b"x").unwrap();

            assert_eq!(find_blueprint_path(&blueprints_dir, &direct), Some(direct));
        }

        #[test]
        fn searches_one_level_of_subdirectories() {
            let tmp = tempfile_dir();
            let blueprints_dir = tmp.join("Blueprints");
            std::fs::create_dir_all(blueprints_dir.join("Structures/Emplacements")).unwrap();
            let actual = blueprints_dir.join("Structures/Emplacements/BPTurret.uasset");
            std::fs::write(&actual, b"x").unwrap();

            let requested = blueprints_dir.join("Structures/BPTurret.uasset");
            assert_eq!(
                find_blueprint_path(&blueprints_dir, &requested),
                Some(actual)
            );
        }

        #[test]
        fn returns_none_when_not_found_anywhere() {
            let tmp = tempfile_dir();
            let blueprints_dir = tmp.join("Blueprints");
            std::fs::create_dir_all(&blueprints_dir).unwrap();
            let requested = blueprints_dir.join("Missing/BPGone.uasset");
            assert_eq!(find_blueprint_path(&blueprints_dir, &requested), None);
        }

        #[test]
        fn blueprint_candidate_maps_object_path_into_blueprints_dir() {
            let blueprints_dir = Path::new("/x/Blueprints");
            assert_eq!(
                blueprint_candidate(blueprints_dir, "/Game/Blueprints/Items/BPFoo"),
                Some(blueprints_dir.join("Items/BPFoo.uasset"))
            );
            assert_eq!(
                blueprint_candidate(blueprints_dir, "/Game/Other/BPFoo"),
                None
            );
        }

        #[test]
        fn blueprint_candidate_refuses_paths_that_escape() {
            let blueprints_dir = Path::new("/x/Blueprints");
            for object_path in [
                "/Game/Blueprints/../../etc/passwd",
                "/Game/Blueprints/Items/..\\..\\secret",
                "/Game/Blueprints/C:/Windows/x",
            ] {
                assert_eq!(blueprint_candidate(blueprints_dir, object_path), None);
            }
            // A doubled prefix keeps the absolute-looking rest inside the dir
            // instead of letting `join` replace it.
            assert_eq!(
                blueprint_candidate(blueprints_dir, "/Game/Blueprints//Game/Foo/X"),
                Some(blueprints_dir.join("Game/Foo/X.uasset"))
            );
        }

        #[cfg(unix)]
        #[test]
        fn does_not_follow_a_symlink_loop() {
            let tmp = tempfile_dir();
            let blueprints_dir = tmp.join("Blueprints");
            std::fs::create_dir_all(blueprints_dir.join("Structures/Sub")).unwrap();
            std::os::unix::fs::symlink(
                blueprints_dir.join("Structures"),
                blueprints_dir.join("Structures/Sub/loop"),
            )
            .unwrap();

            let requested = blueprints_dir.join("Structures/BPGone.uasset");
            assert_eq!(find_blueprint_path(&blueprints_dir, &requested), None);
        }

        fn tempfile_dir() -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "fs-assets-blueprint-parser-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
    }
}
