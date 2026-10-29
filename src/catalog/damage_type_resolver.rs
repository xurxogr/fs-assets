//! Resolution of ammo codes and `DamageType` blueprints for catalog entries.
//!
//! This module holds no state of its own and works on the same
//! `crate::convert::uasset_to_full_json` shape that
//! [`crate::catalog::data_table_lookup`] and
//! [`crate::catalog::blueprint_parser`] understand. It is orchestration and
//! text-resolution glue on top of those two modules (plus
//! [`crate::catalog::localization::LocalizationLookup`] for GUID text
//! lookups). There is no name -> category mapping table; the only fixed list
//! is [`build_damage_type_result`]'s `SIMPLE_PROPERTY_KEYS` (the scalar/flag
//! properties copied verbatim).
//!
//! - [`get_ammo_code`] takes the loaded `BPAmmoDynamicData` asset explicitly
//!   and delegates to
//!   [`crate::catalog::data_table_lookup::DataTable::get_ignoring_case`].
//! - [`resolve_damage_type`] passes the resolved `/Game/Blueprints/...`
//!   object path straight to
//!   [`crate::catalog::blueprint_parser::Blueprints::extract_path`].

use serde_json::{json, Map, Value};

use crate::catalog::blueprint_parser::Blueprints;
use crate::catalog::data_table_lookup::DataTable;
use crate::catalog::localization::{self, LocalizationLookup};
use crate::catalog::utils::{
    blueprint_object_path, extract_localized_text, is_truthy, normalize_object_path,
    value_to_display_string,
};

/// Determine the ammo code for resolving `DamageType`.
///
/// `data` is a blueprint's catalog data dict (e.g. from
/// `blueprint_parser::extract_catalog_data`). `ammo_table` is the
/// `BPAmmoDynamicData` table, used to check "is this item itself an ammo
/// type" via [`DataTable::get_ignoring_case`].
///
/// Cases are checked in this order:
/// 1. The item's own `CodeName` has an `AmmoDynamicData` entry.
/// 2. A single-entry `MultiAmmo` list.
/// 3. `CompatibleAmmoCodeName`.
/// 4. `ProjectileClass.ExplosiveCodeName`.
pub fn get_ammo_code(data: &Map<String, Value>, ammo_table: &DataTable) -> Option<String> {
    let code_name = data.get("CodeName").and_then(Value::as_str).unwrap_or("");

    // Case 1: Item itself is ammo (has AmmoDynamicData entry).
    if ammo_table.get_ignoring_case(code_name).is_some() {
        return Some(code_name.to_string());
    }

    let item_comp = data.get("ItemComponentClass")?.as_object()?;

    // Case 2: Single ammo type from MultiAmmo.
    if let Some(multi_ammo) = item_comp.get("MultiAmmo").and_then(Value::as_array) {
        if multi_ammo.len() == 1 {
            return Some(value_to_display_string(&multi_ammo[0]));
        }
    }

    // Case 3: CompatibleAmmoCodeName (single ammo weapons).
    if let Some(compat_ammo) = item_comp.get("CompatibleAmmoCodeName") {
        if is_truthy(compat_ammo) {
            return Some(value_to_display_string(compat_ammo));
        }
    }

    // Case 4: ProjectileClass.ExplosiveCodeName.
    if let Some(proj_class) = item_comp.get("ProjectileClass").and_then(Value::as_object) {
        if let Some(explosive) = proj_class.get("ExplosiveCodeName") {
            if is_truthy(explosive) {
                return Some(value_to_display_string(explosive));
            }
        }
    }

    None
}

/// Scalar/flag properties copied verbatim from a `DamageType` blueprint's
/// extracted data into the catalog's resolved `DamageType`. Not a
/// name->category mapping table — just the set of keys worth surfacing.
const SIMPLE_PROPERTY_KEYS: &[&str] = &[
    "Type",
    "Icon",
    "VehicleSubsystemDisableMultipliers",
    "bApplyDamageFalloff",
    "bCanWoundCharacter",
    "bExposeInUI",
    "bAlwaysAppliesBleeding",
    "bCanRuinStructures",
    // Tank armour related.
    "TankArmourPenetrationFactor",
    "TankArmourEffectType",
    "bApplyTankArmourMechanics",
    "bApplyTankArmourAngleRangeBonuses",
];

/// A damage type's `DescriptionDetails` lines with text, joined with
/// newlines, and the translations of those with a localization key, per
/// language and joined the same way (`None` when no line has one); `None`
/// when no line has text.
fn description_details(
    lines: &[Value],
    loc_service: &LocalizationLookup,
) -> Option<(String, Option<Map<String, Value>>)> {
    let mut texts = Vec::new();
    let mut guids = Vec::new();
    for (text, guid) in lines.iter().filter_map(description_line) {
        if text.is_empty() {
            continue;
        }
        // A GUID text is the line's localization key, so its translations
        // are looked up by it too.
        if localization::is_guid(&text) {
            texts.push(loc_service.get_with_fallback(&text, None));
            guids.push(text);
        } else {
            // Already resolved text (from CultureInvariantString).
            texts.push(text);
            guids.extend(guid);
        }
    }
    if texts.is_empty() {
        return None;
    }

    let translations: Vec<_> = guids
        .iter()
        .map(|guid| loc_service.get_all_languages(guid))
        .collect();
    let languages: std::collections::BTreeSet<&String> =
        translations.iter().flat_map(|t| t.keys()).collect();
    let locales: Map<String, Value> = languages
        .into_iter()
        .map(|language| {
            let lines: Vec<&str> = translations
                .iter()
                .filter_map(|t| t.get(language).map(String::as_str))
                .collect();
            (language.clone(), json!(lines.join("\n")))
        })
        .collect();
    Some((texts.join("\n"), (!locales.is_empty()).then_some(locales)))
}

/// One `DescriptionDetails` line: its text, and the `Guid` a nested
/// `{"Text": {"Text": "...", "Guid": "..."}}` one has; `None` for a line
/// that isn't text at all.
fn description_line(line: &Value) -> Option<(String, Option<String>)> {
    match line {
        Value::Object(m) => Some(match m.get("Text") {
            Some(Value::Object(inner)) => (
                inner
                    .get("Text")
                    .map(value_to_display_string)
                    .unwrap_or_default(),
                inner.get("Guid").and_then(Value::as_str).map(String::from),
            ),
            Some(text) if is_truthy(text) => (value_to_display_string(text), None),
            _ => (String::new(), None),
        }),
        Value::String(s) => Some((s.clone(), None)),
        _ => None,
    }
}

/// Build a resolved `DamageType` dict from blueprint data.
///
/// `dt_data` is the extracted damage type blueprint data (e.g. from
/// `blueprint_parser::extract_catalog_data`). `object_path` is its
/// `/Game/Blueprints/...` object path, written normalized as `ObjectPath`
/// (e.g. `War/Content/Blueprints/DamageTypes/BPFoo.0`). `loc_service`
/// resolves GUIDs in `DisplayName`/`DescriptionDetails`.
pub fn build_damage_type_result(
    dt_data: &Value,
    object_path: &str,
    loc_service: &LocalizationLookup,
) -> Value {
    let object_path = normalize_object_path(object_path);

    let mut resolved = Map::new();
    resolved.insert("ObjectPath".to_string(), json!(object_path));

    // Copy relevant properties.
    for key in SIMPLE_PROPERTY_KEYS {
        if let Some(value) = dt_data.get(*key) {
            let mut value = value.clone();
            // Simplify Icon to just the ResourceObject path.
            if *key == "Icon" {
                if let Some(resource_object) =
                    value.as_object().and_then(|m| m.get("ResourceObject"))
                {
                    value = resource_object.clone();
                }
            }
            resolved.insert((*key).to_string(), value);
        }
    }

    // Resolve DisplayName.
    if let Some(display_name) = dt_data.get("DisplayName") {
        if let Some(text) = extract_localized_text(display_name) {
            if !text.is_empty() {
                if localization::is_guid(&text) {
                    resolved.insert(
                        "DisplayName".to_string(),
                        json!(loc_service.get_with_fallback(&text, None)),
                    );
                } else {
                    resolved.insert("DisplayName".to_string(), json!(text));
                }
            }
        }
    }

    // Resolve DescriptionDetails (array of tooltip texts joined with newlines).
    let description = dt_data
        .get("DescriptionDetails")
        .and_then(Value::as_array)
        .and_then(|lines| description_details(lines, loc_service));
    let description_details_set = description.is_some();
    if let Some((text, locales)) = description {
        resolved.insert("DescriptionDetails".to_string(), json!(text));
        if let Some(locales) = locales {
            resolved.insert(
                "DescriptionDetailsLocales".to_string(),
                Value::Object(locales),
            );
        }
    }

    // Generate DescriptionDetails from boolean properties if not already set.
    if !description_details_set {
        let mut generated_texts: Vec<&str> = Vec::new();
        if dt_data
            .get("bBreachesBunkers")
            .map(is_truthy)
            .unwrap_or(false)
        {
            generated_texts.push("Always has a chance to breach bunkers");
        }
        if !generated_texts.is_empty() {
            resolved.insert(
                "DescriptionDetails".to_string(),
                json!(generated_texts.join("\n")),
            );
        }
    }

    Value::Object(resolved)
}

/// Resolve `DamageType` import reference to a full object.
///
/// `ammo_code` is the ammo `CodeName` to resolve `DamageType` for.
/// `ammo_table` is the `BPAmmoDynamicData` table the `DamageType` import is
/// resolved through (see [`DataTable::resolve_damage_type_import`]). The
/// `DamageType`
/// blueprint is extracted from `blueprints` by its `/Game/Blueprints/...`
/// object path, mirroring `blueprint_parser::filesystem_loader`'s convention.
///
/// `/Script/` (native class) paths short-circuit to `{"Type": ...}`;
/// `/Game/` paths get the `_C`-suffix / duplicate-last-segment cleanup
/// applied before loading.
pub fn resolve_damage_type(
    ammo_code: &str,
    ammo_table: &DataTable,
    blueprints: &Blueprints,
    loc_service: &LocalizationLookup,
) -> Option<Value> {
    // Get the damage type path from the ammo data table.
    let damage_type_path = ammo_table.resolve_damage_type_import(ammo_code)?;

    // Handle Script paths (C++ classes) - return as Type field.
    if let Some(_rest) = damage_type_path.strip_prefix("/Script/") {
        let parts: Vec<&str> = damage_type_path.split('/').collect();
        if parts.len() >= 3 {
            return Some(json!({"Type": format!("/Script/{}", parts[2])}));
        }
        return Some(json!({"Type": damage_type_path}));
    }

    // The blueprint's object path: only a segment's trailing `_C` (the
    // generated-class suffix) goes, a name that merely contains `_C`, like
    // `_Cluster`, is kept whole; and `.../BPFoo/BPFoo` becomes `.../BPFoo`.
    let mut segments: Vec<&str> = damage_type_path
        .split('/')
        .map(|segment| segment.strip_suffix("_C").unwrap_or(segment))
        .collect();
    if segments.len() >= 2 && segments[segments.len() - 1] == segments[segments.len() - 2] {
        segments.pop();
    }
    let object_path = blueprint_object_path(&segments.join("/"))?;
    let dt_data = Value::Object(blueprints.extract_path(&object_path)?);

    Some(build_damage_type_result(
        &dt_data,
        &object_path,
        loc_service,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::data_table_lookup::AMMO_DYNAMIC_DATA;
    use std::collections::HashMap;

    fn ammo_asset(rows: Value) -> Value {
        json!({
            "imports": [],
            "exports": [
                {
                    "object_name": "BPAmmoDynamicData",
                    "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "data_table",
                    "properties": {},
                    "rows": rows,
                }
            ],
        })
    }

    fn loc_service() -> LocalizationLookup {
        let tmp = std::env::temp_dir().join(format!(
            "fs-assets-damage-type-resolver-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        LocalizationLookup::new(&tmp, None)
    }

    mod get_ammo_code_tests {
        use super::*;

        fn ammo_code(data: &Value, ammo: &Value) -> Option<String> {
            get_ammo_code(
                data.as_object().unwrap(),
                &DataTable::new(ammo, AMMO_DYNAMIC_DATA),
            )
        }

        #[test]
        fn item_itself_is_ammo() {
            let ammo = ammo_asset(json!({"RifleAmmo": {"Damage": 5}}));
            let data = json!({"CodeName": "RifleAmmo"});
            assert_eq!(ammo_code(&data, &ammo), Some("RifleAmmo".to_string()));
        }

        #[test]
        fn no_item_component_class_returns_none() {
            let ammo = ammo_asset(json!({}));
            assert_eq!(ammo_code(&json!({"CodeName": "Foo"}), &ammo), None);
            assert_eq!(
                ammo_code(
                    &json!({"CodeName": "Foo", "ItemComponentClass": "not-a-dict"}),
                    &ammo
                ),
                None
            );
        }

        #[test]
        fn single_multi_ammo_entry() {
            let ammo = ammo_asset(json!({}));
            let data =
                json!({"CodeName": "Weapon", "ItemComponentClass": {"MultiAmmo": ["RifleAmmo"]}});
            assert_eq!(ammo_code(&data, &ammo), Some("RifleAmmo".to_string()));
        }

        #[test]
        fn multiple_multi_ammo_entries_not_used() {
            let ammo = ammo_asset(json!({}));
            let data = json!({
                "CodeName": "Weapon",
                "ItemComponentClass": {"MultiAmmo": ["A", "B"]},
            });
            assert_eq!(ammo_code(&data, &ammo), None);
        }

        #[test]
        fn compatible_ammo_code_name() {
            let ammo = ammo_asset(json!({}));
            let data = json!({
                "CodeName": "Weapon",
                "ItemComponentClass": {"CompatibleAmmoCodeName": "RifleAmmo"},
            });
            assert_eq!(ammo_code(&data, &ammo), Some("RifleAmmo".to_string()));
        }

        #[test]
        fn projectile_class_explosive_code_name() {
            let ammo = ammo_asset(json!({}));
            let data = json!({
                "CodeName": "Weapon",
                "ItemComponentClass": {"ProjectileClass": {"ExplosiveCodeName": "RPGExplosive"}},
            });
            assert_eq!(ammo_code(&data, &ammo), Some("RPGExplosive".to_string()));
        }

        #[test]
        fn projectile_class_without_explosive_code_name() {
            let ammo = ammo_asset(json!({}));
            let data = json!({
                "CodeName": "Weapon",
                "ItemComponentClass": {"ProjectileClass": {"Speed": 500}},
            });
            assert_eq!(ammo_code(&data, &ammo), None);
        }

        #[test]
        fn no_matching_case_returns_none() {
            let ammo = ammo_asset(json!({}));
            let data = json!({"CodeName": "Weapon", "ItemComponentClass": {"SomethingElse": true}});
            assert_eq!(ammo_code(&data, &ammo), None);
        }

        #[test]
        fn empty_compatible_ammo_code_name_is_falsy() {
            let ammo = ammo_asset(json!({}));
            let data = json!({
                "CodeName": "Weapon",
                "ItemComponentClass": {"CompatibleAmmoCodeName": ""},
            });
            assert_eq!(ammo_code(&data, &ammo), None);
        }
    }

    mod build_damage_type_result_tests {
        use super::*;

        #[test]
        fn builds_object_path_from_bp_path() {
            let loc = loc_service();
            let result = build_damage_type_result(
                &json!({}),
                "/Game/Blueprints/DamageTypes/BPKinetic",
                &loc,
            );
            assert!(result.get("ObjectPath").is_some());
            assert_eq!(
                result["ObjectPath"],
                json!("War/Content/Blueprints/DamageTypes/BPKinetic.0")
            );
        }

        #[test]
        fn copies_known_properties() {
            let loc = loc_service();
            let dt_data = json!({"Type": "Kinetic", "bCanWoundCharacter": true});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert_eq!(result["Type"], json!("Kinetic"));
            assert_eq!(result["bCanWoundCharacter"], json!(true));
        }

        #[test]
        fn icon_simplified_to_resource_object() {
            let loc = loc_service();
            let dt_data = json!({"Icon": {"ResourceObject": "/Game/Icons/Foo"}});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert_eq!(result["Icon"], json!("/Game/Icons/Foo"));
        }

        #[test]
        fn display_name_plain_text_used_as_is() {
            let loc = loc_service();
            let dt_data = json!({"DisplayName": "Kinetic Damage"});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert_eq!(result["DisplayName"], json!("Kinetic Damage"));
        }

        #[test]
        fn display_name_guid_resolved_with_fallback_to_guid_itself() {
            // No locres files loaded, so get_with_fallback falls back to the
            // GUID string itself -- still exercises the is_guid branch.
            let loc = loc_service();
            let guid = "8BB336F4459740A6ADA7B28B2D91748B";
            let dt_data = json!({"DisplayName": guid});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert_eq!(result["DisplayName"], json!(guid));
        }

        #[test]
        fn description_details_string_items() {
            let loc = loc_service();
            let dt_data = json!({"DescriptionDetails": ["Line one", "Line two"]});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert_eq!(result["DescriptionDetails"], json!("Line one\nLine two"));
        }

        #[test]
        fn description_details_dict_item_plain_text() {
            let loc = loc_service();
            let dt_data = json!({"DescriptionDetails": [{"Text": "Plain text"}]});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert_eq!(result["DescriptionDetails"], json!("Plain text"));
        }

        #[test]
        fn description_details_skips_unrecognized_items() {
            let loc = loc_service();
            let dt_data = json!({"DescriptionDetails": [123, ""]});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert!(result.get("DescriptionDetails").is_none());
        }

        #[test]
        fn description_details_guid_items_build_locales() {
            // Build a real locres fixture so is_guid + get_all_languages
            // exercise the full GUID-resolution/locales path end-to-end.
            let tmp = std::env::temp_dir().join(format!(
                "fs-assets-damage-type-resolver-locres-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let guid = "8BB336F4459740A6ADA7B28B2D91748B";
            write_mock_locres(
                &tmp.join("Foxhole-Content/en/test.locres"),
                guid,
                "English text",
            );
            write_mock_locres(
                &tmp.join("Foxhole-Content/fr/test.locres"),
                guid,
                "French text",
            );

            let loc = LocalizationLookup::new(&tmp, None);
            let dt_data = json!({"DescriptionDetails": [guid]});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);

            assert_eq!(result["DescriptionDetails"], json!("English text"));
            assert_eq!(
                result["DescriptionDetailsLocales"],
                json!({"en": "English text", "fr": "French text"})
            );
        }

        #[test]
        fn nested_description_lines_are_translated_once_and_empty_ones_skipped() {
            let tmp = tempfile::tempdir().unwrap();
            let guid = "8BB336F4459740A6ADA7B28B2D91748B";
            let empty_guid = "0BB336F4459740A6ADA7B28B2D91748B";
            for (language, text) in [("en", "English text"), ("fr", "French text")] {
                write_mock_locres(
                    &tmp.path()
                        .join(format!("Foxhole-Content/{language}/test.locres")),
                    guid,
                    text,
                );
            }

            let loc = LocalizationLookup::new(tmp.path(), None);
            let dt_data = json!({"DescriptionDetails": [
                {"Text": {"Text": guid, "Guid": guid}},
                {"Text": {"Text": "", "Guid": empty_guid}},
            ]});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);

            assert_eq!(result["DescriptionDetails"], json!("English text"));
            assert_eq!(
                result["DescriptionDetailsLocales"],
                json!({"en": "English text", "fr": "French text"})
            );
        }

        #[test]
        fn description_details_generated_from_breaches_bunkers() {
            let loc = loc_service();
            let dt_data = json!({"bBreachesBunkers": true});
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert_eq!(
                result["DescriptionDetails"],
                json!("Always has a chance to breach bunkers")
            );
        }

        #[test]
        fn no_description_details_when_nothing_applies() {
            let loc = loc_service();
            let result =
                build_damage_type_result(&json!({}), "/Game/Blueprints/DamageTypes/BP", &loc);
            assert!(result.get("DescriptionDetails").is_none());
        }

        #[test]
        fn breaches_bunkers_ignored_when_description_details_already_set() {
            let loc = loc_service();
            let dt_data = json!({
                "DescriptionDetails": ["Explicit line"],
                "bBreachesBunkers": true,
            });
            let result =
                build_damage_type_result(&dt_data, "/Game/Blueprints/DamageTypes/BP", &loc);
            assert_eq!(result["DescriptionDetails"], json!("Explicit line"));
        }

        /// Writes a `.locres` holding the single `guid` -> `text` pair.
        fn write_mock_locres(filepath: &std::path::Path, guid: &str, text: &str) {
            std::fs::create_dir_all(filepath.parent().unwrap()).unwrap();
            let document = crate::locres::LocresDocument {
                version: 2,
                namespaces: vec![crate::locres::LocresNamespace {
                    namespace_hash: 0,
                    namespace: String::new(),
                    entries: vec![crate::locres::LocresEntry {
                        key_hash: 0,
                        key: guid.to_string(),
                        source_hash: 0,
                        text: text.to_string(),
                    }],
                }],
            };
            crate::locres::write(&document, filepath).unwrap();
        }
    }

    mod resolve_damage_type_tests {
        use super::*;

        fn no_loader(_: &str) -> Option<Value> {
            None
        }

        fn map_loader(assets: HashMap<String, Value>) -> impl Fn(&str) -> Option<Value> {
            move |path: &str| assets.get(path).cloned()
        }

        #[test]
        fn no_damage_type_path_returns_none() {
            let ammo = ammo_asset(json!({}));
            let loc = loc_service();
            let result = resolve_damage_type(
                "RifleAmmo",
                &DataTable::new(&ammo, AMMO_DYNAMIC_DATA),
                &Blueprints::new(&no_loader, false),
                &loc,
            );
            assert_eq!(result, None);
        }

        #[test]
        fn script_path_multi_segment_returns_type() {
            let ammo = json!({
                "imports": [
                    {"object_name": "/Script/War", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                    {"object_name": "DamageType", "class_name": "Class", "class_package": "/Script/CoreUObject", "outer_index": -1},
                ],
                "exports": [
                    {
                        "object_name": "BPAmmoDynamicData",
                        "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "data_table",
                        "properties": {},
                        "rows": {"RifleAmmo": {"DamageType": "Reference: -2"}},
                    }
                ],
            });
            let loc = loc_service();
            let result = resolve_damage_type(
                "RifleAmmo",
                &DataTable::new(&ammo, AMMO_DYNAMIC_DATA),
                &Blueprints::new(&no_loader, false),
                &loc,
            );
            assert_eq!(result, Some(json!({"Type": "/Script/War"})));
        }

        #[test]
        fn game_path_missing_blueprint_returns_none() {
            let ammo = json!({
                "imports": [
                    {"object_name": "/Game/Blueprints/DamageTypes", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                    {"object_name": "BPKinetic_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -1},
                ],
                "exports": [
                    {
                        "object_name": "BPAmmoDynamicData",
                        "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "data_table",
                        "properties": {},
                        "rows": {"RifleAmmo": {"DamageType": "Reference: -2"}},
                    }
                ],
            });
            let loc = loc_service();
            let result = resolve_damage_type(
                "RifleAmmo",
                &DataTable::new(&ammo, AMMO_DYNAMIC_DATA),
                &Blueprints::new(&no_loader, false),
                &loc,
            );
            assert_eq!(result, None);
        }

        #[test]
        fn game_path_resolves_full_result() {
            let ammo = json!({
                "imports": [
                    {"object_name": "/Game/Blueprints/DamageTypes", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                    {"object_name": "BPKinetic_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -1},
                ],
                "exports": [
                    {
                        "object_name": "BPAmmoDynamicData",
                        "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "data_table",
                        "properties": {},
                        "rows": {"RifleAmmo": {"DamageType": "Reference: -2"}},
                    }
                ],
            });

            let dt_asset = json!({
                "imports": [],
                "exports": [
                    {"object_name": "BPKinetic_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                    {
                        "object_name": "Default__BPKinetic_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"Type": "Kinetic"},
                    },
                ],
            });
            let mut assets = HashMap::new();
            assets.insert(
                "/Game/Blueprints/DamageTypes/BPKinetic".to_string(),
                dt_asset,
            );
            let loader = map_loader(assets);

            let loc = loc_service();
            let result = resolve_damage_type(
                "RifleAmmo",
                &DataTable::new(&ammo, AMMO_DYNAMIC_DATA),
                &Blueprints::new(&loader, false),
                &loc,
            )
            .unwrap();
            assert_eq!(result["Type"], json!("Kinetic"));
            assert_eq!(
                result["ObjectPath"],
                json!("War/Content/Blueprints/DamageTypes/BPKinetic.0")
            );
        }

        #[test]
        fn dedupes_trailing_duplicate_path_segment_and_strips_c_suffix() {
            // When the last two path segments are equal they collapse: a `.../BPKinetic_C/BPKinetic_C` import path (post `_C` strip:
            // `.../BPKinetic/BPKinetic`) collapses to `.../BPKinetic`.
            let ammo = json!({
                "imports": [
                    {"object_name": "/Game/Blueprints/DamageTypes/BPKinetic_C", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                    {"object_name": "BPKinetic_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -1},
                ],
                "exports": [
                    {
                        "object_name": "BPAmmoDynamicData",
                        "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "data_table",
                        "properties": {},
                        "rows": {"RifleAmmo": {"DamageType": "Reference: -2"}},
                    }
                ],
            });

            let dt_asset = json!({
                "imports": [],
                "exports": [
                    {
                        "object_name": "Default__BPKinetic_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"Type": "Kinetic"},
                    },
                ],
            });
            let mut assets = HashMap::new();
            assets.insert(
                "/Game/Blueprints/DamageTypes/BPKinetic".to_string(),
                dt_asset,
            );
            let loader = map_loader(assets);

            let loc = loc_service();
            let result = resolve_damage_type(
                "RifleAmmo",
                &DataTable::new(&ammo, AMMO_DYNAMIC_DATA),
                &Blueprints::new(&loader, false),
                &loc,
            )
            .unwrap();
            assert_eq!(result["Type"], json!("Kinetic"));
        }

        #[test]
        fn keeps_c_inside_a_name_and_only_strips_the_class_suffix() {
            let ammo = json!({
                "imports": [
                    {"object_name": "/Game/Blueprints/DamageTypes/BPShrapnel_Cluster", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                    {"object_name": "BPShrapnel_Cluster_C", "class_name": "BlueprintGeneratedClass", "class_package": "/Script/Engine", "outer_index": -1},
                ],
                "exports": [
                    {
                        "object_name": "BPAmmoDynamicData",
                        "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "data_table",
                        "properties": {},
                        "rows": {"ClusterAmmo": {"DamageType": "Reference: -2"}},
                    }
                ],
            });

            let dt_asset = json!({
                "imports": [],
                "exports": [
                    {
                        "object_name": "Default__BPShrapnel_Cluster_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                        "kind": "normal",
                        "properties": {"Type": "Shrapnel"},
                    },
                ],
            });
            let mut assets = HashMap::new();
            assets.insert(
                "/Game/Blueprints/DamageTypes/BPShrapnel_Cluster".to_string(),
                dt_asset,
            );
            let loader = map_loader(assets);

            let loc = loc_service();
            let result = resolve_damage_type(
                "ClusterAmmo",
                &DataTable::new(&ammo, AMMO_DYNAMIC_DATA),
                &Blueprints::new(&loader, false),
                &loc,
            )
            .unwrap();
            assert_eq!(result["Type"], json!("Shrapnel"));
        }
    }
}
