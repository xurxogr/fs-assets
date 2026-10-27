//! Stockpile classification for catalog entries.
//!
//! Decides, for each blueprint, which stockpile category it belongs to and
//! whether it can be stored loose (stockpilable) and/or in crate form
//! (cratable). An entry is only kept in the catalog when it can appear in a
//! stockpile at all (see [`StockpileRules::included`]).

use serde_json::{Map, Value};

use crate::catalog::utils::is_truthy;

/// Vehicles produced at these build sites can be packed into crates.
const CRATABLE_VEHICLE_BUILD_TYPES: &[&str] = &["AircraftFactory", "VehicleFactory", "Shipyard"];

/// Structures produced at these build sites can be packed into crates.
const CRATABLE_STRUCTURE_BUILD_LOCATIONS: &[&str] = &["ConstructionYard"];

/// Relic vehicles cannot be packed into crates.
const NON_CRATABLE_VEHICLES: &[&str] = &[
    "RelicArmouredCar",
    "RelicLightTank",
    "RelicScoutVehicle",
    "RelicTruck",
    "RelicMediumTank",
];

/// Assembly stations ship in crates even though they are built anywhere.
const CRATABLE_STRUCTURES: &[&str] = &["FacilityVehicleFactory1", "FacilityVehicleFactory2"];

/// Stockpile category an entry is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockpileCategory {
    Items,
    Vehicles,
    Structures,
}

impl StockpileCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            StockpileCategory::Items => "Items",
            StockpileCategory::Vehicles => "Vehicles",
            StockpileCategory::Structures => "Structures",
        }
    }

    /// Convert a category name or blueprint source directory (e.g.
    /// `ItemPickups`) to a category, or `None` if it matches none.
    pub fn from_string(value: &str) -> Option<StockpileCategory> {
        match value.trim().to_lowercase().as_str() {
            "itempickups" | "items" => Some(StockpileCategory::Items),
            "vehicles" => Some(StockpileCategory::Vehicles),
            "structures" => Some(StockpileCategory::Structures),
            _ => None,
        }
    }

    /// Derive the category from a blueprint object path, using the directory
    /// right below `Blueprints` (e.g.
    /// `War/Content/Blueprints/ItemPickups/BPRifle` -> `Items`).
    pub fn from_object_path(object_path: &str) -> Option<StockpileCategory> {
        let mut segments = object_path.split('/').skip_while(|s| *s != "Blueprints");
        segments.next()?;
        segments.next().and_then(StockpileCategory::from_string)
    }
}

/// Resolved stockpile rules for a catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StockpileRules {
    pub category: StockpileCategory,
    /// Whether it can be stored loose.
    pub stockpilable: bool,
    /// Whether it can be stored in crate form.
    pub cratable: bool,
}

impl StockpileRules {
    /// Whether the entry can appear in a stockpile at all.
    pub fn included(&self) -> bool {
        self.stockpilable || self.cratable
    }
}

/// Resolve the stockpile rules for a blueprint.
///
/// Anything with an item profile is an item, wherever it is stored (e.g.
/// banners live with structures). Otherwise the category comes from the
/// blueprint source directory.
pub fn classify(data: &Map<String, Value>, category: StockpileCategory) -> StockpileRules {
    let has_item_profile = data.get("ItemProfileType").is_some_and(is_truthy);
    if has_item_profile || category == StockpileCategory::Items {
        return classify_item(data);
    }
    if category == StockpileCategory::Vehicles {
        return classify_vehicle(data);
    }
    classify_structure(data)
}

/// Items are stockpilable/cratable as flagged in their item profile, unless
/// their content is hidden.
fn classify_item(data: &Map<String, Value>) -> StockpileRules {
    let profile = data.get("ItemProfileData").and_then(Value::as_object);
    let hidden = is_true(data.get("bHideContent"));
    let (stockpilable, cratable) = match profile {
        Some(profile) if !hidden => (
            is_true(profile.get("bIsStockpilable")),
            is_true(profile.get("bIsCratable")),
        ),
        _ => (false, false),
    };
    StockpileRules {
        category: StockpileCategory::Items,
        stockpilable,
        cratable,
    }
}

/// Vehicles are stockpilable unless flagged otherwise, and cratable when
/// built at a vehicle factory, shipyard or aircraft factory.
fn classify_vehicle(data: &Map<String, Value>) -> StockpileRules {
    let build_type = enum_member(data.get("VehicleBuildType"));
    let code_name = data.get("CodeName").and_then(Value::as_str).unwrap_or("");
    StockpileRules {
        category: StockpileCategory::Vehicles,
        stockpilable: data
            .get("bIsStockpilable")
            .is_none_or(|v| v == &Value::Bool(true)),
        cratable: CRATABLE_VEHICLE_BUILD_TYPES.contains(&build_type)
            && !NON_CRATABLE_VEHICLES.contains(&code_name),
    }
}

/// Structures are only stockpilable when flagged, and cratable when built at
/// a Construction Yard (plus the assembly stations).
fn classify_structure(data: &Map<String, Value>) -> StockpileRules {
    let location = enum_member(data.get("BuildLocationType"));
    let code_name = data.get("CodeName").and_then(Value::as_str).unwrap_or("");
    StockpileRules {
        category: StockpileCategory::Structures,
        stockpilable: is_true(data.get("bIsStockpilable")),
        cratable: CRATABLE_STRUCTURE_BUILD_LOCATIONS.contains(&location)
            || CRATABLE_STRUCTURES.contains(&code_name),
    }
}

/// Strip the enum prefix from a serialized enum value
/// (`EVehicleBuildType::VehicleFactory` -> `VehicleFactory`), or `""` when
/// the value is not a string.
fn enum_member(value: Option<&Value>) -> &str {
    let Some(s) = value.and_then(Value::as_str) else {
        return "";
    };
    s.rsplit("::").next().unwrap_or(s)
}

fn is_true(value: Option<&Value>) -> bool {
    value == Some(&Value::Bool(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    // ===== StockpileCategory =====

    #[test]
    fn category_from_string_accepts_names_and_directories() {
        assert_eq!(
            StockpileCategory::from_string("ItemPickups"),
            Some(StockpileCategory::Items)
        );
        assert_eq!(
            StockpileCategory::from_string(" items "),
            Some(StockpileCategory::Items)
        );
        assert_eq!(
            StockpileCategory::from_string("VEHICLES"),
            Some(StockpileCategory::Vehicles)
        );
        assert_eq!(
            StockpileCategory::from_string("Structures"),
            Some(StockpileCategory::Structures)
        );
        assert_eq!(StockpileCategory::from_string("Data"), None);
    }

    #[test]
    fn category_from_object_path_uses_directory_below_blueprints() {
        assert_eq!(
            StockpileCategory::from_object_path(
                "War/Content/Blueprints/ItemPickups/Weapons/BPRifle"
            ),
            Some(StockpileCategory::Items)
        );
        assert_eq!(
            StockpileCategory::from_object_path("/Game/Blueprints/Vehicles/BPTruck"),
            Some(StockpileCategory::Vehicles)
        );
        assert_eq!(
            StockpileCategory::from_object_path("War/Content/Blueprints/Data/BPX"),
            None
        );
        assert_eq!(
            StockpileCategory::from_object_path("War/Content/Blueprints"),
            None
        );
        assert_eq!(
            StockpileCategory::from_object_path("Vehicles/BPTruck"),
            None
        );
    }

    // ===== Items =====

    #[test]
    fn item_uses_profile_flags() {
        let data = map(json!({"ItemProfileData": {"bIsStockpilable": true, "bIsCratable": false}}));
        let rules = classify(&data, StockpileCategory::Items);
        assert_eq!(
            rules,
            StockpileRules {
                category: StockpileCategory::Items,
                stockpilable: true,
                cratable: false
            }
        );
    }

    #[test]
    fn item_without_profile_is_excluded() {
        let rules = classify(&Map::new(), StockpileCategory::Items);
        assert!(!rules.included());
    }

    #[test]
    fn hidden_item_is_excluded() {
        let data = map(json!({
            "bHideContent": true,
            "ItemProfileData": {"bIsStockpilable": true, "bIsCratable": true},
        }));
        assert!(!classify(&data, StockpileCategory::Items).included());
    }

    #[test]
    fn item_profile_wins_over_source_directory() {
        let data = map(json!({
            "ItemProfileType": "EItemProfileType::Banner",
            "ItemProfileData": {"bIsStockpilable": true},
        }));
        let rules = classify(&data, StockpileCategory::Structures);
        assert_eq!(rules.category, StockpileCategory::Items);
        assert!(rules.stockpilable);
    }

    // ===== Vehicles =====

    #[test]
    fn vehicle_is_stockpilable_by_default() {
        let rules = classify(&Map::new(), StockpileCategory::Vehicles);
        assert_eq!(rules.category, StockpileCategory::Vehicles);
        assert!(rules.stockpilable);
        assert!(!rules.cratable);
    }

    #[test]
    fn vehicle_flagged_not_stockpilable() {
        let data = map(json!({"bIsStockpilable": false}));
        assert!(!classify(&data, StockpileCategory::Vehicles).stockpilable);
    }

    #[test]
    fn vehicle_cratable_by_build_type() {
        for build_type in ["AircraftFactory", "VehicleFactory", "Shipyard"] {
            let data = map(json!({"VehicleBuildType": format!("EVehicleBuildType::{build_type}")}));
            assert!(
                classify(&data, StockpileCategory::Vehicles).cratable,
                "{build_type}"
            );
        }
        let data = map(json!({"VehicleBuildType": "EVehicleBuildType::Garage"}));
        assert!(!classify(&data, StockpileCategory::Vehicles).cratable);
    }

    #[test]
    fn relic_vehicle_is_not_cratable() {
        let data = map(
            json!({"CodeName": "RelicTruck", "VehicleBuildType": "EVehicleBuildType::VehicleFactory"}),
        );
        assert!(!classify(&data, StockpileCategory::Vehicles).cratable);
    }

    // ===== Structures =====

    #[test]
    fn structure_is_not_stockpilable_by_default() {
        let rules = classify(&Map::new(), StockpileCategory::Structures);
        assert_eq!(rules.category, StockpileCategory::Structures);
        assert!(!rules.included());
    }

    #[test]
    fn structure_flagged_stockpilable() {
        let data = map(json!({"bIsStockpilable": true}));
        assert!(classify(&data, StockpileCategory::Structures).stockpilable);
    }

    #[test]
    fn structure_cratable_by_build_location() {
        let data = map(json!({"BuildLocationType": "EBuildLocationType::ConstructionYard"}));
        assert!(classify(&data, StockpileCategory::Structures).cratable);
        let data = map(json!({"BuildLocationType": "EBuildLocationType::Anywhere"}));
        assert!(!classify(&data, StockpileCategory::Structures).cratable);
    }

    #[test]
    fn assembly_station_is_cratable() {
        let data = map(
            json!({"CodeName": "FacilityVehicleFactory1", "BuildLocationType": "EBuildLocationType::Anywhere"}),
        );
        assert!(classify(&data, StockpileCategory::Structures).cratable);
    }
}
