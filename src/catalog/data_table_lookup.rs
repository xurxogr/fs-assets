//! Data table lookup over parsed `.uasset` data tables.
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
//! For a `kind == "data_table"` export, `rows` is already a `CodeName ->
//! processed-property-map` dict — `convert::property_to_value` has already
//! walked every row's properties (struct/array/enum/etc.) while parsing, so
//! no per-property extraction is needed here; a row lookup is just an index
//! into `rows`.
//!
//! "Profile tables" are a plain blueprint's `Default__` export with a
//! `Map`-typed property (e.g. `ItemProfileTable`), which
//! `convert::property_to_value`'s `MapProperty` arm (see that module) emits
//! as `[[key, value], ...]` pairs with both sides already fully processed.
//!
//! `ObjectProperty` values show up as the `"Reference: N"` sentinel string
//! described in `convert::property_to_value`. Row and profile lookups
//! ([`get`], [`get_profile`]) resolve import references to asset paths (e.g.
//! the curve assets in `BPVehicleMovementProfileList`) and drop null ones;
//! [`get_ammo_dynamic_data`] keeps them raw because `DamageType` is resolved
//! separately by [`resolve_damage_type_import`].
//!
//! Assets are loaded through a [`crate::catalog::blueprint_parser::AssetLoader`]
//! closure keyed by whatever name identifies them to the caller (a data
//! table's file stem, or a factory blueprint's path relative to the
//! Blueprints dir), so tests can inject an in-memory lookup. No cross-call
//! cache is kept by any function in this module; memoizing repeated loads
//! across a whole catalog build is the caller's responsibility, e.g. by
//! giving the loader closure its own cache.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::catalog::import_resolution::{resolve_import_asset_path, to_pascalcase_import};
use crate::catalog::utils::{import_at, load_full_json, parse_reference, resolve_import_path};

// -- Data table file names (relative to the Data directory, no extension:
// these are the `.uasset` file stems parsed via
// `crate::convert::uasset_to_full_json`).

pub const ITEM_DYNAMIC_DATA: &str = "BPItemDynamicData";
pub const ITEM_PROFILE_TABLE: &str = "BPItemProfileTable";
pub const AMMO_DYNAMIC_DATA: &str = "BPAmmoDynamicData";
pub const WEAPON_DYNAMIC_DATA: &str = "BPWeaponDynamicData";
pub const GRENADE_DYNAMIC_DATA: &str = "BPGrenadeDynamicData";
pub const MELEE_DYNAMIC_DATA: &str = "BPMeleeDynamicData";
pub const STRUCTURE_DYNAMIC_DATA: &str = "BPStructureDynamicData";
/// Note: List, not Table — matches the game's asset name.
pub const STRUCTURE_PROFILE_TABLE: &str = "BPStructureProfileList";
pub const VEHICLE_DYNAMIC_DATA: &str = "BPVehicleDynamicData";
/// Note: List, not Table — matches the game's asset name.
pub const VEHICLE_PROFILE_TABLE: &str = "BPVehicleProfileList";
/// Note: List, not Table — matches the game's asset name.
pub const VEHICLE_MOVEMENT_PROFILE: &str = "BPVehicleMovementProfileList";
pub const SHIP_DYNAMIC_DATA: &str = "BPShipDynamicData";

/// Factory blueprint paths, relative to the Blueprints folder (not Data).
pub const FACTORY_BLUEPRINT: &str = "Structures/BPFactory";
pub const MASS_PRODUCTION_BLUEPRINT: &str = "Structures/BPMassProduction";

// ---------------------------------------------------------------------
// Data table row lookup
// ---------------------------------------------------------------------

fn find_data_table_export(asset: &Value) -> Option<&Value> {
    asset
        .get("exports")?
        .as_array()?
        .iter()
        .find(|e| e.get("kind").and_then(Value::as_str) == Some("data_table"))
}

/// ObjectPath for a data table (`War/Content/Blueprints/Data/{table_name}`).
pub fn table_object_path(table_name: &str) -> String {
    format!("War/Content/Blueprints/Data/{table_name}")
}

/// Clone `row` into an owned map with `ObjectPath` added (a non-object row
/// yields a map containing only `ObjectPath`).
fn with_object_path(row: &Value, table_name: &str) -> Value {
    let mut map = row.as_object().cloned().unwrap_or_default();
    map.insert(
        "ObjectPath".to_string(),
        json!(table_object_path(table_name)),
    );
    Value::Object(map)
}

/// Look up a row by exact `CodeName` in a data table asset. Returns `None`
/// if `asset` has no `data_table`-kind export, or the export has no such row.
pub fn get(asset: &Value, table_name: &str, key: &str) -> Option<Value> {
    let row = find_data_table_export(asset)?.get("rows")?.get(key)?;
    Some(with_object_path(
        &resolve_references(row, &asset_imports(asset)),
        table_name,
    ))
}

/// Look up a value from a "profile table": a plain blueprint's `Default__`
/// export with a `Map`-typed property (see module docs). `map_name` is the
/// name of that Map property (e.g. `"ItemProfileTable"`).
pub fn get_profile(asset: &Value, table_name: &str, map_name: &str, key: &str) -> Option<Value> {
    let table = parse_profile_table(asset, map_name);
    let imports = asset_imports(asset);
    table
        .get(key)
        .map(|v| with_object_path(&resolve_references(v, &imports), table_name))
}

/// The asset's imports, adapted to the PascalCase shape the import
/// resolution helpers expect.
fn asset_imports(asset: &Value) -> Vec<Value> {
    asset
        .get("imports")
        .and_then(Value::as_array)
        .map(|imports| imports.iter().map(to_pascalcase_import).collect())
        .unwrap_or_default()
}

/// Recursively replace `"Reference: N"` sentinels with the referenced
/// asset path (`War/Content/....0`). Null references (`0`) are dropped,
/// except as a side of a map's `[key, value]` pair, where they become
/// `null`; references that don't point at a game asset are kept as-is.
fn resolve_references(value: &Value, imports: &[Value]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter_map(|(k, v)| resolve_reference_value(v, imports).map(|v| (k.clone(), v)))
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .filter_map(|v| match v {
                    // A list in a list is one `[key, value]` pair of a map:
                    // a null reference stays as `null` so the pair keeps
                    // both sides.
                    Value::Array(pair) => Some(Value::Array(
                        pair.iter()
                            .map(|side| {
                                resolve_reference_value(side, imports).unwrap_or(Value::Null)
                            })
                            .collect(),
                    )),
                    v => resolve_reference_value(v, imports),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Resolve one value; `None` when it is a null reference to drop.
fn resolve_reference_value(value: &Value, imports: &[Value]) -> Option<Value> {
    let Some(index) = value.as_str().and_then(parse_reference) else {
        return Some(resolve_references(value, imports));
    };
    if index == 0 {
        return None;
    }
    let path =
        import_at(index, imports).and_then(|import| resolve_import_asset_path(import, imports));
    Some(path.map(Value::String).unwrap_or_else(|| value.clone()))
}

fn find_default_export(asset: &Value) -> Option<&Value> {
    asset.get("exports")?.as_array()?.iter().find(|e| {
        e.get("object_name")
            .and_then(Value::as_str)
            .map(|s| s.starts_with("Default__"))
            .unwrap_or(false)
    })
}

/// Parse a profile table's Map property into a `key -> processed value`
/// dict. `convert::property_to_value`'s `MapProperty` arm has already
/// reduced the Map to `[[key, value], ...]` pairs with both sides fully
/// processed, so this only collects them.
pub fn parse_profile_table(asset: &Value, map_name: &str) -> HashMap<String, Value> {
    let mut result = HashMap::new();

    let Some(export) = find_default_export(asset) else {
        return result;
    };
    let Some(props) = export.get("properties").and_then(Value::as_object) else {
        return result;
    };
    let Some(map_value) = props.get(map_name).and_then(Value::as_array) else {
        return result;
    };

    for entry in map_value {
        let Some(pair) = entry.as_array() else {
            continue;
        };
        if pair.len() != 2 {
            continue;
        }
        let key = match &pair[0] {
            Value::String(s) if !s.is_empty() => s.clone(),
            _ => continue,
        };
        result.insert(key, pair[1].clone());
    }

    result
}

/// Get `AmmoDynamicData` for an ammo type by `CodeName`, case-insensitively
/// (component blueprints may use different casing than the data table,
/// e.g. `RPGAmmo` vs `RpgAmmo`). An exact match is preferred over a
/// case-insensitive one.
pub fn get_ammo_dynamic_data(asset: &Value, code_name: &str) -> Option<Value> {
    if code_name.is_empty() {
        return None;
    }

    let rows = find_data_table_export(asset)?.get("rows")?.as_object()?;

    if let Some(row) = rows.get(code_name) {
        return Some(with_object_path(row, AMMO_DYNAMIC_DATA));
    }

    let lower = code_name.to_lowercase();
    for (k, v) in rows {
        if k.to_lowercase() == lower {
            return Some(with_object_path(v, AMMO_DYNAMIC_DATA));
        }
    }

    None
}

/// Resolve a `DamageType` import reference for an ammo type to its
/// blueprint path. `ammo_asset` is the already-loaded `BPAmmoDynamicData`
/// asset (in `uasset_to_full_json` shape) — its own `imports` array is used
/// for resolution.
pub fn resolve_damage_type_import(ammo_asset: &Value, code_name: &str) -> Option<String> {
    let ammo_data = get_ammo_dynamic_data(ammo_asset, code_name)?;

    let damage_type_ref = ammo_data.get("DamageType")?.as_str()?;
    let ref_idx = parse_reference(damage_type_ref)?;
    if ref_idx >= 0 {
        return None;
    }

    let imports = ammo_asset.get("imports")?.as_array()?;
    let adapted: Vec<Value> = imports.iter().map(to_pascalcase_import).collect();
    resolve_import_path(ref_idx, &adapted)
}

// ---------------------------------------------------------------------
// Production categories (SpecializedFactoryComponent walk)
// ---------------------------------------------------------------------

fn find_specialized_factory_export(asset: &Value) -> Option<&Value> {
    asset.get("exports")?.as_array()?.iter().find(|e| {
        e.get("object_name")
            .and_then(Value::as_str)
            .map(|s| s.contains("SpecializedFactoryComponent"))
            .unwrap_or(false)
    })
}

/// Parse `CodeName -> QueueType` from one factory blueprint's
/// `SpecializedFactoryComponent` export's `ProductionCategories` property.
///
/// `uasset_to_full_json` has already flattened
/// `ProductionCategories` (an array of structs) into
/// `[{"Type": ..., "CategoryItems": [{"CodeName": ...}, ...]}, ...]`
/// via `convert::property_to_value`'s `ArrayProperty`/`StructProperty`
/// handling, so this only reads `Type` and each item's `CodeName`.
pub fn parse_factory_production_categories(asset: &Value) -> HashMap<String, String> {
    let mut result = HashMap::new();

    let Some(export) = find_specialized_factory_export(asset) else {
        return result;
    };
    let Some(props) = export.get("properties").and_then(Value::as_object) else {
        return result;
    };
    let Some(categories) = props.get("ProductionCategories").and_then(Value::as_array) else {
        return result;
    };

    for category in categories {
        let Some(cat_map) = category.as_object() else {
            continue;
        };
        let Some(cat_type) = cat_map.get("Type").and_then(Value::as_str) else {
            continue;
        };
        let Some(items) = cat_map.get("CategoryItems").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            if let Some(code_name) = item.get("CodeName").and_then(Value::as_str) {
                result.insert(code_name.to_string(), cat_type.to_string());
            }
        }
    }

    result
}

/// Load and merge production categories from the Factory and
/// MassProductionFactory blueprints, keyed by lowercase `CodeName`, each
/// mapping to `{"Factory": QueueType}` and/or `{"MassProductionFactory":
/// QueueType}`. Takes already-loaded assets rather than owning a cache —
/// see the module docs for why.
pub fn load_production_categories(
    factory_asset: Option<&Value>,
    mass_production_asset: Option<&Value>,
) -> HashMap<String, HashMap<String, String>> {
    let mut result: HashMap<String, HashMap<String, String>> = HashMap::new();

    if let Some(asset) = factory_asset {
        for (code_name, queue_type) in parse_factory_production_categories(asset) {
            result
                .entry(code_name.to_lowercase())
                .or_default()
                .insert("Factory".to_string(), queue_type);
        }
    }

    if let Some(asset) = mass_production_asset {
        for (code_name, queue_type) in parse_factory_production_categories(asset) {
            result
                .entry(code_name.to_lowercase())
                .or_default()
                .insert("MassProductionFactory".to_string(), queue_type);
        }
    }

    result
}

/// Get production categories for an item by `CodeName` (case-insensitive),
/// from an already-built `load_production_categories` map.
pub fn get_production_categories<'a>(
    categories: &'a HashMap<String, HashMap<String, String>>,
    code_name: &str,
) -> Option<&'a HashMap<String, String>> {
    if code_name.is_empty() {
        return None;
    }
    categories.get(&code_name.to_lowercase())
}

// ---------------------------------------------------------------------
// Filesystem-backed loaders
// ---------------------------------------------------------------------

/// Builds a filesystem-backed
/// [`AssetLoader`](crate::catalog::blueprint_parser::AssetLoader) for data
/// tables: maps a table name (e.g. `"BPAmmoDynamicData"`, no extension) to
/// `<data_dir>/<name>.uasset`, parsed via
/// `crate::convert::uasset_to_full_json`. Returns `None` if the file doesn't
/// exist or fails to parse (the latter with a warning).
pub fn filesystem_data_loader(data_dir: &Path) -> impl Fn(&str) -> Option<Value> + '_ {
    move |table_name: &str| load_full_json(data_dir, &data_dir.join(format!("{table_name}.uasset")))
}

/// Builds a filesystem-backed
/// [`AssetLoader`](crate::catalog::blueprint_parser::AssetLoader) for factory
/// blueprints: maps a path relative to the Blueprints dir (e.g.
/// `"Structures/BPFactory"`, no extension — see
/// [`FACTORY_BLUEPRINT`]/[`MASS_PRODUCTION_BLUEPRINT`]) to
/// `<blueprints_dir>/<relative>.uasset`, parsed via
/// `crate::convert::uasset_to_full_json`. Returns `None` if the file doesn't
/// exist or fails to parse (the latter with a warning).
pub fn filesystem_blueprint_loader(blueprints_dir: &Path) -> impl Fn(&str) -> Option<Value> + '_ {
    move |relative: &str| {
        load_full_json(
            blueprints_dir,
            &blueprints_dir.join(format!("{relative}.uasset")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_null_reference_in_a_map_pair_keeps_its_place() {
        let value = json!({
            "List": ["a", "Reference: 0", "b"],
            "Map": [["Rifle", "Reference: 0"], ["Pistol", 2]],
        });

        assert_eq!(
            resolve_references(&value, &[]),
            json!({"List": ["a", "b"], "Map": [["Rifle", null], ["Pistol", 2]]})
        );
    }

    #[test]
    fn an_out_of_range_reference_is_kept_as_is() {
        let value = json!(format!("Reference: {}", i64::MIN));
        assert_eq!(resolve_reference_value(&value, &[]), Some(value.clone()));
    }

    fn data_table_asset(rows: Value) -> Value {
        json!({
            "imports": [],
            "exports": [
                {
                    "object_name": "BPWeaponDynamicData",
                    "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "data_table",
                    "properties": {},
                    "rows": rows,
                }
            ],
        })
    }

    mod get_tests {
        use super::*;

        #[test]
        fn returns_none_for_missing_row() {
            let asset = data_table_asset(json!({}));
            assert_eq!(get(&asset, "BPWeaponDynamicData", "NonexistentKey"), None);
        }

        #[test]
        fn returns_row_data_for_existing_key() {
            let asset = data_table_asset(json!({
                "RifleItem": {"Damage": 25.0, "Range": 100, "bIsAutomatic": false},
                "PistolItem": {"Damage": 15.0, "Range": 50, "bIsAutomatic": false},
            }));
            let result = get(&asset, "BPWeaponDynamicData", "RifleItem").unwrap();
            assert_eq!(result["Damage"], json!(25.0));
            assert_eq!(result["Range"], json!(100));
            assert_eq!(result["bIsAutomatic"], json!(false));
        }

        #[test]
        fn includes_object_path() {
            let asset = data_table_asset(json!({"RifleItem": {"Damage": 25.0}}));
            let result = get(&asset, "BPWeaponDynamicData", "RifleItem").unwrap();
            assert_eq!(
                result["ObjectPath"],
                json!("War/Content/Blueprints/Data/BPWeaponDynamicData")
            );
        }

        #[test]
        fn returns_none_when_no_data_table_export() {
            let asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "SomeOther", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}}
                ],
            });
            assert_eq!(get(&asset, "BPWeaponDynamicData", "AnyKey"), None);
        }

        #[test]
        fn returns_none_for_empty_exports() {
            let asset = json!({"imports": [], "exports": []});
            assert_eq!(get(&asset, "BPWeaponDynamicData", "AnyKey"), None);
        }

        #[test]
        fn nested_struct_and_array_properties_pass_through_already_processed() {
            // uasset_to_full_json already flattens struct/array properties;
            // there's nothing left for this module to do but pass them
            // through unchanged.
            let asset = data_table_asset(json!({
                "TestItem": {
                    "Stats": {"Damage": 25.0, "Range": 100},
                    "Tags": ["weapon", "rifle"],
                }
            }));
            let result = get(&asset, "TestTable", "TestItem").unwrap();
            assert_eq!(result["Stats"], json!({"Damage": 25.0, "Range": 100}));
            assert_eq!(result["Tags"], json!(["weapon", "rifle"]));
        }

        #[test]
        fn object_property_reference_sentinel_passes_through_raw() {
            // ObjectProperty values show up as "Reference: N" sentinel
            // strings (see convert::property_to_value), not raw ints;
            // downstream resolution (e.g.
            // resolve_damage_type_import) is responsible for parsing them.
            let asset = data_table_asset(json!({
                "TestAmmo": {"DamageType": "Reference: -5"}
            }));
            let result = get(&asset, "TestTable", "TestAmmo").unwrap();
            assert_eq!(result["DamageType"], json!("Reference: -5"));
        }
    }

    mod get_profile_tests {
        use super::*;

        fn profile_asset(map_name: &str, entries: Value) -> Value {
            json!({
                "imports": [],
                "exports": [
                    {
                        "object_name": "Default__BPItemProfileTable_C",
                        "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": { map_name: entries },
                    }
                ],
            })
        }

        #[test]
        fn returns_none_for_missing_table() {
            let asset = json!({"imports": [], "exports": []});
            assert_eq!(
                get_profile(&asset, "BPItemProfileTable", "ItemProfileTable", "SomeKey"),
                None
            );
        }

        #[test]
        fn returns_data_for_existing_key() {
            let asset = profile_asset(
                "ItemProfileTable",
                json!([
                    ["EItemProfileType::Rifle", {"Weight": 5.0, "Stackable": false}],
                    ["EItemProfileType::Pistol", {"Weight": 2.0, "Stackable": false}],
                ]),
            );
            let result = get_profile(
                &asset,
                "BPItemProfileTable",
                "ItemProfileTable",
                "EItemProfileType::Rifle",
            )
            .unwrap();
            assert_eq!(result["Weight"], json!(5.0));
            assert_eq!(result["Stackable"], json!(false));
        }

        #[test]
        fn includes_object_path() {
            let asset = profile_asset(
                "ItemProfileTable",
                json!([["EItemProfileType::Rifle", {"Weight": 5.0}]]),
            );
            let result = get_profile(
                &asset,
                "BPItemProfileTable",
                "ItemProfileTable",
                "EItemProfileType::Rifle",
            )
            .unwrap();
            assert!(result.get("ObjectPath").is_some());
        }

        #[test]
        fn returns_none_for_non_default_export() {
            let asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "NotDefault", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {"TestMap": []}}
                ],
            });
            assert_eq!(get_profile(&asset, "T", "TestMap", "K"), None);
        }

        #[test]
        fn returns_none_for_wrong_map_name() {
            let asset = profile_asset("WrongMapName", json!([]));
            assert_eq!(get_profile(&asset, "T", "CorrectMapName", "SomeKey"), None);
        }

        #[test]
        fn skips_invalid_map_entries() {
            let asset = profile_asset("TestMap", json!(["not a pair", ["Key1"], ["Key2", {}],]));
            let result = get_profile(&asset, "T", "TestMap", "Key2");
            assert!(result.is_some());
        }

        #[test]
        fn skips_entries_with_empty_key() {
            let asset = profile_asset(
                "TestMap",
                json!([
                    ["", {"Ignored": true}],
                    ["ValidKey", {"Kept": true}],
                ]),
            );
            assert!(get_profile(&asset, "T", "TestMap", "").is_none());
            assert!(get_profile(&asset, "T", "TestMap", "ValidKey").is_some());
        }
    }

    mod reference_resolution_tests {
        use super::*;

        fn curve_imports() -> Value {
            json!([
                {"object_name": "/Game/Blueprints/Vehicles/EngineCurves/TruckEngineCurve", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                {"object_name": "TruckEngineCurve", "class_name": "CurveFloat", "class_package": "/Script/Engine", "outer_index": -1},
                {"object_name": "CurveFloat", "class_name": "Class", "class_package": "/Script/CoreUObject", "outer_index": 0},
            ])
        }

        #[test]
        fn get_resolves_import_reference_to_asset_path() {
            let mut asset = data_table_asset(
                json!({"Truck": {"EngineCurve": "Reference: -2", "Gears": ["Reference: -2"]}}),
            );
            asset["imports"] = curve_imports();
            let result = get(&asset, "T", "Truck").unwrap();
            let path = json!("War/Content/Blueprints/Vehicles/EngineCurves/TruckEngineCurve.0");
            assert_eq!(result["EngineCurve"], path);
            assert_eq!(result["Gears"], json!([path]));
        }

        #[test]
        fn get_profile_resolves_nested_import_reference() {
            let asset = json!({
                "imports": curve_imports(),
                "exports": [{
                    "object_name": "Default__BPVehicleMovementProfileList_C",
                    "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "normal",
                    "properties": {"Map": [["Truck", {"Movement": {"FrontGripCurve": "Reference: -2"}}]]},
                }],
            });
            let result = get_profile(&asset, "T", "Map", "Truck").unwrap();
            assert_eq!(
                result["Movement"]["FrontGripCurve"],
                json!("War/Content/Blueprints/Vehicles/EngineCurves/TruckEngineCurve.0")
            );
        }

        #[test]
        fn null_reference_is_dropped() {
            let asset =
                data_table_asset(json!({"Truck": {"LowGearCurve": "Reference: 0", "Speed": 5}}));
            let result = get(&asset, "T", "Truck").unwrap();
            assert!(result.get("LowGearCurve").is_none());
            assert_eq!(result["Speed"], json!(5));
        }

        #[test]
        fn unresolvable_reference_is_kept() {
            let mut asset = data_table_asset(
                json!({"Truck": {"Kind": "Reference: -3", "Missing": "Reference: -9"}}),
            );
            asset["imports"] = curve_imports();
            let result = get(&asset, "T", "Truck").unwrap();
            assert_eq!(result["Kind"], json!("Reference: -3"));
            assert_eq!(result["Missing"], json!("Reference: -9"));
        }

        #[test]
        fn ammo_damage_type_reference_stays_raw() {
            let mut asset = data_table_asset(json!({"RifleAmmo": {"DamageType": "Reference: -2"}}));
            asset["imports"] = curve_imports();
            let result = get_ammo_dynamic_data(&asset, "RifleAmmo").unwrap();
            assert_eq!(result["DamageType"], json!("Reference: -2"));
        }
    }

    mod get_ammo_dynamic_data_tests {
        use super::*;

        #[test]
        fn returns_none_for_missing_table() {
            let asset = json!({"imports": [], "exports": []});
            assert_eq!(get_ammo_dynamic_data(&asset, "SomeAmmo"), None);
        }

        #[test]
        fn returns_none_for_empty_codename() {
            let asset = data_table_asset(json!({"RpgAmmo": {"Damage": 500.0}}));
            assert_eq!(get_ammo_dynamic_data(&asset, ""), None);
        }

        #[test]
        fn performs_case_insensitive_lookup() {
            let asset = data_table_asset(json!({"RpgAmmo": {"Damage": 500.0}}));
            let result = get_ammo_dynamic_data(&asset, "RPGAmmo").unwrap();
            assert_eq!(result["Damage"], json!(500.0));
        }

        #[test]
        fn exact_match_takes_priority() {
            let asset = data_table_asset(json!({
                "RpgAmmo": {"Damage": 1.0},
                "rpgammo": {"Damage": 2.0},
            }));
            let result = get_ammo_dynamic_data(&asset, "RpgAmmo").unwrap();
            assert_eq!(result["Damage"], json!(1.0));
        }
    }

    mod resolve_damage_type_import_tests {
        use super::*;

        #[test]
        fn returns_none_for_missing_ammo() {
            let asset = json!({"imports": [], "exports": []});
            assert_eq!(resolve_damage_type_import(&asset, "NonexistentAmmo"), None);
        }

        #[test]
        fn returns_none_for_non_negative_damage_type() {
            let asset = data_table_asset(json!({
                "TestAmmo": {"DamageType": "Reference: 5"}
            }));
            assert_eq!(resolve_damage_type_import(&asset, "TestAmmo"), None);
        }

        #[test]
        fn returns_none_for_null_damage_type_reference() {
            let asset = data_table_asset(json!({
                "TestAmmo": {"DamageType": "Reference: 0"}
            }));
            assert_eq!(resolve_damage_type_import(&asset, "TestAmmo"), None);
        }

        #[test]
        fn resolves_damage_type_path_from_imports() {
            let asset = json!({
                "imports": [
                    {"object_name": "/Game/Blueprints/DamageTypes", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                    {"object_name": "BPExplosive_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -1},
                ],
                "exports": [
                    {
                        "object_name": "BPAmmoDynamicData",
                        "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "data_table",
                        "properties": {},
                        "rows": {
                            "TestAmmo": {"DamageType": "Reference: -2"},
                        },
                    }
                ],
            });
            let result = resolve_damage_type_import(&asset, "TestAmmo");
            assert_eq!(
                result,
                Some("/Game/Blueprints/DamageTypes/BPExplosive_C".to_string())
            );
        }
    }

    mod production_categories_tests {
        use super::*;

        fn factory_asset(entries: Value) -> Value {
            json!({
                "imports": [],
                "exports": [
                    {
                        "object_name": "SpecializedFactoryComponent",
                        "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"ProductionCategories": entries},
                    }
                ],
            })
        }

        #[test]
        fn parses_factory_blueprint_categories() {
            let asset = factory_asset(json!([
                {
                    "Type": "EFactoryQueueType::Weapons",
                    "CategoryItems": [
                        {"CodeName": "RifleW"},
                        {"CodeName": "PistolW"},
                    ],
                }
            ]));
            let categories = parse_factory_production_categories(&asset);
            assert_eq!(
                categories.get("RifleW"),
                Some(&"EFactoryQueueType::Weapons".to_string())
            );
            assert_eq!(
                categories.get("PistolW"),
                Some(&"EFactoryQueueType::Weapons".to_string())
            );
        }

        #[test]
        fn returns_empty_when_no_specialized_factory_export() {
            let asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "SomethingElse", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}}
                ],
            });
            assert!(parse_factory_production_categories(&asset).is_empty());
        }

        #[test]
        fn get_production_categories_returns_none_for_unknown_item() {
            let categories = HashMap::new();
            assert_eq!(get_production_categories(&categories, "UnknownItem"), None);
        }

        #[test]
        fn load_production_categories_merges_factory_and_mass_production() {
            let factory = factory_asset(json!([
                {"Type": "EFactoryQueueType::Weapons", "CategoryItems": [{"CodeName": "RifleW"}]}
            ]));
            let mass_production = factory_asset(json!([
                {"Type": "EFactoryQueueType::Materials", "CategoryItems": [{"CodeName": "MaterialsItem"}]}
            ]));

            let categories = load_production_categories(Some(&factory), Some(&mass_production));

            let rifle = get_production_categories(&categories, "rifleW").unwrap();
            assert_eq!(
                rifle.get("Factory"),
                Some(&"EFactoryQueueType::Weapons".to_string())
            );

            let materials = get_production_categories(&categories, "MaterialsItem").unwrap();
            assert_eq!(
                materials.get("MassProductionFactory"),
                Some(&"EFactoryQueueType::Materials".to_string())
            );
        }

        #[test]
        fn load_production_categories_handles_missing_assets() {
            let categories = load_production_categories(None, None);
            assert!(categories.is_empty());
        }

        #[test]
        fn case_insensitive_lookup() {
            let factory = factory_asset(json!([
                {"Type": "EFactoryQueueType::Weapons", "CategoryItems": [{"CodeName": "RifleW"}]}
            ]));
            let categories = load_production_categories(Some(&factory), None);
            assert!(get_production_categories(&categories, "RIFLEW").is_some());
        }
    }

    mod filesystem_loader_tests {
        use super::*;
        use std::path::PathBuf;

        fn tempfile_dir(label: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "fs-assets-data-table-lookup-test-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn data_loader_returns_none_for_missing_file() {
            let dir = tempfile_dir("data");
            let loader = filesystem_data_loader(&dir);
            assert!(loader("BPNonexistentTable").is_none());
        }

        #[test]
        fn blueprint_loader_returns_none_for_missing_file() {
            let dir = tempfile_dir("blueprints");
            let loader = filesystem_blueprint_loader(&dir);
            assert!(loader("Structures/BPFactory").is_none());
        }
    }
}
