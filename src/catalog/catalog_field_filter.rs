//! Catalog field filter.
//!
//! Removes fields that carry no usable information about an entry: audio,
//! visual effects, meshes, engine components, collision/socket locations,
//! camera curves, networking settings and serialization metadata. Gameplay
//! data (damage, costs, production, health, etc.) is kept. The full entries
//! remain available on demand (`--full`).

use serde_json::{Map, Value};

/// Fields whose name ends with one of these (ignoring trailing digits) are
/// noise.
const NOISE_FIELD_SUFFIXES: &[&str] = &[
    // Audio
    "Sound",
    "Cue",
    "Loop",
    // Visual effects and meshes
    "PS",
    "FX",
    "ParticleSystem",
    "Emitter",
    "Mesh",
    "Meshes",
    // Engine components, collision and sockets
    "Component",
    "Collision",
    "Location",
    "Group",
    "Volume",
    "Box",
    // Asset curves for camera, sound pitch and handling
    "Curve",
];

/// Noise fields that do not follow a suffix pattern.
const NOISE_FIELDS: &[&str] = &[
    // Serialization metadata
    "$type",
    "UberGraphFrame",
    "PrimaryActorTick",
    "SimulatedHitNotifies",
    // Networking and logging
    "NetCullDistanceSquared",
    "NetUpdateFrequency",
    "MinNetUpdateFrequency",
    "NetDormancy",
    "RemoteRole",
    "ServerAnimTickMode",
    "StructureNetRelevancySize",
    "bReplicates",
    "bLogWhenDestroyed",
    // Visual presentation
    "Colors",
    "MeshLights",
    "ImpactEffectClass",
    "BombBayDoorState",
    "TreadInfo",
    "Treads",
    "DefaultTreadTime",
    "bInterpolatePitch",
    "bUsesImpactsMaterial",
    // UI presentation
    "ScreenToUse",
    "MapIconType",
    // Physics rig, sockets and placement offsets
    "ArmourDirectionBoneNamesList",
    "BuildOffset",
    "BuildRotation",
    "CharacterMovement",
    "CollisionCapsules",
    "DropLocationOffset",
    "Engines",
    "FuelTankerInfo",
    "GrindingCapsule",
    "Masses",
    "MuzzleInfo",
    "PassengerArea",
    "PowerSocket",
    "PropellerPositions",
    "RudderPivot",
    "Surfaces",
    "Wheels",
];

/// Fields that are noise only when their value starts with the given prefix:
/// every data-table row carries the path of its table, identical for all
/// rows of that table.
const NOISE_FIELD_VALUE_PREFIXES: &[(&str, &str)] =
    &[("ObjectPath", "War/Content/Blueprints/Data/")];

/// The game's empty value (an unset name or code name).
const EMPTY_VALUE: &str = "None";

/// Return a copy of the catalog without noise fields, removed at every
/// nesting level (inside objects and lists). Entry order is preserved and the
/// input is left untouched.
pub fn filter_noise(catalog: &[Value]) -> Vec<Value> {
    catalog.iter().map(clean_value).collect()
}

fn clean_value(value: &Value) -> Value {
    match value {
        Value::Object(m) => Value::Object(clean_map(m)),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(clean_value)
                .filter(|v| !is_empty_slot(v))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn clean_map(data: &Map<String, Value>) -> Map<String, Value> {
    data.iter()
        .filter(|(key, value)| !is_noise(key) && !is_noise_value(key, value))
        .map(|(key, value)| (key.clone(), clean_value(value)))
        .filter(|(_, value)| !is_empty_slot(value) && !is_empty_container(value))
        .collect()
}

/// Whether a cleaned value is an empty list or object, which carries no more
/// data than an unset field (e.g. a `CostPerCrate` left with only empty
/// slots, or a struct whose fields were all noise).
fn is_empty_container(value: &Value) -> bool {
    match value {
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// Whether a cleaned value is an empty resource slot: one whose code name was
/// empty, leaving only a zero quantity (e.g. `{"CodeName": "None",
/// "Quantity": 0}`).
fn is_empty_slot(value: &Value) -> bool {
    matches!(value.as_object(), Some(m) if m.len() == 1 && m.get("Quantity").and_then(Value::as_f64) == Some(0.0))
}

/// Whether a field name (e.g. `DestroyedSoundCue`, `LadderLocation1`) is
/// noise.
fn is_noise(key: &str) -> bool {
    if NOISE_FIELDS.contains(&key) {
        return true;
    }
    let stem = key.trim_end_matches(|c: char| c.is_ascii_digit());
    NOISE_FIELD_SUFFIXES
        .iter()
        .any(|suffix| stem.ends_with(suffix))
}

/// Whether a field is noise because of its value: the game's empty value, or
/// e.g. a data-table row's `ObjectPath` pointing at the table itself.
fn is_noise_value(key: &str, value: &Value) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    text == EMPTY_VALUE
        || NOISE_FIELD_VALUE_PREFIXES
            .iter()
            .any(|(field, prefix)| *field == key && text.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry() -> Value {
        json!({
            "CodeName": "TruckW",
            "FactionVariant": "EFactionId::Wardens",
            "Icon": "War/Content/Textures/UI/VehicleIcons/TruckWIcon.0",
            "SubTypeIcon": "",
            "DisplayName": "Dunne Transport",
            "Description": "A heavy-duty truck.",
            "StockpileCategory": "Vehicles",
            "bIsStockpilable": true,
            "bIsCratable": true,
            "Health": 1500,
            "VehicleDynamicData": {"MaxHealth": 1500},
            "DestroyedSoundCue": "War/Content/Audio/SFX/Vehicles/Truck/VehicleDeath_Cue.0",
            "DriveLoop": "DriveLoop",
            "MainDirtHighSpeedPS": "MainDirtHighSpeedPS",
            "DisabledTurretFX": "DisabledTurretFX",
            "BackLeftWaterEmitter": "BackLeftWaterEmitter",
            "ComponentsParticleSystem": "ComponentsParticleSystem",
            "DestroyedMesh": "DestroyedMesh",
            "BackpackMeshes": ["War/Content/Meshes/Character/Back/SK_FlamesPackC.0"],
            "StealthComponent": "StealthComponent",
            "BoxCollision": "VehicleCollision",
            "RearAxleCastLocation": "RearAxleCastLocation",
            "LadderLocation1": "LadderLocation1",
            "EngineGroup": "EngineGroup",
            "DriveLoopPitchCurve": "War/Content/Blueprints/Vehicles/TruckDriveLoopPitchCurve.0",
            "KillVolume": "KillVolume",
            "ItemBox": "ItemBox",
            "NetUpdateFrequency": 30,
            "UberGraphFrame": {},
            "TreadInfo": [{"PhysicalMaterial": "War/Content/PhysicalMaterials/DirtPhys.0"}],
        })
    }

    #[test]
    fn removes_noise_fields() {
        assert_eq!(
            filter_noise(&[entry()]),
            vec![json!({
                "CodeName": "TruckW",
                "FactionVariant": "EFactionId::Wardens",
                "Icon": "War/Content/Textures/UI/VehicleIcons/TruckWIcon.0",
                "SubTypeIcon": "",
                "DisplayName": "Dunne Transport",
                "Description": "A heavy-duty truck.",
                "StockpileCategory": "Vehicles",
                "bIsStockpilable": true,
                "bIsCratable": true,
                "Health": 1500,
                "VehicleDynamicData": {"MaxHealth": 1500},
            })]
        );
    }

    #[test]
    fn removes_nested_noise() {
        let entry = json!({
            "CodeName": "AmbulanceW",
            "Horn": {
                "Sound": "War/Content/Audio/SFX/Vehicles/Ambulance/AmbulanceHorn_Cue.0",
                "CooldownSeconds": 8,
            },
            "WeaponConfigs": [{"MaxAmmo": 40, "ShotSoundCue": "Shot_Cue.0"}],
            "Ramp": {
                "Pivot": {"$type": "UAssetAPI.UnrealTypes.FVector, UAssetAPI", "X": 565},
                "MaxExtension": 761,
            },
        });
        assert_eq!(
            filter_noise(&[entry]),
            vec![json!({
                "CodeName": "AmbulanceW",
                "Horn": {"CooldownSeconds": 8},
                "WeaponConfigs": [{"MaxAmmo": 40}],
                "Ramp": {"Pivot": {"X": 565}, "MaxExtension": 761},
            })]
        );
    }

    #[test]
    fn keeps_gameplay_data() {
        let entry = json!({
            "CodeName": "AAAmmo",
            "AmmoDynamicData": {"Damage": 1200, "ExplosionRadius": 1150},
            "ItemDynamicData": {"CostPerCrate": [{"ItemCodeName": "Cloth", "Quantity": 100}]},
            "ProductionCategories": {"Factory": "EFactoryQueueType::HeavyAmmo"},
            "BuildLocationType": "EBuildLocationType::ConstructionYard",
            "Encumbrance": 300,
        });
        assert_eq!(filter_noise(std::slice::from_ref(&entry)), vec![entry]);
    }

    #[test]
    fn removes_empty_lists_and_objects() {
        let entry = json!({
            "CodeName": "Diesel",
            "ItemDynamicData": {
                "CostPerCrate": [{"ItemCodeName": "None", "Quantity": 0}],
                "QuantityPerCrate": 100,
            },
            "Tags": [],
            "ProfileData": {"ObjectPath": "War/Content/Blueprints/Data/BPItemProfileTable"},
        });
        assert_eq!(
            filter_noise(&[entry]),
            vec![json!({
                "CodeName": "Diesel",
                "ItemDynamicData": {"QuantityPerCrate": 100},
            })]
        );
    }

    #[test]
    fn removes_actor_network_role_and_dormancy() {
        let entry = json!({
            "CodeName": "AssaultRifleHeavyC",
            "RemoteRole": "ROLE_None",
            "NetDormancy": "DORM_Awake",
        });
        assert_eq!(
            filter_noise(&[entry]),
            vec![json!({"CodeName": "AssaultRifleHeavyC"})]
        );
    }

    #[test]
    fn removes_ui_presentation_fields() {
        let entry = json!({
            "CodeName": "FacilityVehicleFactory1",
            "ScreenToUse": "ESimScreen::AssemblyStation",
            "MapIconType": "EMapIconType::FacilityVehicleFactory1",
        });
        assert_eq!(
            filter_noise(&[entry]),
            vec![json!({"CodeName": "FacilityVehicleFactory1"})]
        );
    }

    #[test]
    fn removes_data_table_object_path() {
        let entry = json!({
            "ObjectPath": "War/Content/Blueprints/ItemPickups/BPRifle",
            "ItemDynamicData": {
                "ObjectPath": "War/Content/Blueprints/Data/BPItemDynamicData",
                "QuantityPerCrate": 20,
            },
        });
        assert_eq!(
            filter_noise(&[entry]),
            vec![json!({
                "ObjectPath": "War/Content/Blueprints/ItemPickups/BPRifle",
                "ItemDynamicData": {"QuantityPerCrate": 20},
            })]
        );
    }

    #[test]
    fn keeps_referenced_object_paths() {
        let entry = json!({
            "AmmoDynamicData": {
                "DamageType": {"ObjectPath": "War/Content/Blueprints/DamageTypes/BPLightKineticDamageType.0"},
            },
            "ItemComponentClass": {"ObjectPath": "War/Content/Blueprints/Items/BPAAAmmoComponent.0"},
            "ParentBlueprint": "War/Content/Blueprints/ItemPickups/Uniforms/BPUniformPickup.0",
            "Config": {"MaxDistance": 300},
        });
        assert_eq!(filter_noise(std::slice::from_ref(&entry)), vec![entry]);
    }

    #[test]
    fn removes_none_values() {
        let entry = json!({
            "CodeName": "FieldATC",
            "FuelTank": "None",
            "Modifications": {"EFortModificationType::MotorPool": [
                {"CodeName": "ArmoredCarOffensiveC", "CrateCodeName": "None", "RequiredCodeName": "ArmoredCarC"},
            ]},
        });
        assert_eq!(
            filter_noise(&[entry]),
            vec![json!({
                "CodeName": "FieldATC",
                "Modifications": {"EFortModificationType::MotorPool": [
                    {"CodeName": "ArmoredCarOffensiveC", "RequiredCodeName": "ArmoredCarC"},
                ]},
            })]
        );
    }

    #[test]
    fn keeps_list_positions_when_removing_none_values() {
        let entry = json!({"BuildSteps": [
            {"NextStructureCodeName": "None", "RequiredCodeName": "None"},
            {"NextStructureCodeName": "None", "RequiredCodeName": "FortStructurePart"},
        ]});
        assert_eq!(
            filter_noise(&[entry]),
            vec![json!({"BuildSteps": [{}, {"RequiredCodeName": "FortStructurePart"}]})]
        );
    }

    #[test]
    fn removes_empty_resource_slots() {
        let entry = json!({
            "VehicleDynamicData": {
                "ResourceAmounts": {"OtherResources": [], "Resource": {"CodeName": "None", "Quantity": 0}},
                "AltResourceAmounts": {
                    "OtherResources": [{"CodeName": "FacilityMaterials5", "Quantity": 10}],
                    "Resource": {"CodeName": "None", "Quantity": 0},
                },
            },
            "ItemDynamicData": {"CostPerCrate": [{"ItemCodeName": "None", "Quantity": 0}, {"ItemCodeName": "Cloth", "Quantity": 100}]},
        });
        assert_eq!(
            filter_noise(&[entry]),
            vec![json!({
                "VehicleDynamicData": {
                    "AltResourceAmounts": {"OtherResources": [{"CodeName": "FacilityMaterials5", "Quantity": 10}]},
                },
                "ItemDynamicData": {"CostPerCrate": [{"ItemCodeName": "Cloth", "Quantity": 100}]},
            })]
        );
    }

    #[test]
    fn keeps_zero_quantity_with_code_name() {
        let entry = json!({"Resource": {"CodeName": "Cloth", "Quantity": 0}});
        assert_eq!(filter_noise(std::slice::from_ref(&entry)), vec![entry]);
    }

    #[test]
    fn does_not_mutate_input() {
        let catalog = vec![entry()];
        filter_noise(&catalog);
        assert_eq!(catalog, vec![entry()]);
    }

    #[test]
    fn preserves_order() {
        let catalog = vec![json!({"CodeName": "B"}), json!({"CodeName": "A"})];
        assert_eq!(filter_noise(&catalog), catalog);
    }

    #[test]
    fn empty_catalog() {
        assert!(filter_noise(&[]).is_empty());
    }
}
