//! Catalog assembly.
//!
//! This module is the final wiring step of the catalog-builder pipeline: it
//! combines the outputs of three services —
//! [`crate::catalog::blueprint_parser`] (inheritance-merged blueprint
//! extraction), [`crate::catalog::data_table_lookup`] (data-table row/profile
//! lookups and production categories) and
//! [`crate::catalog::damage_type_resolver`] (ammo-code/`DamageType`
//! resolution) — plus [`crate::catalog::localization::LocalizationLookup`]
//! for GUID text resolution, into a single per-blueprint catalog entry, and
//! then filters/cleans/sorts a batch of those into the final catalog list.
//!
//! Like those three modules, everything here is a pure function over
//! `&serde_json::Value` plus loader closures/refs — there is no stateful
//! assembler object owning parsers, caches or running stats. Preloaded
//! data-table assets are passed
//! in via [`DataTables`]/[`AssemblerContext`] instead of being lazily loaded
//! and cached by a long-lived service object. The referenced blueprints are
//! loaded through one [`Blueprints`], which extracts each once for the whole
//! catalog build.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::catalog::blueprint_parser::Blueprints;
use crate::catalog::damage_type_resolver;
use crate::catalog::data_table_lookup;
use crate::catalog::localization::{self, LocalizationLookup};
use crate::catalog::stockpile_classifier::{self, StockpileCategory};
use crate::catalog::utils::{
    blueprint_object_path, extract_localized_text, is_truthy, normalize_object_path,
};

/// Base path for hardcoded subtype icons (see [`add_subtype_icon`]).
const SUBTYPE_ICONS_BASE: &str = "War/Content/Textures/UI/ItemIcons/";

/// Fields eligible for GUID-key extraction / locale enrichment, used by both
/// [`assemble_catalog_entry`]'s GUID-extraction step and [`enrich_locales`].
pub const LOCALIZABLE_FIELDS: &[&str] = &["DisplayName", "Description", "ChassisName"];

/// Data-table/profile-table assets used by [`expand_data`], preloaded (in
/// `uasset_to_full_json` shape) by the caller. `None` means "table not
/// available" and causes that lookup to be silently skipped.
#[derive(Default)]
pub struct DataTables<'a> {
    pub item_dynamic_data: Option<&'a Value>,
    pub weapon_dynamic_data: Option<&'a Value>,
    pub grenade_dynamic_data: Option<&'a Value>,
    pub melee_dynamic_data: Option<&'a Value>,
    pub structure_dynamic_data: Option<&'a Value>,
    pub vehicle_dynamic_data: Option<&'a Value>,
    pub ship_dynamic_data: Option<&'a Value>,
    pub ammo_dynamic_data: Option<&'a Value>,
    pub item_profile_table: Option<&'a Value>,
    pub structure_profile_table: Option<&'a Value>,
    pub vehicle_profile_table: Option<&'a Value>,
    pub vehicle_movement_profile: Option<&'a Value>,
}

/// Everything [`assemble_catalog_entry`] needs beyond the blueprint asset
/// itself: the data tables, the merged production-categories map (see
/// `data_table_lookup::load_production_categories`), the localization
/// service, and the referenced blueprints.
pub struct AssemblerContext<'a> {
    pub blueprints: &'a Blueprints<'a>,
    pub tables: DataTables<'a>,
    pub production_categories: &'a HashMap<String, HashMap<String, String>>,
    pub loc_service: &'a LocalizationLookup,
}

// ---------------------------------------------------------------------
// Content filtering
// ---------------------------------------------------------------------

/// Validate required fields and keep entries that can be stored in a
/// stockpile, either loose or in crate form (as resolved by
/// [`assemble_catalog_entry`]).
pub fn filter_blueprint_by_content(data: &Value) -> bool {
    let required_fields = ["CodeName", "DisplayName", "Icon", "Description"];
    if !required_fields
        .iter()
        .all(|field| data.get(*field).map(is_truthy).unwrap_or(false))
    {
        return false;
    }

    data.get("bIsStockpilable") == Some(&Value::Bool(true))
        || data.get("bIsCratable") == Some(&Value::Bool(true))
}

// ---------------------------------------------------------------------
// Cleaning helpers
// ---------------------------------------------------------------------

/// Detect the empty `ResourceAmounts` pattern:
/// `{"OtherResources": [], "Resource": {"CodeName": "None", "Quantity": 0}}`.
pub fn is_empty_resource_amounts(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };

    match map.get("OtherResources") {
        Some(Value::Array(a)) if a.is_empty() => {}
        _ => return false,
    }

    let Some(resource) = map.get("Resource").and_then(Value::as_object) else {
        return false;
    };

    resource.get("CodeName").and_then(Value::as_str) == Some("None")
        && resource.get("Quantity").and_then(Value::as_f64) == Some(0.0)
}

/// Remove empty/default `AltResourceAmounts` from a data-table entry.
pub fn clean_data_table_entry(entry: &Value) -> Value {
    let Some(map) = entry.as_object() else {
        return entry.clone();
    };

    let mut cleaned = Map::new();
    for (key, value) in map {
        if key == "AltResourceAmounts" && is_empty_resource_amounts(value) {
            continue;
        }
        cleaned.insert(key.clone(), value.clone());
    }
    Value::Object(cleaned)
}

/// Remove `None`/null values and normalize `/Game/` paths to `War/Content/`
/// format, recursively, and turn string-keyed maps (serialized as
/// `[[key, value], ...]`) into objects.
pub fn clean_entry(data: &Value) -> Value {
    let Some(map) = data.as_object() else {
        return data.clone();
    };

    let mut result = Map::new();
    for (key, value) in map {
        if value.is_null() {
            continue;
        }
        result.insert(key.clone(), clean_value(value));
    }
    Value::Object(result)
}

fn clean_value(value: &Value) -> Value {
    match value {
        Value::String(s) if s.starts_with("/Game/") => json!(normalize_object_path(s)),
        Value::Object(_) => clean_entry(value),
        Value::Array(items) if is_string_keyed_map(items) => Value::Object(
            items
                .iter()
                .filter_map(Value::as_array)
                .filter_map(|pair| Some((pair[0].as_str()?.to_string(), clean_value(&pair[1]))))
                .collect(),
        ),
        Value::Array(items) => {
            let mut cleaned = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Object(_) => cleaned.push(clean_entry(item)),
                    Value::String(s) if s.starts_with("/Game/") => {
                        cleaned.push(json!(normalize_object_path(s)))
                    }
                    other => cleaned.push(other.clone()),
                }
            }
            Value::Array(cleaned)
        }
        other => other.clone(),
    }
}

/// Whether a list is a serialized map with string keys: a non-empty list of
/// `[key, value]` pairs. Asset arrays cannot nest, so a list of lists only
/// comes from a map.
fn is_string_keyed_map(items: &[Value]) -> bool {
    !items.is_empty()
        && items.iter().all(
            |item| matches!(item.as_array(), Some(pair) if pair.len() == 2 && pair[0].is_string()),
        )
}

// ---------------------------------------------------------------------
// Locale enrichment
// ---------------------------------------------------------------------

/// Enrich fields with all available locales (in-place): for each field with a
/// corresponding `*Key` GUID field, add a `*Locales` field with translations
/// in all available languages, then remove the `*Key` field. An invalid
/// (non-GUID) key field is left in place (only ever removed once resolution actually
/// happens, or if `*Locales` already existed).
pub fn enrich_locales(
    data: &mut Map<String, Value>,
    fields: &[&str],
    loc_service: &LocalizationLookup,
) {
    for field in fields {
        let key_field = format!("{field}Key");
        let locales_field = format!("{field}Locales");

        if data.contains_key(&locales_field) {
            data.remove(&key_field);
            continue;
        }

        let Some(guid) = data
            .get(&key_field)
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if !localization::is_guid(&guid) {
            continue;
        }

        let translations = loc_service.get_all_languages(&guid);
        if !translations.is_empty() {
            data.insert(locales_field, json!(translations));
        }
        data.remove(&key_field);
    }
}

// ---------------------------------------------------------------------
// Data-table / referenced-blueprint expansion
// ---------------------------------------------------------------------

/// Expand a catalog entry with data-table lookups and the referenced
/// `ItemComponentClass` sub-blueprint. The data-table/profile-table lookups
/// run in a fixed order.
pub fn expand_data(
    data: &mut Map<String, Value>,
    code_name: &str,
    tables: &DataTables,
    blueprints: &Blueprints,
) {
    if code_name.is_empty() {
        return;
    }

    let dynamic_data_lookups: [(&str, Option<&Value>, &str); 7] = [
        (
            "ItemDynamicData",
            tables.item_dynamic_data,
            data_table_lookup::ITEM_DYNAMIC_DATA,
        ),
        (
            "WeaponDynamicData",
            tables.weapon_dynamic_data,
            data_table_lookup::WEAPON_DYNAMIC_DATA,
        ),
        (
            "GrenadeDynamicData",
            tables.grenade_dynamic_data,
            data_table_lookup::GRENADE_DYNAMIC_DATA,
        ),
        (
            "MeleeDynamicData",
            tables.melee_dynamic_data,
            data_table_lookup::MELEE_DYNAMIC_DATA,
        ),
        (
            "StructureDynamicData",
            tables.structure_dynamic_data,
            data_table_lookup::STRUCTURE_DYNAMIC_DATA,
        ),
        (
            "VehicleDynamicData",
            tables.vehicle_dynamic_data,
            data_table_lookup::VEHICLE_DYNAMIC_DATA,
        ),
        (
            "ShipDynamicData",
            tables.ship_dynamic_data,
            data_table_lookup::SHIP_DYNAMIC_DATA,
        ),
    ];
    for (field_name, asset, table_name) in dynamic_data_lookups {
        if let Some(asset) = asset {
            if let Some(row) = data_table_lookup::get(asset, table_name, code_name) {
                data.insert(field_name.to_string(), clean_data_table_entry(&row));
            }
        }
    }

    // Ammo data (case-insensitive lookup).
    if let Some(ammo_asset) = tables.ammo_dynamic_data {
        if let Some(ammo_data) = data_table_lookup::get_ammo_dynamic_data(ammo_asset, code_name) {
            data.insert("AmmoDynamicData".to_string(), ammo_data);
        }
    }

    // Profile table lookups by profile type.
    let profile_lookups: [(&str, &str, Option<&Value>, &str, &str); 4] = [
        (
            "ItemProfileData",
            "ItemProfileType",
            tables.item_profile_table,
            data_table_lookup::ITEM_PROFILE_TABLE,
            "ItemProfileTable",
        ),
        (
            "ProfileData",
            "ProfileType",
            tables.structure_profile_table,
            data_table_lookup::STRUCTURE_PROFILE_TABLE,
            "StructureProfileMap",
        ),
        (
            "VehicleProfileData",
            "VehicleProfileType",
            tables.vehicle_profile_table,
            data_table_lookup::VEHICLE_PROFILE_TABLE,
            "VehicleProfileMap",
        ),
        (
            "VehicleMovementProfileData",
            "VehicleMovementProfileType",
            tables.vehicle_movement_profile,
            data_table_lookup::VEHICLE_MOVEMENT_PROFILE,
            "VehicleMovementProfileMap",
        ),
    ];
    for (field_name, type_field, asset, table_name, map_name) in profile_lookups {
        let Some(profile_type) = data.get(type_field).and_then(Value::as_str) else {
            continue;
        };
        if profile_type.is_empty() {
            continue;
        }
        if let Some(asset) = asset {
            if let Some(profile_data) =
                data_table_lookup::get_profile(asset, table_name, map_name, profile_type)
            {
                data.insert(field_name.to_string(), profile_data);
            }
        }
    }

    enrich_item_component_class(data, blueprints);
}

/// Enrich `ItemComponentClass` with all properties from the referenced
/// component sub-blueprint (in-place), including resolving `ProjectileClass`
/// for single-ammo weapons.
///
/// [`Blueprints::extract_path`] returns its own copy of what it remembers, so
/// merging it in can't alter what later lookups get.
pub fn enrich_item_component_class(data: &mut Map<String, Value>, blueprints: &Blueprints) {
    let Some(item_comp_raw) = data.get("ItemComponentClass").cloned() else {
        return;
    };

    // The component's path, as `blueprint_object_path` takes it, from either
    // a `/Game/Blueprints/...` reference or a normalized
    // `War/Content/Blueprints/....0` object path.
    let (mut item_comp, component_path) = match &item_comp_raw {
        Value::String(obj_path) => {
            let mut m = Map::new();
            m.insert("ObjectPath".to_string(), json!(obj_path));
            (m, obj_path.clone())
        }
        Value::Object(m) => {
            let Some(obj_path) = m.get("ObjectPath").and_then(Value::as_str) else {
                return;
            };
            if obj_path.is_empty() {
                return;
            }
            let relative_path = obj_path
                .strip_prefix("War/Content/Blueprints/")
                .unwrap_or(obj_path);
            let relative_path = relative_path
                .strip_suffix(".0")
                .unwrap_or(relative_path)
                .to_string();
            (m.clone(), relative_path)
        }
        _ => return,
    };

    let comp_data = blueprint_object_path(&component_path)
        .and_then(|object_path| blueprints.extract_path(&object_path));
    let Some(comp_data) = comp_data else {
        // Parser couldn't load the component blueprint: still write back the
        // normalized (string -> dict) shape so consumers always see a
        // dict, even when the component couldn't be parsed.
        data.insert("ItemComponentClass".to_string(), Value::Object(item_comp));
        return;
    };

    for (key, value) in &comp_data {
        if key == "ObjectPath" {
            continue;
        }
        if key == "MultiAmmo" {
            match value {
                Value::Array(arr) if !arr.is_empty() => {
                    item_comp.insert("MultiAmmo".to_string(), value.clone());
                }
                Value::Object(m) => {
                    if let Some(names) = m.get("CompatibleAmmoNames").and_then(Value::as_array) {
                        if !names.is_empty() {
                            item_comp.insert("MultiAmmo".to_string(), Value::Array(names.clone()));
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        item_comp.insert(key.clone(), value.clone());
    }

    // Resolve ProjectileClasses to get ExplosiveCodeName etc. Skip for
    // multi-ammo weapons (they use different projectiles per ammo).
    let multi_ammo_len = item_comp
        .get("MultiAmmo")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);

    if multi_ammo_len <= 1 {
        if let Some(Value::Array(projectile_classes)) = item_comp.get("ProjectileClasses") {
            if let Some(Value::String(first_proj)) = projectile_classes.first() {
                if first_proj.starts_with("/Game/") {
                    let proj_data = blueprint_object_path(first_proj)
                        .and_then(|object_path| blueprints.extract_path(&object_path));
                    if let Some(proj_data) = proj_data {
                        let mut proj_class = Map::new();
                        for (k, v) in proj_data {
                            if k != "ObjectPath" {
                                proj_class.insert(k, v);
                            }
                        }
                        item_comp.insert("ProjectileClass".to_string(), Value::Object(proj_class));
                    }
                }
            }
        }
    }

    data.insert("ItemComponentClass".to_string(), Value::Object(item_comp));
}

// ---------------------------------------------------------------------
// SubTypeIcon
// ---------------------------------------------------------------------

/// Add `SubTypeIcon` from `AmmoDynamicData.DamageType.Icon` (in-place), when
/// applicable, including the hardcoded `ISGTC` case and the mine-detection
/// exclusion.
pub fn add_subtype_icon(
    data: &mut Map<String, Value>,
    code_name: &str,
    ammo_dynamic_data_asset: &Value,
) {
    if data.get("SubTypeIcon").map(is_truthy).unwrap_or(false) {
        return;
    }

    let ammo_data = match data.get("AmmoDynamicData").and_then(Value::as_object) {
        Some(m) => m.clone(),
        None => {
            // HARDCODED: ISGTC has no ammo info in blueprints.
            if code_name == "ISGTC" {
                data.insert(
                    "SubTypeIcon".to_string(),
                    json!(format!("{SUBTYPE_ICONS_BASE}SubtypeSEIcon.0")),
                );
            }
            return;
        }
    };

    let Some(damage_type) = ammo_data.get("DamageType").and_then(Value::as_object) else {
        return;
    };
    let Some(icon) = damage_type.get("Icon").filter(|v| is_truthy(v)).cloned() else {
        return;
    };

    let item_comp = data
        .get("ItemComponentClass")
        .and_then(Value::as_object)
        .cloned();

    // Exclude mines - they have damage data but don't render subicons
    // in-game. Mine detection requires ItemProfileType::Tool AND one of:
    // bAutoActivate=True (land mines), OR no DeployCodeName/ExplosiveClass/
    // ProjectileClass (water mines).
    let profile_type = data
        .get("ItemProfileType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_tool = profile_type.contains("Tool");

    if is_tool {
        if let Some(ic) = &item_comp {
            let is_auto_activate = ic.get("bAutoActivate") == Some(&Value::Bool(true));
            let has_deploy_code = ic.get("DeployCodeName").map(is_truthy).unwrap_or(false);
            let has_explosive_class = ic.get("ExplosiveClass").map(is_truthy).unwrap_or(false);
            let has_projectile_class = ic.get("ProjectileClass").map(is_truthy).unwrap_or(false);
            let is_mine = is_auto_activate
                || (!has_deploy_code && !has_explosive_class && !has_projectile_class);
            if is_mine {
                return;
            }
        }
    }

    // Case 3a: Item's own CodeName has an AmmoDynamicData entry.
    if !code_name.is_empty()
        && data_table_lookup::get_ammo_dynamic_data(ammo_dynamic_data_asset, code_name).is_some()
    {
        data.insert("SubTypeIcon".to_string(), icon);
        return;
    }

    if let Some(ic) = &item_comp {
        // Case 3b: ProjectileClass.ExplosiveCodeName (single-use/rocket).
        if let Some(proj_class) = ic.get("ProjectileClass").and_then(Value::as_object) {
            if proj_class
                .get("ExplosiveCodeName")
                .map(is_truthy)
                .unwrap_or(false)
            {
                data.insert("SubTypeIcon".to_string(), icon);
                return;
            }
        }
        // Case 3c: DeployCodeName (deployable tripod weapons).
        if ic.get("DeployCodeName").map(is_truthy).unwrap_or(false) {
            data.insert("SubTypeIcon".to_string(), icon);
        }
    }
    // Otherwise: ammo comes from MultiAmmo/CompatibleAmmoCodeName - no SubTypeIcon.
}

// ---------------------------------------------------------------------
// Top-level per-blueprint assembly
// ---------------------------------------------------------------------

/// Build a complete catalog entry from an already-parsed blueprint asset.
///
/// Extracts blueprint data (via [`Blueprints::extract`]), resolves localization GUIDs,
/// expands with data tables and referenced blueprints (via [`expand_data`]),
/// adds/resolves `AmmoDynamicData`+`DamageType` (via
/// `damage_type_resolver::get_ammo_code`/`resolve_damage_type`), adds
/// `SubTypeIcon`, enriches locales, adds `ProductionCategories`, and resolves
/// the stockpile rules (`StockpileCategory`, `bIsStockpilable`,
/// `bIsCratable`, via [`stockpile_classifier::classify`]).
///
/// Returns `None` if [`Blueprints::extract`] can't extract
/// anything, or
/// if `object_path` is not under a stockpile source directory
/// (`ItemPickups`/`Vehicles`/`Structures`).
pub fn assemble_catalog_entry(
    object_path: &str,
    asset: &Value,
    ctx: &AssemblerContext,
) -> Option<Value> {
    let category = StockpileCategory::from_object_path(object_path)?;
    let cached = ctx.blueprints.extract(object_path, asset)?;
    let Value::Object(mut data) = cached else {
        return None;
    };
    if data.is_empty() {
        return None;
    }

    let code_name = data
        .get("CodeName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Extract localization GUIDs from text fields.
    for field in LOCALIZABLE_FIELDS {
        let Some(value) = data.get(*field).cloned() else {
            continue;
        };
        match &value {
            Value::Object(m) if m.contains_key("Guid") => {
                data.insert(format!("{field}Key"), m["Guid"].clone());
                let text = extract_localized_text(&value);
                data.insert(
                    (*field).to_string(),
                    text.map(|t| json!(t)).unwrap_or(Value::Null),
                );
            }
            Value::String(s) if localization::is_guid(s) => {
                data.insert(format!("{field}Key"), json!(s));
                data.insert(
                    (*field).to_string(),
                    json!(ctx.loc_service.get_with_fallback(s, None)),
                );
            }
            _ => {}
        }
    }

    // Expand with data tables and referenced blueprints.
    expand_data(&mut data, &code_name, &ctx.tables, ctx.blueprints);

    // Add AmmoDynamicData and resolve DamageType.
    let empty_ammo_asset = json!({"imports": [], "exports": []});
    let ammo_asset = ctx.tables.ammo_dynamic_data.unwrap_or(&empty_ammo_asset);
    let ammo_code = damage_type_resolver::get_ammo_code(&Value::Object(data.clone()), ammo_asset);

    if let Some(ammo_code) = &ammo_code {
        // Add AmmoDynamicData from ammo if not already present (for weapons
        // using ammo).
        if !data.contains_key("AmmoDynamicData") {
            if let Some(ammo_dynamic) =
                data_table_lookup::get_ammo_dynamic_data(ammo_asset, ammo_code)
            {
                data.insert(
                    "AmmoDynamicData".to_string(),
                    clean_data_table_entry(&ammo_dynamic),
                );
            }
        }

        // Resolve DamageType (expand import index like -25 to full object).
        let needs_resolution = matches!(
            data.get("AmmoDynamicData"),
            Some(Value::Object(m)) if !matches!(m.get("DamageType"), Some(Value::Object(_)))
        );
        if needs_resolution {
            if let Some(damage_type) = damage_type_resolver::resolve_damage_type(
                ammo_code,
                ammo_asset,
                ctx.blueprints,
                ctx.loc_service,
            ) {
                if let Some(Value::Object(ammo_map)) = data.get_mut("AmmoDynamicData") {
                    ammo_map.insert("DamageType".to_string(), damage_type);
                }
            }
        }
    }

    // Add SubTypeIcon.
    add_subtype_icon(&mut data, &code_name, ammo_asset);

    // Add locales.
    enrich_locales(&mut data, LOCALIZABLE_FIELDS, ctx.loc_service);

    // Add ProductionCategories.
    let prod_cats =
        data_table_lookup::get_production_categories(ctx.production_categories, &code_name);
    if let Some(pc) = prod_cats {
        let mut m = Map::new();
        for (k, v) in pc {
            m.insert(k.clone(), json!(v));
        }
        data.insert("ProductionCategories".to_string(), Value::Object(m));
    }

    // Resolve stockpile rules (category, loose and crated storage).
    let rules = stockpile_classifier::classify(&data, category);
    data.insert(
        "StockpileCategory".to_string(),
        json!(rules.category.as_str()),
    );
    data.insert("bIsStockpilable".to_string(), json!(rules.stockpilable));
    data.insert("bIsCratable".to_string(), json!(rules.cratable));

    Some(Value::Object(data))
}

// ---------------------------------------------------------------------
// Top-level catalog assembly
// ---------------------------------------------------------------------

/// Sorts finished catalog entries by `CodeName`, ignoring case.
pub fn sort_catalog(catalog: &mut [Value]) {
    catalog.sort_by(|a, b| {
        let key_a = a
            .get("CodeName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let key_b = b
            .get("CodeName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        key_a.cmp(&key_b)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::localization::LocalizationLookup;

    fn no_loader(_: &str) -> Option<Value> {
        None
    }

    fn temp_loc_service() -> LocalizationLookup {
        let tmp = std::env::temp_dir().join(format!(
            "fs-assets-catalog-assembler-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        LocalizationLookup::new(&tmp, None)
    }

    fn empty_ctx<'a>(
        blueprints: &'a Blueprints<'a>,
        loc: &'a LocalizationLookup,
        prod_cats: &'a HashMap<String, HashMap<String, String>>,
    ) -> AssemblerContext<'a> {
        AssemblerContext {
            blueprints,
            tables: DataTables::default(),
            production_categories: prod_cats,
            loc_service: loc,
        }
    }

    fn default_blueprint_asset(properties: Value) -> Value {
        json!({
            "imports": [],
            "exports": [
                {"object_name": "Test_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}},
                {
                    "object_name": "Default__Test_C", "class_index": 1, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "normal",
                    "properties": properties,
                },
            ],
        })
    }

    mod filter_blueprint_by_content_tests {
        use super::*;

        #[test]
        fn rejects_missing_required_fields() {
            assert!(!filter_blueprint_by_content(
                &json!({"DisplayName": "Test", "Icon": "icon.png"})
            ));
            assert!(!filter_blueprint_by_content(
                &json!({"CodeName": "Test", "Icon": "icon.png"})
            ));
            assert!(!filter_blueprint_by_content(
                &json!({"CodeName": "Test", "DisplayName": "Test"})
            ));
        }

        #[test]
        fn accepts_stockpilable_entry() {
            let data = json!({
                "CodeName": "TestItem", "DisplayName": "Test Item", "Description": "D", "Icon": "icon.png",
                "bIsStockpilable": true, "bIsCratable": false,
            });
            assert!(filter_blueprint_by_content(&data));
        }

        #[test]
        fn accepts_cratable_only_entry() {
            let data = json!({
                "CodeName": "Bunker", "DisplayName": "Bunker", "Description": "D", "Icon": "icon.png",
                "bIsStockpilable": false, "bIsCratable": true,
            });
            assert!(filter_blueprint_by_content(&data));
        }

        #[test]
        fn rejects_entry_neither_stockpilable_nor_cratable() {
            let data = json!({
                "CodeName": "Train", "DisplayName": "Train", "Description": "D", "Icon": "icon.png",
                "bIsStockpilable": false, "bIsCratable": false,
            });
            assert!(!filter_blueprint_by_content(&data));
        }

        #[test]
        fn rejects_entry_without_stockpile_flags() {
            let data = json!({
                "CodeName": "TestItem", "DisplayName": "Test Item", "Description": "D", "Icon": "icon.png",
                "ItemCategory": "EItemCategory::Weapon",
            });
            assert!(!filter_blueprint_by_content(&data));
        }
    }

    mod is_empty_resource_amounts_tests {
        use super::*;

        #[test]
        fn detects_empty_pattern() {
            let value =
                json!({"OtherResources": [], "Resource": {"CodeName": "None", "Quantity": 0}});
            assert!(is_empty_resource_amounts(&value));
        }

        #[test]
        fn non_empty_other_resources_is_not_empty() {
            let value = json!({
                "OtherResources": [{"CodeName": "BasicMaterials", "Quantity": 10}],
                "Resource": {"CodeName": "None", "Quantity": 0},
            });
            assert!(!is_empty_resource_amounts(&value));
        }

        #[test]
        fn non_none_resource_is_not_empty() {
            let value = json!({
                "OtherResources": [],
                "Resource": {"CodeName": "BasicMaterials", "Quantity": 100},
            });
            assert!(!is_empty_resource_amounts(&value));
        }

        #[test]
        fn non_dict_is_not_empty() {
            assert!(!is_empty_resource_amounts(&json!("string")));
            assert!(!is_empty_resource_amounts(&Value::Null));
            assert!(!is_empty_resource_amounts(&json!([])));
        }
    }

    mod clean_data_table_entry_tests {
        use super::*;

        #[test]
        fn removes_empty_alt_resource_amounts() {
            let entry = json!({
                "Damage": 100,
                "AltResourceAmounts": {"OtherResources": [], "Resource": {"CodeName": "None", "Quantity": 0}},
            });
            let result = clean_data_table_entry(&entry);
            assert!(result.get("AltResourceAmounts").is_none());
            assert_eq!(result["Damage"], json!(100));
        }

        #[test]
        fn keeps_non_empty_alt_resource_amounts() {
            let entry = json!({
                "AltResourceAmounts": {"OtherResources": [], "Resource": {"CodeName": "BasicMaterials", "Quantity": 100}},
            });
            let result = clean_data_table_entry(&entry);
            assert!(result.get("AltResourceAmounts").is_some());
        }
    }

    mod clean_entry_tests {
        use super::*;

        #[test]
        fn removes_none_values() {
            let data = json!({"CodeName": "Test", "NullField": null, "ValidField": "value"});
            let result = clean_entry(&data);
            assert!(result.get("NullField").is_none());
            assert_eq!(result["ValidField"], json!("value"));
        }

        #[test]
        fn normalizes_game_paths() {
            let data = json!({"Icon": "/Game/Textures/UI/ItemIcons/Rifle"});
            let result = clean_entry(&data);
            assert_eq!(
                result["Icon"],
                json!("War/Content/Textures/UI/ItemIcons/Rifle.0")
            );
        }

        #[test]
        fn normalizes_paths_in_lists() {
            let data = json!({"Icons": ["/Game/Textures/Icon1", "/Game/Textures/Icon2"]});
            let result = clean_entry(&data);
            assert_eq!(
                result["Icons"],
                json!([
                    "War/Content/Textures/Icon1.0",
                    "War/Content/Textures/Icon2.0"
                ])
            );
        }

        #[test]
        fn recursively_cleans_nested_dicts() {
            let data = json!({
                "OuterField": "value",
                "Nested": {"Icon": "/Game/Textures/Icon", "NullField": null},
            });
            let result = clean_entry(&data);
            assert!(result["Nested"].get("NullField").is_none());
            assert_eq!(
                result["Nested"]["Icon"],
                json!("War/Content/Textures/Icon.0")
            );
        }

        #[test]
        fn flattens_string_keyed_maps() {
            let data = json!({"Modifications": [
                ["EFortModificationType::MotorPool", [{"CodeName": "ArmoredCarTwinC", "Duration": 300}]],
                ["EFortModificationType::TankAssembly", [{"CodeName": "LightTankC", "Duration": 600}]],
            ]});
            let result = clean_entry(&data);
            assert_eq!(
                result["Modifications"],
                json!({
                    "EFortModificationType::MotorPool": [{"CodeName": "ArmoredCarTwinC", "Duration": 300}],
                    "EFortModificationType::TankAssembly": [{"CodeName": "LightTankC", "Duration": 600}],
                })
            );
        }

        #[test]
        fn flattens_single_entry_map() {
            let data = json!({"Modifications": [["EFortModificationType::TrainAssembly", []]]});
            let result = clean_entry(&data);
            assert_eq!(
                result["Modifications"],
                json!({"EFortModificationType::TrainAssembly": []})
            );
        }

        #[test]
        fn cleans_flattened_map_values() {
            let data =
                json!({"Map": [["Key", {"Icon": "/Game/Textures/Icon", "NullField": null}]]});
            let result = clean_entry(&data);
            assert_eq!(
                result["Map"],
                json!({"Key": {"Icon": "War/Content/Textures/Icon.0"}})
            );
        }

        #[test]
        fn keeps_lists_that_are_not_maps() {
            let data = json!({
                "Empty": [],
                "NonStringKeys": [[1, "a"], [2, "b"]],
                "Triples": [["a", 1, 2]],
                "Strings": ["a", "b"],
            });
            assert_eq!(clean_entry(&data), data);
        }
    }

    mod enrich_locales_tests {
        use super::*;

        #[test]
        fn adds_locales_from_guid() {
            let loc = temp_loc_service();
            // No locres files loaded, so get_all_languages returns empty and
            // is_guid must accept the fixed-format GUID string.
            let guid = "8BB336F4459740A6ADA7B28B2D91748B";
            let mut data = Map::new();
            data.insert("DisplayName".to_string(), json!("Test"));
            data.insert("DisplayNameKey".to_string(), json!(guid));
            enrich_locales(&mut data, LOCALIZABLE_FIELDS, &loc);
            assert!(!data.contains_key("DisplayNameKey"));
        }

        #[test]
        fn skips_if_locales_already_present() {
            let loc = temp_loc_service();
            let mut data = Map::new();
            data.insert("DisplayName".to_string(), json!("Test"));
            data.insert("DisplayNameKey".to_string(), json!("GUID"));
            data.insert("DisplayNameLocales".to_string(), json!({"en": "Existing"}));
            enrich_locales(&mut data, LOCALIZABLE_FIELDS, &loc);
            assert_eq!(data["DisplayNameLocales"], json!({"en": "Existing"}));
            assert!(!data.contains_key("DisplayNameKey"));
        }

        #[test]
        fn invalid_guid_key_field_left_in_place() {
            let loc = temp_loc_service();
            let mut data = Map::new();
            data.insert("DisplayName".to_string(), json!("Test"));
            data.insert("DisplayNameKey".to_string(), json!("not-a-guid"));
            enrich_locales(&mut data, LOCALIZABLE_FIELDS, &loc);
            assert!(data.contains_key("DisplayNameKey"));
            assert!(!data.contains_key("DisplayNameLocales"));
        }
    }

    mod expand_data_tests {
        use super::*;

        #[test]
        fn adds_item_dynamic_data() {
            let asset = default_blueprint_asset_for_table("ItemDynamicData", "TestItem");
            let tables = DataTables {
                item_dynamic_data: Some(&asset),
                ..Default::default()
            };
            let mut data = Map::new();
            expand_data(
                &mut data,
                "TestItem",
                &tables,
                &Blueprints::new(&no_loader, false),
            );
            assert!(data.contains_key("ItemDynamicData"));
        }

        #[test]
        fn adds_ammo_dynamic_data() {
            let asset = json!({
                "imports": [],
                "exports": [{
                    "object_name": "BPAmmoDynamicData", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "data_table", "properties": {},
                    "rows": {"TestAmmo": {"Damage": 100}},
                }],
            });
            let tables = DataTables {
                ammo_dynamic_data: Some(&asset),
                ..Default::default()
            };
            let mut data = Map::new();
            expand_data(
                &mut data,
                "TestAmmo",
                &tables,
                &Blueprints::new(&no_loader, false),
            );
            assert_eq!(data["AmmoDynamicData"]["Damage"], json!(100));
        }

        #[test]
        fn adds_profile_data() {
            let asset = json!({
                "imports": [],
                "exports": [{
                    "object_name": "Default__BPItemProfileTable_C", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "normal",
                    "properties": {"ItemProfileTable": [["EItemProfileType::Small", {"MaxStack": 10}]]},
                }],
            });
            let tables = DataTables {
                item_profile_table: Some(&asset),
                ..Default::default()
            };
            let mut data = Map::new();
            data.insert(
                "ItemProfileType".to_string(),
                json!("EItemProfileType::Small"),
            );
            expand_data(
                &mut data,
                "TestItem",
                &tables,
                &Blueprints::new(&no_loader, false),
            );
            assert!(data.contains_key("ItemProfileData"));
        }

        #[test]
        fn skips_empty_codename() {
            let mut data = Map::new();
            expand_data(
                &mut data,
                "",
                &DataTables::default(),
                &Blueprints::new(&no_loader, false),
            );
            assert!(data.is_empty());
        }

        fn default_blueprint_asset_for_table(_field: &str, code_name: &str) -> Value {
            json!({
                "imports": [],
                "exports": [{
                    "object_name": "BPItemDynamicData", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "data_table", "properties": {},
                    "rows": {code_name: {"Weight": 10}},
                }],
            })
        }
    }

    mod enrich_item_component_class_tests {
        use super::*;
        use std::collections::HashMap as StdHashMap;

        fn map_loader(assets: StdHashMap<String, Value>) -> impl Fn(&str) -> Option<Value> {
            move |path: &str| assets.get(path).cloned()
        }

        #[test]
        fn string_path_is_enriched() {
            let comp_asset = default_blueprint_asset(
                json!({"CompatibleAmmoCodeName": "RifleAmmo", "Damage": 25}),
            );
            let mut assets = StdHashMap::new();
            assets.insert(
                "/Game/Blueprints/Items/BPRifleComponent".to_string(),
                comp_asset,
            );
            let loader = map_loader(assets);

            let mut data = Map::new();
            data.insert(
                "ItemComponentClass".to_string(),
                json!("/Game/Blueprints/Items/BPRifleComponent"),
            );
            enrich_item_component_class(&mut data, &Blueprints::new(&loader, false));

            assert!(data["ItemComponentClass"].is_object());
            assert_eq!(
                data["ItemComponentClass"]["CompatibleAmmoCodeName"],
                json!("RifleAmmo")
            );
        }

        #[test]
        fn dict_path_is_enriched() {
            let comp_asset = default_blueprint_asset(json!({"Damage": 25}));
            let mut assets = StdHashMap::new();
            assets.insert("/Game/Blueprints/Items/BPComp".to_string(), comp_asset);
            let loader = map_loader(assets);

            let mut data = Map::new();
            data.insert(
                "ItemComponentClass".to_string(),
                json!({"ObjectPath": "War/Content/Blueprints/Items/BPComp.0"}),
            );
            enrich_item_component_class(&mut data, &Blueprints::new(&loader, false));
            assert_eq!(data["ItemComponentClass"]["Damage"], json!(25));
        }

        #[test]
        fn dict_path_keeps_a_dot_zero_inside_the_name() {
            let comp_asset = default_blueprint_asset(json!({"Damage": 25}));
            let mut assets = StdHashMap::new();
            assets.insert("/Game/Blueprints/Items/BPComp.0Mk2".to_string(), comp_asset);
            let loader = map_loader(assets);

            let mut data = Map::new();
            data.insert(
                "ItemComponentClass".to_string(),
                json!({"ObjectPath": "War/Content/Blueprints/Items/BPComp.0Mk2.0"}),
            );
            enrich_item_component_class(&mut data, &Blueprints::new(&loader, false));
            assert_eq!(data["ItemComponentClass"]["Damage"], json!(25));
        }

        #[test]
        fn no_object_path_is_left_unmodified() {
            let mut data = Map::new();
            data.insert(
                "ItemComponentClass".to_string(),
                json!({"SomeOther": "value"}),
            );
            enrich_item_component_class(&mut data, &Blueprints::new(&no_loader, false));
            assert_eq!(data["ItemComponentClass"], json!({"SomeOther": "value"}));
        }

        #[test]
        fn invalid_type_is_left_unmodified() {
            let mut data = Map::new();
            data.insert("ItemComponentClass".to_string(), json!(123));
            enrich_item_component_class(&mut data, &Blueprints::new(&no_loader, false));
            assert_eq!(data["ItemComponentClass"], json!(123));
        }

        #[test]
        fn missing_field_adds_nothing() {
            let mut data = Map::new();
            enrich_item_component_class(&mut data, &Blueprints::new(&no_loader, false));
            assert!(!data.contains_key("ItemComponentClass"));
        }

        #[test]
        fn multiammo_list_is_normalized() {
            let comp_asset =
                default_blueprint_asset(json!({"MultiAmmo": ["Ammo1", "Ammo2", "Ammo3"]}));
            let mut assets = StdHashMap::new();
            assets.insert("/Game/Blueprints/Items/BPComp".to_string(), comp_asset);
            let loader = map_loader(assets);

            let mut data = Map::new();
            data.insert(
                "ItemComponentClass".to_string(),
                json!("/Game/Blueprints/Items/BPComp"),
            );
            enrich_item_component_class(&mut data, &Blueprints::new(&loader, false));
            assert_eq!(
                data["ItemComponentClass"]["MultiAmmo"],
                json!(["Ammo1", "Ammo2", "Ammo3"])
            );
        }

        #[test]
        fn multiammo_dict_is_normalized() {
            let comp_asset = default_blueprint_asset(json!({
                "MultiAmmo": {"CompatibleAmmoNames": ["Ammo1", "Ammo2"]}
            }));
            let mut assets = StdHashMap::new();
            assets.insert("/Game/Blueprints/Items/BPComp".to_string(), comp_asset);
            let loader = map_loader(assets);

            let mut data = Map::new();
            data.insert(
                "ItemComponentClass".to_string(),
                json!("/Game/Blueprints/Items/BPComp"),
            );
            enrich_item_component_class(&mut data, &Blueprints::new(&loader, false));
            assert_eq!(
                data["ItemComponentClass"]["MultiAmmo"],
                json!(["Ammo1", "Ammo2"])
            );
        }

        #[test]
        fn resolves_projectile_class() {
            let comp_asset = default_blueprint_asset(json!({
                "ProjectileClasses": ["/Game/Blueprints/Projectiles/BPProj"]
            }));
            let proj_asset =
                default_blueprint_asset(json!({"ExplosiveCodeName": "RPGExplosive", "Speed": 500}));

            let mut assets = StdHashMap::new();
            assets.insert("/Game/Blueprints/Items/BPComp".to_string(), comp_asset);
            assets.insert(
                "/Game/Blueprints/Projectiles/BPProj".to_string(),
                proj_asset,
            );
            let loader = map_loader(assets);

            let mut data = Map::new();
            data.insert(
                "ItemComponentClass".to_string(),
                json!("/Game/Blueprints/Items/BPComp"),
            );
            enrich_item_component_class(&mut data, &Blueprints::new(&loader, false));

            let proj_class = &data["ItemComponentClass"]["ProjectileClass"];
            assert_eq!(proj_class["ExplosiveCodeName"], json!("RPGExplosive"));
        }

        #[test]
        fn multiammo_greater_than_one_skips_projectile_resolution() {
            let comp_asset = default_blueprint_asset(json!({
                "MultiAmmo": ["A", "B"],
                "ProjectileClasses": ["/Game/Blueprints/Projectiles/BPProj"],
            }));
            let proj_asset =
                default_blueprint_asset(json!({"ExplosiveCodeName": "ShouldNotAppear"}));

            let mut assets = StdHashMap::new();
            assets.insert("/Game/Blueprints/Items/BPComp".to_string(), comp_asset);
            assets.insert(
                "/Game/Blueprints/Projectiles/BPProj".to_string(),
                proj_asset,
            );
            let loader = map_loader(assets);

            let mut data = Map::new();
            data.insert(
                "ItemComponentClass".to_string(),
                json!("/Game/Blueprints/Items/BPComp"),
            );
            enrich_item_component_class(&mut data, &Blueprints::new(&loader, false));
            assert!(data["ItemComponentClass"].get("ProjectileClass").is_none());
        }
    }

    mod add_subtype_icon_tests {
        use super::*;

        fn empty_ammo_asset() -> Value {
            json!({"imports": [], "exports": []})
        }

        #[test]
        fn adds_icon_for_ammo_item() {
            let ammo_asset = json!({
                "imports": [], "exports": [{
                    "object_name": "BPAmmoDynamicData", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "data_table", "properties": {}, "rows": {"TestAmmo": {"Damage": 100}},
                }],
            });
            let mut data = Map::new();
            data.insert("CodeName".to_string(), json!("TestAmmo"));
            data.insert(
                "AmmoDynamicData".to_string(),
                json!({"DamageType": {"Icon": "explosion_icon.png"}}),
            );
            add_subtype_icon(&mut data, "TestAmmo", &ammo_asset);
            assert_eq!(data["SubTypeIcon"], json!("explosion_icon.png"));
        }

        #[test]
        fn adds_icon_for_projectile_weapon() {
            let mut data = Map::new();
            data.insert("CodeName".to_string(), json!("RPGLauncher"));
            data.insert(
                "AmmoDynamicData".to_string(),
                json!({"DamageType": {"Icon": "rpg_icon.png"}}),
            );
            data.insert(
                "ItemComponentClass".to_string(),
                json!({"ProjectileClass": {"ExplosiveCodeName": "RPGExplosive"}}),
            );
            add_subtype_icon(&mut data, "RPGLauncher", &empty_ammo_asset());
            assert_eq!(data["SubTypeIcon"], json!("rpg_icon.png"));
        }

        #[test]
        fn adds_icon_for_deployable() {
            let mut data = Map::new();
            data.insert("CodeName".to_string(), json!("TripodWeapon"));
            data.insert(
                "AmmoDynamicData".to_string(),
                json!({"DamageType": {"Icon": "tripod_icon.png"}}),
            );
            data.insert(
                "ItemComponentClass".to_string(),
                json!({"DeployCodeName": "DeployedTripod"}),
            );
            add_subtype_icon(&mut data, "TripodWeapon", &empty_ammo_asset());
            assert_eq!(data["SubTypeIcon"], json!("tripod_icon.png"));
        }

        #[test]
        fn skips_handheld_weapon() {
            let mut data = Map::new();
            data.insert("CodeName".to_string(), json!("Rifle"));
            data.insert(
                "AmmoDynamicData".to_string(),
                json!({"DamageType": {"Icon": "bullet_icon.png"}}),
            );
            data.insert(
                "ItemComponentClass".to_string(),
                json!({"CompatibleAmmoCodeName": "RifleAmmo"}),
            );
            add_subtype_icon(&mut data, "Rifle", &empty_ammo_asset());
            assert!(!data.contains_key("SubTypeIcon"));
        }

        #[test]
        fn hardcoded_isgtc() {
            let mut data = Map::new();
            data.insert("CodeName".to_string(), json!("ISGTC"));
            add_subtype_icon(&mut data, "ISGTC", &empty_ammo_asset());
            assert_eq!(
                data["SubTypeIcon"],
                json!("War/Content/Textures/UI/ItemIcons/SubtypeSEIcon.0")
            );
        }

        #[test]
        fn no_ammo_data_adds_nothing() {
            let mut data = Map::new();
            data.insert("CodeName".to_string(), json!("Hammer"));
            add_subtype_icon(&mut data, "Hammer", &empty_ammo_asset());
            assert!(!data.contains_key("SubTypeIcon"));
        }

        #[test]
        fn no_damage_type_adds_nothing() {
            let mut data = Map::new();
            data.insert("CodeName".to_string(), json!("TestItem"));
            data.insert("AmmoDynamicData".to_string(), json!({"Damage": 100}));
            add_subtype_icon(&mut data, "TestItem", &empty_ammo_asset());
            assert!(!data.contains_key("SubTypeIcon"));
        }

        #[test]
        fn mine_with_auto_activate_is_excluded() {
            let mut data = Map::new();
            data.insert("CodeName".to_string(), json!("LandMine"));
            data.insert(
                "ItemProfileType".to_string(),
                json!("EItemProfileType::Tool"),
            );
            data.insert(
                "AmmoDynamicData".to_string(),
                json!({"DamageType": {"Icon": "mine_icon.png"}}),
            );
            data.insert(
                "ItemComponentClass".to_string(),
                json!({"bAutoActivate": true}),
            );
            add_subtype_icon(&mut data, "LandMine", &empty_ammo_asset());
            assert!(!data.contains_key("SubTypeIcon"));
        }
    }

    mod assemble_catalog_entry_tests {
        use super::*;

        #[test]
        fn happy_path_item_category() {
            let asset = default_blueprint_asset(json!({
                "CodeName": "TestItem",
                "DisplayName": {"Text": "Test Item", "Guid": "GUID-ABC"},
                "Icon": "icon.png",
                "ItemCategory": "EItemCategory::Weapon",
            }));
            let loc = temp_loc_service();
            let prod_cats = HashMap::new();
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = empty_ctx(&blueprints, &loc, &prod_cats);

            let result =
                assemble_catalog_entry("/Game/Blueprints/Items/BPTest", &asset, &ctx).unwrap();
            assert_eq!(result["CodeName"], json!("TestItem"));
            assert_eq!(result["DisplayNameKey"], json!("GUID-ABC"));
            assert_eq!(result["DisplayName"], json!("Test Item"));
        }

        #[test]
        fn legacy_guid_string_display_name_resolved() {
            let guid = "8BB336F4459740A6ADA7B28B2D91748B";
            let asset = default_blueprint_asset(json!({
                "CodeName": "TestItem",
                "DisplayName": guid,
                "Icon": "icon.png",
                "ItemCategory": "EItemCategory::Weapon",
            }));
            let loc = temp_loc_service();
            let prod_cats = HashMap::new();
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = empty_ctx(&blueprints, &loc, &prod_cats);

            let result =
                assemble_catalog_entry("/Game/Blueprints/Items/BPTest", &asset, &ctx).unwrap();
            // No locres files loaded => fallback returns the GUID itself, but
            // the DisplayNameKey/Locales dance must still have run.
            assert_eq!(result["DisplayName"], json!(guid));
            assert!(result.get("DisplayNameKey").is_none());
        }

        #[test]
        fn resolves_item_stockpile_rules() {
            let asset = default_blueprint_asset(json!({
                "CodeName": "RifleW",
                "DisplayName": "Rifle",
                "Icon": "icon.png",
                "ItemProfileData": {"bIsStockpilable": true, "bIsCratable": true},
            }));
            let loc = temp_loc_service();
            let prod_cats = HashMap::new();
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = empty_ctx(&blueprints, &loc, &prod_cats);

            let result =
                assemble_catalog_entry("/Game/Blueprints/ItemPickups/BPTest", &asset, &ctx)
                    .unwrap();
            assert_eq!(result["StockpileCategory"], json!("Items"));
            assert_eq!(result["bIsStockpilable"], json!(true));
            assert_eq!(result["bIsCratable"], json!(true));
        }

        #[test]
        fn resolves_vehicle_stockpile_rules() {
            let asset = default_blueprint_asset(json!({
                "CodeName": "TruckW",
                "DisplayName": "Truck",
                "Icon": "icon.png",
                "VehicleBuildType": "EVehicleBuildType::VehicleFactory",
            }));
            let loc = temp_loc_service();
            let prod_cats = HashMap::new();
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = empty_ctx(&blueprints, &loc, &prod_cats);

            let result =
                assemble_catalog_entry("/Game/Blueprints/Vehicles/BPTest", &asset, &ctx).unwrap();
            assert_eq!(result["StockpileCategory"], json!("Vehicles"));
            assert_eq!(result["bIsStockpilable"], json!(true));
            assert_eq!(result["bIsCratable"], json!(true));
            assert!(result.get("ProductionCategories").is_none());
        }

        #[test]
        fn resolves_structure_stockpile_rules() {
            let asset = default_blueprint_asset(json!({
                "CodeName": "TestStructure",
                "DisplayName": "Test Structure",
                "Icon": "icon.png",
                "BuildLocationType": "EBuildLocationType::ConstructionYard",
            }));
            let loc = temp_loc_service();
            let prod_cats = HashMap::new();
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = empty_ctx(&blueprints, &loc, &prod_cats);

            let result =
                assemble_catalog_entry("/Game/Blueprints/Structures/BPTest", &asset, &ctx).unwrap();
            assert_eq!(result["StockpileCategory"], json!("Structures"));
            assert_eq!(result["bIsStockpilable"], json!(false));
            assert_eq!(result["bIsCratable"], json!(true));
        }

        #[test]
        fn returns_none_outside_stockpile_directories() {
            let asset = default_blueprint_asset(
                json!({"CodeName": "X", "DisplayName": "X", "Icon": "icon.png"}),
            );
            let loc = temp_loc_service();
            let prod_cats = HashMap::new();
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = empty_ctx(&blueprints, &loc, &prod_cats);
            assert_eq!(
                assemble_catalog_entry("/Game/Blueprints/Data/BPTest", &asset, &ctx),
                None
            );
        }

        #[test]
        fn existing_production_categories_with_mass_production_are_not_overridden() {
            let asset = default_blueprint_asset(json!({
                "CodeName": "RifleW",
                "DisplayName": "Rifle",
                "Icon": "icon.png",
                "ItemCategory": "EItemCategory::Weapon",
            }));
            let loc = temp_loc_service();
            let mut prod_cats = HashMap::new();
            let mut entry = HashMap::new();
            entry.insert(
                "MassProductionFactory".to_string(),
                "EFactoryQueueType::Weapons".to_string(),
            );
            prod_cats.insert("riflew".to_string(), entry);
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = empty_ctx(&blueprints, &loc, &prod_cats);

            let result =
                assemble_catalog_entry("/Game/Blueprints/Items/BPTest", &asset, &ctx).unwrap();
            assert_eq!(
                result["ProductionCategories"]["MassProductionFactory"],
                json!("EFactoryQueueType::Weapons")
            );
        }

        #[test]
        fn resolves_ammo_and_damage_type_wiring() {
            // Weapon with CompatibleAmmoCodeName; ammo table has an
            // AmmoDynamicData row with an unresolved DamageType reference
            // pointing at a Script (C++) damage type.
            let asset = default_blueprint_asset(json!({
                "CodeName": "TestWeapon",
                "DisplayName": "Test Weapon",
                "Icon": "icon.png",
                "ItemCategory": "EItemCategory::Weapon",
                "ItemComponentClass": {"CompatibleAmmoCodeName": "RifleAmmo"},
            }));

            let ammo_asset = json!({
                "imports": [
                    {"object_name": "/Script/War", "class_name": "Package", "class_package": "/Script/CoreUObject", "outer_index": 0},
                    {"object_name": "DamageType", "class_name": "Class", "class_package": "/Script/CoreUObject", "outer_index": -1},
                ],
                "exports": [{
                    "object_name": "BPAmmoDynamicData", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0,
                    "kind": "data_table", "properties": {},
                    "rows": {"RifleAmmo": {"Damage": 100, "DamageType": "Reference: -2"}},
                }],
            });

            let loc = temp_loc_service();
            let prod_cats = HashMap::new();
            let tables = DataTables {
                ammo_dynamic_data: Some(&ammo_asset),
                ..Default::default()
            };
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = AssemblerContext {
                blueprints: &blueprints,
                tables,
                production_categories: &prod_cats,
                loc_service: &loc,
            };

            let result =
                assemble_catalog_entry("/Game/Blueprints/Items/BPTest", &asset, &ctx).unwrap();
            let ammo_data = &result["AmmoDynamicData"];
            assert_eq!(ammo_data["Damage"], json!(100));
            assert_eq!(ammo_data["DamageType"], json!({"Type": "/Script/War"}));
        }

        #[test]
        fn returns_none_when_extraction_yields_no_default_export() {
            let asset = json!({
                "imports": [],
                "exports": [{"object_name": "NotDefault", "class_index": 0, "super_index": 0, "template_index": 0, "outer_index": 0, "kind": "normal", "properties": {}}],
            });
            let loc = temp_loc_service();
            let prod_cats = HashMap::new();
            let blueprints = Blueprints::new(&no_loader, false);
            let ctx = empty_ctx(&blueprints, &loc, &prod_cats);
            assert_eq!(
                assemble_catalog_entry("/Game/Blueprints/Items/BPTest", &asset, &ctx),
                None
            );
        }
    }

    mod sort_catalog_tests {
        use super::*;

        #[test]
        fn sorts_by_codename_ignoring_case() {
            let mut catalog = vec![
                json!({"CodeName": "zebra"}),
                json!({"CodeName": "Alpha"}),
                json!({}),
                json!({"CodeName": "beta"}),
            ];
            sort_catalog(&mut catalog);
            assert_eq!(
                catalog,
                vec![
                    json!({}),
                    json!({"CodeName": "Alpha"}),
                    json!({"CodeName": "beta"}),
                    json!({"CodeName": "zebra"}),
                ]
            );
        }
    }
}
